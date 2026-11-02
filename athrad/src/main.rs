//! `athrad`, the command.
//!
//! * `athrad build [CARGO ARGS]`, `athrad run [CARGO ARGS]`: `cargo build`
//!   and `cargo run` with Athrad compiling every crate, the package's and
//!   its dependencies'. Cargo runs `athrad-driver` as its `RUSTC_WRAPPER`,
//!   and the output goes to `target/athrad`, apart from `rustc`'s.
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
    let status = match args.first().map(String::as_str) {
        Some(cmd @ ("build" | "run")) => {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let mut c = Command::new(&cargo);
            c.arg(cmd).args(&args[1..]).env("RUSTC_WRAPPER", &driver);
            if std::env::var_os("CARGO_TARGET_DIR").is_none() && !args.iter().any(|a| a.starts_with("--target-dir")) {
                let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
                c.env("CARGO_TARGET_DIR", workspace_root(&cwd)?.join("target").join("athrad"));
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
