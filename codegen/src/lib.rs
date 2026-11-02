//! Lower Athrad IR through Cranelift into an object for the host.
//!
//! The IR is already SSA with block parameters, Cranelift's own shape, so
//! lowering maps blocks to blocks and values to values with no variable
//! renaming. The module must have passed [`ir::verify`]; this crate panics on
//! IR the verifier rejects.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result, anyhow, bail};
use cranelift_codegen::Context;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    self as clif, AbiParam, ArgumentPurpose, BlockArg, InstBuilder, MemFlagsData, StackSlotData, StackSlotKind,
    TrapCode, types,
};
use cranelift_codegen::isa::{CallConv, OwnedTargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module, ModuleReloc, ModuleRelocTarget};
use cranelift_object::{ObjectBuilder, ObjectModule};

mod code;
mod unwind;

/// Compile `module` for the host into an object named `name`.
///
/// Every function is compiled, on every core once there are enough of them
/// to be worth a thread. The object is then filled in source order, so two
/// compiles of the same module produce the same bytes.
pub fn compile(module: &ir::Module, name: &str) -> Result<object::write::Object<'static>> {
    let verifier = ir::Verifier::new(module).context("invalid IR")?;
    let isa = host_isa()?;
    let call_conv = isa.default_call_conv();
    let builder = ObjectBuilder::new(isa.clone(), name, cranelift_module::default_libcall_names())?;
    let mut obj = ObjectModule::new(builder);

    // Declare every symbol first, so code and data can refer to any of them.
    let mut ids: HashMap<&str, Id> = HashMap::new();
    for imp in &module.imports {
        let id = match &imp.kind {
            ir::ImportKind::Func(sig) => {
                Id::Func(obj.declare_function(&imp.name, Linkage::Import, &signature(call_conv, sig))?)
            }
            ir::ImportKind::Data => Id::Data(obj.declare_data(&imp.name, Linkage::Import, false, false)?),
            ir::ImportKind::ThreadLocal => Id::Data(obj.declare_data(&imp.name, Linkage::Import, true, true)?),
        };
        ids.insert(&imp.name, id);
    }
    for func in &module.functions {
        let sig = signature(call_conv, &func.sig);
        ids.insert(&func.name, Id::Func(obj.declare_function(&func.name, linkage(func.linkage), &sig)?));
    }
    for f in &module.raw {
        let sig = signature(call_conv, &f.sig);
        let id = obj.declare_function(&f.name, Linkage::Local, &sig)?;
        obj.define_function_bytes(id, 16, &f.bytes, &[]).map_err(|e| anyhow!("{e:?}"))?;
        ids.insert(&f.name, Id::Func(id));
    }
    for d in &module.data {
        ids.insert(&d.name, Id::Data(obj.declare_data(&d.name, linkage(d.linkage), d.writable, d.tls)?));
    }

    for d in &module.data {
        let mut desc = DataDescription::new();
        desc.define(d.bytes.clone().into_boxed_slice());
        desc.set_align(d.align);
        for r in &d.relocs {
            match ids[r.symbol.as_str()] {
                Id::Func(f) => {
                    if r.addend != 0 {
                        bail!("data `{}`: function address `{}` with an addend", d.name, r.symbol);
                    }
                    let f = obj.declare_func_in_data(f, &mut desc);
                    desc.write_function_addr(r.offset, f);
                }
                Id::Data(target) => {
                    let gv = obj.declare_data_in_data(target, &mut desc);
                    desc.write_data_addr(r.offset, gv, r.addend);
                }
            }
        }
        let Id::Data(id) = ids[d.name.as_str()] else { unreachable!() };
        obj.define_data(id, &desc).with_context(|| format!("defining data {}", d.name))?;
    }

    let mut by_name: HashMap<&str, Sym<'_>> = HashMap::new();
    for imp in &module.imports {
        let (ir_sig, tls) = match &imp.kind {
            ir::ImportKind::Func(sig) => (Some(sig), false),
            ir::ImportKind::Data => (None, false),
            ir::ImportKind::ThreadLocal => (None, true),
        };
        let id = ids[imp.name.as_str()];
        let clif_sig = ir_sig.map(|sig| signature(call_conv, sig));
        by_name.insert(imp.name.as_str(), Sym { id, ir_sig, clif_sig, colocated: false, tls });
    }
    for func in &module.functions {
        let id = ids[func.name.as_str()];
        let clif_sig = Some(signature(call_conv, &func.sig));
        by_name.insert(func.name.as_str(), Sym { id, ir_sig: Some(&func.sig), clif_sig, colocated: true, tls: false });
    }
    for f in &module.raw {
        let id = ids[f.name.as_str()];
        let clif_sig = Some(signature(call_conv, &f.sig));
        by_name.insert(f.name.as_str(), Sym { id, ir_sig: Some(&f.sig), clif_sig, colocated: true, tls: false });
    }
    for d in &module.data {
        let id = ids[d.name.as_str()];
        by_name.insert(d.name.as_str(), Sym { id, ir_sig: None, clif_sig: None, colocated: true, tls: d.tls });
    }
    let syms = Syms { by_name, call_conv, target_config: isa.frontend_config() };

    let names: HashMap<u32, &str> = ids
        .iter()
        .filter_map(|(&n, id)| match id {
            Id::Func(f) => Some((f.as_u32(), n)),
            Id::Data(_) => None,
        })
        .collect();
    let data_names: HashMap<u32, &str> = ids
        .iter()
        .filter_map(|(&n, id)| match id {
            Id::Data(d) => Some((d.as_u32(), n)),
            Id::Func(_) => None,
        })
        .collect();

    let compiled = compile_functions(&*isa, &verifier, &syms, module, &names, &data_names)?;

    let mut unwind_entries = Vec::new();
    for (func, entry) in module.functions.iter().zip(compiled) {
        let Id::Func(id) = ids[func.name.as_str()] else { unreachable!() };
        let relocs: Vec<ModuleReloc> = entry
            .relocs
            .iter()
            .map(|r| ModuleReloc {
                offset: r.offset,
                kind: r.kind,
                addend: r.addend,
                name: match &r.target {
                    code::Target::Func(n) => match ids[n.as_str()] {
                        Id::Func(f) => ModuleRelocTarget::from(f),
                        Id::Data(_) => unreachable!("`{n}` is data, and the code calls it"),
                    },
                    code::Target::Data(n) => match ids[n.as_str()] {
                        Id::Data(d) => ModuleRelocTarget::from(d),
                        Id::Func(_) => unreachable!("`{n}` is a function, and the code reads it"),
                    },
                    code::Target::LibCall(l) => ModuleRelocTarget::LibCall(*l),
                    code::Target::Known(k) => ModuleRelocTarget::KnownSymbol(*k),
                    code::Target::SelfOffset(o) => ModuleRelocTarget::FunctionOffset(id, *o),
                },
            })
            .collect();
        obj.define_function_bytes(id, entry.align, &entry.code, &relocs)
            .map_err(|e| anyhow!("{e:?}"))
            .with_context(|| format!("defining fn {}", func.name))?;
        if let Some(info) = entry.unwind {
            unwind_entries.push(unwind::Entry {
                func: id,
                info,
                size: entry.code.len() as u32,
                call_sites: entry.call_sites,
            });
        }
    }

    let mut product = obj.finish();
    unwind::emit(&*isa, &mut product, unwind_entries)?;
    Ok(product.object)
}

