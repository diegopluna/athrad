//! Rust in: `rustc` parses, checks, and monomorphizes; Athrad takes the MIR
//! of every function `rustc` would have compiled and lowers it to IR.
//!
//! This drives the pinned `rustc` through its own crates (`rustc-dev`), so the
//! frontend is the oracle's compiler. It stops after analysis: no LLVM runs.
//! The program links against the real `std` from the sysroot, the same rlibs
//! `rustc` links, so printing, panics, and exit codes are `std`'s own.

#![feature(rustc_private)]

extern crate rustc_abi;
extern crate rustc_ast;
extern crate rustc_attr_parsing;
extern crate rustc_codegen_ssa;
extern crate rustc_data_structures;
extern crate rustc_driver;
extern crate rustc_feature;
extern crate rustc_hir;
extern crate rustc_index;
extern crate rustc_interface;
extern crate rustc_metadata;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;
extern crate rustc_symbol_mangling;
extern crate rustc_target;

mod consts;
mod cx;
mod func;
mod stdabi;
mod unwind;

use std::any::Any;
use std::fmt::Display;
use std::sync::{Arc, Mutex};

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use rustc_ast::expand::allocator::{AllocatorTy, NO_ALLOC_SHIM_IS_UNSTABLE, default_fn_name, global_fn_name};
use rustc_codegen_ssa::base::{allocator_kind_for_codegen, allocator_shim_contents};
use rustc_codegen_ssa::traits::CodegenBackend;
use rustc_codegen_ssa::{CompiledModule, CompiledModules, CrateInfo, ModuleKind, TargetConfig};
use rustc_driver::Compilation;
use rustc_hir::LangItem;
use rustc_hir::attrs::{Linkage as RLinkage, NativeLibKind};
use rustc_interface::interface::{Compiler, Config};
use rustc_metadata::EncodedMetadata;
use rustc_middle::dep_graph::WorkProductMap;
use rustc_middle::middle::dependency_format::Linkage as DepLinkage;
use rustc_middle::mono::{CollectionMode, MonoItem, MonoItemData, Visibility};
use rustc_middle::ty::{self, Instance, TyCtxt, Unnormalized};
use rustc_middle::util::Providers;
use rustc_session::Session;
use rustc_session::config::{CrateType, EntryFnType};
use rustc_session::config::{OutputFilenames, PrintRequest};
use rustc_span::DUMMY_SP;
use rustc_symbol_mangling::mangle_internal_symbol;

use crate::consts::ModCx;
pub use crate::stdabi::{LocationLayout, StdAbi};

pub type Result<T, E = anyhow::Error> = std::result::Result<T, E>;

/// A program Athrad cannot yet compile the way `rustc` would.
pub(crate) fn unsupported(what: impl Display) -> anyhow::Error {
    anyhow!("Athrad does not support {what} yet")
}

/// Turns one codegen unit's IR into an object file and returns its path.
/// Called while `rustc` runs.
pub type Codegen = Arc<dyn Fn(&ir::Module, &str) -> Result<PathBuf> + Send + Sync>;

/// A crate, compiled to objects.
pub struct Output {
    /// One object per codegen unit, by unit name, in the work directory.
    pub objects: Vec<(String, PathBuf)>,
    /// The allocator shim's object, for crates that link one.
    pub allocator: Option<PathBuf>,
    /// For an executable: what to link the objects with, `std`'s rlibs and
    /// the system libraries they need.
    pub link: Option<link::Inputs>,
    /// This crate's trait-object drops and vtables, for the executable that
    /// links it.
    facts: unwind::Facts,
    /// How long Athrad's own lowering took, out of the whole compile.
    pub lower_time: std::time::Duration,
    pub units: usize,
    /// What Athrad's own frontend needs to know about `std`, when asked
    /// for: see [`StdAbi`].
    pub std_abi: Option<StdAbi>,
}

