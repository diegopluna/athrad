//! `athrad`, the command.
//!
//! * `athrad build [CARGO ARGS]`, `athrad run [CARGO ARGS]`: `cargo build`
//!   and `cargo run` with Athrad compiling every crate, the package's and
//!   its dependencies'. Cargo runs `athrad-driver` as its `RUSTC_WRAPPER`,
//!   and the output goes to `target/athrad`, apart from `rustc`'s.
//! * `athrad build --release --profile PROFILE [CARGO ARGS]`: a release
//!   build split by a run profile. LLVM optimizes the functions the profile
//!   found hot, Athrad compiles the rest. `PROFILE` is LLVM's: a `.profdata`
//!   file, or a directory of `.profraw` files.
//! * `athrad hot PROFILE -o FILE.txt`: the hot functions of a profile, for
//!   `--profile FILE.txt`, which then reads nothing else.
//! * `athrad build --release --profile-generate DIR [CARGO ARGS]`: the build
//!   to collect that profile with: `rustc` and LLVM, instrumented, into
//!   `target/athrad-profile`. Its programs write `.profraw` files into
//!   `DIR` when they run. LLVM's pre-inliner is off, so every function
//!   keeps a count of its own: one inlined into its callers before it was
//!   counted would look cold.
//! * `athrad FILE.rs [-o OUT]`, `athrad watch FILE.rs [-o OUT] [--run]`: one
//!   file, without Cargo; see `athrad-driver`.
//!
//! This program does not link the compiler, so it starts in a millisecond
//! (`rustc_driver` takes 20 ms to load); it hands everything to
//! `athrad-driver`, which sits beside it.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let me = std::env::current_exe().map_err(|e| format!("finding the athrad executable: {e}"))?;
    let driver = me.with_file_name(format!("athrad-driver{}", std::env::consts::EXE_SUFFIX));
    if !driver.exists() {
        return Err(format!("{} is missing", driver.display()));
    }
    if args.first().map(String::as_str) == Some("hot") {
        // `athrad hot PROFILE -o FILE`: the hot list, once, for `--profile`.
        let (mut profile, mut out) = (None, None);
        let mut it = args[1..].iter();
        while let Some(a) = it.next() {
            if a == "-o" {
                out = it.next().map(PathBuf::from);
            } else {
                profile = Some(PathBuf::from(a));
            }
        }
        let (Some(profile), Some(out)) = (profile, out) else {
            return Err("usage: athrad hot PROFILE -o FILE.txt".into());
        };
        let dir = std::env::temp_dir().join(format!("athrad-hot-{}", std::process::id()));
        let list = hot_list(&profile, &dir)?;
        std::fs::copy(&list, &out).map_err(|e| format!("writing {}: {e}", out.display()))?;
        let _ = std::fs::remove_dir_all(&dir);
        return Ok(ExitCode::SUCCESS);
    }
    let status = match args.first().map(String::as_str) {
        Some(cmd @ ("build" | "run"))
            if args.iter().any(|a| a == "--profile-generate" || a.starts_with("--profile-generate=")) =>
        {
            let mut dir = None;
            let mut rest = Vec::new();
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                if a == "--profile-generate" {
                    dir = it.next().cloned();
                } else if let Some(d) = a.strip_prefix("--profile-generate=") {
                    dir = Some(d.to_owned());
                } else {
                    rest.push(a.clone());
                }
            }
            let dir =
                std::path::absolute(dir.ok_or("`--profile-generate` needs a directory")?).map_err(|e| e.to_string())?;
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let mut c = Command::new(&cargo);
            let mut flags = std::env::var("RUSTFLAGS").unwrap_or_default();
            flags.push_str(&format!(" -Cprofile-generate={} -Cllvm-args=-disable-preinline", dir.display()));
            c.arg(cmd).args(&rest).env("RUSTFLAGS", flags.trim());
            if std::env::var_os("CARGO_TARGET_DIR").is_none() && !rest.iter().any(|a| a.starts_with("--target-dir")) {
                let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
                c.env("CARGO_TARGET_DIR", workspace_root(&cwd)?.join("target").join("athrad-profile"));
            }
            c.status().map_err(|e| format!("running {}: {e}", cargo.to_string_lossy()))?
        }
        Some(cmd @ ("build" | "run")) => {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let (profile, cargo_args) = take_profile(&args[1..])?;
            let mut c = Command::new(&cargo);
            c.arg(cmd).args(&cargo_args).env("RUSTC_WRAPPER", &driver);
            let target_dir = match std::env::var_os("CARGO_TARGET_DIR") {
                Some(dir) => Some(PathBuf::from(dir)),
                None if args.iter().any(|a| a.starts_with("--target-dir")) => None,
                None => {
                    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
                    let dir = workspace_root(&cwd)?.join("target").join("athrad");
                    c.env("CARGO_TARGET_DIR", &dir);
                    Some(dir)
                }
            };
            if let Some(profile) = profile {
                if !cargo_args.iter().any(|a| a == "--release" || a == "-r") {
                    return Err("`--profile` splits a release build: add `--release`".into());
                }
                let dir = target_dir.unwrap_or_else(std::env::temp_dir);
                let hot = hot_list(&profile, &dir)?;
                c.env("ATHRAD_HOT", hot);
            }
            c.status().map_err(|e| format!("running {}: {e}", cargo.to_string_lossy()))?
        }
        _ => Command::new(&driver).args(args).status().map_err(|e| format!("running {}: {e}", driver.display()))?,
    };
    Ok(match status.code() {
        Some(0) => ExitCode::SUCCESS,
        Some(n) => ExitCode::from(u8::try_from(n).unwrap_or(1)),
        None => ExitCode::FAILURE,
    })
}

