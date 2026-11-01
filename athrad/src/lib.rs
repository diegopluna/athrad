//! The Athrad pipeline: IR in, executable out.
//!
//! ```text
//! ir::Module ──verify──▶ codegen (Cranelift) ──▶ link::write_object ──▶ link::Linker ──▶ exe
//! ```
//!
//! Until milestone 1 there is no Rust frontend: programs arrive as IR built by
//! hand. A program defines [`rt::USER_MAIN`], the body of Rust's `fn main`;
//! [`rt::add_runtime`] supplies the C `main` that calls it, the way
//! `std::rt::lang_start` does for `rustc`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use link::{Linker, ObjectFormat};

pub mod rt;
pub mod watch;

/// Build `program` into the executable at `output` with `linker`, keeping
/// intermediate files in `work_dir`.
///
/// The linker is an argument so a caller finds it once: finding the host C
/// toolchain ([`link::HostCc::new`]) costs more than the link itself.
pub fn build(program: &ir::Module, linker: &dyn Linker, work_dir: &Path, output: &Path) -> Result<()> {
    let format = ObjectFormat::host();
    let mut module = program.clone();
    rt::add_runtime(&mut module, format);
    ir::verify(&module).context("invalid IR")?;

    let stem = output.file_stem().and_then(|s| s.to_str()).unwrap_or("athrad");
    let obj = codegen::compile(&module, stem)?;

    std::fs::create_dir_all(work_dir).with_context(|| format!("creating {}", work_dir.display()))?;
    let obj_path: PathBuf = work_dir.join(format!("{stem}.{}", format.extension()));
    link::write_object(&obj, format, &obj_path)?;
    linker.link(&[obj_path], output)
}