/// Compile a binary crate. `args` is a `rustc` command line, program name
/// first. `rustc`'s diagnostics go to stderr, as they would from `rustc`.
///
/// Athrad is `rustc`'s codegen backend for the compile, as
/// `rustc_codegen_cranelift` is: `rustc` runs its whole pipeline and hands
/// the backend the crate to generate code for. The backend lowers every
/// unit to IR and gives it back here, for the caller to link; `rustc`
/// writes nothing. Everything else (the target's `cfg`s, its features, its
/// CPU) comes from `rustc`'s own LLVM backend, so a program sees the
/// configuration it would under `rustc`.
///
/// Each codegen unit becomes an object through `codegen`, in `work_dir`.
pub fn compile(args: &[String], work_dir: &Path, codegen: Codegen) -> Result<Output> {
    compile_with(args, work_dir, codegen, false)
}

/// [`compile`], and with `std_abi`, read [`StdAbi`] too.
pub fn compile_with(args: &[String], work_dir: &Path, codegen: Codegen, std_abi: bool) -> Result<Output> {
    std::fs::create_dir_all(work_dir).with_context(|| format!("creating {}", work_dir.display()))?;
    let slot = Slot::default();
    let mut cb = Callbacks { slot: slot.clone(), codegen, rustc_links: false, std_abi };
    let ran = rustc_driver::catch_fatal_errors(|| rustc_driver::run_compiler(args, &mut cb));
    let result = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    match (ran, result) {
        (Ok(()), Some(result)) => result,
        _ => Err(anyhow!("rustc rejected the program")),
    }
}

/// Be `rustc`: run the command line `args` (program name first) as `rustc`
/// would, with Athrad generating the code, and return `rustc`'s exit code.
/// This is how Cargo drives Athrad, through `RUSTC_WRAPPER`. `rustc` links
/// what Athrad generates itself: archives for libraries, with their
/// metadata, and executables, with the link arguments Cargo and the crates
/// give. A crate Athrad cannot compile the way `rustc` would is an error.
pub fn run_as_rustc(args: &[String], work_dir: &Path, codegen: Codegen) -> std::process::ExitCode {
    if let Err(e) = std::fs::create_dir_all(work_dir) {
        eprintln!("error: creating {}: {e}", work_dir.display());
        return std::process::ExitCode::FAILURE;
    }
    let mut cb = Callbacks { slot: Slot::default(), codegen, rustc_links: true, std_abi: false };
    rustc_driver::catch_with_exit_code(|| rustc_driver::run_compiler(args, &mut cb))
}

type Slot = Arc<Mutex<Option<Result<Output>>>>;

struct Callbacks {
    slot: Slot,
    codegen: Codegen,
    rustc_links: bool,
    std_abi: bool,
}

impl rustc_driver::Callbacks for Callbacks {
    fn config(&mut self, config: &mut Config) {
        // `-C incremental` remembers an earlier build. Athrad recompiles the
        // crate it was given. One unit: several only pay when an earlier
        // build's units are kept, and they cost a link each.
        config.opts.incremental = None;
        config.opts.cg.incremental = None;
        config.opts.cli_forced_codegen_units = Some(1);
        // `rustc`'s own queries run on every core. 0 would mean the same;
        // a number past the jobserver's tokens waits, it does not oversubscribe.
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(32);
        if threads > 1 {
            config.opts.unstable_opts.threads = Some(threads);
        }
        // `RUSTC_BOOTSTRAP` is set for Athrad's own build. The program must
        // be held to stable Rust, as `rustc` holds it. (`ATHRAD_RUSTC_ARGS`,
        // for looking inside, may pass `-Z` flags.)
        if std::env::var_os("ATHRAD_RUSTC_ARGS").is_none() {
            config.opts.unstable_features = rustc_feature::UnstableFeatures::Disallow;
        }
        let slot = self.slot.clone();
        let codegen = self.codegen.clone();
        let (rustc_links, std_abi) = (self.rustc_links, self.std_abi);
        config.make_codegen_backend = Some(Box::new(move |sess| {
            let early = rustc_session::EarlyDiagCtxt::new(sess.opts.error_format);
            let llvm = rustc_interface::util::get_codegen_backend(&early, &sess.opts.sysroot, None, &sess.target);
            Box::new(Backend { llvm, slot, codegen, rustc_links, std_abi })
        }));
    }
}

struct Backend {
    llvm: Box<dyn CodegenBackend>,
    slot: Slot,
    codegen: Codegen,
    /// `rustc` links the result (Athrad is driven by Cargo), rather than the
    /// caller of [`compile`].
    rustc_links: bool,
    std_abi: bool,
}

