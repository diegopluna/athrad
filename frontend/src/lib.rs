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
extern crate rustc_index;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;
extern crate rustc_symbol_mangling;
extern crate rustc_target;

mod consts;
mod cx;
mod func;
mod unwind;

use std::fmt::Display;

use anyhow::anyhow;
use rustc_ast::expand::allocator::{AllocatorTy, NO_ALLOC_SHIM_IS_UNSTABLE, default_fn_name, global_fn_name};
use rustc_codegen_ssa::CrateInfo;
use rustc_codegen_ssa::base::{allocator_kind_for_codegen, allocator_shim_contents};
use rustc_driver::Compilation;
use rustc_hir::LangItem;
use rustc_hir::attrs::{Linkage as RLinkage, NativeLibKind};
use rustc_interface::interface::{Compiler, Config};
use rustc_middle::middle::dependency_format::Linkage as DepLinkage;
use rustc_middle::mono::{MonoItem, Visibility};
use rustc_middle::ty::{self, Instance, TyCtxt, Unnormalized};
use rustc_session::config::{CrateType, EntryFnType};
use rustc_span::DUMMY_SP;
use rustc_symbol_mangling::mangle_internal_symbol;

use crate::consts::ModCx;

pub type Result<T, E = anyhow::Error> = std::result::Result<T, E>;

/// A program Athrad cannot yet compile the way `rustc` would.
pub(crate) fn unsupported(what: impl Display) -> anyhow::Error {
    anyhow!("Athrad does not support {what} yet")
}

/// A crate, lowered.
pub struct Output {
    pub module: ir::Module,
    /// What to link the module's object with: `std`'s rlibs and the system
    /// libraries they need. The caller adds its own object.
    pub link: link::Inputs,
}

/// Compile a binary crate. `args` is a `rustc` command line, program name
/// first. `rustc`'s diagnostics go to stderr, as they would from `rustc`.
pub fn compile(args: &[String]) -> Result<Output> {
    struct Callbacks {
        result: Option<Result<Output>>,
    }
    impl rustc_driver::Callbacks for Callbacks {
        fn config(&mut self, config: &mut Config) {
            // One codegen unit: every instance once, in one object.
            config.opts.cg.codegen_units = Some(1);
            // `RUSTC_BOOTSTRAP` is set for Athrad's own build. The program
            // must be held to stable Rust, as `rustc` holds it.
            config.opts.unstable_features = rustc_feature::UnstableFeatures::Disallow;
        }

        fn after_analysis(&mut self, _: &Compiler, tcx: TyCtxt<'_>) -> Compilation {
            self.result = Some(lower_crate(tcx));
            Compilation::Stop
        }
    }

    let mut cb = Callbacks { result: None };
    let ran = rustc_driver::catch_fatal_errors(|| rustc_driver::run_compiler(args, &mut cb));
    match (ran, cb.result) {
        (Ok(()), Some(result)) => result,
        _ => Err(anyhow!("rustc rejected the program")),
    }
}

fn lower_crate(tcx: TyCtxt<'_>) -> Result<Output> {
    if tcx.data_layout.pointer_size().bytes() != 8 || tcx.data_layout.endian != rustc_abi::Endian::Little {
        return Err(unsupported("targets that are not 64-bit little-endian"));
    }
    if !tcx.crate_types().contains(&CrateType::Executable) {
        return Err(unsupported("crate types other than `bin`"));
    }
    let mut m = ModCx::new(tcx);
    let parts = tcx.collect_and_partition_mono_items(());
    for cgu in parts.codegen_units {
        for (item, data) in cgu.items_in_deterministic_order(tcx) {
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
    }
    entry(&mut m)?;
    allocator_shim(&mut m);
    let link = link_inputs(tcx)?;
    Ok(Output { module: m.finish(), link })
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
    let add_libs = |inputs: &mut link::Inputs, libs: &[rustc_codegen_ssa::NativeLib], statics: bool| -> Result<()> {
        for lib in libs {
            if let Some(cfg) = &lib.cfg
                && !rustc_attr_parsing::eval_config_entry(sess, cfg).as_bool()
            {
                continue;
            }
            let kind = match lib.kind {
                NativeLibKind::Static { bundle: Some(false), .. } if statics => link::LibKind::Static,
                // Bundled into the rlib already.
                NativeLibKind::Static { .. } => continue,
                NativeLibKind::Dylib { .. } | NativeLibKind::Unspecified => link::LibKind::Dylib,
                NativeLibKind::Framework { .. } => link::LibKind::Framework,
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
    add_libs(&mut inputs, &info.used_libraries, true)?;
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
