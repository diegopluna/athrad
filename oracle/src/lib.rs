//! `rustc` is the oracle.
//!
//! For each program in `oracle/programs`: compile it with `rustc` and with
//! Athrad, run both executables, and compare stdout, stderr, and exit status.
//! Any difference is an Athrad bug.

// Athrad runs rustc in-process, so everything that links it links rustc_driver.
#![feature(rustc_private)]

extern crate rustc_driver;

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};

/// A program both compilers must agree on: one Rust file.
#[derive(Clone, Debug)]
pub struct Program {
    pub name: String,
    pub path: PathBuf,
}

/// The programs in `oracle/programs`, sorted by name. Athrad must build
/// each one and agree with `rustc`.
pub fn programs() -> Result<Vec<Program>> {
    programs_in(&Path::new(env!("CARGO_MANIFEST_DIR")).join("programs"))
}

/// The programs in `oracle/programs/later`: ones Athrad may still reject.
/// When it builds one, it must agree with `rustc` all the same.
pub fn later_programs() -> Result<Vec<Program>> {
    programs_in(&Path::new(env!("CARGO_MANIFEST_DIR")).join("programs/later"))
}

fn programs_in(dir: &Path) -> Result<Vec<Program>> {
    let mut programs = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "rs") {
            let name = path.file_stem().and_then(|s| s.to_str()).context("program name")?.to_owned();
            programs.push(Program { name, path });
        }
    }
    programs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(programs)
}

/// The program named `name` in `oracle/programs`.
pub fn program(name: &str) -> Result<Program> {
    programs()?.into_iter().find(|p| p.name == name).with_context(|| format!("no program `{name}`"))
}

/// Athrad refused to build a program: an honest "not yet", as opposed to a
/// disagreement with `rustc`.
#[derive(Debug)]
pub struct Rejected(pub String);

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Rejected {}

/// What one run of an executable did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// `None` when the process was killed by a signal.
    pub code: Option<i32>,
}

impl From<Output> for Run {
    fn from(out: Output) -> Self {
        Run { stdout: out.stdout, stderr: out.stderr, code: out.status.code() }
    }
}

impl fmt::Display for Run {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "  exit:   {:?}", self.code)?;
        writeln!(f, "  stdout: {:?}", String::from_utf8_lossy(&self.stdout))?;
        write!(f, "  stderr: {:?}", String::from_utf8_lossy(&self.stderr))
    }
}

/// Both runs, when they agree.
#[derive(Debug)]
pub struct Agreement {
    pub rustc: Run,
    pub athrad: Run,
}

/// Build `program` both ways inside `work_dir`, run both, and fail unless
/// they behave the same.
pub fn check(program: &Program, work_dir: &Path) -> Result<Agreement> {
    check_sources(&program.name, &program.path, &program.path, work_dir)
}

/// Like [`check`], with Athrad's own frontend allowed to build the program
/// (it falls back to `rustc` for anything outside its subset). Returns
/// whether it did, with the agreement. The runtime it links with is built
/// once in `work_dir/runtime`.
pub fn check_subset(program: &Program, work_dir: &Path) -> Result<(Agreement, bool)> {
    let name = &program.name;
    let dir = work_dir.join("subset").join(name);
    std::fs::create_dir_all(dir.join("src"))?;
    let src = dir.join("src").join(format!("{name}.rs"));
    std::fs::copy(&program.path, &src)?;
    let rustc_exe = build_with_rustc(name, &src, &dir.join("rustc"))?;
    let exe = dir.join("athrad").join(link::executable_name(name));
    std::fs::create_dir_all(dir.join("athrad"))?;
    let mut caches = athrad::Caches { runtime: Some(work_dir.join("runtime")), ..Default::default() };
    let t = athrad::build_cached(&src, linker()?, &mut caches, &dir.join("work"), &exe)
        .map_err(|e| Rejected(format!("{e:#}")))?;
    let (rustc, athrad) = (run(&rustc_exe)?, run(&exe)?);
    if rustc != athrad {
        let how = if t.subset { "Athrad's own frontend" } else { "Athrad" };
        bail!("`{name}`: {how} disagrees with rustc\nrustc:\n{rustc}\nathrad:\n{athrad}");
    }
    Ok((Agreement { rustc, athrad }, t.subset))
}