/// What `codegen_crate` hands `join_codegen`.
struct Ongoing {
    result: Result<Output>,
    work_products: WorkProductMap,
}

/// A crate `rustc`'s LLVM backend compiles instead: a procedural macro,
/// which runs only inside the compiler, never in the program.
struct ByLlvm(Box<dyn Any>);

/// Whether a crate is one the compiler loads rather than one in the
/// program: then LLVM builds it, as `rustc` would.
fn for_the_compiler(sess: &Session) -> bool {
    sess.opts.crate_types.contains(&CrateType::ProcMacro)
}

impl CodegenBackend for Backend {
    fn name(&self) -> &'static str {
        "athrad"
    }
    fn init(&self, sess: &Session) {
        self.llvm.init(sess)
    }
    fn print(&self, req: &PrintRequest, out: &mut String, sess: &Session) {
        self.llvm.print(req, out, sess)
    }
    fn print_version(&self) {
        self.llvm.print_version()
    }
    fn target_config(&self, sess: &Session) -> TargetConfig {
        self.llvm.target_config(sess)
    }
    fn provide(&self, providers: &mut Providers) {
        self.llvm.provide(providers)
    }
    fn target_cpu(&self, sess: &Session) -> String {
        self.llvm.target_cpu(sess)
    }
    fn codegen_crate<'tcx>(&self, tcx: TyCtxt<'tcx>) -> Box<dyn Any> {
        if self.rustc_links && for_the_compiler(tcx.sess) {
            return Box::new(ByLlvm(self.llvm.codegen_crate(tcx)));
        }
        let work_products = WorkProductMap::default();
        let result = lower_crate(tcx, &self.codegen, self.rustc_links, self.std_abi);
        if self.rustc_links
            && let Err(e) = &result
        {
            tcx.dcx().fatal(format!("{e:#}"));
        }
        Box::new(Ongoing { result, work_products })
    }
    fn join_codegen(
        &self,
        ongoing: Box<dyn Any>,
        sess: &Session,
        outputs: &OutputFilenames,
        crate_info: &CrateInfo,
    ) -> (CompiledModules, WorkProductMap) {
        let ongoing = match ongoing.downcast::<ByLlvm>() {
            Ok(by_llvm) => return self.llvm.join_codegen(by_llvm.0, sess, outputs, crate_info),
            Err(ongoing) => ongoing,
        };
        let ongoing = ongoing.downcast::<Ongoing>().expect("what codegen_crate returned");
        let Ongoing { result, work_products } = *ongoing;
        if !self.rustc_links {
            *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
            return (CompiledModules { modules: Vec::new(), allocator_module: None }, work_products);
        }
        let out = result.expect("an error is fatal in codegen_crate");
        // The executable that links this crate checks the trait-object drops
        // of every crate in it against the vtables of every crate in it;
        // leave this crate's beside each of its outputs.
        if let Ok(bytes) = postcard::to_allocvec(&out.facts) {
            for &t in &sess.opts.crate_types {
                let path = rustc_session::output::out_filename(sess, t, outputs, crate_info.local_crate_name);
                let _ = std::fs::write(facts_path_for(path.as_path()), &bytes);
            }
        }
        let module = |name: String, path: PathBuf, kind: ModuleKind| CompiledModule {
            name,
            kind,
            object: Some(path),
            global_asm_object: None,
            dwarf_object: None,
            bytecode: None,
            assembly: None,
            llvm_ir: None,
            links_from_incr_cache: Vec::new(),
        };
        let modules = out.objects.into_iter().map(|(n, p)| module(n, p, ModuleKind::Regular)).collect();
        let allocator_module = out.allocator.map(|p| module("athrad-allocator-shim".into(), p, ModuleKind::Allocator));
        (CompiledModules { modules, allocator_module }, work_products)
    }
    fn link(
        &self,
        sess: &Session,
        modules: CompiledModules,
        info: CrateInfo,
        metadata: EncodedMetadata,
        outputs: &OutputFilenames,
    ) {
        if self.rustc_links {
            self.llvm.link(sess, modules, info, metadata, outputs);
        }
        // Otherwise the caller of `compile` links, after rustc is done.
    }
}