/// How many functions one thread compiles before another thread is worth spawning.
const PARALLEL_AT: usize = 8;

fn compile_functions(
    isa: &dyn cranelift_codegen::isa::TargetIsa,
    verifier: &ir::Verifier<'_>,
    syms: &Syms<'_>,
    module: &ir::Module,
    names: &HashMap<u32, &str>,
    data_names: &HashMap<u32, &str>,
) -> Result<Vec<code::Function>> {
    let n = module.functions.len();
    // A fresh context per function spends the time in the allocator. One per
    // thread, cleared between functions, keeps those buffers.
    let compile_at = |i: usize, ctx: &mut Context, fb_ctx: &mut FunctionBuilderContext| {
        ctx.clear();
        let func = &module.functions[i];
        verifier.function(func).context("invalid IR")?;
        ctx.func.signature = syms.signature(&func.sig);
        lower(syms, func, ctx, fb_ctx);
        compile_one(isa, ctx, names, data_names).with_context(|| format!("compiling fn {}", func.name))
    };
    let workers = std::thread::available_parallelism().map(|p| p.get()).unwrap_or(1);
    if n < PARALLEL_AT || workers == 1 {
        let mut ctx = Context::new();
        let mut fb_ctx = FunctionBuilderContext::new();
        return (0..n).map(|i| compile_at(i, &mut ctx, &mut fb_ctx)).collect();
    }
    let workers = workers.min(n);
    let next = AtomicUsize::new(0);
    let mut parts = Vec::new();
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            handles.push(
                std::thread::Builder::new()
                    .name("athrad-codegen".into())
                    .stack_size(8 << 20)
                    .spawn_scoped(scope, || {
                        let mut ctx = Context::new();
                        let mut fb_ctx = FunctionBuilderContext::new();
                        let mut local = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            if i >= n {
                                break;
                            }
                            local.push((i, compile_at(i, &mut ctx, &mut fb_ctx)));
                        }
                        local
                    })
                    .expect("spawning a codegen thread"),
            );
        }
        for handle in handles {
            parts.push(handle.join().unwrap_or_else(|e| std::panic::resume_unwind(e)));
        }
    });
    let mut slots: Vec<Option<Result<code::Function>>> = (0..n).map(|_| None).collect();
    for part in parts {
        for (i, compiled) in part {
            slots[i] = Some(compiled);
        }
    }
    slots.into_iter().map(|slot| slot.expect("every function was compiled")).collect()
}

/// Compile the function in `ctx`: its bytes, its relocations with symbols
/// by name, and its unwind info.
fn compile_one(
    isa: &dyn cranelift_codegen::isa::TargetIsa,
    ctx: &mut Context,
    names: &HashMap<u32, &str>,
    data_names: &HashMap<u32, &str>,
) -> Result<code::Function> {
    let code = ctx
        .compile(isa, &mut cranelift_codegen::control::ControlPlane::default())
        .map_err(|e| anyhow!("{:?}", e.inner))?;
    let align = u64::from(code.buffer.min_alignment);
    let unwind = code.create_unwind_info(isa)?;
    let code = ctx.compiled_code().expect("compiled");
    let mut relocs = Vec::new();
    for r in code.buffer.relocs() {
        let m = ModuleReloc::from_mach_reloc(r, &ctx.func, FuncId::from_u32(0));
        let target = match m.name {
            ModuleRelocTarget::User { namespace: 0, index } => code::Target::Func(names[&index].to_owned()),
            ModuleRelocTarget::User { namespace: 1, index } => code::Target::Data(data_names[&index].to_owned()),
            ModuleRelocTarget::User { namespace, .. } => bail!("relocation in namespace {namespace}"),
            ModuleRelocTarget::LibCall(l) => code::Target::LibCall(l),
            ModuleRelocTarget::KnownSymbol(k) => code::Target::Known(k),
            ModuleRelocTarget::FunctionOffset(_, o) => code::Target::SelfOffset(o),
        };
        relocs.push(code::RelocEntry { offset: m.offset, kind: m.kind, target, addend: m.addend });
    }
    let mut call_sites = Vec::new();
    for site in code.buffer.call_sites() {
        let mut pad = None;
        for h in site.exception_handlers {
            if let cranelift_codegen::MachExceptionHandler::Tag(tag, at) = h {
                pad = Some((cranelift_codegen::entity::EntityRef::index(*tag) as u32 == TAG_CATCH, at.as_offset()));
            }
        }
        call_sites.push(code::CallSite { ret_addr: site.ret_addr, pad });
    }
    // Only a function with a landing pad needs its call sites listed.
    if call_sites.iter().all(|c| c.pad.is_none()) {
        call_sites.clear();
    }
    Ok(code::Function { align, code: code.buffer.data().to_vec(), relocs, unwind, call_sites })
}

