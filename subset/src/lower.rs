//! The checked subset, lowered to Athrad IR the way `rustc` would compile
//! it in a debug build: every local in a stack slot, overflow and division
//! checks that call `core`'s panic functions with the location `rustc`
//! would report, and printing through the runtime's `print!` helpers.

use std::collections::HashMap;

use ir::{BinOp as IBin, BlockId, Cmp, FunctionBuilder, Linkage, OverflowOp, PTR, Signature, Slot, Type, Value};

use crate::check::{Ty, Typed};
use crate::syntax::{BinOp, Block, Expr, ExprKind, Func, IntConst, IntTy, Piece, Span, Stmt, UnOp};
use crate::{Abi, Fallback, fallback};

/// The function the runtime's `main` calls.
pub const USER_MAIN: &str = "athrad_user_main";

pub fn lower(funcs: &[Func], typed: &Typed, abi: &Abi, file: &str) -> Result<ir::Module, Fallback> {
    let mut m = Mod { module: ir::Module::new(), abi, file, datas: HashMap::new(), funcs };
    for f in funcs {
        let func = FnCx::new(&mut m, f, typed)?.lower(f)?;
        m.module.push(func);
    }
    Ok(m.module)
}

fn sym(name: &str) -> String {
    if name == "main" { USER_MAIN.to_owned() } else { format!("__athrad_subset_{name}") }
}

fn ir_ty(t: Ty) -> Option<Type> {
    match t {
        Ty::Int(i) => Some(int_ty(i)),
        Ty::Bool => Some(Type::I8),
        Ty::Unit | Ty::Never => None,
    }
}

fn int_ty(i: IntTy) -> Type {
    match i.bits() {
        8 => Type::I8,
        16 => Type::I16,
        32 => Type::I32,
        64 => Type::I64,
        _ => Type::I128,
    }
}

struct Mod<'a> {
    module: ir::Module,
    abi: &'a Abi,
    file: &'a str,
    /// Data symbols by contents.
    datas: HashMap<Vec<u8>, String>,
    funcs: &'a [Func],
}

impl Mod<'_> {
    fn data(&mut self, bytes: Vec<u8>, align: u64, relocs: Vec<ir::Reloc>) -> String {
        let mut key = bytes.clone();
        for r in &relocs {
            key.extend_from_slice(r.symbol.as_bytes());
            key.extend_from_slice(&r.offset.to_le_bytes());
        }
        if let Some(n) = self.datas.get(&key) {
            return n.clone();
        }
        let name = format!("__athrad_subset_data_{}", self.datas.len());
        self.module.push_data(ir::Data {
            name: name.clone(),
            linkage: Linkage::Local,
            writable: false,
            align,
            bytes,
            relocs,
        });
        self.datas.insert(key, name.clone());
        name
    }

    fn location(&mut self, span: Span) -> String {
        let l = &self.abi.location;
        let mut file = self.file.as_bytes().to_vec();
        if l.file_nul {
            file.push(0);
        }
        let len = file.len() as u64;
        let file_sym = self.data(file, 1, Vec::new());
        let mut bytes = vec![0u8; l.size as usize];
        bytes[l.file_len as usize..l.file_len as usize + 8].copy_from_slice(&len.to_le_bytes());
        bytes[l.line as usize..l.line as usize + 4].copy_from_slice(&span.line.to_le_bytes());
        bytes[l.col as usize..l.col as usize + 4].copy_from_slice(&span.col.to_le_bytes());
        let reloc = ir::Reloc { offset: l.file_ptr as u32, symbol: file_sym, addend: 0 };
        self.data(bytes, l.align, vec![reloc])
    }

    fn import(&mut self, name: &str, sig: Signature) {
        if self.module.signature(name).is_none() {
            self.module.import_func(name, sig);
        }
    }
}

struct FnCx<'a, 'm> {
    m: &'m mut Mod<'a>,
    typed: &'a Typed,
    b: FunctionBuilder,
    slots: HashMap<usize, Slot>,
    /// `(continue, break)` targets of the loops around.
    loops: Vec<(BlockId, BlockId)>,
}