/// Whether `path` is in the sysroot: a crate of the standard library.
pub(crate) fn in_sysroot(tcx: TyCtxt<'_>, path: &Path) -> bool {
    let sysroot = tcx.sess.opts.sysroot.path();
    let canonical = std::fs::canonicalize(sysroot).unwrap_or_else(|_| sysroot.to_owned());
    path.starts_with(sysroot) || path.starts_with(canonical)
}

/// Where Athrad keeps a crate's facts: beside its rlib or executable.
fn facts_path_for(output: &Path) -> PathBuf {
    let mut p = output.as_os_str().to_owned();
    p.push(".athrad-facts");
    PathBuf::from(p)
}

/// The facts of every crate this one depends on that Athrad compiled. A
/// dependency outside the sysroot that Athrad did not compile has none, and
/// its vtables are unknown.
fn upstream_facts(tcx: TyCtxt<'_>) -> Result<Vec<unwind::Facts>, String> {
    let mut all = Vec::new();
    for &cnum in tcx.crates(()) {
        let src = tcx.used_crate_source(cnum);
        let Some(lib) = src.rlib.as_ref().or(src.rmeta.as_ref()) else { continue };
        if in_sysroot(tcx, lib) {
            continue;
        }
        let rlib = lib.with_extension("rlib");
        match std::fs::read(facts_path_for(&rlib)) {
            Ok(bytes) => all.push(postcard::from_bytes(&bytes).map_err(|e| e.to_string())?),
            Err(_) => return Err(format!("`{}` was not compiled by Athrad", tcx.crate_name(cnum))),
        }
    }
    Ok(all)
}

/// The functions a lazy collection starts from: the entry point, and every
/// monomorphic item reachable from outside the crate. Same roots `rustc`
/// uses, so the queries warmed here are the ones the walk will ask for.
fn mono_roots<'tcx>(tcx: TyCtxt<'tcx>) -> Vec<Instance<'tcx>> {
    let entry = tcx.entry_fn(()).and_then(|(def_id, _)| def_id.as_local());
    let mut roots = Vec::new();
    let mut push = |def_id: rustc_hir::def_id::LocalDefId| {
        if tcx.generics_of(def_id).requires_monomorphization(tcx) {
            return;
        }
        if entry == Some(def_id) || tcx.is_reachable_non_generic(def_id.to_def_id()) {
            roots.push(Instance::mono(tcx, def_id.to_def_id()));
        }
    };
    let items = tcx.hir_crate_items(());
    for id in items.free_items() {
        if tcx.def_kind(id.owner_id) == rustc_hir::def::DefKind::Fn {
            push(id.owner_id.def_id);
        }
    }
    for id in items.impl_items() {
        if tcx.def_kind(id.owner_id) == rustc_hir::def::DefKind::AssocFn {
            push(id.owner_id.def_id);
        }
    }
    roots
}

fn warm_mono_queries(tcx: TyCtxt<'_>) {
    if tcx.sess.threads().unwrap_or(1) <= 1 {
        return;
    }
    let mut used = rustc_data_structures::fx::FxHashSet::default();
    used.reserve(8192);
    let mut wave = mono_roots(tcx);
    for inst in &wave {
        used.insert(MonoItem::Fn(*inst));
    }
    // Mentioned items are visited too, but only to evaluate their consts.
    // They are not compiled unless the used-set also reaches them.
    let mut mentioned = rustc_data_structures::fx::FxHashSet::default();
    let mut mentioned_wave = Vec::new();

    while !wave.is_empty() {
        let batches: Vec<(Vec<MonoItem<'_>>, Vec<MonoItem<'_>>)> =
            rustc_data_structures::sync::par_map(std::mem::take(&mut wave), |instance| {
                let Ok((used_items, mentioned_items)) = tcx.items_of_instance((instance, CollectionMode::UsedItems))
                else {
                    return (Vec::new(), Vec::new());
                };
                // Mangling and the size estimate are queries the partition
                // asks for next. Doing them on this core, after the body it
                // just scanned, saves a second pass over every item.
                let _ = tcx.symbol_name(instance);
                let _ = tcx.size_estimate(instance);
                (
                    used_items.iter().map(|spanned| spanned.node).collect(),
                    mentioned_items.iter().map(|spanned| spanned.node).collect(),
                )
            });
        for (used_items, mentioned_items) in batches {
            for item in used_items {
                if used.insert(item)
                    && let MonoItem::Fn(inst) = item
                {
                    wave.push(inst);
                }
            }
            for item in mentioned_items {
                if !used.contains(&item)
                    && mentioned.insert(item)
                    && let MonoItem::Fn(inst) = item
                {
                    mentioned_wave.push(inst);
                }
            }
        }
    }

    mentioned_wave.retain(|inst| !used.contains(&MonoItem::Fn(*inst)));
    while !mentioned_wave.is_empty() {
        let batches: Vec<Vec<MonoItem<'_>>> =
            rustc_data_structures::sync::par_map(std::mem::take(&mut mentioned_wave), |instance| {
                let Ok((_, mentioned_items)) = tcx.items_of_instance((instance, CollectionMode::MentionedItems)) else {
                    return Vec::new();
                };
                mentioned_items.iter().map(|spanned| spanned.node).collect()
            });
        for items in batches {
            for item in items {
                if !used.contains(&item)
                    && mentioned.insert(item)
                    && let MonoItem::Fn(inst) = item
                {
                    mentioned_wave.push(inst);
                }
            }
        }
    }
}

