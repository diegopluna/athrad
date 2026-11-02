//! The Athrad pipeline: Rust source in, executable out.
//!
//! ```text
//! .rs ─rustc─▶ MIR ─frontend─▶ ir per codegen unit ─verify, codegen─▶ object per unit ─link─▶ exe
//!                                                        std's rlibs and system libraries ─┘
//! ```
//!
//! `rustc` parses, type-checks, borrow-checks, monomorphizes, and partitions;
//! Athrad is its codegen backend, and links the result against the real
//! `std`. Two caches make a rebuild after an edit cheap:
//!
//! * `rustc`'s incremental compilation, with Athrad's objects as the work
//!   products of their codegen units: a unit `rustc` finds unchanged keeps
//!   its object and is not lowered at all;
//! * Athrad's per-function machine code ([`codegen::Cache`]): in a unit that
//!   did change, only the functions whose IR changed are compiled.

#![feature(rustc_private)]

extern crate rustc_driver;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use link::{Linker, ObjectFormat};

pub mod runtime;
pub mod watch;

/// What a build may reuse from earlier builds.
#[derive(Default)]
pub struct Caches {
    /// Machine code by function.
    pub code: codegen::Cache,
    /// `rustc`'s incremental directory, if incremental compilation is on.
    pub incremental: Option<PathBuf>,
    /// Where the runtime of Athrad's own frontend is kept. With one, a
    /// program in the subset `subset` covers skips `rustc` altogether.
    pub runtime: Option<PathBuf>,
}

/// Build the binary crate rooted at `source` into the executable `output`
/// with `linker`, keeping intermediate files in `work_dir`. Nothing is
/// reused from or kept for other builds.
///
/// `source` reaches `rustc` as given, and `rustc` writes it into panic
/// messages as given: build with the same path `rustc` gets to agree with it.
pub fn build(source: &Path, linker: &dyn Linker, work_dir: &Path, output: &Path) -> Result<Timings> {
    build_cached(source, linker, &mut Caches::default(), work_dir, output)
}

/// [`build`], reusing what `caches` hold and adding to them. The caller
/// saves the code cache.
pub fn build_cached(
    source: &Path,
    linker: &dyn Linker,
    caches: &mut Caches,
    work_dir: &Path,
    output: &Path,
) -> Result<Timings> {
    let stem = output.file_stem().and_then(|s| s.to_str()).unwrap_or("athrad");
    // A fresh directory each build: objects from the last one must not be
    // linked by mistake.
    let _ = std::fs::remove_dir_all(work_dir);
    std::fs::create_dir_all(work_dir).with_context(|| format!("creating {}", work_dir.display()))?;

    let mut fallback = None;
    if let Some(dir) = caches.runtime.clone() {
        match build_subset(source, linker, &mut caches.code, &dir, work_dir, output)? {
            Ok(t) => return Ok(t),
            Err(why) => fallback = Some(why),
        }
    }

    let state = Arc::new(Mutex::new(CodegenState { cache: std::mem::take(&mut caches.code), ..Default::default() }));
    let codegen_fn: frontend::Codegen = {
        let state = state.clone();
        let dir = work_dir.to_owned();
        Arc::new(move |module: &ir::Module, unit: &str| {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.codegen(module, unit, &dir)
        })
    };

    let start = Instant::now();
    let result = run_rustc(source, stem, caches.incremental.as_deref(), work_dir, codegen_fn);
    let rustc_total = start.elapsed();
    let state = Arc::try_unwrap(state).ok().expect("rustc is done with the codegen callback").into_inner();
    let state = state.unwrap_or_else(|e| e.into_inner());
    caches.code = state.cache;
    let out = result?;

    let mut t = Timings {
        fallback,
        lower: out.lower_time,
        codegen: state.codegen_time,
        cache: state.stats,
        units: out.units,
        reused_units: out.reused,
        ..Timings::default()
    };
    t.rustc = rustc_total.saturating_sub(t.lower + t.codegen);

    let mut inputs = out.link.expect("an executable has link inputs");
    inputs.objects.splice(0..0, out.objects.into_iter().map(|(_, p)| p).chain(out.allocator));
    let start = Instant::now();
    linker.link(&inputs, output)?;
    t.link = start.elapsed();
    Ok(t)
}