impl<'a, 'm> FnCx<'a, 'm> {
    fn new(m: &'m mut Mod<'a>, f: &Func, typed: &'a Typed) -> Result<Self, Fallback> {
        let params: Vec<Type> =
            typed.params[&f.name].iter().map(|&id| ir_ty(typed.bindings[id].ty).expect("a value type")).collect();
        let ret: Vec<Type> = match f.ret {
            crate::syntax::TyName::Int(i) => vec![int_ty(i)],
            crate::syntax::TyName::Bool => vec![Type::I8],
            crate::syntax::TyName::Unit => vec![],
        };
        let linkage = if f.name == "main" { Linkage::Export } else { Linkage::Local };
        let b = FunctionBuilder::new(&sym(&f.name), linkage, Signature::new(params, ret));
        Ok(FnCx { m, typed, b, slots: HashMap::new(), loops: Vec::new() })
    }

    fn lower(mut self, f: &Func) -> Result<ir::Function, Fallback> {
        let params = self.b.params();
        for (i, &id) in self.typed.params[&f.name].iter().enumerate() {
            let s = self.slot(id);
            let a = self.b.slot_addr(s);
            self.b.store(a, 0, params[i]);
        }
        let v = self.block(&f.body)?;
        if !self.b.is_terminated() {
            match v {
                Some(v) => self.b.ret(&[v]),
                None if f.ret == crate::syntax::TyName::Unit => self.b.ret(&[]),
                None => self.b.trap(),
            }
        }
        Ok(self.b.finish())
    }

    fn slot(&mut self, id: usize) -> Slot {
        if let Some(s) = self.slots.get(&id) {
            return *s;
        }
        let t = ir_ty(self.typed.bindings[id].ty).expect("a value type");
        let s = self.b.slot(t.bytes(), t.bytes());
        self.slots.insert(id, s);
        s
    }

    fn load(&mut self, id: usize) -> Value {
        let t = ir_ty(self.typed.bindings[id].ty).expect("a value type");
        let s = self.slot(id);
        let a = self.b.slot_addr(s);
        self.b.load(t, a, 0)
    }

    fn store(&mut self, id: usize, v: Value) {
        let s = self.slot(id);
        let a = self.b.slot_addr(s);
        self.b.store(a, 0, v);
    }

    /// After code that does not continue (a return, a panic), go on in a
    /// block nothing reaches.
    fn unreachable(&mut self) {
        let (b, _) = self.b.block(&[]);
        self.b.switch_to(b);
    }

    fn block(&mut self, b: &Block) -> Result<Option<Value>, Fallback> {
        for s in &b.stmts {
            match s {
                Stmt::Let { init, .. } => {
                    let v = self.expr(init)?;
                    let id = self.typed.lets[&(s as *const Stmt)];
                    if let Some(v) = v {
                        self.store(id, v);
                    }
                }
                Stmt::Expr { expr, .. } => {
                    self.expr(expr)?;
                }
            }
        }
        match &b.tail {
            Some(t) => self.expr(t),
            None => Ok(None),
        }
    }

    fn panic(&mut self, item: &str, span: Span) -> Result<(), Fallback> {
        let callee = self.m.abi.panic(item).ok_or_else(|| fallback(format!("no `{item}`")))?.to_owned();
        let loc = self.m.location(span);
        self.m.import(&callee, Signature::new([PTR], []));
        let l = self.b.symbol_addr(&loc);
        self.b.call(&callee, &[l], &[]);
        self.b.trap();
        Ok(())
    }

    /// Panic with `item` when `cond` is set, else go on.
    fn check(&mut self, cond: Value, item: &str, span: Span) -> Result<(), Fallback> {
        let (bad, _) = self.b.block(&[]);
        let (ok, _) = self.b.block(&[]);
        self.b.branch(cond, (bad, &[]), (ok, &[]));
        self.b.switch_to(bad);
        self.panic(item, span)?;
        self.b.switch_to(ok);
        Ok(())
    }