fn lower_crate(tcx: TyCtxt<'_>, codegen: &Codegen, rustc_links: bool, want_std_abi: bool) -> Result<Output> {
    if tcx.data_layout.pointer_size().bytes() != 8 || tcx.data_layout.endian != rustc_abi::Endian::Little {
        return Err(unsupported("targets that are not 64-bit little-endian"));
    }
    for t in tcx.crate_types() {
        match t {
            CrateType::Executable => {}
            CrateType::Rlib if rustc_links => {}
            other => return Err(unsupported(format!("the `{other}` crate type"))),
        }
    }
    // The monomorphization walk is one thread. Its queries are not, and they
    // do not depend on walk order, so evaluate them on the pool first. The
    // memo is this compile's; the walk still decides the item set.
    warm_mono_queries(tcx);
    let parts = tcx.collect_and_partition_mono_items(());
    let main = tcx.entry_fn(()).map(|(def, _)| MonoItem::Fn(Instance::mono(tcx, def)));

    let mut objects = Vec::new();
    let mut all_facts = Vec::new();
    let mut lower_time = std::time::Duration::ZERO;
    for cgu in parts.codegen_units {
        let name = cgu.name().to_string();
        let start = std::time::Instant::now();
        let (module, facts) = lower_cgu(tcx, cgu, main)?;
        lower_time += start.elapsed();
        let obj_path = codegen(&module, &name)?;
        all_facts.push(facts);
        objects.push((name, obj_path));
    }
    // A library's trait-object drops are checked when an executable links
    // it, against every vtable in the program.
    if tcx.crate_types().contains(&CrateType::Executable) {
        let upstream = upstream_facts(tcx).map_err(|why| unsupported(format!("this program's dependencies: {why}")))?;
        let mut checked = all_facts.clone();
        checked.extend(upstream);
        unwind::check_trait_objects(&checked).map_err(unsupported)?;
    }

    // The allocator shim depends on no unit; it is small enough to make
    // every time.
    let mut m = ModCx::new(tcx);
    allocator_shim(&mut m);
    let shim = m.finish();
    let allocator = if shim.functions.is_empty() { None } else { Some(codegen(&shim, "athrad-allocator-shim")?) };

    let link = if rustc_links { None } else { Some(link_inputs(tcx)?) };
    let std_abi = if want_std_abi { Some(stdabi::std_abi(tcx)?) } else { None };
    Ok(Output {
        objects,
        allocator,
        link,
        facts: unwind::Facts::merged(&all_facts),
        lower_time,
        units: parts.codegen_units.len(),
        std_abi,
    })
}