/// `--profile PATH` (or `--profile=PATH`) out of the Cargo arguments.
fn take_profile(args: &[String]) -> Result<(Option<PathBuf>, Vec<String>), String> {
    let mut profile = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--profile" {
            profile = Some(PathBuf::from(it.next().ok_or("`--profile` needs a path")?));
        } else if let Some(p) = a.strip_prefix("--profile=") {
            profile = Some(PathBuf::from(p));
        } else {
            rest.push(a.clone());
        }
    }
    Ok((profile, rest))
}

/// How many blocks a function must have executed, over the profiled runs,
/// to be hot. Below it, code Cranelift makes even ten times slower adds
/// about a millisecond; a hot function sent to Cranelift instead costs the
/// whole run several times over, so the line errs toward hot.
const HOT_MIN: u128 = 10_000;

/// The hot functions of an LLVM profile, written one symbol per line into
/// `dir`: every function that executed at least [`HOT_MIN`] blocks
/// (`ATHRAD_HOT_MIN` overrides it).
fn hot_list(profile: &Path, dir: &Path) -> Result<PathBuf, String> {
    // A hot list already (from `athrad hot`): use it as it is.
    if profile.extension().is_some_and(|x| x == "txt") {
        return std::path::absolute(profile).map_err(|e| e.to_string());
    }
    let profdata = llvm_profdata()?;
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let merged = if profile.is_dir() {
        let raws: Vec<PathBuf> = std::fs::read_dir(profile)
            .map_err(|e| format!("reading {}: {e}", profile.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "profraw"))
            .collect();
        if raws.is_empty() {
            return Err(format!("no .profraw files in {}", profile.display()));
        }
        let out = dir.join("athrad.profdata");
        let st = Command::new(&profdata).arg("merge").arg("-o").arg(&out).args(&raws).status();
        if !st.map_err(|e| e.to_string())?.success() {
            return Err("llvm-profdata merge failed".into());
        }
        out
    } else {
        profile.to_owned()
    };
    let out = Command::new(&profdata)
        .args(["show", "--all-functions", "--counts"])
        .arg(&merged)
        .output()
        .map_err(|e| format!("running {}: {e}", profdata.display()))?;
    if !out.status.success() {
        return Err(format!("llvm-profdata show failed:\n{}", String::from_utf8_lossy(&out.stderr)));
    }
    let min = std::env::var("ATHRAD_HOT_MIN").ok().and_then(|s| s.parse().ok()).unwrap_or(HOT_MIN);
    let hot = hottest(&String::from_utf8_lossy(&out.stdout), min);
    let path = dir.join("athrad-hot.txt");
    std::fs::write(&path, hot.join("\n")).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(path)
}

/// From `llvm-profdata show --all-functions --counts`: the symbols that
/// executed at least `min` blocks (a function's entry is one of its blocks).
/// A function with internal linkage is listed as `UNIT;SYMBOL`.
fn hottest(show: &str, min: u128) -> Vec<String> {
    // A function's entry count, then its other blocks' counts; a function
    // of one block has only the entry count.
    let mut funcs: Vec<(String, u128)> = Vec::new();
    for line in show.lines() {
        let t = line.trim();
        if line.starts_with("  ") && !line.starts_with("   ") && line.ends_with(':') {
            let n = &line[2..line.len() - 1];
            funcs.push((n.rsplit(';').next().unwrap_or(n).to_owned(), 0));
        } else if let Some(n) = t.strip_prefix("Function count: ") {
            if let (Some(f), Ok(n)) = (funcs.last_mut(), n.trim().parse::<u128>()) {
                f.1 += n;
            }
        } else if let Some(counts) = t.strip_prefix("Block counts: [") {
            let total: u128 =
                counts.trim_end_matches(']').split(',').filter_map(|c| c.trim().parse::<u128>().ok()).sum();
            if let Some(f) = funcs.last_mut() {
                f.1 += total;
            }
        }
    }
    let mut hot: Vec<String> = funcs.into_iter().filter(|f| f.1 >= min).map(|f| f.0).collect();
    hot.sort();
    hot.dedup();
    hot
}

/// `llvm-profdata` from the `llvm-tools` of the toolchain Athrad was built
/// with, which is the one it runs: the driver links that toolchain's
/// compiler.
fn llvm_profdata() -> Result<PathBuf, String> {
    let tool = Path::new(env!("ATHRAD_SYSROOT"))
        .join("lib/rustlib")
        .join(env!("ATHRAD_HOST"))
        .join("bin")
        .join(format!("llvm-profdata{}", std::env::consts::EXE_SUFFIX));
    if tool.exists() {
        Ok(tool)
    } else {
        Err(format!("{} is missing: `rustup component add llvm-tools`", tool.display()))
    }
}

/// The directory of the workspace the current directory is in, as Cargo
/// finds it: the nearest `Cargo.toml` up from `dir`, or an ancestor's that
/// has a `[workspace]` table. (Asking `cargo locate-project` costs 37 ms.)
fn workspace_root(dir: &Path) -> Result<PathBuf, String> {
    let mut package = None;
    for d in dir.ancestors() {
        let Ok(text) = std::fs::read_to_string(d.join("Cargo.toml")) else { continue };
        if text.lines().any(|l| l.trim() == "[workspace]") {
            return Ok(d.to_owned());
        }
        package.get_or_insert_with(|| d.to_owned());
    }
    package.ok_or_else(|| "could not find `Cargo.toml` in this directory or any above it".into())
}