/// Like [`check`], with a different source for each compiler: how the
/// oracle's own test makes Athrad wrong on purpose.
pub fn check_sources(name: &str, rustc_src: &Path, athrad_src: &Path, work_dir: &Path) -> Result<Agreement> {
    let dir = work_dir.join(name);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // Panic messages carry the source path as the compiler was given it, so
    // both compilers get the same path: `dir/src/NAME.rs`.
    let src = dir.join("src").join(format!("{name}.rs"));
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::copy(rustc_src, &src).with_context(|| format!("copying {}", rustc_src.display()))?;
    let rustc_exe = build_with_rustc(name, &src, &dir.join("rustc"))?;
    std::fs::copy(athrad_src, &src).with_context(|| format!("copying {}", athrad_src.display()))?;
    let athrad_exe = build_with_athrad(name, &src, &dir.join("athrad")).map_err(|e| Rejected(format!("{e:#}")))?;

    let rustc = run(&rustc_exe)?;
    let athrad = run(&athrad_exe)?;
    if rustc != athrad {
        bail!("`{name}`: Athrad disagrees with rustc\nrustc:\n{rustc}\nathrad:\n{athrad}");
    }
    Ok(Agreement { rustc, athrad })
}

/// The `// edit: FROM => TO` lines of a program, in order. One edit may
/// make several replacements at once, separated by ` ;; `, for a change
/// that only compiles whole.
pub fn edits(program: &Program) -> Result<Vec<Vec<(String, String)>>> {
    let text = std::fs::read_to_string(&program.path)?;
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("// edit:") {
            let mut edit = Vec::new();
            for part in rest.split(" ;; ") {
                let (from, to) = part.split_once("=>").with_context(|| format!("an edit without `=>`: {line}"))?;
                edit.push((from.trim().to_owned(), to.trim().to_owned()));
            }
            out.push(edit);
        }
    }
    Ok(out)
}

/// What [`check_edits`] saw Athrad reuse, over the rebuilds after edits.
#[derive(Clone, Copy, Debug, Default)]
pub struct Reuse {
    pub builds: usize,
    pub reused_units: usize,
    pub cached_functions: usize,
    pub compiled_functions: usize,
}