/// A declared symbol, as lowering needs it. `colocated` is Cranelift's
/// `Linkage::is_final`: a direct reference, rather than one through the GOT.
struct Sym<'a> {
    id: Id,
    ir_sig: Option<&'a ir::Signature>,
    clif_sig: Option<clif::Signature>,
    colocated: bool,
    tls: bool,
}

struct Syms<'a> {
    by_name: HashMap<&'a str, Sym<'a>>,
    call_conv: CallConv,
    target_config: cranelift_codegen::isa::TargetFrontendConfig,
}

impl<'a> Syms<'a> {
    fn signature(&self, sig: &ir::Signature) -> clif::Signature {
        signature(self.call_conv, sig)
    }

    fn ir_sig(&self, name: &str) -> &'a ir::Signature {
        self.by_name[name].ir_sig.expect("verified IR: a function")
    }

    fn func_ref(
        &self,
        b: &mut FunctionBuilder,
        funcs: &mut HashMap<&'a str, clif::FuncRef>,
        name: &str,
    ) -> clif::FuncRef {
        let (key, sym) = self.by_name.get_key_value(name).expect("verified IR");
        *funcs.entry(*key).or_insert_with(|| import_func(sym, b.func))
    }

    fn data_gv(
        &self,
        b: &mut FunctionBuilder,
        globals: &mut HashMap<&'a str, clif::GlobalValue>,
        name: &str,
    ) -> clif::GlobalValue {
        let (key, sym) = self.by_name.get_key_value(name).expect("verified IR");
        *globals.entry(*key).or_insert_with(|| import_data(sym, b.func))
    }
}

/// What [`cranelift_module::Module::declare_func_in_func`] records, without
/// the module: the functions of one module are lowered at the same time.
fn import_func(sym: &Sym<'_>, func: &mut clif::Function) -> clif::FuncRef {
    let Id::Func(id) = sym.id else { unreachable!("verified IR: a function") };
    let signature = func.import_signature(sym.clif_sig.clone().expect("a function signature"));
    let user = func.declare_imported_user_function(clif::UserExternalName { namespace: 0, index: id.as_u32() });
    func.import_function(clif::ExtFuncData {
        name: clif::ExternalName::user(user),
        signature,
        colocated: sym.colocated,
        patchable: false,
    })
}

fn import_data(sym: &Sym<'_>, func: &mut clif::Function) -> clif::GlobalValue {
    let Id::Data(id) = sym.id else { unreachable!("verified IR: data") };
    let user = func.declare_imported_user_function(clif::UserExternalName { namespace: 1, index: id.as_u32() });
    func.create_global_value(clif::GlobalValueData::Symbol {
        name: clif::ExternalName::user(user),
        offset: clif::immediates::Imm64::new(0),
        colocated: sym.colocated,
        tls: sym.tls,
    })
}

/// Exception tags of `try_call` handlers: to clean up and resume, or to
/// catch (stop the unwinding).
const TAG_CLEANUP: u32 = 0;
const TAG_CATCH: u32 = 1;

#[derive(Clone, Copy)]
enum Id {
    Func(FuncId),
    Data(DataId),
}

fn linkage(l: ir::Linkage) -> Linkage {
    match l {
        ir::Linkage::Local => Linkage::Local,
        ir::Linkage::Hidden => Linkage::Hidden,
        ir::Linkage::Export => Linkage::Export,
    }
}

/// The host ISA, set up for fast compiles of position-independent code.
fn host_isa() -> Result<OwnedTargetIsa> {
    let mut flags = settings::builder();
    flags.set("opt_level", "none")?;
    // Cranelift's verifier walks the IR it was just given and does not change
    // the machine code. The module was already checked before this.
    flags.set("enable_verifier", "false")?;
    // `backtracking` spends its time on code quality a dev build does not use.
    flags.set("regalloc_algorithm", "single_pass")?;
    flags.set("preserve_frame_pointers", "true")?;
    // Pass i128 the way LLVM does, which is how rustc's code expects it.
    flags.set("enable_llvm_abi_extensions", "true")?;
    // ELF and Mach-O executables are position independent by default. On
    // COFF, link.exe makes import thunks and base relocations itself.
    let pic = !cfg!(windows);
    flags.set("is_pic", if pic { "true" } else { "false" })?;
    // How thread-locals are reached: through TLV descriptors on macOS, the
    // general-dynamic model on ELF, the TLS index on Windows.
    let tls = if cfg!(target_vendor = "apple") {
        "macho"
    } else if cfg!(windows) {
        "coff"
    } else {
        "elf_gd"
    };
    flags.set("tls_model", tls)?;
    let isa_builder = cranelift_native::builder().map_err(|e| anyhow!("host ISA: {e}"))?;
    Ok(isa_builder.finish(settings::Flags::new(flags))?)
}

fn clif_type(ty: ir::Type) -> clif::Type {
    match ty {
        ir::Type::I8 => types::I8,
        ir::Type::I16 => types::I16,
        ir::Type::I32 => types::I32,
        ir::Type::I64 => types::I64,
        ir::Type::I128 => types::I128,
    }
}

/// The Cranelift type a parameter has at the call boundary.
fn param_type(p: &ir::Param) -> clif::Type {
    match (p.float, p.ty) {
        (true, ir::Type::I32) => types::F32,
        (true, ir::Type::I64) => types::F64,
        (true, other) => panic!("verified IR: no {other} float"),
        (false, ty) => clif_type(ty),
    }
}

fn float_type(ft: ir::FloatTy) -> clif::Type {
    match ft {
        ir::FloatTy::F32 => types::F32,
        ir::FloatTy::F64 => types::F64,
    }
}

/// Move values between their IR integer type and the float type `params`
/// says the calling convention wants. `to_abi` goes from IR to the call.
fn abi_values(b: &mut FunctionBuilder, params: &[ir::Param], vs: &[clif::Value], to_abi: bool) -> Vec<clif::Value> {
    vs.iter()
        .zip(params)
        .map(|(&v, p)| {
            if !p.float {
                return v;
            }
            let ty = if to_abi { param_type(p) } else { clif_type(p.ty) };
            b.ins().bitcast(ty, MemFlagsData::new(), v)
        })
        .collect()
}

