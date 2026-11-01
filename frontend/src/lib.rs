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
extern crate rustc_driver;
extern crate rustc_feature;
extern crate rustc_hir;
extern crate rustc_incremental;
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
mod unwind;

use std::any::Any;
use std::fmt::Display;
use std::sync::{Arc, Mutex};

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use rustc_ast::expand::allocator::{AllocatorTy, NO_ALLOC_SHIM_IS_UNSTABLE, default_fn_name, global_fn_name};
use rustc_codegen_ssa::assert_module_sources::CguReuse;
use rustc_codegen_ssa::base::{allocator_kind_for_codegen, allocator_shim_contents, determine_cgu_reuse};
use rustc_codegen_ssa::traits::CodegenBackend;
use rustc_codegen_ssa::{CompiledModules, CrateInfo, TargetConfig};
use rustc_driver::Compilation;
use rustc_hir::LangItem;
use rustc_hir::attrs::{Linkage as RLinkage, NativeLibKind};
use rustc_interface::interface::{Compiler, Config};
use rustc_metadata::EncodedMetadata;
use rustc_middle::dep_graph::WorkProductMap;
use rustc_middle::middle::dependency_format::Linkage as DepLinkage;
use rustc_middle::mono::{MonoItem, Visibility};
use rustc_middle::ty::{self, Instance, TyCtxt, Unnormalized};
use rustc_middle::util::Providers;
use rustc_session::Session;
use rustc_session::config::{CrateType, EntryFnType};
use rustc_session::config::{OutputFilenames, PrintRequest};
use rustc_span::DUMMY_SP;
use rustc_symbol_mangling::mangle_internal_symbol;

use crate::consts::ModCx;

pub type Result<T, E = anyhow::Error> = std::result::Result<T, E>;

/// A program Athrad cannot yet compile the way `rustc` would.
pub(crate) fn unsupported(what: impl Display) -> anyhow::Error {
    anyhow!("Athrad does not support {what} yet")
}

/// Turns one codegen unit's IR into an object file and returns its path.
/// The caller decides how (verification, the per-function cache); the
/// frontend calls it while `rustc` runs, so the object can become one of
/// `rustc`'s incremental work products.
pub type Codegen = Arc<dyn Fn(&ir::Module, &str) -> Result<PathBuf> + Send + Sync>;

/// A crate, compiled to objects.
pub struct Output {
    /// One object per codegen unit, and one for the allocator shim, all in
    /// the work directory.
    pub objects: Vec<PathBuf>,
    /// What to link the objects with: `std`'s rlibs and the system
    /// libraries they need.
    pub link: link::Inputs,
    /// How long Athrad's own lowering took, out of the whole compile.
    pub lower_time: std::time::Duration,
    /// Codegen units `rustc` found unchanged since the last build, whose
    /// objects were reused, out of all of them.
    pub reused: usize,
    pub units: usize,
}