/// The edit loop under the oracle: build `program`, then make each of its
/// [`edits`] in turn (each to the first line of code with its `FROM`), and
/// rebuild with Athrad's caches warm from the build before. Every build must
/// agree with `rustc`'s build of the same text.
pub fn check_edits(program: &Program, work_dir: &Path) -> Result<Reuse> {
    let name = &program.name;
    let edits = edits(program)?;
    let dir = work_dir.join(format!("{name}-edits"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src"))?;
    let src = dir.join("src").join(format!("{name}.rs"));
    let mut lines: Vec<String> = std::fs::read_to_string(&program.path)?.lines().map(str::to_owned).collect();
    let mut caches = athrad::Caches { code: Default::default(), incremental: Some(dir.join("incr")), runtime: None };
    let mut reuse = Reuse::default();
    for step in 0..=edits.len() {
        if step > 0 {
            for (from, to) in &edits[step - 1] {
                let line = lines
                    .iter_mut()
                    .find(|l| !l.trim_start().starts_with("// edit:") && l.contains(from.as_str()))
                    .with_context(|| format!("`{name}`: edit {step} finds no `{from}`"))?;
                *line = line.replacen(from.as_str(), to, 1);
            }
        }
        std::fs::write(&src, lines.join("\n") + "\n")?;
        let rustc_exe = build_with_rustc(name, &src, &dir.join("rustc"))?;
        let exe = dir.join("athrad").join(link::executable_name(name));
        std::fs::create_dir_all(dir.join("athrad"))?;
        let t = athrad::build_cached(&src, linker()?, &mut caches, &dir.join("work"), &exe)
            .with_context(|| format!("athrad failed on `{name}` after edit {step}"))?;
        if step > 0 {
            reuse.builds += 1;
            reuse.reused_units += t.reused_units;
            reuse.cached_functions += t.cache.hits;
            reuse.compiled_functions += t.cache.misses;
        }
        let (rustc, athrad) = (run(&rustc_exe)?, run(&exe)?);
        if rustc != athrad {
            bail!("`{name}` after edit {step}: Athrad disagrees with rustc\nrustc:\n{rustc}\nathrad:\n{athrad}");
        }
    }
    Ok(reuse)
}

/// Compile with `rustc` as `cargo build` would for a debug binary.
pub fn build_with_rustc(name: &str, source: &Path, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let exe = dir.join(link::executable_name(name));
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let out = Command::new(&rustc)
        // The workspace sets this for its own rustc_private crates; the
        // oracle is stable rustc.
        .env_remove("RUSTC_BOOTSTRAP")
        .args(["--edition", "2024", "--crate-type", "bin", "--crate-name", name])
        .arg("-o")
        .arg(&exe)
        .arg(source)
        .output()
        .context("running rustc")?;
    if !out.status.success() {
        bail!("rustc rejected `{name}`:\n{}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(exe)
}

/// The host linker, found once per process and shared by every build.
pub fn linker() -> Result<&'static link::HostCc> {
    static LINKER: OnceLock<link::HostCc> = OnceLock::new();
    if let Some(l) = LINKER.get() {
        return Ok(l);
    }
    let found = link::HostCc::new()?;
    Ok(LINKER.get_or_init(|| found))
}

/// Build with Athrad, in this process.
pub fn build_with_athrad(name: &str, source: &Path, dir: &Path) -> Result<PathBuf> {
    let exe = dir.join(link::executable_name(name));
    athrad::build(source, linker()?, dir, &exe).map(drop).with_context(|| format!("athrad failed on `{name}`"))?;
    Ok(exe)
}

fn run(exe: &Path) -> Result<Run> {
    // A backtrace depends on debug info, which Athrad does not write yet,
    // and is not part of what a program does. Run both without one.
    let out = Command::new(exe)
        .env_remove("RUST_BACKTRACE")
        .env_remove("RUST_LIB_BACKTRACE")
        .output()
        .with_context(|| format!("running {}", exe.display()))?;
    let mut run = Run::from(out);
    run.stderr = mask_thread_ids(&run.stderr);
    Ok(run)
}

/// Panic messages name the thread with its OS id, `thread 'main' (12345)`,
/// which differs from run to run. Replace each id with `N`.
pub fn mask_thread_ids(stderr: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(stderr);
    let mut out = String::with_capacity(text.len());
    let mut rest = &*text;
    while let Some(at) = rest.find("thread '") {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        // `thread '<name>' (<digits>)`
        let masked = rest["thread '".len()..].find("' (").and_then(|q| {
            let open = "thread '".len() + q + "' (".len();
            let digits = rest[open..].bytes().take_while(u8::is_ascii_digit).count();
            (digits > 0 && rest[open + digits..].starts_with(')')).then_some((open, open + digits))
        });
        match masked {
            Some((open, close)) => {
                out.push_str(&rest[..open]);
                out.push('N');
                rest = &rest[close..];
            }
            None => {
                out.push_str("thread '");
                rest = &rest["thread '".len()..];
            }
        }
    }
    out.push_str(rest);
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    #[test]
    fn masks_thread_ids() {
        let s = b"\nthread 'main' (149409625) panicked at a.rs:2:5:\nthread 'x' (no)\n";
        assert_eq!(super::mask_thread_ids(s), b"\nthread 'main' (N) panicked at a.rs:2:5:\nthread 'x' (no)\n");
    }
}

/// A Cargo project the oracle builds with `cargo build` and with
/// `athrad build`, in `oracle/crates`.
#[derive(Clone, Debug)]
pub struct Crate {
    pub name: String,
    pub dir: PathBuf,
}

/// The projects in `oracle/crates`, sorted by name.
pub fn crates() -> Result<Vec<Crate>> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.join("Cargo.toml").exists() {
            let name = path.file_name().and_then(|s| s.to_str()).context("crate name")?.to_owned();
            out.push(Crate { name, dir: path });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Build `krate` with `cargo build` (that is, `rustc`) and with `athrad
/// build` (the `athrad` executable at `athrad`), run the binary each makes,
/// and fail unless they agree. Then make each edit in the project's `edits`
/// file in turn (`FILE: FROM ==> TO`), rebuilding both and comparing again,
/// so Athrad's incremental rebuilds across crates are checked too. Returns
/// how many builds agreed.
pub fn check_crate(krate: &Crate, athrad: &Path, work_dir: &Path) -> Result<usize> {
    let dir = work_dir.join("crates").join(&krate.name);
    let _ = std::fs::remove_dir_all(&dir);
    copy_tree(&krate.dir, &dir)?;
    let edits: Vec<Vec<(String, String, String)>> = match std::fs::read_to_string(dir.join("edits")) {
        Ok(text) => text
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .map(|l| {
                l.split(" ;; ")
                    .map(|part| {
                        let (file, rest) = part.split_once(": ").context("an edit without `FILE: `")?;
                        let (from, to) = rest.split_once(" ==> ").context("an edit without ` ==> `")?;
                        Ok((file.to_owned(), from.to_owned(), to.to_owned()))
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?,
        Err(_) => Vec::new(),
    };
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let build = |wrapper: Option<&Path>| -> Result<PathBuf> {
        let mut c = Command::new(wrapper.unwrap_or(Path::new(&cargo)));
        if wrapper.is_some() {
            c.env("CARGO", &cargo);
        }
        c.current_dir(&dir)
            .arg("build")
            .args(["--quiet", "--message-format", "short"])
            .env_remove("RUSTC_BOOTSTRAP")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("RUSTFLAGS");
        let out = c.output().context("running cargo")?;
        let who = if wrapper.is_some() { "athrad build" } else { "cargo build" };
        if !out.status.success() {
            bail!("{who} failed on `{}`:\n{}", krate.name, String::from_utf8_lossy(&out.stderr));
        }
        let target = if wrapper.is_some() { dir.join("target/athrad/debug") } else { dir.join("target/debug") };
        // The project's one binary.
        let bins: Vec<PathBuf> = std::fs::read_dir(&target)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file() && is_executable(p))
            .collect();
        match bins.as_slice() {
            [one] => Ok(one.clone()),
            _ => bail!("{who} of `{}` made {} executables", krate.name, bins.len()),
        }
    };
    let mut agreed = 0;
    for step in 0..=edits.len() {
        for (file, from, to) in edits.get(step.wrapping_sub(1)).into_iter().flatten() {
            let path = dir.join(file);
            let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            if !text.contains(from.as_str()) {
                bail!("`{}`: edit {step} finds no `{from}` in {file}", krate.name);
            }
            std::fs::write(&path, text.replacen(from.as_str(), to, 1))?;
        }
        let rustc_exe = build(None)?;
        let athrad_exe = build(Some(athrad)).map_err(|e| Rejected(format!("{e:#}")))?;
        let (rustc, athrad_run) = (run(&rustc_exe)?, run(&athrad_exe)?);
        if rustc != athrad_run {
            bail!(
                "`{}` after edit {step}: Athrad disagrees with rustc\nrustc:\n{rustc}\nathrad:\n{athrad_run}",
                krate.name
            );
        }
        agreed += 1;
    }
    Ok(agreed)
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if entry.file_name() == "target" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

fn is_executable(p: &Path) -> bool {
    if cfg!(windows) {
        return p.extension().is_some_and(|e| e == "exe");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
    }
    #[allow(unreachable_code)]
    false
}
