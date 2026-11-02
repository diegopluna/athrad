//! The Athrad pipeline: Rust source in, executable out.
//!
//! ```text
//! .rs ─rustc─▶ MIR ─frontend─▶ ir per codegen unit ─verify, codegen─▶ object per unit ─link─▶ exe
//!                                                        std's rlibs and system libraries ─┘
//! ```
//!
//! `rustc` parses, type-checks, borrow-checks, monomorphizes, and partitions;
//! Athrad is its codegen backend, and links the result against the real
//! `std`. Nothing is kept for the next build: every function is compiled
//! again, and `rustc` is not asked to reuse a unit. A one-file program in
//! the subset `subset` covers skips `rustc` and links against the runtime
//! built into this compiler.

#![feature(rustc_private)]

extern crate rustc_driver;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use link::{Linker, ObjectFormat};

pub mod runtime;
pub mod watch;

/// Build the binary crate rooted at `source` into the executable `output`
/// with `linker`, keeping intermediate files in `work_dir`.
///
/// `source` reaches `rustc` as given, and `rustc` writes it into panic
/// messages as given: build with the same path `rustc` gets to agree with it.
pub fn build(source: &Path, linker: &dyn Linker, work_dir: &Path, output: &Path) -> Result<Timings> {
    fresh(work_dir)?;
    compile_rustc(source, linker, work_dir, output, None)
}

/// [`build`], trying Athrad's own frontend first. A program outside its
/// subset is built with `rustc`, and [`Timings::fallback`] says why.
pub fn build_one_file(source: &Path, linker: &dyn Linker, work_dir: &Path, output: &Path) -> Result<Timings> {
    fresh(work_dir)?;
    match try_subset(source, linker, work_dir, output)? {
        Ok(t) => Ok(t),
        Err(why) => {
            fresh(work_dir)?;
            compile_rustc(source, linker, work_dir, output, Some(why))
        }
    }
}

fn fresh(work_dir: &Path) -> Result<()> {
    // A fresh directory each build: objects from the last one must not be
    // linked by mistake.
    let _ = std::fs::remove_dir_all(work_dir);
    std::fs::create_dir_all(work_dir).with_context(|| format!("creating {}", work_dir.display()))
}