/// Compile a binary crate. `args` is a `rustc` command line, program name
/// first. `rustc`'s diagnostics go to stderr, as they would from `rustc`.
///
/// Athrad is `rustc`'s codegen backend for the compile, as
/// `rustc_codegen_cranelift` is: `rustc` runs its whole pipeline, including
/// incremental compilation when `args` ask for it, and hands the backend the
/// crate to generate code for. The backend lowers it to IR and gives it back
/// here; `rustc` writes nothing. Everything else (the target's `cfg`s, its
/// features, its CPU) comes from `rustc`'s own LLVM backend, so a program
/// sees the same configuration it would under `rustc`.
///
/// Each codegen unit becomes an object through `codegen`, in `work_dir`.
/// With `-C incremental` in `args`, a unit `rustc` finds unchanged since the
/// last build keeps its object from then: the object is the unit's work
/// product, as LLVM's is under `rustc`, and `rustc` decides when it is still
/// good.
pub fn compile(args: &[String], work_dir: &Path, codegen: Codegen) -> Result<Output> {
    type Slot = Arc<Mutex<Option<Result<Output>>>>;

    struct Callbacks {
        slot: Slot,
        work_dir: PathBuf,
        codegen: Codegen,
    }
    impl rustc_driver::Callbacks for Callbacks {
        fn config(&mut self, config: &mut Config) {
            // Without incremental compilation nothing is reused, and one
            // unit means each instance is lowered once. With it, 16 units:
            // `rustc`'s incremental default is a unit per module (and per
            // upstream module with generic instances, 67 for `heap.rs`),
            // and each unit costs about a millisecond to check and to link,
            // which outweighs what finer reuse saves for one-file programs.
            // 16 is what `rustc` uses without incremental compilation.
            if config.opts.cli_forced_codegen_units.is_none() {
                config.opts.cli_forced_codegen_units = Some(if config.opts.incremental.is_none() { 1 } else { 16 });
            }
            // `RUSTC_BOOTSTRAP` is set for Athrad's own build. The program
            // must be held to stable Rust, as `rustc` holds it.
            // (`ATHRAD_RUSTC_ARGS`, for looking inside, may pass `-Z` flags.)
            if std::env::var_os("ATHRAD_RUSTC_ARGS").is_none() {
                config.opts.unstable_features = rustc_feature::UnstableFeatures::Disallow;
            }
            let slot = self.slot.clone();
            let work_dir = self.work_dir.clone();
            let codegen = self.codegen.clone();
            config.make_codegen_backend = Some(Box::new(move |sess| {
                let early = rustc_session::EarlyDiagCtxt::new(sess.opts.error_format);
                let llvm = rustc_interface::util::get_codegen_backend(&early, &sess.opts.sysroot, None, &sess.target);
                Box::new(Backend { llvm, slot, work_dir, codegen })
            }));
        }
    }

    struct Backend {
        llvm: Box<dyn CodegenBackend>,
        slot: Slot,
        work_dir: PathBuf,
        codegen: Codegen,
    }

    /// What `codegen_crate` hands `join_codegen`.
    struct Ongoing {
        result: Result<Output>,
        work_products: WorkProductMap,
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
            let mut work_products = WorkProductMap::default();
            let result = lower_crate(tcx, &self.work_dir, &self.codegen, &mut work_products);
            Box::new(Ongoing { result, work_products })
        }
        fn join_codegen(
            &self,
            ongoing: Box<dyn Any>,
            _: &Session,
            _: &OutputFilenames,
            _: &CrateInfo,
        ) -> (CompiledModules, WorkProductMap) {
            let ongoing = ongoing.downcast::<Ongoing>().expect("what codegen_crate returned");
            let Ongoing { result, work_products } = *ongoing;
            *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
            (CompiledModules { modules: Vec::new(), allocator_module: None }, work_products)
        }
        fn link(&self, _: &Session, _: CompiledModules, _: CrateInfo, _: EncodedMetadata, _: &OutputFilenames) {
            // Athrad links, with its own object, after rustc is done.
        }
    }

    let slot = Slot::default();
    std::fs::create_dir_all(work_dir).with_context(|| format!("creating {}", work_dir.display()))?;
    let mut cb = Callbacks { slot: slot.clone(), work_dir: work_dir.to_owned(), codegen };
    let ran = rustc_driver::catch_fatal_errors(|| rustc_driver::run_compiler(args, &mut cb));
    let result = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    match (ran, result) {
        (Ok(()), Some(result)) => result,
        _ => Err(anyhow!("rustc rejected the program")),
    }
}