/// Lower one codegen unit to its own module: everything it defines, the
/// constants it uses, and imports for the rest. The C `main` goes in the
/// unit with the program's `main`, as `rustc` puts it.
///
/// Functions are independent, so once there are enough of them they are
/// lowered on `rustc`'s thread pool and the pieces merged. Shared constants
/// are named by their contents, so the pieces agree on those symbols.
fn lower_cgu<'tcx>(
    tcx: TyCtxt<'tcx>,
    cgu: &rustc_middle::mono::CodegenUnit<'tcx>,
    main: Option<MonoItem<'tcx>>,
) -> Result<(ir::Module, unwind::Facts)> {
    let items = cgu.items_in_deterministic_order(tcx);
    let threads = tcx.sess.threads().unwrap_or(1);
    if items.len() < PARALLEL_ITEMS || threads == 1 {
        let (module, facts, _) = lower_items(tcx, &items, main, true)?;
        return Ok((module, facts));
    }
    let chunks = split_chunks(items, threads);
    // `anyhow::Error` is not `DynSend` (it holds a raw pointer), so the pool
    // carries the message as a `String`.
    let partials: Vec<Result<(ir::Module, unwind::Facts, bool), String>> =
        rustc_data_structures::sync::par_map(chunks, |chunk| {
            lower_items(tcx, &chunk, main, false).map_err(|e| format!("{e:#}"))
        });
    let mut modules = Vec::new();
    let mut facts = Vec::new();
    let mut has_main = false;
    for part in partials {
        let (module, fact, main_here) = part.map_err(|e| anyhow!(e))?;
        has_main |= main_here;
        modules.push(module);
        facts.push(fact);
    }
    if has_main {
        let mut m = ModCx::new(tcx);
        entry(&mut m)?;
        modules.push(m.finish());
    }
    Ok((merge_modules(modules)?, unwind::Facts::merged(&facts)))
}

/// How many items one thread lowers before another thread is worth it.
const PARALLEL_ITEMS: usize = 32;