fn abi_param(p: &ir::Param) -> AbiParam {
    let a = if p.sret {
        AbiParam::special(param_type(p), ArgumentPurpose::StructReturn)
    } else {
        AbiParam::new(param_type(p))
    };
    match p.ext {
        ir::Ext::None => a,
        ir::Ext::Zero => a.uext(),
        ir::Ext::Sign => a.sext(),
    }
}

fn signature(call_conv: CallConv, sig: &ir::Signature) -> clif::Signature {
    let mut s = clif::Signature::new(call_conv);
    s.params.extend(sig.params.iter().map(abi_param));
    s.returns.extend(sig.returns.iter().map(abi_param));
    s
}

fn lower(syms: &Syms<'_>, func: &ir::Function, ctx: &mut Context, fb_ctx: &mut FunctionBuilderContext) {
    let mut b = FunctionBuilder::new(&mut ctx.func, fb_ctx);

    let blocks: Vec<clif::Block> = func.blocks.iter().map(|_| b.create_block()).collect();
    let slots: Vec<clif::StackSlot> = func
        .slots
        .iter()
        .map(|s| {
            let shift = s.align.trailing_zeros() as u8;
            b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, s.size, shift))
        })
        .collect();
    let mut values: Vec<Option<clif::Value>> = vec![None; func.values.len()];
    let mut funcs: HashMap<&str, clif::FuncRef> = HashMap::new();
    let mut globals: HashMap<&str, clif::GlobalValue> = HashMap::new();

    // Block parameters first: a branch may target a block lowered later.
    for (i, block) in func.blocks.iter().enumerate() {
        for &p in &block.params {
            let ty = clif_type(func.values[p.0 as usize]);
            values[p.0 as usize] = Some(b.append_block_param(blocks[i], ty));
        }
    }

    // Cranelift's entry block takes the parameters as the calling convention
    // passes them; hand them to the IR's entry block as integers.
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let params = b.block_params(entry).to_vec();
    let params = abi_values(&mut b, &func.sig.params, &params, false);
    let params: Vec<BlockArg> = params.into_iter().map(BlockArg::Value).collect();
    b.ins().jump(blocks[0], &params);

    // Walk in reverse postorder so definitions are lowered before their uses.
    for i in reverse_postorder(func) {
        let block = &func.blocks[i];
        b.switch_to_block(blocks[i]);
        let get = |values: &[Option<clif::Value>], v: ir::Value| values[v.0 as usize].expect("verified IR");
        for inst in &block.insts {
            let mut func_ref =
                |b: &mut FunctionBuilder, name: &str| -> clif::FuncRef { syms.func_ref(b, &mut funcs, name) };
            let results: Vec<clif::Value> = match &inst.op {
                ir::Op::Const(ty, n) => vec![iconst(&mut b, *ty, *n)],
                ir::Op::Binary(op @ (ir::BinOp::UDiv | ir::BinOp::URem | ir::BinOp::SDiv | ir::BinOp::SRem), x, y)
                    if func.values[x.0 as usize] == ir::Type::I128 =>
                {
                    vec![div128(&mut b, *op, get(&values, *x), get(&values, *y))]
                }
                ir::Op::Binary(op, x, y) => {
                    let (x, y) = (get(&values, *x), get(&values, *y));
                    let ins = b.ins();
                    vec![match op {
                        ir::BinOp::Add => ins.iadd(x, y),
                        ir::BinOp::Sub => ins.isub(x, y),
                        ir::BinOp::Mul => ins.imul(x, y),
                        ir::BinOp::UDiv => ins.udiv(x, y),
                        ir::BinOp::URem => ins.urem(x, y),
                        ir::BinOp::SDiv => ins.sdiv(x, y),
                        ir::BinOp::SRem => ins.srem(x, y),
                        ir::BinOp::And => ins.band(x, y),
                        ir::BinOp::Or => ins.bor(x, y),
                        ir::BinOp::Xor => ins.bxor(x, y),
                        ir::BinOp::Shl => ins.ishl(x, y),
                        ir::BinOp::UShr => ins.ushr(x, y),
                        ir::BinOp::SShr => ins.sshr(x, y),
                        ir::BinOp::Rotl => ins.rotl(x, y),
                        ir::BinOp::Rotr => ins.rotr(x, y),
                    }]
                }
                ir::Op::Unary(op, x) => {
                    let x = get(&values, *x);
                    let narrow = b.func.dfg.value_type(x) == types::I8;
                    let ins = b.ins();
                    vec![match op {
                        ir::UnOp::Popcnt => ins.popcnt(x),
                        ir::UnOp::Clz => ins.clz(x),
                        ir::UnOp::Ctz => ins.ctz(x),
                        // One byte reversed is itself.
                        ir::UnOp::Bswap if narrow => x,
                        ir::UnOp::Bswap => ins.bswap(x),
                        ir::UnOp::Bitrev => ins.bitrev(x),
                    }]
                }
                ir::Op::Overflow(op, signed, x, y) if func.values[x.0 as usize] == ir::Type::I128 => {
                    let (x, y) = (get(&values, *x), get(&values, *y));
                    let (r, of) = match op {
                        ir::OverflowOp::Mul => mul128_overflow(&mut b, *signed, x, y),
                        _ => addsub128_overflow(&mut b, *op, *signed, x, y),
                    };
                    vec![r, of]
                }
                ir::Op::Overflow(op, signed, x, y) => {
                    let (x, y) = (get(&values, *x), get(&values, *y));
                    let ins = b.ins();
                    let (r, of) = match (op, signed) {
                        (ir::OverflowOp::Add, false) => ins.uadd_overflow(x, y),
                        (ir::OverflowOp::Add, true) => ins.sadd_overflow(x, y),
                        (ir::OverflowOp::Sub, false) => ins.usub_overflow(x, y),
                        (ir::OverflowOp::Sub, true) => ins.ssub_overflow(x, y),
                        (ir::OverflowOp::Mul, false) => ins.umul_overflow(x, y),
                        (ir::OverflowOp::Mul, true) => ins.smul_overflow(x, y),
                    };
                    vec![r, of]
                }
                ir::Op::Cmp(c, x, y) => {
                    let cc = match c {
                        ir::Cmp::Eq => IntCC::Equal,
                        ir::Cmp::Ne => IntCC::NotEqual,
                        ir::Cmp::SLt => IntCC::SignedLessThan,
                        ir::Cmp::SLe => IntCC::SignedLessThanOrEqual,
                        ir::Cmp::SGt => IntCC::SignedGreaterThan,
                        ir::Cmp::SGe => IntCC::SignedGreaterThanOrEqual,
                        ir::Cmp::ULt => IntCC::UnsignedLessThan,
                        ir::Cmp::ULe => IntCC::UnsignedLessThanOrEqual,
                        ir::Cmp::UGt => IntCC::UnsignedGreaterThan,
                        ir::Cmp::UGe => IntCC::UnsignedGreaterThanOrEqual,
                    };
                    vec![b.ins().icmp(cc, get(&values, *x), get(&values, *y))]
                }
                ir::Op::Select(c, x, y) => {
                    vec![b.ins().select(get(&values, *c), get(&values, *x), get(&values, *y))]
                }
                ir::Op::ZExt(ty, v) => vec![b.ins().uextend(clif_type(*ty), get(&values, *v))],
                ir::Op::SExt(ty, v) => vec![b.ins().sextend(clif_type(*ty), get(&values, *v))],
                ir::Op::Trunc(ty, v) => vec![b.ins().ireduce(clif_type(*ty), get(&values, *v))],
                ir::Op::SlotAddr(s) => vec![b.ins().stack_addr(types::I64, slots[s.0 as usize], 0)],
                ir::Op::SymbolAddr(name) => match syms.by_name[name.as_str()].id {
                    Id::Func(_) => {
                        let f = func_ref(&mut b, name);
                        vec![b.ins().func_addr(types::I64, f)]
                    }
                    Id::Data(_) => {
                        let gv = syms.data_gv(&mut b, &mut globals, name);
                        vec![b.ins().symbol_value(types::I64, gv)]
                    }
                },
                ir::Op::TlsAddr(name) => {
                    let gv = syms.data_gv(&mut b, &mut globals, name);
                    vec![b.ins().tls_value(types::I64, gv)]
                }
                ir::Op::Load(ty, addr, offset) => {
                    vec![b.ins().load(clif_type(*ty), MemFlagsData::new(), get(&values, *addr), *offset)]
                }
                ir::Op::Store(addr, offset, v) => {
                    b.ins().store(MemFlagsData::new(), get(&values, *v), get(&values, *addr), *offset);
                    vec![]
                }
                ir::Op::AtomicLoad(ty, addr) => {
                    vec![b.ins().atomic_load(clif_type(*ty), MemFlagsData::trusted(), get(&values, *addr))]
                }
                ir::Op::AtomicStore(addr, v) => {
                    b.ins().atomic_store(MemFlagsData::trusted(), get(&values, *v), get(&values, *addr));
                    vec![]
                }
                ir::Op::AtomicRmw(op, addr, v) => {
                    use cranelift_codegen::ir::AtomicRmwOp as A;
                    let op = match op {
                        ir::RmwOp::Xchg => A::Xchg,
                        ir::RmwOp::Add => A::Add,
                        ir::RmwOp::Sub => A::Sub,
                        ir::RmwOp::And => A::And,
                        ir::RmwOp::Nand => A::Nand,
                        ir::RmwOp::Or => A::Or,
                        ir::RmwOp::Xor => A::Xor,
                        ir::RmwOp::Smax => A::Smax,
                        ir::RmwOp::Smin => A::Smin,
                        ir::RmwOp::Umax => A::Umax,
                        ir::RmwOp::Umin => A::Umin,
                    };
                    let v = get(&values, *v);
                    let ty = b.func.dfg.value_type(v);
                    vec![b.ins().atomic_rmw(ty, MemFlagsData::trusted(), op, get(&values, *addr), v)]
                }
                ir::Op::AtomicCas(addr, e, n) => {
                    let (a, e, n) = (get(&values, *addr), get(&values, *e), get(&values, *n));
                    vec![b.ins().atomic_cas(MemFlagsData::trusted(), a, e, n)]
                }
                ir::Op::Fence => {
                    b.ins().fence();
                    vec![]
                }
                ir::Op::Call(name, args) => {
                    let callee = func_ref(&mut b, name);
                    let sig = syms.ir_sig(name);
                    let args: Vec<clif::Value> = args.iter().map(|&a| get(&values, a)).collect();
                    let args = abi_values(&mut b, &sig.params, &args, true);
                    let call = b.ins().call(callee, &args);
                    let results = b.inst_results(call).to_vec();
                    abi_values(&mut b, &sig.returns, &results, false)
                }
                ir::Op::CallIndirect(sig, callee, args) => {
                    let clif_sig = b.import_signature(syms.signature(sig));
                    let args: Vec<clif::Value> = args.iter().map(|&a| get(&values, a)).collect();
                    let args = abi_values(&mut b, &sig.params, &args, true);
                    let call = b.ins().call_indirect(clif_sig, get(&values, *callee), &args);
                    let results = b.inst_results(call).to_vec();
                    abi_values(&mut b, &sig.returns, &results, false)
                }
                ir::Op::FBinary(op, ft, x, y) => {
                    let t = float_type(*ft);
                    let x = b.ins().bitcast(t, MemFlagsData::new(), get(&values, *x));
                    let y = b.ins().bitcast(t, MemFlagsData::new(), get(&values, *y));
                    let ins = b.ins();
                    let r = match op {
                        ir::FBinOp::Add => ins.fadd(x, y),
                        ir::FBinOp::Sub => ins.fsub(x, y),
                        ir::FBinOp::Mul => ins.fmul(x, y),
                        ir::FBinOp::Div => ins.fdiv(x, y),
                        ir::FBinOp::Min => ins.fmin(x, y),
                        ir::FBinOp::Max => ins.fmax(x, y),
                        ir::FBinOp::Copysign => ins.fcopysign(x, y),
                    };
                    vec![b.ins().bitcast(clif_type(ft.bits_type()), MemFlagsData::new(), r)]
                }
                ir::Op::FUnary(op, ft, x) => {
                    let t = float_type(*ft);
                    let x = b.ins().bitcast(t, MemFlagsData::new(), get(&values, *x));
                    let ins = b.ins();
                    let r = match op {
                        ir::FUnOp::Neg => ins.fneg(x),
                        ir::FUnOp::Abs => ins.fabs(x),
                        ir::FUnOp::Sqrt => ins.sqrt(x),
                        ir::FUnOp::Floor => ins.floor(x),
                        ir::FUnOp::Ceil => ins.ceil(x),
                        ir::FUnOp::Trunc => ins.trunc(x),
                        ir::FUnOp::Nearest => ins.nearest(x),
                    };
                    vec![b.ins().bitcast(clif_type(ft.bits_type()), MemFlagsData::new(), r)]
                }
                ir::Op::FCmp(c, ft, x, y) => {
                    let t = float_type(*ft);
                    let x = b.ins().bitcast(t, MemFlagsData::new(), get(&values, *x));
                    let y = b.ins().bitcast(t, MemFlagsData::new(), get(&values, *y));
                    let cc = match c {
                        ir::FCmp::Eq => FloatCC::Equal,
                        ir::FCmp::Ne => FloatCC::NotEqual,
                        ir::FCmp::Lt => FloatCC::LessThan,
                        ir::FCmp::Le => FloatCC::LessThanOrEqual,
                        ir::FCmp::Gt => FloatCC::GreaterThan,
                        ir::FCmp::Ge => FloatCC::GreaterThanOrEqual,
                        ir::FCmp::Uno => FloatCC::Unordered,
                    };
                    vec![b.ins().fcmp(cc, x, y)]
                }
                ir::Op::FToInt { from, to, signed, v } => {
                    let x = b.ins().bitcast(float_type(*from), MemFlagsData::new(), get(&values, *v));
                    vec![float_to_int(&mut b, x, *to, *signed)]
                }
                ir::Op::IntToF { to, signed, v } => {
                    let mut x = get(&values, *v);
                    // Cranelift converts from 32 and 64 bits.
                    if b.func.dfg.value_type(x).bits() < 32 {
                        x = if *signed { b.ins().sextend(types::I32, x) } else { b.ins().uextend(types::I32, x) };
                    }
                    let t = float_type(*to);
                    let f = if *signed { b.ins().fcvt_from_sint(t, x) } else { b.ins().fcvt_from_uint(t, x) };
                    vec![b.ins().bitcast(clif_type(to.bits_type()), MemFlagsData::new(), f)]
                }
                ir::Op::FConvert { from, to, v } => {
                    let x = b.ins().bitcast(float_type(*from), MemFlagsData::new(), get(&values, *v));
                    let t = float_type(*to);
                    let f = if t.bits() > float_type(*from).bits() {
                        b.ins().fpromote(t, x)
                    } else {
                        b.ins().fdemote(t, x)
                    };
                    vec![b.ins().bitcast(clif_type(to.bits_type()), MemFlagsData::new(), f)]
                }
            };
            assert_eq!(results.len(), inst.results.len(), "verified IR: result counts match");
            for (r, v) in inst.results.iter().zip(results) {
                values[r.0 as usize] = Some(v);
            }
        }
        let args = |values: &[Option<clif::Value>], t: &ir::Target| -> Vec<BlockArg> {
            t.args.iter().map(|&a| BlockArg::Value(get(values, a))).collect()
        };
        match block.term.as_ref().expect("verified IR: every block is terminated") {
            ir::Terminator::Return(vs) => {
                let vs: Vec<clif::Value> = vs.iter().map(|&v| get(&values, v)).collect();
                let vs = abi_values(&mut b, &func.sig.returns, &vs, true);
                b.ins().return_(&vs);
            }
            ir::Terminator::Jump(t) => {
                let a = args(&values, t);
                b.ins().jump(blocks[t.block.0 as usize], &a);
            }
            ir::Terminator::Branch { cond, then, otherwise } => {
                let (ta, oa) = (args(&values, then), args(&values, otherwise));
                b.ins().brif(
                    get(&values, *cond),
                    blocks[then.block.0 as usize],
                    &ta,
                    blocks[otherwise.block.0 as usize],
                    &oa,
                );
            }
            ir::Terminator::Trap => {
                b.ins().trap(TrapCode::user(1).expect("1 is a user trap code"));
            }
            ir::Terminator::TryCall { callee, args, normal, unwind, catch } => {
                let (sig, target) = match callee {
                    ir::Callee::Direct(name) => (syms.ir_sig(name).clone(), None),
                    ir::Callee::Indirect(sig, ptr) => (sig.clone(), Some(get(&values, *ptr))),
                };
                let call_args: Vec<clif::Value> = args.iter().map(|&a| get(&values, a)).collect();
                let call_args = abi_values(&mut b, &sig.params, &call_args, true);
                let sig_ref = b.import_signature(syms.signature(&sig));
                // Returns arrive as the calling convention passes them; a
                // block of our own turns floats back into integers.
                let ret_block = b.create_block();
                let ret_types: Vec<clif::Type> = sig.returns.iter().map(param_type).collect();
                let ret_args: Vec<BlockArg> = (0..ret_types.len()).map(|i| BlockArg::TryCallRet(i as u32)).collect();
                let normal_call = b.func.dfg.block_call(ret_block, &ret_args);
                let pad = b.create_block();
                let pad_call = b.func.dfg.block_call(pad, &[BlockArg::TryCallExn(0)]);
                let tag =
                    cranelift_codegen::ir::ExceptionTag::with_number(if *catch { TAG_CATCH } else { TAG_CLEANUP })
                        .expect("a small tag");
                let table = b.func.dfg.exception_tables.push(cranelift_codegen::ir::ExceptionTableData::new(
                    sig_ref,
                    normal_call,
                    [cranelift_codegen::ir::ExceptionTableItem::Tag(tag, pad_call)],
                ));
                match (callee, target) {
                    (ir::Callee::Direct(name), _) => {
                        let f = syms.func_ref(&mut b, &mut funcs, name);
                        b.ins().try_call(f, &call_args, table);
                    }
                    (_, Some(ptr)) => {
                        b.ins().try_call_indirect(ptr, &call_args, table);
                    }
                    _ => unreachable!("an indirect callee has an address"),
                }
                b.switch_to_block(ret_block);
                let rets: Vec<clif::Value> = ret_types.iter().map(|&t| b.append_block_param(ret_block, t)).collect();
                let rets = abi_values(&mut b, &sig.returns, &rets, false);
                let rets: Vec<BlockArg> = rets.into_iter().map(BlockArg::Value).collect();
                b.ins().jump(blocks[normal.0 as usize], &rets);
                b.switch_to_block(pad);
                b.set_cold_block(pad);
                let exn = b.append_block_param(pad, types::I64);
                b.ins().jump(blocks[unwind.0 as usize], &[BlockArg::Value(exn)]);
            }
        }
    }

    b.seal_all_blocks();
    b.finalize(syms.target_config);
}

