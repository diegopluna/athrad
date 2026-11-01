//! Checks a module before codegen sees it. Codegen may assume a verified
//! module and panic on anything this lets through.

use std::collections::HashSet;
use std::fmt;

use crate::{BlockId, Function, Inst, Module, Op, PTR, Symbol, Target, Terminator, Type, Value};

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
    let module_err = |message: String| VerifyError { function: None, message };
    let mut names = HashSet::new();
    let all = module
        .imports
        .iter()
        .map(|e| &e.name)
        .chain(module.functions.iter().map(|f| &f.name))
        .chain(module.data.iter().map(|d| &d.name));
    for name in all {
        if name.is_empty() {
            return Err(module_err("empty symbol name".into()));
        }
        if !names.insert(name) {
            return Err(module_err(format!("symbol `{name}` is defined twice")));
        }
    }
    for d in &module.data {
        if !d.align.is_power_of_two() {
            return Err(module_err(format!("data `{}` has alignment {}", d.name, d.align)));
        }
        for r in &d.relocs {
            if r.offset as usize + 8 > d.bytes.len() {
                return Err(module_err(format!("data `{}` has a relocation past its end", d.name)));
            }
            if module.symbol(&r.symbol).is_none() {
                return Err(module_err(format!("data `{}` refers to unknown symbol `{}`", d.name, r.symbol)));
            }
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
        if entry_types != func.sig.param_types() {
            return Err("entry block parameters do not match the signature".into());
        }
        for s in &func.slots {
            if !s.align.is_power_of_two() {
                return Err(format!("stack slot with alignment {}", s.align));
            }
        }

        // Every value is defined exactly once, by a block parameter or an instruction.
        let mut defined_in = vec![None; func.values.len()];
        for (b, block) in func.blocks.iter().enumerate() {
            let defs = block.params.iter().chain(block.insts.iter().flat_map(|i| &i.results));
            for &v in defs {
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
                let types = self.check_inst(inst, |v| available(v, &local)).map_err(|e| format!("{id}: {e}"))?;
                let results: Vec<Type> = inst.results.iter().map(|&r| self.ty(r)).collect::<Result<_, _>>()?;
                if results != types {
                    return Err(format!("{id}: results of `{:?}` have the wrong types", inst.op));
                }
                local.extend(inst.results.iter().copied());
            }
            match &block.term {
                None => return Err(format!("{id} has no terminator")),
                Some(Terminator::Return(vs)) => {
                    let types: Vec<Type> = vs.iter().map(|&v| available(v, &local)).collect::<Result<_, _>>()?;
                    if types != func.sig.return_types() {
                        return Err(format!("{id}: return values do not match the signature"));
                    }
                }
                Some(Terminator::Jump(t)) => self.check_target(t, |v| available(v, &local))?,
                Some(Terminator::Branch { cond, then, otherwise }) => {
                    available(*cond, &local)?;
                    self.check_target(then, |v| available(v, &local))?;
                    self.check_target(otherwise, |v| available(v, &local))?;
                }
                Some(Terminator::Trap) => {}
            }
        }
        Ok(())
    }

    fn ty(&self, v: Value) -> Result<Type, String> {
        self.func.value_type(v).ok_or_else(|| format!("{v} has no type"))
    }

    /// The types of the values `inst` defines.
    fn check_inst(&self, inst: &Inst, use_: impl Fn(Value) -> Result<Type, String>) -> Result<Vec<Type>, String> {
        let same = |a: Value, b: Value, what: &dyn fmt::Debug| -> Result<Type, String> {
            let (ta, tb) = (use_(a)?, use_(b)?);
            if ta != tb {
                return Err(format!("{what:?} of {ta} and {tb}"));
            }
            Ok(ta)
        };
        let ptr = |v: Value| -> Result<(), String> {
            let t = use_(v)?;
            if t != PTR { Err(format!("{v} is used as an address but is {t}")) } else { Ok(()) }
        };
        let args_match = |name: &str, args: &[Value], params: Vec<Type>| -> Result<(), String> {
            let types: Vec<Type> = args.iter().map(|&a| use_(a)).collect::<Result<_, _>>()?;
            if types != params {
                return Err(format!("arguments to `{name}` do not match its signature"));
            }
            Ok(())
        };
        Ok(match &inst.op {
            Op::Const(ty, _) => vec![*ty],
            Op::Binary(op, a, b) => vec![same(*a, *b, op)?],
            Op::Unary(_, a) => vec![use_(*a)?],
            Op::FBinary(op, ft, a, b) => {
                if same(*a, *b, op)? != ft.bits_type() {
                    return Err(format!("{op:?} of {ft:?} on {}", use_(*a)?));
                }
                vec![ft.bits_type()]
            }
            Op::FUnary(op, ft, a) => {
                if use_(*a)? != ft.bits_type() {
                    return Err(format!("{op:?} of {ft:?} on {}", use_(*a)?));
                }
                vec![ft.bits_type()]
            }
            Op::FCmp(c, ft, a, b) => {
                if same(*a, *b, c)? != ft.bits_type() {
                    return Err(format!("{c:?} of {ft:?} on {}", use_(*a)?));
                }
                vec![Type::I8]
            }
            Op::FToInt { from, to, v, .. } => {
                if use_(*v)? != from.bits_type() || *to == Type::I128 {
                    return Err(format!("conversion of {v} from {from:?} to {to}"));
                }
                vec![*to]
            }
            Op::IntToF { to, v, .. } => {
                if use_(*v)? == Type::I128 {
                    return Err(format!("conversion of i128 {v} to {to:?}"));
                }
                vec![to.bits_type()]
            }
            Op::FConvert { from, to, v } => {
                if use_(*v)? != from.bits_type() || from == to {
                    return Err(format!("conversion of {v} from {from:?} to {to:?}"));
                }
                vec![to.bits_type()]
            }
            Op::Overflow(op, _, a, b) => {
                let t = same(*a, *b, op)?;
                if t == Type::I128 && *op == crate::OverflowOp::Mul {
                    return Err(format!("{op:?} overflow check of i128"));
                }
                vec![t, Type::I8]
            }
            Op::Cmp(c, a, b) => {
                same(*a, *b, c)?;
                vec![Type::I8]
            }
            Op::Select(c, a, b) => {
                use_(*c)?;
                vec![same(*a, *b, &"select")?]
            }
            Op::ZExt(to, v) | Op::SExt(to, v) => {
                if use_(*v)?.bits() >= to.bits() {
                    return Err(format!("extension of {v} to {to} does not widen"));
                }
                vec![*to]
            }
            Op::Trunc(to, v) => {
                if use_(*v)?.bits() <= to.bits() {
                    return Err(format!("truncation of {v} to {to} does not narrow"));
                }
                vec![*to]
            }
            Op::SlotAddr(s) => {
                if s.0 as usize >= self.func.slots.len() {
                    return Err(format!("{s} does not exist"));
                }
                vec![PTR]
            }
            Op::SymbolAddr(name) => {
                self.module.symbol(name).ok_or_else(|| format!("address of unknown symbol `{name}`"))?;
                vec![PTR]
            }
            Op::Load(ty, addr, _) => {
                ptr(*addr)?;
                vec![*ty]
            }
            Op::Store(addr, _, v) => {
                ptr(*addr)?;
                use_(*v)?;
                vec![]
            }
            Op::Call(name, args) => {
                let sig = match self.module.symbol(name) {
                    Some(Symbol::Func(sig)) => sig,
                    Some(Symbol::Data) => return Err(format!("call to data symbol `{name}`")),
                    None => return Err(format!("call to unknown function `{name}`")),
                };
                args_match(name, args, sig.param_types())?;
                sig.return_types()
            }
            Op::CallIndirect(sig, callee, args) => {
                ptr(*callee)?;
                args_match("indirect callee", args, sig.param_types())?;
                sig.return_types()
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
            for s in successors(block.term.as_ref()) {
                preds.get_mut(s.0 as usize).ok_or_else(|| format!("branch to missing {s}"))?.push(b);
            }
        }
        Ok(preds)
    }
}

fn successors(term: Option<&Terminator>) -> Vec<BlockId> {
    match term {
        Some(Terminator::Jump(t)) => vec![t.block],
        Some(Terminator::Branch { then, otherwise, .. }) => vec![then.block, otherwise.block],
        _ => vec![],
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
        let mut b = FunctionBuilder::new("add", Linkage::Local, Signature::new([Type::I64, Type::I64], [Type::I64]));
        let p = b.params();
        let s = b.add(p[0], p[1]);
        b.ret(&[s]);
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
        let mut b = FunctionBuilder::new("f", Linkage::Local, Signature::new([], [Type::I64]));
        let v = b.iconst(Type::I32, 1);
        b.ret(&[v]);
        let mut m = Module::new();
        m.push(b.finish());
        assert!(verify(&m).is_err());
    }

    #[test]
    fn rejects_unknown_callee() {
        let mut b = FunctionBuilder::new("f", Linkage::Local, Signature::default());
        b.call("missing", &[], &[]);
        b.ret(&[]);
        let mut m = Module::new();
        m.push(b.finish());
        assert!(verify(&m).unwrap_err().message.contains("missing"));
    }

    #[test]
    fn rejects_use_without_dominating_def() {
        let mut b = FunctionBuilder::new("f", Linkage::Local, Signature::new([Type::I8], [Type::I64]));
        let c = b.params()[0];
        let (left, _) = b.block(&[]);
        let (right, _) = b.block(&[]);
        b.branch(c, (left, &[]), (right, &[]));
        b.switch_to(left);
        let v = b.iconst(Type::I64, 1);
        b.ret(&[v]);
        b.switch_to(right);
        b.ret(&[v]);
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

    #[test]
    fn rejects_non_pointer_address() {
        let mut b = FunctionBuilder::new("f", Linkage::Local, Signature::new([Type::I32], []));
        let p = b.params()[0];
        b.load(Type::I8, p, 0);
        b.ret(&[]);
        let mut m = Module::new();
        m.push(b.finish());
        assert!(verify(&m).unwrap_err().message.contains("address"));
    }

    #[test]
    fn rejects_relocation_to_unknown_symbol() {
        let mut m = Module::new();
        m.push_data(Data {
            name: "d".into(),
            linkage: Linkage::Local,
            writable: false,
            align: 8,
            bytes: vec![0; 8],
            relocs: vec![Reloc { offset: 0, symbol: "nowhere".into(), addend: 0 }],
        });
        assert!(verify(&m).unwrap_err().message.contains("nowhere"));
    }
}