#[derive(Default)]
struct CodegenState {
    cache: codegen::Cache,
    stats: codegen::Stats,
    codegen_time: Duration,
}

impl CodegenState {
    fn codegen(&mut self, module: &ir::Module, unit: &str, dir: &Path) -> Result<PathBuf> {
        let start = Instant::now();
        let format = ObjectFormat::host();
        let (obj, stats) = codegen::compile_cached(module, unit, &mut self.cache)?;
        let path = dir.join(format!("{unit}.{}", format.extension()));
        link::write_object(&obj, format, &path)?;
        self.stats.hits += stats.hits;
        self.stats.misses += stats.misses;
        self.codegen_time += start.elapsed();
        Ok(path)
    }
}

/// Build `source` with Athrad's own frontend, if it is in the subset that
/// covers: `Ok(Err(why))` when it is not, and `rustc` should build it.
fn build_subset(
    source: &Path,
    linker: &dyn Linker,
    code: &mut codegen::Cache,
    runtime_dir: &Path,
    work_dir: &Path,
    output: &Path,
) -> Result<std::result::Result<Timings, String>> {
    let start = Instant::now();
    let text = std::fs::read_to_string(source).with_context(|| format!("reading {}", source.display()))?;
    let rt = match runtime::ensure(runtime_dir) {
        Ok(rt) => rt,
        Err(e) => return Ok(Err(format!("its runtime could not be built: {e:#}"))),
    };
    let rt_time = start.elapsed();
    let start = Instant::now();
    let module = match subset::compile(&text, &source.to_string_lossy(), &rt.subset_abi()) {
        Ok(m) => m,
        Err(f) => return Ok(Err(f.to_string())),
    };
    let mut t = Timings { subset: true, rustc: rt_time, lower: start.elapsed(), ..Timings::default() };
    let mut state = CodegenState { cache: std::mem::take(code), ..Default::default() };
    let obj = state.codegen(&module, "subset", work_dir);
    *code = state.cache;
    let obj = obj?;
    t.codegen = state.codegen_time;
    t.cache = state.stats;
    let mut inputs = rt.link.clone();
    inputs.objects = rt.objects.iter().cloned().chain([obj]).collect();
    let start = Instant::now();
    linker.link(&inputs, output)?;
    t.link = start.elapsed();
    Ok(Ok(t))
}

/// Where a build's time went.
#[derive(Clone, Debug, Default)]
pub struct Timings {
    /// Athrad's own frontend built the program, without `rustc`.
    pub subset: bool,
    /// Why Athrad's own frontend handed the program to `rustc`, if it did.
    pub fallback: Option<String>,
    /// `rustc`: parsing through partitioning, and its incremental bookkeeping.
    pub rustc: Duration,
    /// MIR to IR.
    pub lower: Duration,
    /// Verifying IR, generating machine code, and writing objects.
    pub codegen: Duration,
    pub link: Duration,
    /// Functions whose machine code came from the cache, and ones compiled.
    pub cache: codegen::Stats,
    /// Codegen units, and those whose objects were reused whole.
    pub units: usize,
    pub reused_units: usize,
}

impl Timings {
    pub fn total(&self) -> Duration {
        self.rustc + self.lower + self.codegen + self.link
    }
}

impl std::fmt::Display for Timings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        if self.subset {
            return write!(
                f,
                "Athrad's frontend {:.1} ms, codegen {:.1} ms ({} functions cached, {} compiled), link {:.1} ms, total {:.1} ms",
                ms(self.lower),
                ms(self.codegen),
                self.cache.hits,
                self.cache.misses,
                ms(self.link),
                ms(self.total())
            );
        }
        if let Some(why) = &self.fallback {
            write!(f, "rustc, since {why}; ")?;
        }
        write!(
            f,
            "rustc {:.1} ms, lower {:.1} ms ({} of {} units reused), codegen {:.1} ms ({} functions cached, {} compiled), link {:.1} ms, total {:.1} ms",
            ms(self.rustc),
            ms(self.lower),
            self.reused_units,
            self.units,
            ms(self.codegen),
            self.cache.hits,
            self.cache.misses,
            ms(self.link),
            ms(self.total())
        )
    }
}