/// 128-bit division and remainder. Cranelift has no 128-bit divide, and
/// compiler-builtins' `__udivti3` passes `i128` differently on each OS, so
/// Athrad divides inline, one bit at a time. Division by zero and
/// `MIN / -1` never get here: MIR checks them first.
fn div128(b: &mut FunctionBuilder, op: ir::BinOp, n: clif::Value, d: clif::Value) -> clif::Value {
    let signed = matches!(op, ir::BinOp::SDiv | ir::BinOp::SRem);
    let zero = b.ins().iconst(types::I64, 0);
    let zero = b.ins().uextend(types::I128, zero);
    let (un, ud, n_neg, d_neg) = if signed {
        let n_neg = b.ins().icmp(IntCC::SignedLessThan, n, zero);
        let d_neg = b.ins().icmp(IntCC::SignedLessThan, d, zero);
        let nn = b.ins().ineg(n);
        let nd = b.ins().ineg(d);
        let un = b.ins().select(n_neg, nn, n);
        let ud = b.ins().select(d_neg, nd, d);
        (un, ud, Some(n_neg), Some(d_neg))
    } else {
        (n, d, None, None)
    };

    let header = b.create_block();
    let body = b.create_block();
    let exit = b.create_block();
    for blk in [header, exit] {
        if blk == header {
            b.append_block_param(header, types::I32);
        }
        b.append_block_param(blk, types::I128);
        b.append_block_param(blk, types::I128);
    }
    let start = b.ins().iconst(types::I32, 127);
    b.ins().jump(header, &[BlockArg::Value(start), BlockArg::Value(zero), BlockArg::Value(zero)]);

    b.switch_to_block(header);
    let p = b.block_params(header).to_vec();
    let (i, q, r) = (p[0], p[1], p[2]);
    let more = b.ins().icmp_imm_s(IntCC::SignedGreaterThanOrEqual, i, 0);
    b.ins().brif(more, body, &[], exit, &[BlockArg::Value(q), BlockArg::Value(r)]);

    b.switch_to_block(body);
    let i128_i = b.ins().uextend(types::I128, i);
    let shifted = b.ins().ushr(un, i128_i);
    let one = b.ins().iconst(types::I64, 1);
    let one = b.ins().uextend(types::I128, one);
    let bit = b.ins().band(shifted, one);
    let r2 = b.ins().ishl_imm_u(r, 1);
    let r2 = b.ins().bor(r2, bit);
    let fits = b.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, r2, ud);
    let sub = b.ins().isub(r2, ud);
    let r3 = b.ins().select(fits, sub, r2);
    let qbit = b.ins().ishl(one, i128_i);
    let qset = b.ins().bor(q, qbit);
    let q2 = b.ins().select(fits, qset, q);
    let i2 = b.ins().iadd_imm_s(i, -1);
    b.ins().jump(header, &[BlockArg::Value(i2), BlockArg::Value(q2), BlockArg::Value(r3)]);

    b.switch_to_block(exit);
    let p = b.block_params(exit).to_vec();
    let (q, r) = (p[0], p[1]);
    match (op, n_neg, d_neg) {
        (ir::BinOp::UDiv, ..) => q,
        (ir::BinOp::URem, ..) => r,
        (ir::BinOp::SDiv, Some(nn), Some(dn)) => {
            let neg = b.ins().bxor(nn, dn);
            let nq = b.ins().ineg(q);
            b.ins().select(neg, nq, q)
        }
        (_, Some(nn), _) => {
            let nr = b.ins().ineg(r);
            b.ins().select(nn, nr, r)
        }
        _ => unreachable!("a division"),
    }
}