fn compile_rustc(
    source: &Path,
    linker: &dyn Linker,
    work_dir: &Path,
    output: &Path,
    fallback: Option<String>,
) -> Result<Timings> {
    let stem = output.file_stem().and_then(|s| s.to_str()).unwrap_or("athrad");
    let state = Arc::new(Mutex::new(CodegenState::default()));
    let codegen_fn: frontend::Codegen = {
        let state = state.clone();
        let dir = work_dir.to_owned();
        Arc::new(move |module: &ir::Module, unit: &str| {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.codegen(module, unit, &dir)
        })
    };

    let start = Instant::now();
    let result = run_rustc(source, stem, work_dir, codegen_fn);
    let rustc_total = start.elapsed();
    let state = Arc::try_unwrap(state).ok().expect("rustc is done with the codegen callback").into_inner();
    let state = state.unwrap_or_else(|e| e.into_inner());
    let out = result?;

    let mut t = Timings {
        fallback,
        lower: out.lower_time,
        codegen: state.codegen_time,
        functions: state.functions,
        units: out.units,
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
    functions: usize,
    codegen_time: Duration,
}

impl CodegenState {
    fn codegen(&mut self, module: &ir::Module, unit: &str, dir: &Path) -> Result<PathBuf> {
        let start = Instant::now();
        let format = ObjectFormat::host();
        let obj = codegen::compile(module, unit)?;
        let path = dir.join(format!("{unit}.{}", format.extension()));
        link::write_object(&obj, format, &path)?;
        self.functions += module.functions.len();
        self.codegen_time += start.elapsed();
        Ok(path)
    }
}

/// Build `source` with Athrad's own frontend, if it is in the subset that
/// covers: `Ok(Err(why))` when it is not, and `rustc` should build it.
fn try_subset(
    source: &Path,
    linker: &dyn Linker,
    work_dir: &Path,
    output: &Path,
) -> Result<std::result::Result<Timings, String>> {
    let text = std::fs::read_to_string(source).with_context(|| format!("reading {}", source.display()))?;
    let rt = match runtime::bundled() {
        Ok(rt) => rt,
        Err(e) => return Ok(Err(format!("its runtime could not be loaded: {e:#}"))),
    };
    let start = Instant::now();
    let module = match subset::compile(&text, &source.to_string_lossy(), &rt.subset_abi()) {
        Ok(m) => m,
        Err(f) => return Ok(Err(f.to_string())),
    };
    let mut t = Timings { subset: true, lower: start.elapsed(), ..Timings::default() };
    let mut state = CodegenState::default();
    let obj = state.codegen(&module, "subset", work_dir)?;
    t.codegen = state.codegen_time;
    t.functions = state.functions;
    let mut inputs = rt.link.clone();
    inputs.objects = runtime::write_objects(&rt, work_dir)?.into_iter().chain([obj]).collect();
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
    /// `rustc`: parsing through partitioning.
    pub rustc: Duration,
    /// MIR to IR, or Athrad's own frontend.
    pub lower: Duration,
    /// Verifying IR, generating machine code, and writing objects.
    pub codegen: Duration,
    pub link: Duration,
    /// Functions compiled to machine code.
    pub functions: usize,
    /// Codegen units.
    pub units: usize,
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
                "Athrad's frontend {:.1} ms, codegen {:.1} ms ({} functions), link {:.1} ms, total {:.1} ms",
                ms(self.lower),
                ms(self.codegen),
                self.functions,
                ms(self.link),
                ms(self.total())
            );
        }
        if let Some(why) = &self.fallback {
            write!(f, "rustc, since {why}; ")?;
        }
        write!(
            f,
            "rustc {:.1} ms, lower {:.1} ms ({} units), codegen {:.1} ms ({} functions), link {:.1} ms, total {:.1} ms",
            ms(self.rustc),
            ms(self.lower),
            self.units,
            ms(self.codegen),
            self.functions,
            ms(self.link),
            ms(self.total())
        )
    }
}

/// Run `rustc` on `source` with Athrad as its backend. `rustc`'s own
/// messages go to stderr.
fn run_rustc(source: &Path, crate_name: &str, work_dir: &Path, codegen: frontend::Codegen) -> Result<frontend::Output> {
    let mut args: Vec<String> =
        ["athrad", "--edition", "2024", "--crate-type", "bin", "--crate-name", crate_name, &source.to_string_lossy()]
            .iter()
            .map(|s| s.to_string())
            .collect();
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
/// Cargo may pass `-C incremental`. It is ignored: the crate Cargo asked
/// for is compiled whole. Cargo still skips a dependency whose fingerprint
/// has not changed, which is Cargo's own decision, made before it runs this.
pub fn rustc_wrapper(args: &[String]) -> std::process::ExitCode {
    let mut rustc_args = vec!["rustc".to_owned()];
    rustc_args.extend(args.iter().cloned());
    let crate_name = args.iter().position(|a| a == "--crate-name").and_then(|i| args.get(i + 1));
    let work_dir = std::env::temp_dir().join(format!("athrad-rustc-{}", std::process::id()));
    let state = Arc::new(Mutex::new(CodegenState::default()));
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
    let state = state.lock().unwrap_or_else(|e| e.into_inner());
    if std::env::var_os("ATHRAD_TIME").is_some() && state.functions > 0 {
        eprintln!(
            "athrad: {} in {:.1} ms; codegen {:.1} ms ({} functions)",
            crate_name.map(String::as_str).unwrap_or("?"),
            start.elapsed().as_secs_f64() * 1e3,
            state.codegen_time.as_secs_f64() * 1e3,
            state.functions
        );
    }
    code
}
