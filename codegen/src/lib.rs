//! Lower Athrad IR through Cranelift into an object for the host.
//!
//! The IR is already SSA with block parameters, Cranelift's own shape, so
//! lowering maps blocks to blocks and values to values with no variable
//! renaming. The module must have passed [`ir::verify`]; this crate panics on
//! IR the verifier rejects.

use std::collections::HashMap;

use anyhow::{Context as _, Result, anyhow, bail};
use cranelift_codegen::Context;
use cranelift_codegen::ir::condcodes::IntCC;
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

    let mut ctx = obj.make_context();
    let mut fb_ctx = FunctionBuilderContext::new();
    let mut unwind_entries = Vec::new();
    for func in &module.functions {
        ctx.func.signature = signature(&obj, &func.sig);
        lower(&mut obj, &ids, func, &mut ctx, &mut fb_ctx);
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

fn abi_param(p: &ir::Param) -> AbiParam {
    let a = if p.sret {
        AbiParam::special(clif_type(p.ty), ArgumentPurpose::StructReturn)
    } else {
        AbiParam::new(clif_type(p.ty))
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
                    }]
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
                    let args: Vec<clif::Value> = args.iter().map(|&a| get(&values, a)).collect();
                    let call = b.ins().call(callee, &args);
                    b.inst_results(call).to_vec()
                }
                ir::Op::CallIndirect(sig, callee, args) => {
                    let sig = b.import_signature(signature(obj, sig));
                    let args: Vec<clif::Value> = args.iter().map(|&a| get(&values, a)).collect();
                    let call = b.ins().call_indirect(sig, get(&values, *callee), &args);
                    b.inst_results(call).to_vec()
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
