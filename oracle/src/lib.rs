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
    athrad::build(source, linker()?, dir, &exe).with_context(|| format!("athrad failed on `{name}`"))?;
    Ok(exe)
}

fn run(exe: &Path) -> Result<Run> {
    let out = Command::new(exe).output().with_context(|| format!("running {}", exe.display()))?;
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
