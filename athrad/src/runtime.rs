//! The runtime of the programs Athrad's own frontend (`subset`) compiles.
//!
//! It is a Rust program Athrad compiles once with `rustc` as the frontend,
//! like any other: its objects hold `std`'s side of the program (the C
//! `main`, `lang_start`, the allocator shim) and the `print!` helpers the
//! subset's code calls. Its `main` calls `athrad_user_main`, which the
//! subset's object defines. Building it also reads, from `rustc`, what the
//! subset needs to know about `std` ([`frontend::StdAbi`]).
//!
//! It is built on first use and kept in a directory with a stamp of the
//! Athrad executable that built it; a different Athrad builds it again.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The runtime's source.
pub const SOURCE: &str = r#"// Athrad's runtime for the programs its own frontend compiles.
unsafe extern "C-unwind" {
    fn athrad_user_main();
}

fn main() {
    unsafe { athrad_user_main() }
}

fn text<'a>(p: *const u8, n: usize) -> &'a str {
    unsafe { str::from_utf8_unchecked(std::slice::from_raw_parts(p, n)) }
}

fn wide(lo: u64, hi: u64) -> u128 {
    (hi as u128) << 64 | lo as u128
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_print_str(p: *const u8, n: usize) {
    print!("{}", text(p, n));
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_print_i64(x: i64) {
    print!("{x}");
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_print_u64(x: u64) {
    print!("{x}");
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_print_i128(lo: u64, hi: u64) {
    print!("{}", wide(lo, hi) as i128);
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_print_u128(lo: u64, hi: u64) {
    print!("{}", wide(lo, hi));
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_print_bool(b: u32) {
    print!("{}", b != 0);
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_eprint_str(p: *const u8, n: usize) {
    eprint!("{}", text(p, n));
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_eprint_i64(x: i64) {
    eprint!("{x}");
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_eprint_u64(x: u64) {
    eprint!("{x}");
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_eprint_i128(lo: u64, hi: u64) {
    eprint!("{}", wide(lo, hi) as i128);
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_eprint_u128(lo: u64, hi: u64) {
    eprint!("{}", wide(lo, hi));
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_eprint_bool(b: u32) {
    eprint!("{}", b != 0);
}
"#;

/// A built runtime.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Runtime {
    pub objects: Vec<PathBuf>,
    pub link: link::Inputs,
    pub abi: frontend::StdAbi,
    stamp: String,
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

/// Which Athrad built a runtime: its executable's size and time, and the
/// runtime's source.
fn stamp() -> String {
    let exe = std::env::current_exe().ok().and_then(|p| std::fs::metadata(p).ok());
    let (len, time) = exe.map(|m| (m.len(), m.modified().ok())).unwrap_or((0, None));
    let mut h = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(SOURCE, &mut h);
    format!("{len}-{time:?}-{:016x}", std::hash::Hasher::finish(&h))
}

/// The runtime in `dir`, built first if it is missing or stale.
pub fn ensure(dir: &Path) -> Result<Runtime> {
    // Within a process, build each directory's runtime once.
    static BUILT: Mutex<Vec<(PathBuf, Runtime)>> = Mutex::new(Vec::new());
    let mut built = BUILT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, rt)) = built.iter().find(|(d, _)| d == dir) {
        return Ok(rt.clone());
    }
    let manifest = dir.join("runtime.manifest");
    let stamp = stamp();
    if let Ok(bytes) = std::fs::read(&manifest)
        && let Ok(rt) = postcard::from_bytes::<Runtime>(&bytes)
        && rt.stamp == stamp
        && rt.objects.iter().all(|o| o.exists())
    {
        built.push((dir.to_owned(), rt.clone()));
        return Ok(rt);
    }
    // Build in a directory of its own and move it into place, so builds
    // racing in other processes never see half of one.
    let parent = dir.parent().context("the runtime directory's parent")?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".runtime-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let rt = build(&tmp, dir, &stamp)?;
    std::fs::write(tmp.join("runtime.manifest"), postcard::to_allocvec(&rt)?)?;
    let _ = std::fs::remove_dir_all(dir);
    if std::fs::rename(&tmp, dir).is_err() {
        // Another process put one there first; use it.
        let _ = std::fs::remove_dir_all(&tmp);
        let rt: Runtime = postcard::from_bytes(&std::fs::read(&manifest)?)?;
        built.push((dir.to_owned(), rt.clone()));
        return Ok(rt);
    }
    built.push((dir.to_owned(), rt.clone()));
    Ok(rt)
}

/// Build the runtime in `tmp`, naming its objects as they will be once
/// `tmp` is moved to `dir`.
fn build(tmp: &Path, dir: &Path, stamp: &str) -> Result<Runtime> {
    let src = tmp.join("athrad_runtime.rs");
    let objs = tmp.join("objects");
    std::fs::create_dir_all(&objs)?;
    std::fs::write(&src, SOURCE)?;
    let state = Arc::new(Mutex::new(codegen::Cache::in_memory()));
    let codegen_fn: frontend::Codegen = {
        let objs = objs.clone();
        Arc::new(move |module: &ir::Module, unit: &str| {
            let mut cache = state.lock().unwrap_or_else(|e| e.into_inner());
            let format = link::ObjectFormat::host();
            let (obj, _) = codegen::compile_cached(module, unit, &mut cache)?;
            let path = objs.join(format!("{unit}.{}", format.extension()));
            link::write_object(&obj, format, &path)?;
            Ok(path)
        })
    };
    let args: Vec<String> = ["athrad", "--edition", "2024", "--crate-type", "bin", "--crate-name", "athrad_runtime"]
        .iter()
        .map(|s| s.to_string())
        .chain([src.to_string_lossy().into_owned()])
        .collect();
    let out = crate::with_rustc(|| frontend::compile_with(&args, &objs.join("work"), codegen_fn, true))
        .context("building Athrad's runtime")?;
    let moved = |p: &Path| dir.join(p.strip_prefix(tmp).expect("in the build directory"));
    Ok(Runtime {
        objects: out.objects.iter().map(|(_, p)| moved(p)).chain(out.allocator.as_deref().map(moved)).collect(),
        link: out.link.context("the runtime is an executable")?,
        abi: out.std_abi.context("the runtime reads std's ABI")?,
        stamp: stamp.to_owned(),
    })
}