fn lower_items<'tcx>(
    tcx: TyCtxt<'tcx>,
    items: &[(MonoItem<'tcx>, MonoItemData)],
    main: Option<MonoItem<'tcx>>,
    with_entry: bool,
) -> Result<(ir::Module, unwind::Facts, bool)> {
    let mut m = ModCx::new(tcx);
    let mut has_main = false;
    for &(item, data) in items {
        has_main |= Some(item) == main;
        let linkage = match (data.linkage, data.visibility) {
            (RLinkage::Internal, _) => ir::Linkage::Local,
            (RLinkage::External, Visibility::Hidden) => ir::Linkage::Hidden,
            (RLinkage::External, _) => ir::Linkage::Export,
            (other, _) => return Err(unsupported(format!("{other:?} linkage (for `{item}`)"))),
        };
        match item {
            MonoItem::Fn(instance) => func::lower(&mut m, instance, linkage)?,
            MonoItem::Static(def_id) => m.define_static(def_id, linkage)?,
            MonoItem::GlobalAsm(_) => return Err(unsupported("`global_asm!`")),
        }
    }
    if with_entry && has_main {
        entry(&mut m)?;
    }
    let vtables = std::mem::take(&mut m.vtables);
    let facts = m.skippable.facts(tcx, &vtables);
    Ok((m.finish(), facts, has_main))
}

fn split_chunks<T>(mut items: Vec<T>, threads: usize) -> Vec<Vec<T>> {
    let threads = threads.max(1).min(items.len().max(1));
    let base = items.len() / threads;
    let extra = items.len() % threads;
    let mut chunks = Vec::with_capacity(threads);
    for i in 0..threads {
        let n = base + usize::from(i < extra);
        let tail = items.split_off(n);
        chunks.push(std::mem::replace(&mut items, tail));
    }
    chunks
}

/// One module from pieces lowered apart. A symbol defined in any piece is
/// not imported from another. Constants lowered twice have the same name and
/// the same bytes.
fn merge_modules(parts: Vec<ir::Module>) -> Result<ir::Module> {
    // Names defined by any piece. Imports of those are dropped.
    let mut defined = std::collections::HashSet::new();
    for part in &parts {
        for f in &part.functions {
            defined.insert(f.name.clone());
        }
        for f in &part.raw {
            defined.insert(f.name.clone());
        }
        for d in &part.data {
            defined.insert(d.name.clone());
        }
    }
    let mut module = ir::Module::new();
    let mut seen_data = std::collections::HashSet::new();
    let mut seen_import = std::collections::HashSet::new();
    for part in parts {
        module.functions.extend(part.functions);
        module.raw.extend(part.raw);
        for data in part.data {
            if !seen_data.insert(data.name.clone()) {
                let previous = module.data.iter().find(|d| d.name == data.name).expect("inserted");
                if previous != &data {
                    return Err(anyhow!("constant `{}` lowered two different ways", data.name));
                }
                continue;
            }
            module.data.push(data);
        }
        for import in part.imports {
            if defined.contains(&import.name) || !seen_import.insert(import.name.clone()) {
                continue;
            }
            module.imports.push(import);
        }
    }
    Ok(module)
}

/// The C `main`: start `std`'s runtime, which runs the program's `main`.
/// This is what `rustc`'s backends emit, through `#[lang = "start"]`.
fn entry(m: &mut ModCx<'_>) -> Result<()> {
    let tcx = m.tcx;
    let Some((main_def, EntryFnType::Main { sigpipe })) = tcx.entry_fn(()) else {
        return Ok(());
    };
    let ret = tcx.fn_sig(main_def).no_bound_vars().expect("main is not generic").output();
    let ret = tcx.normalize_erasing_regions(
        ty::TypingEnv::fully_monomorphized(),
        Unnormalized::new_wip(ret.no_bound_vars().expect("main has no late-bound regions")),
    );
    let start_def = tcx.require_lang_item(LangItem::Start, DUMMY_SP);
    let start = Instance::expect_resolve(
        tcx,
        ty::TypingEnv::fully_monomorphized(),
        start_def,
        tcx.mk_args(&[ret.into()]),
        DUMMY_SP,
    );
    let (start_name, start_sig) = m.func_symbol(start)?;
    let (main_name, _) = m.func_symbol(Instance::mono(tcx, main_def))?;
    if start_sig.param_types() != [ir::PTR, ir::PTR, ir::PTR, ir::Type::I8] || start_sig.return_types() != [ir::PTR] {
        return Err(unsupported(format!("`lang_start` with the signature {start_sig}")));
    }

    let entry_name = tcx.sess.target.entry_name.as_ref();
    let sig = ir::Signature::new([ir::Type::I32, ir::PTR], [ir::Type::I32]);
    let mut b = ir::FunctionBuilder::new(entry_name, ir::Linkage::Export, sig);
    let p = b.params();
    let argc = b.sext(ir::PTR, p[0]);
    let main_fn = b.symbol_addr(&main_name);
    let sigpipe = b.iconst(ir::Type::I8, sigpipe.into());
    let r = b.call(&start_name, &[main_fn, argc, p[1], sigpipe], &[ir::PTR]);
    let code = b.trunc(ir::Type::I32, r[0]);
    b.ret(&[code]);
    m.module.push(b.finish());
    Ok(())
}

/// `__rust_alloc` and friends forward to `std`'s default allocator, unless
/// the program names a `#[global_allocator]`. `rustc` generates the same.
fn allocator_shim(m: &mut ModCx<'_>) {
    let tcx = m.tcx;
    let Some(kind) = allocator_kind_for_codegen(tcx) else { return };
    for method in allocator_shim_contents(tcx, kind) {
        let mut params = Vec::new();
        for input in method.inputs {
            match input.ty {
                AllocatorTy::Layout => params.extend([ir::PTR, ir::PTR]),
                AllocatorTy::Ptr | AllocatorTy::Usize => params.push(ir::PTR),
                AllocatorTy::Never | AllocatorTy::ResultPtr | AllocatorTy::Unit => {
                    unreachable!("not an allocator argument")
                }
            }
        }
        let returns: Vec<ir::Type> = match method.output {
            AllocatorTy::ResultPtr => vec![ir::PTR],
            _ => vec![],
        };
        let sig = ir::Signature::new(params, returns.clone());
        let wrapper = mangle_internal_symbol(tcx, &global_fn_name(method.name));
        let callee = mangle_internal_symbol(tcx, &default_fn_name(method.name));
        m.use_func(&callee, sig.clone());
        let mut b = ir::FunctionBuilder::new(&wrapper, ir::Linkage::Export, sig);
        let args = b.params();
        let r = b.call(&callee, &args, &returns);
        b.ret(&r);
        m.module.push(b.finish());
    }
    let name = mangle_internal_symbol(tcx, NO_ALLOC_SHIM_IS_UNSTABLE);
    let mut b = ir::FunctionBuilder::new(&name, ir::Linkage::Export, ir::Signature::default());
    b.ret(&[]);
    m.module.push(b.finish());
}

/// The rlibs and system libraries `rustc` would link this binary with, in
/// `rustc`'s order.
fn link_inputs(tcx: TyCtxt<'_>) -> Result<link::Inputs> {
    let info = CrateInfo::new(tcx, String::new());
    let formats = info.dependency_formats.get(&CrateType::Executable).expect("an executable's dependency formats");
    let mut inputs = link::Inputs::default();
    let sess = tcx.sess;
    // `upstream`: the libraries of an rlib, rather than of the crate being built.
    let add_libs = |inputs: &mut link::Inputs, libs: &[rustc_codegen_ssa::NativeLib], upstream: bool| -> Result<()> {
        for lib in libs {
            if let Some(cfg) = &lib.cfg
                && !rustc_attr_parsing::eval_config_entry(sess, cfg).as_bool()
            {
                continue;
            }
            let kind = match lib.kind {
                NativeLibKind::Static { bundle: Some(false), .. } => link::LibKind::Static,
                // Bundled into the rlib already.
                NativeLibKind::Static { .. } => continue,
                NativeLibKind::Dylib { .. } | NativeLibKind::Unspecified => link::LibKind::Dylib,
                NativeLibKind::Framework { .. } => link::LibKind::Framework,
                // On Windows, rustc put the import library for an rlib's
                // raw-dylib imports into the rlib itself (std's do this).
                NativeLibKind::RawDylib { .. } if upstream && sess.target.is_like_windows => continue,
                NativeLibKind::RawDylib { .. } => return Err(unsupported("`raw-dylib` libraries")),
                NativeLibKind::LinkArg => return Err(unsupported("`link-arg` libraries")),
                NativeLibKind::WasmImportModule => continue,
            };
            let lib = link::NativeLib { name: lib.name.to_string(), kind, verbatim: lib.verbatim };
            if !inputs.libs.contains(&lib) {
                inputs.libs.push(lib);
            }
        }
        Ok(())
    };
    add_libs(&mut inputs, &info.used_libraries, false)?;
    for &cnum in &info.used_crates {
        match formats[cnum] {
            DepLinkage::Static => {
                let src = &info.used_crate_source[&cnum];
                let rlib = src.rlib.as_ref().ok_or_else(|| unsupported("a dependency without an rlib"))?;
                inputs.archives.push(rlib.clone());
                add_libs(&mut inputs, &info.native_libraries[&cnum], true)?;
            }
            DepLinkage::NotLinked | DepLinkage::IncludedFromDylib if info.compiler_builtins == Some(cnum) => {
                let src = &info.used_crate_source[&cnum];
                inputs.archives.extend(src.rlib.clone());
            }
            DepLinkage::NotLinked | DepLinkage::IncludedFromDylib => {}
            DepLinkage::Dynamic => return Err(unsupported("linking Rust dylibs")),
        }
    }
    Ok(inputs)
}

/// Print every mono item's MIR, for looking at what `rustc` hands Athrad.
pub fn dump_mir(args: &[String]) {
    struct Cb;
    impl rustc_driver::Callbacks for Cb {
        fn after_analysis(&mut self, _: &Compiler, tcx: TyCtxt<'_>) -> Compilation {
            for cgu in tcx.collect_and_partition_mono_items(()).codegen_units {
                for (item, data) in cgu.items_in_deterministic_order(tcx) {
                    println!("// {item} [{:?} {:?}] {}", data.linkage, data.visibility, item.symbol_name(tcx));
                    if let MonoItem::Fn(inst) = item {
                        let mut s = Vec::new();
                        let mir = tcx.instance_mir(inst.def);
                        rustc_middle::mir::pretty::MirWriter::new(tcx).write_mir_fn(mir, &mut s).unwrap();
                        println!("{}", String::from_utf8_lossy(&s));
                    }
                }
            }
            Compilation::Stop
        }
    }
    rustc_driver::run_compiler(args, &mut Cb);
}