    fn expr(&mut self, e: &Expr) -> Result<Option<Value>, Fallback> {
        let ty = self.typed.ty(e);
        let v = match &e.kind {
            ExprKind::Int(v, _) => {
                let Ty::Int(t) = ty else { return Err(fallback("an integer literal of another type")) };
                Some(self.b.iconst(int_ty(t), *v as i128))
            }
            ExprKind::Bool(b) => Some(self.b.iconst(Type::I8, i128::from(*b))),
            ExprKind::Var(_) => Some(self.load(self.typed.uses[&(e as *const Expr)])),
            ExprKind::IntConst(t, c) => Some(match c {
                IntConst::Max => self.b.iconst(int_ty(*t), t.max() as i128),
                IntConst::Min => self.b.iconst(int_ty(*t), t.min()),
                IntConst::Bits => self.b.iconst(Type::I32, i128::from(t.bits())),
            }),
            ExprKind::Call(name, args) => {
                let mut vs = Vec::new();
                for a in args {
                    match self.expr(a)? {
                        Some(v) => vs.push(v),
                        None => return self.diverged(),
                    }
                }
                let callee = self.m.funcs.iter().find(|f| &f.name == name).expect("checked");
                let ret: Vec<Type> = match callee.ret {
                    crate::syntax::TyName::Int(i) => vec![int_ty(i)],
                    crate::syntax::TyName::Bool => vec![Type::I8],
                    crate::syntax::TyName::Unit => vec![],
                };
                let r = self.b.call(&sym(name), &vs, &ret);
                r.first().copied()
            }
            ExprKind::BlackBox(inner) => self.expr(inner)?,
            // `-` on a literal is a negative literal, as rustc builds it: no
            // negation runs, so `-128i8` does not overflow.
            ExprKind::Unary(UnOp::Neg, inner) if matches!(inner.kind, ExprKind::Int(..)) => {
                let (ExprKind::Int(v, _), Ty::Int(t)) = (&inner.kind, ty) else { unreachable!("matched") };
                Some(self.b.iconst(int_ty(t), (*v as i128).wrapping_neg()))
            }
            ExprKind::Unary(op, inner) => {
                let Some(x) = self.expr(inner)? else { return self.diverged() };
                match (op, ty) {
                    (UnOp::Not, Ty::Bool) => {
                        let one = self.b.iconst(Type::I8, 1);
                        Some(self.b.binary(IBin::Xor, x, one))
                    }
                    (UnOp::Not, Ty::Int(t)) => {
                        let m1 = self.b.iconst(int_ty(t), -1);
                        Some(self.b.binary(IBin::Xor, x, m1))
                    }
                    (UnOp::Neg, Ty::Int(t)) => {
                        let min = self.b.iconst(int_ty(t), t.min());
                        let is_min = self.b.cmp(Cmp::Eq, x, min);
                        self.check(is_min, "panic_const_neg_overflow", e.span)?;
                        let zero = self.b.iconst(int_ty(t), 0);
                        Some(self.b.sub(zero, x))
                    }
                    _ => return Err(fallback("this unary operation")),
                }
            }
            ExprKind::Binary(op @ (BinOp::And | BinOp::Or), a, b) => {
                let Some(x) = self.expr(a)? else { return self.diverged() };
                let (rhs, _) = self.b.block(&[]);
                let (join, p) = self.b.block(&[Type::I8]);
                if *op == BinOp::And {
                    self.b.branch(x, (rhs, &[]), (join, &[x]));
                } else {
                    self.b.branch(x, (join, &[x]), (rhs, &[]));
                }
                self.b.switch_to(rhs);
                if let Some(y) = self.expr(b)? {
                    self.b.jump(join, &[y]);
                }
                self.b.switch_to(join);
                Some(p[0])
            }
            ExprKind::Binary(op, a, b) => {
                let Some(x) = self.expr(a)? else { return self.diverged() };
                let Some(y) = self.expr(b)? else { return self.diverged() };
                let (ta, tb) = (self.typed.ty(a), self.typed.ty(b));
                Some(self.binop(*op, x, y, ta, tb, e.span)?)
            }
            ExprKind::Assign(_, rhs) => {
                let Some(v) = self.expr(rhs)? else { return self.diverged() };
                self.store(self.typed.uses[&(e as *const Expr)], v);
                None
            }
            ExprKind::CompoundAssign(op, _, rhs) => {
                let id = self.typed.uses[&(e as *const Expr)];
                let Some(y) = self.expr(rhs)? else { return self.diverged() };
                let x = self.load(id);
                let t = self.typed.bindings[id].ty;
                let v = self.binop(*op, x, y, t, self.typed.ty(rhs), e.span)?;
                self.store(id, v);
                None
            }
            ExprKind::Cast(inner, to) => {
                let Some(x) = self.expr(inner)? else { return self.diverged() };
                let signed = matches!(self.typed.ty(inner), Ty::Int(t) if t.signed());
                Some(self.b.resize(int_ty(*to), x, signed))
            }
            ExprKind::If(cond, then, els) => {
                let Some(c) = self.expr(cond)? else { return self.diverged() };
                let rt = ir_ty(ty);
                let (tb, _) = self.b.block(&[]);
                let (eb, _) = self.b.block(&[]);
                let (join, p) = self.b.block(rt.as_slice());
                self.b.branch(c, (tb, &[]), (eb, &[]));
                self.b.switch_to(tb);
                let v = self.block(then)?;
                self.end_arm(join, v, rt.is_some());
                self.b.switch_to(eb);
                let v = match els {
                    Some(e) => self.expr(e)?,
                    None => None,
                };
                self.end_arm(join, v, rt.is_some());
                self.b.switch_to(join);
                p.first().copied()
            }
            ExprKind::While(cond, body) => {
                let (head, _) = self.b.block(&[]);
                let (inside, _) = self.b.block(&[]);
                let (exit, _) = self.b.block(&[]);
                self.b.jump(head, &[]);
                self.b.switch_to(head);
                if let Some(c) = self.expr(cond)? {
                    self.b.branch(c, (inside, &[]), (exit, &[]));
                }
                self.b.switch_to(inside);
                self.loops.push((head, exit));
                self.block(body)?;
                self.loops.pop();
                if !self.b.is_terminated() {
                    self.b.jump(head, &[]);
                }
                self.b.switch_to(exit);
                None
            }
            ExprKind::Loop(body) => {
                let (head, _) = self.b.block(&[]);
                let (exit, _) = self.b.block(&[]);
                self.b.jump(head, &[]);
                self.b.switch_to(head);
                self.loops.push((head, exit));
                self.block(body)?;
                self.loops.pop();
                if !self.b.is_terminated() {
                    self.b.jump(head, &[]);
                }
                self.b.switch_to(exit);
                None
            }
            ExprKind::For { start, end, inclusive, body, .. } => {
                let id = self.typed.uses[&(e as *const Expr)];
                let Ty::Int(t) = self.typed.bindings[id].ty else { return Err(fallback("a range of this type")) };
                let Some(lo) = self.expr(start)? else { return self.diverged() };
                let Some(hi) = self.expr(end)? else { return self.diverged() };
                let it = int_ty(t);
                let counter = self.b.slot(it.bytes(), it.bytes());
                let ca = self.b.slot_addr(counter);
                self.b.store(ca, 0, lo);
                let (lt, le) = if t.signed() { (Cmp::SLt, Cmp::SLe) } else { (Cmp::ULt, Cmp::ULe) };
                let (head, _) = self.b.block(&[]);
                let (inside, _) = self.b.block(&[]);
                let (step, _) = self.b.block(&[]);
                let (exit, _) = self.b.block(&[]);
                // `a..=b` is empty when a > b, and stops at b without
                // stepping past it, so it never overflows; `a..b` stops
                // before b.
                self.b.jump(head, &[]);
                self.b.switch_to(head);
                let ca = self.b.slot_addr(counter);
                let i = self.b.load(it, ca, 0);
                let go = self.b.cmp(if *inclusive { le } else { lt }, i, hi);
                self.b.branch(go, (inside, &[]), (exit, &[]));
                self.b.switch_to(inside);
                self.store(id, i);
                self.loops.push((step, exit));
                self.block(body)?;
                self.loops.pop();
                if !self.b.is_terminated() {
                    self.b.jump(step, &[]);
                }
                self.b.switch_to(step);
                let ca = self.b.slot_addr(counter);
                let i = self.b.load(it, ca, 0);
                if *inclusive {
                    let (more, _) = self.b.block(&[]);
                    let last = self.b.cmp(Cmp::Eq, i, hi);
                    self.b.branch(last, (exit, &[]), (more, &[]));
                    self.b.switch_to(more);
                }
                let one = self.b.iconst(it, 1);
                let next = self.b.add(i, one);
                let ca = self.b.slot_addr(counter);
                self.b.store(ca, 0, next);
                self.b.jump(head, &[]);
                self.b.switch_to(exit);
                None
            }
            ExprKind::Block(b) => self.block(b)?,
            ExprKind::Break | ExprKind::Continue => {
                let (cont, brk) = *self.loops.last().expect("checked: in a loop");
                let to = if matches!(e.kind, ExprKind::Break) { brk } else { cont };
                self.b.jump(to, &[]);
                self.unreachable();
                None
            }
            ExprKind::Return(v) => {
                let v = match v {
                    Some(v) => match self.expr(v)? {
                        Some(v) => vec![v],
                        None if self.typed.ty(v) == Ty::Never => return self.diverged(),
                        None => vec![],
                    },
                    None => vec![],
                };
                self.b.ret(&v);
                self.unreachable();
                None
            }
            ExprKind::Print(p) => {
                // Every argument first, as `format_args!` does.
                let mut vals = Vec::new();
                for a in &p.args {
                    let Some(v) = self.expr(a)? else { return self.diverged() };
                    vals.push((v, self.typed.ty(a)));
                }
                let pre = if p.stderr { "athrad_rt_eprint" } else { "athrad_rt_print" };
                for piece in &p.pieces {
                    match piece {
                        Piece::Text(s) => {
                            let d = self.m.data(s.as_bytes().to_vec(), 1, Vec::new());
                            let name = format!("{pre}_str");
                            self.m.import(&name, Signature::new([PTR, PTR], []));
                            let a = self.b.symbol_addr(&d);
                            let n = self.b.iconst(PTR, s.len() as i128);
                            self.b.call(&name, &[a, n], &[]);
                        }
                        Piece::Arg(i) => {
                            let (v, t) = vals[*i];
                            let (name, args, sig) = match t {
                                Ty::Bool => {
                                    let w = self.b.zext(Type::I32, v);
                                    (format!("{pre}_bool"), vec![w], Signature::new([Type::I32], []))
                                }
                                Ty::Int(t) if t.bits() == 128 => {
                                    let lo = self.b.trunc(Type::I64, v);
                                    let sixty_four = self.b.iconst(Type::I128, 64);
                                    let hi = self.b.binary(IBin::UShr, v, sixty_four);
                                    let hi = self.b.trunc(Type::I64, hi);
                                    let name = if t.signed() { "i128" } else { "u128" };
                                    (format!("{pre}_{name}"), vec![lo, hi], Signature::new([Type::I64, Type::I64], []))
                                }
                                Ty::Int(t) => {
                                    let w = self.b.resize(Type::I64, v, t.signed());
                                    let name = if t.signed() { "i64" } else { "u64" };
                                    (format!("{pre}_{name}"), vec![w], Signature::new([Type::I64], []))
                                }
                                _ => return Err(fallback("printing this type")),
                            };
                            self.m.import(&name, sig);
                            self.b.call(&name, &args, &[]);
                        }
                    }
                }
                None
            }
        };
        if ty == Ty::Never && !self.b.is_terminated() {
            // A call that never returns, a loop without a way out: nothing
            // follows it.
            self.b.trap();
            self.unreachable();
        }
        Ok(v)
    }