fn lower_crate(
    tcx: TyCtxt<'_>,
    work_dir: &Path,
    codegen: &Codegen,
    work_products: &mut WorkProductMap,
) -> Result<Output> {
    if tcx.data_layout.pointer_size().bytes() != 8 || tcx.data_layout.endian != rustc_abi::Endian::Little {
        return Err(unsupported("targets that are not 64-bit little-endian"));
    }
    if !tcx.crate_types().contains(&CrateType::Executable) {
        return Err(unsupported("crate types other than `bin`"));
    }
    let sess = tcx.sess;
    let incremental = sess.opts.incremental.is_some();
    let parts = tcx.collect_and_partition_mono_items(());
    let main = tcx.entry_fn(()).map(|(def, _)| MonoItem::Fn(Instance::mono(tcx, def)));

    let mut objects = Vec::new();
    let mut all_facts = Vec::new();
    let mut reused = 0;
    let mut lower_time = std::time::Duration::ZERO;
    for cgu in parts.codegen_units {
        let name = cgu.name().to_string();
        let facts_path = work_dir.join(format!("{name}.facts"));

        if incremental && determine_cgu_reuse(tcx, cgu) != CguReuse::No {
            let id = cgu.work_product_id();
            let wp = tcx.dep_graph.previous_work_product(&id).expect("a reused unit has a work product");
            let (Some(o), Some(f)) = (wp.saved_files.get("o"), wp.saved_files.get("facts")) else {
                return Err(anyhow!("the work product of `{name}` is not Athrad's"));
            };
            // The session directory is renamed when rustc finishes; link
            // the files from where they are now, or copy them.
            let obj_path = work_dir.join(o);
            for (file, to) in [(o, &obj_path), (f, &facts_path)] {
                let from = rustc_incremental::in_incr_comp_dir_sess(sess, file);
                if std::fs::hard_link(&from, to).is_err() {
                    std::fs::copy(&from, to).with_context(|| format!("copying {}", from.display()))?;
                }
            }
            let facts = std::fs::read(&facts_path)?;
            all_facts.push(postcard::from_bytes(&facts).context("reading a unit's facts")?);
            objects.push(obj_path);
            work_products.insert(id, wp);
            reused += 1;
            continue;
        }

        // Lower inside the unit's dep-graph task, so rustc records what the
        // lowering read and can tell next time whether it changed. That
        // includes the unit's list of items, which is why the unit is looked
        // up again by name inside the task: the one in hand was read outside.
        let start = std::time::Instant::now();
        let mut lowered = None;
        let cgu_name = cgu.name();
        tcx.dep_graph.with_task(
            cgu.codegen_dep_node(tcx),
            tcx,
            || lowered = Some(lower_cgu(tcx, tcx.codegen_unit(cgu_name), main)),
            Some(rustc_middle::dep_graph::hash_result),
        );
        let (module, facts) = lowered.expect("the task ran")?;
        lower_time += start.elapsed();

        let obj_path = codegen(&module, &name)?;
        std::fs::write(&facts_path, postcard::to_allocvec(&facts)?)?;
        if incremental {
            let files = [("o", obj_path.as_path()), ("facts", facts_path.as_path())];
            let (id, wp) = rustc_incremental::copy_cgu_workproduct_to_incr_comp_cache_dir(sess, &name, &files, &[]);
            work_products.insert(id, wp);
        }
        all_facts.push(facts);
        objects.push(obj_path);
    }
    unwind::check_trait_objects(&all_facts).map_err(unsupported)?;

    // The allocator shim depends on no unit; it is small enough to make
    // every time.
    let mut m = ModCx::new(tcx);
    allocator_shim(&mut m);
    let shim = m.finish();
    if !shim.functions.is_empty() {
        objects.push(codegen(&shim, "athrad-allocator-shim")?);
    }

    let link = link_inputs(tcx)?;
    Ok(Output { objects, link, lower_time, reused, units: parts.codegen_units.len() })
}

/// Lower one codegen unit to its own module: everything it defines, the
/// constants it uses, and imports for the rest. The C `main` goes in the
/// unit with the program's `main`, as `rustc` puts it.
fn lower_cgu<'tcx>(
    tcx: TyCtxt<'tcx>,
    cgu: &rustc_middle::mono::CodegenUnit<'tcx>,
    main: Option<MonoItem<'tcx>>,
) -> Result<(ir::Module, unwind::Facts)> {
    let mut m = ModCx::new(tcx);
    let mut has_main = false;
    for (item, data) in cgu.items_in_deterministic_order(tcx) {
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
    if has_main {
        entry(&mut m)?;
    }
    let vtables = std::mem::take(&mut m.vtables);
    let facts = m.skippable.facts(tcx, &vtables);
    Ok((m.finish(), facts))
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