/// Run `rustc` on `source` with Athrad as its backend. `rustc`'s own
/// messages go to stderr.
fn run_rustc(
    source: &Path,
    crate_name: &str,
    incremental: Option<&Path>,
    work_dir: &Path,
    codegen: frontend::Codegen,
) -> Result<frontend::Output> {
    let mut args: Vec<String> =
        ["athrad", "--edition", "2024", "--crate-type", "bin", "--crate-name", crate_name, &source.to_string_lossy()]
            .iter()
            .map(|s| s.to_string())
            .collect();
    if let Some(dir) = incremental {
        args.push("-C".into());
        args.push(format!("incremental={}", dir.display()));
    }
    // For looking inside: `ATHRAD_RUSTC_ARGS="-Z time-passes"`.
    if let Ok(extra) = std::env::var("ATHRAD_RUSTC_ARGS") {
        args.extend(extra.split_whitespace().map(str::to_owned));
    }
    with_rustc(|| frontend::compile(&args, work_dir, codegen))
}

/// Run `f`, which runs `rustc`, alone: `rustc`'s driver keeps global
/// state, so one compilation at a time per process.
pub(crate) fn with_rustc<T>(f: impl FnOnce() -> T) -> T {
    static RUSTC: Mutex<()> = Mutex::new(());
    let _one = RUSTC.lock().unwrap_or_else(|e| e.into_inner());
    f()
}

/// Be `rustc` for Cargo, which runs `RUSTC_WRAPPER RUSTC ARGS...`: compile
/// with Athrad generating the code, and `rustc` linking it. `args` are
/// `rustc`'s, without the program name. Returns `rustc`'s exit code.
///
/// The per-function code cache sits beside the crate's incremental
/// directory when Cargo gives one (it does for the workspace's own crates),
/// and is in memory otherwise.
pub fn rustc_wrapper(args: &[String]) -> std::process::ExitCode {
    let mut rustc_args = vec!["rustc".to_owned()];
    rustc_args.extend(args.iter().cloned());
    let codegen_opt = |key: &str| {
        args.iter().enumerate().find_map(|(i, a)| {
            let value = if a == "-C" { args.get(i + 1).map(String::as_str) } else { a.strip_prefix("-C") };
            value.and_then(|v| v.strip_prefix(key)).map(str::to_owned)
        })
    };
    let crate_name = args.iter().position(|a| a == "--crate-name").and_then(|i| args.get(i + 1));
    // Cargo's incremental directory is shared by the package's crates, each
    // in a directory of its own named by crate name and a hash; the cache
    // goes beside, by crate name and `-C metadata`.
    let cache_path = match (codegen_opt("incremental="), crate_name) {
        (Some(dir), Some(name)) => {
            let meta = codegen_opt("metadata=").unwrap_or_default();
            Some(PathBuf::from(dir).join(format!("{name}-{meta}.athrad-code")))
        }
        _ => None,
    };
    let cache = match &cache_path {
        Some(p) => codegen::Cache::load(p).unwrap_or_default(),
        None => codegen::Cache::in_memory(),
    };
    let work_dir = std::env::temp_dir().join(format!("athrad-rustc-{}", std::process::id()));
    let state = Arc::new(Mutex::new(CodegenState { cache, ..Default::default() }));
    let codegen_fn: frontend::Codegen = {
        let state = state.clone();
        let dir = work_dir.clone();
        Arc::new(move |module: &ir::Module, unit: &str| {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.codegen(module, unit, &dir)
        })
    };
    let start = Instant::now();
    let code = frontend::run_as_rustc(&rustc_args, &work_dir, codegen_fn);
    let _ = std::fs::remove_dir_all(&work_dir);
    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
    if std::env::var_os("ATHRAD_TIME").is_some() && state.stats != codegen::Stats::default() {
        eprintln!(
            "athrad: {} in {:.1} ms; codegen {:.1} ms ({} functions cached, {} compiled)",
            crate_name.map(String::as_str).unwrap_or("?"),
            start.elapsed().as_secs_f64() * 1e3,
            state.codegen_time.as_secs_f64() * 1e3,
            state.stats.hits,
            state.stats.misses
        );
    }
    if cache_path.is_some() {
        let _ = state.cache.save();
    }
    code
}
