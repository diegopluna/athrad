//! The Athrad pipeline: Rust source in, executable out.
//!
//! ```text
//! .rs ──rustc (MIR)──▶ frontend ──▶ ir ──verify──▶ codegen (Cranelift) ──▶ link::write_object ──▶ link::Linker ──▶ exe
//!                                                                     std's rlibs and system libraries ──┘
//! ```
//!
//! `rustc` parses, type-checks, borrow-checks, and monomorphizes; Athrad
//! generates the code and links it against the real `std`.

#![feature(rustc_private)]

extern crate rustc_driver;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use link::{Linker, ObjectFormat};

pub mod watch;

/// Build the binary crate rooted at `source` into the executable `output`
/// with `linker`, keeping intermediate files in `work_dir`.
///
/// `source` reaches `rustc` as given, and `rustc` writes it into panic
/// messages as given: build with the same path `rustc` gets to agree with it.
pub fn build(source: &Path, linker: &dyn Linker, work_dir: &Path, output: &Path) -> Result<()> {
    let stem = output.file_stem().and_then(|s| s.to_str()).unwrap_or("athrad");
    let lowered = lower(source, stem)?;
    build_module(&lowered.module, lowered.link, linker, work_dir, output)
}

/// `rustc`'s view of `source`, lowered to IR. `rustc`'s own messages go to
/// stderr.
pub fn lower(source: &Path, crate_name: &str) -> Result<frontend::Output> {
    // rustc's driver keeps global state; run one compilation at a time.
    static RUSTC: Mutex<()> = Mutex::new(());
    let _one = RUSTC.lock().unwrap_or_else(|e| e.into_inner());
    let args: Vec<String> =
        ["athrad", "--edition", "2024", "--crate-type", "bin", "--crate-name", crate_name, &source.to_string_lossy()]
            .iter()
            .map(|s| s.to_string())
            .collect();
    frontend::compile(&args)
}

/// Generate code for `module` and link it with `inputs` into `output`.
pub fn build_module(
    module: &ir::Module,
    mut inputs: link::Inputs,
    linker: &dyn Linker,
    work_dir: &Path,
    output: &Path,
) -> Result<()> {
    let format = ObjectFormat::host();
    ir::verify(module).context("invalid IR")?;
    let stem = output.file_stem().and_then(|s| s.to_str()).unwrap_or("athrad");
    let obj = codegen::compile(module, stem)?;

    std::fs::create_dir_all(work_dir).with_context(|| format!("creating {}", work_dir.display()))?;
    let obj_path: PathBuf = work_dir.join(format!("{stem}.{}", format.extension()));
    link::write_object(&obj, format, &obj_path)?;
    inputs.objects.insert(0, obj_path);
    linker.link(&inputs, output)
}