/// 128-bit add and subtract with an overflow flag, from the sign and carry
/// rules, for the ones Cranelift does not lower at 128 bits.
fn addsub128_overflow(
    b: &mut FunctionBuilder,
    op: ir::OverflowOp,
    signed: bool,
    x: clif::Value,
    y: clif::Value,
) -> (clif::Value, clif::Value) {
    let add = op == ir::OverflowOp::Add;
    let r = if add { b.ins().iadd(x, y) } else { b.ins().isub(x, y) };
    let of = if !signed {
        if add { b.ins().icmp(IntCC::UnsignedLessThan, r, x) } else { b.ins().icmp(IntCC::UnsignedLessThan, x, y) }
    } else {
        // Overflow when the result's sign differs from what the operands'
        // signs allow.
        let a = b.ins().bxor(x, r);
        let c = if add { b.ins().bxor(y, r) } else { b.ins().bxor(x, y) };
        let both = b.ins().band(a, c);
        let zero = b.ins().iconst(types::I64, 0);
        let zero = b.ins().uextend(types::I128, zero);
        b.ins().icmp(IntCC::SignedLessThan, both, zero)
    };
    (r, of)
}

/// 128-bit multiplication with an overflow flag, from 64-bit halves:
/// Cranelift's overflow multiply stops at 64 bits.
fn mul128_overflow(
    b: &mut FunctionBuilder,
    signed: bool,
    x: clif::Value,
    y: clif::Value,
) -> (clif::Value, clif::Value) {
    let r = b.ins().imul(x, y);
    if !signed {
        let of = umul128_overflows(b, x, y);
        return (r, of);
    }
    // Multiply the magnitudes; the signed result fits when the magnitude
    // fits in 127 bits, or is exactly 2^127 for a negative result.
    let zero = b.ins().iconst(types::I64, 0);
    let zero = b.ins().uextend(types::I128, zero);
    let xn = b.ins().icmp(IntCC::SignedLessThan, x, zero);
    let yn = b.ins().icmp(IntCC::SignedLessThan, y, zero);
    let nx = b.ins().ineg(x);
    let ny = b.ins().ineg(y);
    let ax = b.ins().select(xn, nx, x);
    let ay = b.ins().select(yn, ny, y);
    let uof = umul128_overflows(b, ax, ay);
    let m = b.ins().imul(ax, ay);
    let neg = b.ins().bxor(xn, yn);
    let (lo, hi) = b.ins().isplit(m);
    let top = b.ins().ushr_imm_u(hi, 63);
    let top_set = b.ins().icmp_imm_u(IntCC::NotEqual, top, 0);
    // 2^127: hi = 1 << 63, lo = 0.
    let min_hi = b.ins().iconst(types::I64, i64::MIN);
    let is_min_hi = b.ins().icmp(IntCC::Equal, hi, min_hi);
    let lo_zero = b.ins().icmp_imm_u(IntCC::Equal, lo, 0);
    let is_min = b.ins().band(is_min_hi, lo_zero);
    let fits_neg = b.ins().bnot(is_min);
    let neg_bad = b.ins().band(top_set, fits_neg);
    let sof = b.ins().select(neg, neg_bad, top_set);
    let of = b.ins().bor(uof, sof);
    (r, of)
}