    fn diverged(&mut self) -> Result<Option<Value>, Fallback> {
        Ok(None)
    }

    fn end_arm(&mut self, join: BlockId, v: Option<Value>, has_value: bool) {
        if self.b.is_terminated() {
            return;
        }
        match (v, has_value) {
            (Some(v), true) => self.b.jump(join, &[v]),
            (_, false) => self.b.jump(join, &[]),
            // An arm of type `!` that got here falls off: unreachable.
            (None, true) => self.b.trap(),
        }
    }

    /// `x op y`, with the checks a debug build makes.
    fn binop(&mut self, op: BinOp, x: Value, y: Value, tx: Ty, ty: Ty, span: Span) -> Result<Value, Fallback> {
        use BinOp::*;
        if tx == Ty::Bool {
            return Ok(match op {
                BitAnd => self.b.binary(IBin::And, x, y),
                BitOr => self.b.binary(IBin::Or, x, y),
                BitXor => self.b.binary(IBin::Xor, x, y),
                Eq => self.b.cmp(Cmp::Eq, x, y),
                Ne => self.b.cmp(Cmp::Ne, x, y),
                Lt => self.b.cmp(Cmp::ULt, x, y),
                Le => self.b.cmp(Cmp::ULe, x, y),
                Gt => self.b.cmp(Cmp::UGt, x, y),
                Ge => self.b.cmp(Cmp::UGe, x, y),
                _ => return Err(fallback("this operation on `bool`")),
            });
        }
        let Ty::Int(t) = tx else { return Err(fallback("this operation")) };
        let s = t.signed();
        let it = int_ty(t);
        Ok(match op {
            Add | Sub | Mul => {
                let (o, item) = match op {
                    Add => (OverflowOp::Add, "panic_const_add_overflow"),
                    Sub => (OverflowOp::Sub, "panic_const_sub_overflow"),
                    _ => (OverflowOp::Mul, "panic_const_mul_overflow"),
                };
                let (r, of) = self.b.overflow(o, s, x, y);
                self.check(of, item, span)?;
                r
            }
            Div | Rem => {
                let zero = self.b.iconst(it, 0);
                let is_zero = self.b.cmp(Cmp::Eq, y, zero);
                let (z, ov) = if op == Div {
                    ("panic_const_div_by_zero", "panic_const_div_overflow")
                } else {
                    ("panic_const_rem_by_zero", "panic_const_rem_overflow")
                };
                self.check(is_zero, z, span)?;
                if s {
                    let min = self.b.iconst(it, t.min());
                    let m1 = self.b.iconst(it, -1);
                    let a = self.b.cmp(Cmp::Eq, x, min);
                    let b = self.b.cmp(Cmp::Eq, y, m1);
                    let both = self.b.binary(IBin::And, a, b);
                    self.check(both, ov, span)?;
                }
                let o = match (op, s) {
                    (Div, true) => IBin::SDiv,
                    (Div, false) => IBin::UDiv,
                    (_, true) => IBin::SRem,
                    (_, false) => IBin::URem,
                };
                self.b.binary(o, x, y)
            }
            BitAnd => self.b.binary(IBin::And, x, y),
            BitOr => self.b.binary(IBin::Or, x, y),
            BitXor => self.b.binary(IBin::Xor, x, y),
            Shl | Shr => {
                let Ty::Int(rt) = ty else { return Err(fallback("a shift by this type")) };
                // The amount, read as unsigned, must be below the width.
                let bits = self.b.iconst(int_ty(rt), i128::from(t.bits()));
                let too_far = self.b.cmp(Cmp::UGe, y, bits);
                let item = if op == Shl { "panic_const_shl_overflow" } else { "panic_const_shr_overflow" };
                self.check(too_far, item, span)?;
                let n = self.b.resize(it, y, false);
                let o = match (op, s) {
                    (Shl, _) => IBin::Shl,
                    (_, true) => IBin::SShr,
                    (_, false) => IBin::UShr,
                };
                self.b.binary(o, x, n)
            }
            Eq => self.b.cmp(Cmp::Eq, x, y),
            Ne => self.b.cmp(Cmp::Ne, x, y),
            Lt => self.b.cmp(if s { Cmp::SLt } else { Cmp::ULt }, x, y),
            Le => self.b.cmp(if s { Cmp::SLe } else { Cmp::ULe }, x, y),
            Gt => self.b.cmp(if s { Cmp::SGt } else { Cmp::UGt }, x, y),
            Ge => self.b.cmp(if s { Cmp::SGe } else { Cmp::UGe }, x, y),
            And | Or => unreachable!("lowered with branches"),
        })
    }
}
