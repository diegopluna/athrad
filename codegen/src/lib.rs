//! Lower Athrad IR through Cranelift into an object for the host.
//!
//! The IR is already SSA with block parameters, Cranelift's own shape, so
//! lowering maps blocks to blocks and values to values with no variable
//! renaming. The module must have passed [`ir::verify`]; this crate panics on
//! IR the verifier rejects.

use std::collections::HashMap;

use anyhow::{Context as _, Result, anyhow, bail};
use cranelift_codegen::Context;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    self as clif, AbiParam, ArgumentPurpose, BlockArg, InstBuilder, MemFlagsData, StackSlotData, StackSlotKind,
    TrapCode, types,
};
use cranelift_codegen::isa::OwnedTargetIsa;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};

mod unwind;

/// Compile `module` for the host into an object named `name`.
pub fn compile(module: &ir::Module, name: &str) -> Result<object::write::Object<'static>> {
    let isa = host_isa()?;
    let builder = ObjectBuilder::new(isa.clone(), name, cranelift_module::default_libcall_names())?;
    let mut obj = ObjectModule::new(builder);

    // Declare every symbol first, so code and data can refer to any of them.
    let mut ids: HashMap<&str, Id> = HashMap::new();
    for imp in &module.imports {
        let id = match &imp.kind {
            ir::ImportKind::Func(sig) => {
                Id::Func(obj.declare_function(&imp.name, Linkage::Import, &signature(&obj, sig))?)
            }
            ir::ImportKind::Data => Id::Data(obj.declare_data(&imp.name, Linkage::Import, false, false)?),
        };
        ids.insert(&imp.name, id);
    }
    for func in &module.functions {
        let sig = signature(&obj, &func.sig);
        ids.insert(&func.name, Id::Func(obj.declare_function(&func.name, linkage(func.linkage), &sig)?));
    }
    for d in &module.data {
        ids.insert(&d.name, Id::Data(obj.declare_data(&d.name, linkage(d.linkage), d.writable, false)?));
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

    let mut sigs: HashMap<&str, &ir::Signature> = HashMap::new();
    for imp in &module.imports {
        if let ir::ImportKind::Func(sig) = &imp.kind {
            sigs.insert(&imp.name, sig);
        }
    }
    for func in &module.functions {
        sigs.insert(&func.name, &func.sig);
    }

    let mut ctx = obj.make_context();
    let mut fb_ctx = FunctionBuilderContext::new();
    let mut unwind_entries = Vec::new();
    for func in &module.functions {
        ctx.func.signature = signature(&obj, &func.sig);
        lower(&mut obj, &ids, &sigs, func, &mut ctx, &mut fb_ctx);
        let Id::Func(id) = ids[func.name.as_str()] else { unreachable!() };
        obj.define_function(id, &mut ctx)
            .map_err(|e| anyhow!("{e:?}"))
            .with_context(|| format!("compiling fn {}", func.name))?;
        let code = ctx.compiled_code().expect("defined");
        if let Some(info) = code.create_unwind_info(obj.isa())? {
            unwind_entries.push(unwind::Entry { func: id, info, size: code.code_info().total_size });
        }
        obj.clear_context(&mut ctx);
    }

    let mut product = obj.finish();
    unwind::emit(&*isa, &mut product, unwind_entries)?;
    Ok(product.object)
}

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
    flags.set("preserve_frame_pointers", "true")?;
    // ELF and Mach-O executables are position independent by default. On
    // COFF, link.exe makes import thunks and base relocations itself.
    let pic = !cfg!(windows);
    flags.set("is_pic", if pic { "true" } else { "false" })?;
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

fn signature(obj: &ObjectModule, sig: &ir::Signature) -> clif::Signature {
    let mut s = obj.make_signature();
    s.params.extend(sig.params.iter().map(abi_param));
    s.returns.extend(sig.returns.iter().map(abi_param));
    s
}

fn lower(
    obj: &mut ObjectModule,
    ids: &HashMap<&str, Id>,
    sigs: &HashMap<&str, &ir::Signature>,
    func: &ir::Function,
    ctx: &mut Context,
    fb_ctx: &mut FunctionBuilderContext,
) {
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
            let mut func_ref = |b: &mut FunctionBuilder, name: &str| -> clif::FuncRef {
                *funcs.entry(ids.get_key_value(name).expect("verified IR").0).or_insert_with(|| {
                    let Id::Func(id) = ids[name] else { unreachable!("verified IR: `{name}` is a function") };
                    obj.declare_func_in_func(id, b.func)
                })
            };
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
                ir::Op::Overflow(op @ (ir::OverflowOp::Add | ir::OverflowOp::Sub), signed, x, y)
                    if func.values[x.0 as usize] == ir::Type::I128 =>
                {
                    let (x, y) = (get(&values, *x), get(&values, *y));
                    let (r, of) = addsub128_overflow(&mut b, *op, *signed, x, y);
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
                ir::Op::SymbolAddr(name) => match ids[name.as_str()] {
                    Id::Func(_) => {
                        let f = func_ref(&mut b, name);
                        vec![b.ins().func_addr(types::I64, f)]
                    }
                    Id::Data(id) => {
                        let gv = *globals
                            .entry(ids.get_key_value(name.as_str()).unwrap().0)
                            .or_insert_with(|| obj.declare_data_in_func(id, b.func));
                        vec![b.ins().symbol_value(types::I64, gv)]
                    }
                },
                ir::Op::Load(ty, addr, offset) => {
                    vec![b.ins().load(clif_type(*ty), MemFlagsData::new(), get(&values, *addr), *offset)]
                }
                ir::Op::Store(addr, offset, v) => {
                    b.ins().store(MemFlagsData::new(), get(&values, *v), get(&values, *addr), *offset);
                    vec![]
                }
                ir::Op::Call(name, args) => {
                    let callee = func_ref(&mut b, name);
                    let sig = sigs[name.as_str()];
                    let args: Vec<clif::Value> = args.iter().map(|&a| get(&values, a)).collect();
                    let args = abi_values(&mut b, &sig.params, &args, true);
                    let call = b.ins().call(callee, &args);
                    let results = b.inst_results(call).to_vec();
                    abi_values(&mut b, &sig.returns, &results, false)
                }
                ir::Op::CallIndirect(sig, callee, args) => {
                    let clif_sig = b.import_signature(signature(obj, sig));
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
        }
    }

    b.seal_all_blocks();
    b.finalize(obj.target_config());
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