/// Whether the unsigned 128-bit product of `x` and `y` overflows.
fn umul128_overflows(b: &mut FunctionBuilder, x: clif::Value, y: clif::Value) -> clif::Value {
    let (x0, x1) = b.ins().isplit(x);
    let (y0, y1) = b.ins().isplit(y);
    let x1nz = b.ins().icmp_imm_u(IntCC::NotEqual, x1, 0);
    let y1nz = b.ins().icmp_imm_u(IntCC::NotEqual, y1, 0);
    let both = b.ins().band(x1nz, y1nz);
    let carry = b.ins().umulhi(x0, y0);
    let t1 = b.ins().imul(x1, y0);
    let t1h = b.ins().umulhi(x1, y0);
    let t2 = b.ins().imul(x0, y1);
    let t2h = b.ins().umulhi(x0, y1);
    let t1hi = b.ins().icmp_imm_u(IntCC::NotEqual, t1h, 0);
    let t2hi = b.ins().icmp_imm_u(IntCC::NotEqual, t2h, 0);
    let (s, c1) = b.ins().uadd_overflow(t1, t2);
    let (_, c2) = b.ins().uadd_overflow(s, carry);
    let of = b.ins().bor(both, t1hi);
    let of = b.ins().bor(of, t2hi);
    let of = b.ins().bor(of, c1);
    b.ins().bor(of, c2)
}

