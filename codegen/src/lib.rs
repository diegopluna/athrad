//! Lower Athrad IR through Cranelift into an object for the host.
//!
//! The IR is already SSA with block parameters, Cranelift's own shape, so
//! lowering maps blocks to blocks and values to values with no variable
//! renaming. The module must have passed [`ir::verify`]; this crate panics on
//! IR the verifier rejects.

use std::collections::HashMap;

use anyhow::{Context as _, Result, anyhow};
use cranelift_codegen::Context;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{self as clif, AbiParam, BlockArg, InstBuilder, types};
use cranelift_codegen::isa::OwnedTargetIsa;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};

/// Compile `module` for the host into an object named `name`.
pub fn compile(module: &ir::Module, name: &str) -> Result<object::write::Object<'static>> {
    let isa = host_isa()?;
    let builder = ObjectBuilder::new(isa, name, cranelift_module::default_libcall_names())?;
    let mut obj = ObjectModule::new(builder);

    // Declare every symbol first, so calls can refer to any of them.
    let mut ids: HashMap<&str, FuncId> = HashMap::new();
    for ext in &module.externs {
        let sig = signature(&obj, &ext.sig);
        ids.insert(&ext.name, obj.declare_function(&ext.name, Linkage::Import, &sig)?);
    }
    for func in &module.functions {
        let sig = signature(&obj, &func.sig);
        let linkage = match func.linkage {
            ir::Linkage::Local => Linkage::Local,
            ir::Linkage::Export => Linkage::Export,
        };
        ids.insert(&func.name, obj.declare_function(&func.name, linkage, &sig)?);
    }

    let mut ctx = obj.make_context();
    let mut fb_ctx = FunctionBuilderContext::new();
    for func in &module.functions {
        ctx.func.signature = signature(&obj, &func.sig);
        lower(&mut obj, &ids, func, &mut ctx, &mut fb_ctx);
        obj.define_function(ids[func.name.as_str()], &mut ctx)
            .map_err(|e| anyhow!("{e:?}"))
            .with_context(|| format!("compiling fn {}", func.name))?;
        obj.clear_context(&mut ctx);
    }

    Ok(obj.finish().object)
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
        ir::Type::I32 => types::I32,
        ir::Type::I64 => types::I64,
    }
}

fn signature(obj: &ObjectModule, sig: &ir::Signature) -> clif::Signature {
    let mut s = obj.make_signature();
    s.params.extend(sig.params.iter().map(|&t| AbiParam::new(clif_type(t))));
    s.returns.extend(sig.ret.map(|t| AbiParam::new(clif_type(t))));
    s
}

fn lower(
    obj: &mut ObjectModule,
    ids: &HashMap<&str, FuncId>,
    func: &ir::Function,
    ctx: &mut Context,
    fb_ctx: &mut FunctionBuilderContext,
) {
    let mut b = FunctionBuilder::new(&mut ctx.func, fb_ctx);

    let blocks: Vec<clif::Block> = func.blocks.iter().map(|_| b.create_block()).collect();
    let mut values: Vec<Option<clif::Value>> = vec![None; func.values.len()];
    let mut callees: HashMap<&str, clif::FuncRef> = HashMap::new();

    // Block parameters first: a branch may target a block lowered later.
    for (i, block) in func.blocks.iter().enumerate() {
        for &p in &block.params {
            let ty = clif_type(func.values[p.0 as usize]);
            values[p.0 as usize] = Some(b.append_block_param(blocks[i], ty));
        }
    }

    // Lower in dominator-compatible order: the verifier guarantees each use
    // is dominated by its definition, and IR block order puts the entry
    // first, but not every definition before every use in index order. Walk
    // in reverse postorder so definitions are lowered before their uses.
    for i in reverse_postorder(func) {
        let block = &func.blocks[i];
        b.switch_to_block(blocks[i]);
        let get = |values: &[Option<clif::Value>], v: ir::Value| values[v.0 as usize].expect("verified IR");
        for inst in &block.insts {
            let result = match &inst.op {
                ir::Op::Const(ty, n) => Some(b.ins().iconst(clif_type(*ty), *n)),
                ir::Op::Binary(op, x, y) => {
                    let (x, y) = (get(&values, *x), get(&values, *y));
                    let ins = b.ins();
                    Some(match op {
                        ir::BinOp::Add => ins.iadd(x, y),
                        ir::BinOp::Sub => ins.isub(x, y),
                        ir::BinOp::Mul => ins.imul(x, y),
                        ir::BinOp::UDiv => ins.udiv(x, y),
                        ir::BinOp::URem => ins.urem(x, y),
                        ir::BinOp::SDiv => ins.sdiv(x, y),
                        ir::BinOp::SRem => ins.srem(x, y),
                    })
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
                    Some(b.ins().icmp(cc, get(&values, *x), get(&values, *y)))
                }
                ir::Op::ZExt(ty, v) => Some(b.ins().uextend(clif_type(*ty), get(&values, *v))),
                ir::Op::SExt(ty, v) => Some(b.ins().sextend(clif_type(*ty), get(&values, *v))),
                ir::Op::Trunc(ty, v) => Some(b.ins().ireduce(clif_type(*ty), get(&values, *v))),
                ir::Op::Call(name, args) => {
                    let callee = *callees
                        .entry(name.as_str())
                        .or_insert_with(|| obj.declare_func_in_func(ids[name.as_str()], b.func));
                    let args: Vec<clif::Value> = args.iter().map(|&a| get(&values, a)).collect();
                    let call = b.ins().call(callee, &args);
                    b.inst_results(call).first().copied()
                }
            };
            if let Some(r) = inst.result {
                values[r.0 as usize] = Some(result.expect("verified IR: result types match"));
            }
        }
        let args = |values: &[Option<clif::Value>], t: &ir::Target| -> Vec<BlockArg> {
            t.args.iter().map(|&a| BlockArg::Value(get(values, a))).collect()
        };
        match block.term.as_ref().expect("verified IR: every block is terminated") {
            ir::Terminator::Return(v) => {
                let v: Vec<clif::Value> = v.iter().map(|&v| get(&values, v)).collect();
                b.ins().return_(&v);
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
        }
    }

    b.seal_all_blocks();
    b.finalize(obj.target_config());
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
