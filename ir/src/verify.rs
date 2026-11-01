//! Checks a module before codegen sees it. Codegen may assume a verified
//! module and panic on anything this lets through.

use std::collections::HashSet;
use std::fmt;

use crate::{BlockId, Function, Inst, Module, Op, Target, Terminator, Type, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyError {
    pub function: Option<String>,
    pub message: String,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.function {
            Some(name) => write!(f, "in fn {name}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for VerifyError {}

pub fn verify(module: &Module) -> Result<(), VerifyError> {
    let mut names = HashSet::new();
    for name in module.externs.iter().map(|e| &e.name).chain(module.functions.iter().map(|f| &f.name)) {
        if name.is_empty() {
            return Err(VerifyError { function: None, message: "empty symbol name".into() });
        }
        if !names.insert(name) {
            return Err(VerifyError { function: None, message: format!("symbol `{name}` is defined twice") });
        }
    }
    for func in &module.functions {
        Checker { module, func }
            .check()
            .map_err(|message| VerifyError { function: Some(func.name.clone()), message })?;
    }
    Ok(())
}

struct Checker<'a> {
    module: &'a Module,
    func: &'a Function,
}

impl Checker<'_> {
    fn check(&self) -> Result<(), String> {
        let func = self.func;
        let Some(entry) = func.blocks.first() else {
            return Err("no blocks".into());
        };
        let entry_types: Vec<Type> = entry.params.iter().map(|&p| self.ty(p)).collect::<Result<_, _>>()?;
        if entry_types != func.sig.params {
            return Err("entry block parameters do not match the signature".into());
        }

        // Every value is defined exactly once, by a block parameter or an instruction.
        let mut defined_in = vec![None; func.values.len()];
        for (b, block) in func.blocks.iter().enumerate() {
            let defs = block.params.iter().copied().chain(block.insts.iter().filter_map(|i| i.result));
            for v in defs {
                let slot = defined_in.get_mut(v.0 as usize).ok_or_else(|| format!("{v} has no type"))?;
                if slot.replace(b).is_some() {
                    return Err(format!("{v} is defined twice"));
                }
            }
        }

        let preds = self.predecessors()?;
        let doms = dominators(func.blocks.len(), &preds);

        for (b, block) in func.blocks.iter().enumerate() {
            let id = BlockId(b as u32);
            let mut local: HashSet<Value> = block.params.iter().copied().collect();
            let available = |v: Value, local: &HashSet<Value>| -> Result<Type, String> {
                let ty = self.ty(v)?;
                let def = defined_in[v.0 as usize].ok_or_else(|| format!("{v} is never defined"))?;
                let ok = if def == b { local.contains(&v) } else { doms[b].contains(&def) };
                if ok { Ok(ty) } else { Err(format!("{v} is used in {id} where its definition does not dominate")) }
            };
            for inst in &block.insts {
                let ty = self.check_inst(inst, |v| available(v, &local))?;
                match (inst.result, ty) {
                    (Some(r), Some(ty)) if self.ty(r)? == ty => {}
                    (None, None) => {}
                    _ => return Err(format!("{id}: result of `{:?}` has the wrong type", inst.op)),
                }
                if let Some(r) = inst.result {
                    local.insert(r);
                }
            }
            match &block.term {
                None => return Err(format!("{id} has no terminator")),
                Some(Terminator::Return(v)) => {
                    let ty = v.map(|v| available(v, &local)).transpose()?;
                    if ty != func.sig.ret {
                        return Err(format!("{id}: return type does not match the signature"));
                    }
                }
                Some(Terminator::Jump(t)) => self.check_target(t, |v| available(v, &local))?,
                Some(Terminator::Branch { cond, then, otherwise }) => {
                    available(*cond, &local)?;
                    self.check_target(then, |v| available(v, &local))?;
                    self.check_target(otherwise, |v| available(v, &local))?;
                }
            }
        }
        Ok(())
    }

    fn ty(&self, v: Value) -> Result<Type, String> {
        self.func.value_type(v).ok_or_else(|| format!("{v} has no type"))
    }

    fn check_inst(&self, inst: &Inst, use_: impl Fn(Value) -> Result<Type, String>) -> Result<Option<Type>, String> {
        Ok(match &inst.op {
            Op::Const(ty, _) => Some(*ty),
            Op::Binary(op, a, b) => {
                let (ta, tb) = (use_(*a)?, use_(*b)?);
                if ta != tb {
                    return Err(format!("{op:?} of {ta} and {tb}"));
                }
                Some(ta)
            }
            Op::Cmp(c, a, b) => {
                let (ta, tb) = (use_(*a)?, use_(*b)?);
                if ta != tb {
                    return Err(format!("{c:?} of {ta} and {tb}"));
                }
                Some(Type::I8)
            }
            Op::ZExt(to, v) | Op::SExt(to, v) => {
                if use_(*v)?.bits() >= to.bits() {
                    return Err(format!("extension of {v} to {to} does not widen"));
                }
                Some(*to)
            }
            Op::Trunc(to, v) => {
                if use_(*v)?.bits() <= to.bits() {
                    return Err(format!("truncation of {v} to {to} does not narrow"));
                }
                Some(*to)
            }
            Op::Call(name, args) => {
                let sig = self.module.signature(name).ok_or_else(|| format!("call to unknown function `{name}`"))?;
                let types: Vec<Type> = args.iter().map(|&a| use_(a)).collect::<Result<_, _>>()?;
                if types != sig.params {
                    return Err(format!("arguments to `{name}` do not match its signature"));
                }
                sig.ret
            }
        })
    }

    fn check_target(&self, t: &Target, use_: impl Fn(Value) -> Result<Type, String>) -> Result<(), String> {
        if t.block.0 == 0 {
            return Err("branch to the entry block".into());
        }
        let block = self.func.block(t.block).ok_or_else(|| format!("branch to missing {}", t.block))?;
        let args: Vec<Type> = t.args.iter().map(|&a| use_(a)).collect::<Result<_, _>>()?;
        let params: Vec<Type> = block.params.iter().map(|&p| self.ty(p)).collect::<Result<_, _>>()?;
        if args != params {
            return Err(format!("arguments to {} do not match its parameters", t.block));
        }
        Ok(())
    }

    fn predecessors(&self) -> Result<Vec<Vec<usize>>, String> {
        let mut preds = vec![Vec::new(); self.func.blocks.len()];
        for (b, block) in self.func.blocks.iter().enumerate() {
            let succs: Vec<BlockId> = match &block.term {
                Some(Terminator::Jump(t)) => vec![t.block],
                Some(Terminator::Branch { then, otherwise, .. }) => vec![then.block, otherwise.block],
                _ => vec![],
            };
            for s in succs {
                preds.get_mut(s.0 as usize).ok_or_else(|| format!("branch to missing {s}"))?.push(b);
            }
        }
        Ok(preds)
    }
}

/// For each block, the set of blocks that dominate it (itself included).
/// Unreachable blocks are dominated by everything, which is harmless: they
/// never run, and Cranelift drops them.
fn dominators(n: usize, preds: &[Vec<usize>]) -> Vec<HashSet<usize>> {
    let all: HashSet<usize> = (0..n).collect();
    let mut doms = vec![all; n];
    doms[0] = HashSet::from([0]);
    let mut changed = true;
    while changed {
        changed = false;
        for b in 1..n {
            let mut new = preds[b]
                .iter()
                .map(|&p| doms[p].clone())
                .reduce(|a, b| a.intersection(&b).copied().collect())
                .unwrap_or_else(|| (0..n).collect());
            new.insert(b);
            if new != doms[b] {
                doms[b] = new;
                changed = true;
            }
        }
    }
    doms
}

#[cfg(test)]
mod tests {
    use crate::*;

    fn add_fn() -> Function {
        let mut b =
            FunctionBuilder::new("add", Linkage::Local, Signature::new([Type::I64, Type::I64], Some(Type::I64)));
        let p = b.params();
        let s = b.add(p[0], p[1]);
        b.ret(Some(s));
        b.finish()
    }

    #[test]
    fn accepts_add() {
        let mut m = Module::new();
        m.push(add_fn());
        verify(&m).unwrap();
    }

    #[test]
    fn rejects_wrong_return_type() {
        let mut b = FunctionBuilder::new("f", Linkage::Local, Signature::new([], Some(Type::I64)));
        let v = b.iconst(Type::I32, 1);
        b.ret(Some(v));
        let mut m = Module::new();
        m.push(b.finish());
        assert!(verify(&m).is_err());
    }

    #[test]
    fn rejects_unknown_callee() {
        let mut b = FunctionBuilder::new("f", Linkage::Local, Signature::default());
        b.call("missing", &[], None);
        b.ret(None);
        let mut m = Module::new();
        m.push(b.finish());
        assert!(verify(&m).unwrap_err().message.contains("missing"));
    }

    #[test]
    fn rejects_use_without_dominating_def() {
        let mut b = FunctionBuilder::new("f", Linkage::Local, Signature::new([Type::I8], Some(Type::I64)));
        let c = b.params()[0];
        let (left, _) = b.block(&[]);
        let (right, _) = b.block(&[]);
        b.branch(c, (left, &[]), (right, &[]));
        b.switch_to(left);
        let v = b.iconst(Type::I64, 1);
        b.ret(Some(v));
        b.switch_to(right);
        b.ret(Some(v));
        let mut m = Module::new();
        m.push(b.finish());
        assert!(verify(&m).unwrap_err().message.contains("dominate"));
    }

    #[test]
    fn rejects_duplicate_symbols() {
        let mut m = Module::new();
        m.push(add_fn());
        m.push(add_fn());
        assert!(verify(&m).is_err());
    }
}
