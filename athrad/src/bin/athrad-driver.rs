//! `athrad FILE.rs [-o OUTPUT]`: build a one-file binary crate, as
//! `rustc --edition 2024 FILE.rs` would.
//!
//! `athrad watch FILE.rs [-o OUTPUT] [--run]`: build it, then build again on
//! every change to it, from a process that stays warm: the linker found
//! once, every function's machine code in memory. With `--run`, run the
//! program after each build.
//!
//! `athrad-driver RUSTC ARGS...`: be `rustc` for Cargo, as its
//! `RUSTC_WRAPPER`.
//!
//! This is the program with the compiler in it. The `athrad` command runs
//! it, and runs Cargo with it as the wrapper for `athrad build`.
//!
//! Set `ATHRAD_TIME` to see where each build's time went.

#![feature(rustc_private)]

extern crate rustc_driver;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use athrad::watch::{Change, OsWatcher, Watcher};

struct Args {
    watch: bool,
    run: bool,
    source: PathBuf,
    output: PathBuf,
    stem: String,
}

fn parse_args() -> Result<Args> {
    let mut args = std::env::args_os().skip(1).peekable();
    let watch = args.peek().is_some_and(|a| a == "watch");
    if watch {
        args.next();
    }
    let (mut source, mut output, mut run) = (None, None, false);
    while let Some(arg) = args.next() {
        if arg == "-o" {
            output = args.next().map(PathBuf::from);
        } else if arg == "--run" && watch {
            run = true;
        } else if source.is_none() {
            source = Some(PathBuf::from(arg));
        } else {
            bail!("unexpected argument {}", arg.to_string_lossy());
        }
    }
    let Some(source) = source else {
        bail!("usage: athrad FILE.rs [-o OUTPUT]\n       athrad watch FILE.rs [-o OUTPUT] [--run]");
    };
    let stem = source.file_stem().and_then(|s| s.to_str()).unwrap_or("main").to_owned();
    let output = output.unwrap_or_else(|| PathBuf::from(link::executable_name(&stem)));
    Ok(Args { watch, run, source, output, stem })
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    // Cargo runs `RUSTC_WRAPPER RUSTC ARGS...`.
    if argv.get(1).is_some_and(|a| is_rustc(a)) {
        return athrad::rustc_wrapper(&argv[2..]);
    }
    let result = parse_args().and_then(|args| if args.watch { watch(&args) } else { build_once(&args) });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// What is kept between runs, next to the output in `.athrad/`: each
/// function's machine code in `STEM.cache`, and `rustc`'s incremental
/// state, with each codegen unit's object, in `STEM.incr/`; and the runtime
/// of Athrad's own frontend, in `runtime/`.
fn caches(args: &Args) -> Result<athrad::Caches> {
    let dir = args.output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let dir = dir.join(".athrad");
    Ok(athrad::Caches {
        code: codegen::Cache::load(&dir.join(format!("{}.cache", args.stem)))?,
        incremental: Some(dir.join(format!("{}.incr", args.stem))),
        runtime: Some(dir.join("runtime")),
    })
}

fn work_dir(args: &Args) -> PathBuf {
    std::env::temp_dir().join(format!("athrad-{}-{}", args.stem, std::process::id()))
}

fn timing() -> bool {
    std::env::var_os("ATHRAD_TIME").is_some()
}

fn build(args: &Args, linker: &link::HostCc, caches: &mut athrad::Caches, work_dir: &Path) -> Result<athrad::Timings> {
    let result = athrad::build_cached(&args.source, linker, caches, work_dir, &args.output);
    caches.code.save()?;
    let t = result?;
    // Rewrite the file once dead entries outnumber live ones. Functions in
    // reused units were not looked up, so count only after a full build.
    if t.reused_units == 0 && caches.code.len() > 2 * (t.cache.hits + t.cache.misses) {
        caches.code.compact()?;
    }
    Ok(t)
}

fn build_once(args: &Args) -> Result<()> {
    let linker = link::HostCc::new()?;
    let mut caches = caches(args)?;
    let work_dir = work_dir(args);
    let result = build(args, &linker, &mut caches, &work_dir);
    let _ = std::fs::remove_dir_all(&work_dir);
    let t = result?;
    if timing() {
        eprintln!("athrad: {t}");
    }
    Ok(())
}

fn watch(args: &Args) -> Result<()> {
    let mut linker = link::HostCc::new()?;
    linker.resolve();
    let mut caches = caches(args)?;
    let work_dir = work_dir(args);
    let source = std::fs::canonicalize(&args.source).with_context(|| format!("finding {}", args.source.display()))?;
    let dir = source.parent().context("the source's directory")?.to_owned();

    let mut watcher = OsWatcher::new().context("starting the file watcher")?;
    // Editors often save by writing a new file and renaming it over the old
    // one, so watch the directory and pick out the file.
    watcher.watch(&dir).with_context(|| format!("watching {}", dir.display()))?;

    let mut last: Option<Vec<u8>> = None;
    let mut rebuild = true;
    loop {
        if rebuild {
            let contents = std::fs::read(&source).ok();
            if contents.is_some() && contents != last {
                let start = Instant::now();
                let result = build(args, &linker, &mut caches, &work_dir);
                let ms = start.elapsed().as_secs_f64() * 1e3;
                match result {
                    Ok(t) => {
                        eprintln!("athrad: built {} in {ms:.1} ms", args.output.display());
                        if timing() {
                            eprintln!("athrad: {t}");
                        }
                        if args.run {
                            run(&args.output);
                        }
                    }
                    Err(e) => eprintln!("athrad: error: {e:#}"),
                }
                last = contents;
            }
        }
        // Any change in the directory sends Athrad to look at the file, and
        // it builds only if the file's contents changed. Backends name
        // renamed files unreliably (kqueue keeps an inode's first name), so
        // the event's path is not trusted to say which file changed.
        let changes = watcher.wait(None)?;
        rebuild = changes.iter().any(|c| !matches!(c, Change::Removed(_)));
    }
}

fn run(exe: &Path) {
    // A bare name would be looked up on PATH.
    let exe = if exe.components().count() == 1 { Path::new(".").join(exe) } else { exe.to_owned() };
    match std::process::Command::new(&exe).status() {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("athrad: {} exited with {status}", exe.display()),
        Err(e) => eprintln!("athrad: running {}: {e}", exe.display()),
    }
}

fn is_rustc(arg: &str) -> bool {
    Path::new(arg).file_stem().is_some_and(|s| s == "rustc")
}
