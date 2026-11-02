//! The runtime of the programs Athrad's own frontend (`subset`) compiles.
//!
//! It is a small Rust program, compiled when Athrad itself is built and
//! carried in the compiler: the C `main`, `lang_start`, the allocator shim,
//! and the `print!` helpers the subset's code calls. Its `main` calls
//! `athrad_user_main`, which the subset's object defines. The same build
//! reads, from `rustc`, what the subset needs to know about `std`
//! ([`frontend::StdAbi`]).
//!
//! The source is `rt/athrad_runtime.rs`. A user build writes the objects
//! out for the linker; it does not compile them, and it does not keep them.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

mod bundled {
    include!(concat!(env!("OUT_DIR"), "/runtime_gen.rs"));
}

/// The runtime's source.
pub const SOURCE: &str = include_str!("../rt/athrad_runtime.rs");

/// The runtime built into this compiler.
pub struct Runtime {
    pub objects: Vec<&'static [u8]>,
    pub link: link::Inputs,
    pub abi: frontend::StdAbi,
}

impl Runtime {
    pub fn subset_abi(&self) -> subset::Abi {
        let l = &self.abi.location;
        subset::Abi {
            panics: self.abi.panics.clone(),
            location: subset::Location {
                size: l.size,
                align: l.align,
                file_ptr: l.file_ptr,
                file_len: l.file_len,
                file_nul: l.file_nul,
                line: l.line,
                col: l.col,
            },
        }
    }
}

/// The runtime carried in this binary.
pub fn bundled() -> Result<Runtime> {
    let link = postcard::from_bytes(bundled::LINK).context("reading the runtime's link inputs")?;
    let abi = postcard::from_bytes(bundled::ABI).context("reading what the runtime learned about std")?;
    Ok(Runtime { objects: bundled::OBJECTS.to_vec(), link, abi })
}

/// Write the runtime's objects into `dir` for the linker.
pub fn write_objects(rt: &Runtime, dir: &Path) -> Result<Vec<PathBuf>> {
    rt.objects
        .iter()
        .enumerate()
        .map(|(i, bytes)| {
            let path = dir.join(format!("runtime{i}.o"));
            std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
            Ok(path)
        })
        .collect()
}