/// Rust's float-to-integer `as`: toward zero, saturating at the type's
/// bounds, NaN to 0. Cranelift saturates to 32 and 64 bits; narrower types
/// go through 32 bits and a clamp.
fn float_to_int(b: &mut FunctionBuilder, x: clif::Value, to: ir::Type, signed: bool) -> clif::Value {
    let bits = to.bits();
    if bits >= 32 {
        let t = clif_type(to);
        return if signed { b.ins().fcvt_to_sint_sat(t, x) } else { b.ins().fcvt_to_uint_sat(t, x) };
    }
    let wide = if signed { b.ins().fcvt_to_sint_sat(types::I32, x) } else { b.ins().fcvt_to_uint_sat(types::I32, x) };
    let (lo, hi) = if signed { (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1) } else { (0, (1i64 << bits) - 1) };
    let lo = b.ins().iconst(types::I32, lo & 0xffff_ffff);
    let hi = b.ins().iconst(types::I32, hi);
    let clamped = if signed {
        let v = b.ins().smax(wide, lo);
        b.ins().smin(v, hi)
    } else {
        b.ins().umin(wide, hi)
    };
    b.ins().ireduce(clif_type(to), clamped)
}

fn iconst(b: &mut FunctionBuilder, ty: ir::Type, n: i128) -> clif::Value {
    if ty == ir::Type::I128 {
        let lo = b.ins().iconst(types::I64, n as i64);
        let hi = b.ins().iconst(types::I64, (n >> 64) as i64);
        return b.ins().iconcat(lo, hi);
    }
    // Cranelift wants narrow immediates zero-extended to 64 bits.
    let bits = ty.bits();
    let masked = if bits == 64 { n as i64 } else { (n as i64) & ((1i64 << bits) - 1) };
    b.ins().iconst(clif_type(ty), masked)
}

/// Reachable blocks of `func`, each after every block that dominates it.
fn reverse_postorder(func: &ir::Function) -> Vec<usize> {
    let succs = |i: usize| -> Vec<usize> {
        match &func.blocks[i].term {
            Some(ir::Terminator::Jump(t)) => vec![t.block.0 as usize],
            Some(ir::Terminator::Branch { then, otherwise, .. }) => {
                vec![then.block.0 as usize, otherwise.block.0 as usize]
            }
            Some(ir::Terminator::TryCall { normal, unwind, .. }) => vec![normal.0 as usize, unwind.0 as usize],
            _ => vec![],
        }
    };
    let mut seen = vec![false; func.blocks.len()];
    let mut post = Vec::with_capacity(func.blocks.len());
    // Iterative DFS: (block, next successor index).
    let mut stack = vec![(0usize, 0usize)];
    seen[0] = true;
    while let Some((block, next)) = stack.pop() {
        let s = succs(block);
        if let Some(&succ) = s.get(next) {
            stack.push((block, next + 1));
            if !seen[succ] {
                seen[succ] = true;
                stack.push((succ, 0));
            }
        } else {
            post.push(block);
        }
    }
    post.reverse();
    post
}
