//! `rustc` is the oracle.
//!
//! For each program: compile its Rust source with `rustc`, build its IR with
//! Athrad, run both executables, and compare stdout, stderr, and exit status.
//! Any difference is an Athrad bug.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};

pub mod programs;

/// A program both compilers must agree on.
pub struct Program {
    pub name: &'static str,
    /// The Rust source `rustc` compiles.
    pub source: &'static str,
    /// The same program as Athrad IR. Until milestone 1 this is written by
    /// hand; after it, it comes from the MIR of `source`.
    pub ir: fn() -> ir::Module,
}

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
    let dir = work_dir.join(program.name);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let rustc_exe = build_with_rustc(program, &dir.join("rustc"))?;
    let athrad_exe = build_with_athrad(program, &dir.join("athrad"))?;

    let rustc = run(&rustc_exe)?;
    let athrad = run(&athrad_exe)?;
    if rustc != athrad {
        bail!("`{}`: Athrad disagrees with rustc\nrustc:\n{rustc}\nathrad:\n{athrad}", program.name);
    }
    Ok(Agreement { rustc, athrad })
}

/// Compile with `rustc` as `cargo build` would for a debug binary.
pub fn build_with_rustc(program: &Program, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let src = dir.join(format!("{}.rs", program.name));
    std::fs::write(&src, program.source)?;
    let exe = dir.join(link::executable_name(program.name));
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let out = Command::new(&rustc)
        .args(["--edition", "2024", "--crate-type", "bin", "--crate-name", program.name])
        .arg("-o")
        .arg(&exe)
        .arg(&src)
        .output()
        .context("running rustc")?;
    if !out.status.success() {
        bail!("rustc rejected `{}`:\n{}", program.name, String::from_utf8_lossy(&out.stderr));
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

pub fn build_with_athrad(program: &Program, dir: &Path) -> Result<PathBuf> {
    let exe = dir.join(link::executable_name(program.name));
    athrad::build(&(program.ir)(), linker()?, dir, &exe)
        .with_context(|| format!("athrad failed on `{}`", program.name))?;
    Ok(exe)
}

fn run(exe: &Path) -> Result<Run> {
    let out = Command::new(exe).output().with_context(|| format!("running {}", exe.display()))?;
    Ok(out.into())
}
