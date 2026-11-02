//! The deny-by-default lints that make `rustc` reject programs the type
//! checker accepts: `overflowing_literals`, and `arithmetic_overflow` and
//! `unconditional_panic`, which fire when `rustc` can see at compile time
//! that an operation will panic.
//!
//! `rustc` finds those by propagating constants through each function.
//! Athrad must reject (fall back on) at least every program `rustc` does,
//! so it propagates at least as far: through every local, in straight-line
//! code and into both arms of every `if`, forgetting only what a loop body
//! or an arm assigns. When in doubt it reports a panic it cannot rule out,
//! and the program goes to `rustc`, which is never wrong.

use std::collections::HashMap;

use crate::check::{Ty, Typed};
use crate::syntax::{BinOp, Block, Expr, ExprKind, Func, IntConst, IntTy, Stmt, UnOp};
use crate::{Fallback, fallback};

pub fn lint(funcs: &[Func], typed: &Typed) -> Result<(), Fallback> {
    for f in funcs {
        let mut l = Lint { typed, env: HashMap::new() };
        l.block(&f.body)?;
    }
    Ok(())
}

/// A known integer value, as the bits of its type, sign-extended into an
/// `i128` for signed types and zero-extended into a `u128` for unsigned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum K {
    Int(IntTy, u128),
    Bool(bool),
}

struct Lint<'a> {
    typed: &'a Typed,
    /// Bindings with a known value.
    env: HashMap<usize, K>,
}

fn wrap(t: IntTy, v: u128) -> u128 {
    if t.bits() == 128 {
        return v;
    }
    let mask = (1u128 << t.bits()) - 1;
    let v = v & mask;
    if t.signed() && v >> (t.bits() - 1) & 1 == 1 { v | !mask } else { v }
}

fn as_i(v: u128) -> i128 {
    v as i128
}

impl Lint<'_> {
    fn block(&mut self, b: &Block) -> Result<(), Fallback> {
        for s in &b.stmts {
            match s {
                Stmt::Let { init, .. } => {
                    let v = self.expr(init)?;
                    let id = self.typed.lets[&(s as *const Stmt)];
                    self.set(id, v);
                }
                Stmt::Expr { expr, .. } => {
                    self.expr(expr)?;
                }
            }
        }
        if let Some(t) = &b.tail {
            self.expr(t)?;
        }
        Ok(())
    }

    fn set(&mut self, id: usize, v: Option<K>) {
        match v {
            Some(v) => self.env.insert(id, v),
            None => self.env.remove(&id),
        };
    }

    /// Forget what `b` assigns anywhere inside it.
    fn forget_assigned(&mut self, e: &Expr) {
        walk(e, &mut |x| {
            if let ExprKind::Assign(..) | ExprKind::CompoundAssign(..) = x.kind
                && let Some(id) = self.typed.uses.get(&(x as *const Expr))
            {
                self.env.remove(id);
            }
        });
    }

    fn int_ty(&self, e: &Expr) -> Option<IntTy> {
        match self.typed.ty(e) {
            Ty::Int(t) => Some(t),
            _ => None,
        }
    }

    fn expr(&mut self, e: &Expr) -> Result<Option<K>, Fallback> {
        let ty = self.typed.ty(e);
        Ok(match &e.kind {
            ExprKind::Int(v, _) => {
                let Ty::Int(t) = ty else { return Ok(None) };
                if *v > t.max() {
                    return Err(fallback(format!("the literal {v}, out of range for `{t:?}`")));
                }
                Some(K::Int(t, wrap(t, *v)))
            }
            ExprKind::Bool(b) => Some(K::Bool(*b)),
            ExprKind::Var(_) => self.env.get(&self.typed.uses[&(e as *const Expr)]).copied(),
            ExprKind::IntConst(t, c) => Some(match c {
                IntConst::Max => K::Int(*t, wrap(*t, t.max())),
                IntConst::Min => K::Int(*t, t.min() as u128),
                IntConst::Bits => K::Int(IntTy::U32, u128::from(t.bits())),
            }),
            ExprKind::Call(_, args) => {
                for a in args {
                    self.expr(a)?;
                }
                None
            }
            ExprKind::BlackBox(inner) => {
                self.expr(inner)?;
                None
            }
            ExprKind::Unary(UnOp::Neg, inner) => {
                let Some(t) = self.int_ty(inner) else { return Ok(None) };
                if !t.signed() {
                    return Err(fallback("`-` on an unsigned integer"));
                }
                // `-128i8` is a literal rustc accepts though 128 is not an i8.
                if let ExprKind::Int(v, _) = inner.kind {
                    if v > t.max() + 1 {
                        return Err(fallback(format!("the literal -{v}, out of range for `{t:?}`")));
                    }
                    return Ok(Some(K::Int(t, wrap(t, 0u128.wrapping_sub(v)))));
                }
                match self.expr(inner)? {
                    Some(K::Int(_, v)) if as_i(v) == t.min() => {
                        return Err(fallback("a negation rustc sees overflow"));
                    }
                    Some(K::Int(_, v)) => Some(K::Int(t, wrap(t, 0u128.wrapping_sub(v)))),
                    _ => None,
                }
            }
            ExprKind::Unary(UnOp::Not, inner) => match self.expr(inner)? {
                Some(K::Int(t, v)) => Some(K::Int(t, wrap(t, !v))),
                Some(K::Bool(b)) => Some(K::Bool(!b)),
                None => None,
            },
            ExprKind::Binary(op, a, b) => {
                let x = self.expr(a)?;
                if matches!(op, BinOp::And | BinOp::Or) {
                    // The right side may not run; what it assigns is unknown.
                    let saved = self.env.clone();
                    self.expr(b)?;
                    self.env = saved;
                    self.forget_assigned(b);
                    return Ok(None);
                }
                let y = self.expr(b)?;
                self.binop(*op, x, y, self.int_ty(a), self.int_ty(b))?
            }
            ExprKind::Assign(_, rhs) => {
                let v = self.expr(rhs)?;
                self.set(self.typed.uses[&(e as *const Expr)], v);
                None
            }
            ExprKind::CompoundAssign(op, _, rhs) => {
                let id = self.typed.uses[&(e as *const Expr)];
                let y = self.expr(rhs)?;
                let x = self.env.get(&id).copied();
                let t = match self.typed.bindings[id].ty {
                    Ty::Int(t) => Some(t),
                    _ => None,
                };
                let v = self.binop(*op, x, y, t, self.int_ty(rhs))?;
                self.set(id, v);
                None
            }
            ExprKind::Cast(inner, to) => match self.expr(inner)? {
                Some(K::Int(_, v)) => Some(K::Int(*to, wrap(*to, v))),
                Some(K::Bool(b)) => Some(K::Int(*to, u128::from(b))),
                None => None,
            },
            ExprKind::If(cond, then, els) => {
                self.expr(cond)?;
                let before = self.env.clone();
                self.block(then)?;
                let after_then = std::mem::replace(&mut self.env, before);
                if let Some(e) = els {
                    self.expr(e)?;
                }
                // Keep what both arms agree on.
                self.env.retain(|k, v| after_then.get(k) == Some(v));
                None
            }
            ExprKind::While(cond, body) => {
                self.forget_assigned(e);
                self.expr(cond)?;
                self.block(body)?;
                self.forget_assigned(e);
                let _ = cond;
                None
            }
            ExprKind::Loop(body) => {
                self.forget_assigned(e);
                self.block(body)?;
                self.forget_assigned(e);
                None
            }
            ExprKind::For { start, end, body, .. } => {
                self.expr(start)?;
                self.expr(end)?;
                self.forget_assigned(e);
                self.block(body)?;
                self.forget_assigned(e);
                None
            }
            ExprKind::Block(b) => {
                self.block(b)?;
                None
            }
            ExprKind::Break | ExprKind::Continue | ExprKind::Return(None) => None,
            ExprKind::Return(Some(v)) => {
                self.expr(v)?;
                None
            }
            ExprKind::Print(p) => {
                for a in &p.args {
                    self.expr(a)?;
                }
                None
            }
        })
    }

    fn binop(
        &self,
        op: BinOp,
        x: Option<K>,
        y: Option<K>,
        tx: Option<IntTy>,
        ty: Option<IntTy>,
    ) -> Result<Option<K>, Fallback> {
        use BinOp::*;
        let panics = |what: &str| Err(fallback(format!("{what}, which rustc sees at compile time")));
        // Panics rustc finds from the right side alone.
        if let (Some(t), Some(K::Int(_, v))) = (tx, y) {
            match op {
                Shl | Shr => {
                    let amount = ty.map(|ty| if ty.signed() { as_i(v) } else { v as i128 }).unwrap_or(0);
                    if amount < 0 || amount >= i128::from(t.bits()) {
                        return panics("a shift by too much");
                    }
                }
                Div | Rem if v == 0 => return panics("a division by zero"),
                _ => {}
            }
        }
        let (Some(x), Some(y)) = (x, y) else { return Ok(None) };
        Ok(match (x, y) {
            (K::Bool(a), K::Bool(b)) => Some(K::Bool(match op {
                BitAnd => a & b,
                BitOr => a | b,
                BitXor => a ^ b,
                Eq => a == b,
                Ne => a != b,
                Lt => !a & b,
                Le => a <= b,
                Gt => a & !b,
                Ge => a >= b,
                _ => return Ok(None),
            })),
            (K::Int(t, a), K::Int(_, b)) => {
                let s = t.signed();
                let cmp = |f: fn(std::cmp::Ordering) -> bool| {
                    let o = if s { as_i(a).cmp(&as_i(b)) } else { a.cmp(&b) };
                    Some(K::Bool(f(o)))
                };
                let in_range = |r: Option<i128>, ru: Option<u128>| -> Result<Option<K>, Fallback> {
                    let fits = if s {
                        r.is_some_and(|r| r >= t.min() && r <= t.max() as i128)
                    } else {
                        ru.is_some_and(|r| r <= t.max())
                    };
                    if !fits {
                        return panics("an overflow");
                    }
                    Ok(Some(K::Int(t, if s { r.expect("fits") as u128 } else { ru.expect("fits") })))
                };
                match op {
                    Add => in_range(as_i(a).checked_add(as_i(b)), a.checked_add(b))?,
                    Sub => in_range(as_i(a).checked_sub(as_i(b)), a.checked_sub(b))?,
                    Mul => in_range(as_i(a).checked_mul(as_i(b)), a.checked_mul(b))?,
                    Div | Rem => {
                        if s && as_i(a) == t.min() && as_i(b) == -1 {
                            return panics("an overflowing division");
                        }
                        let r = if op == Div {
                            if s { (as_i(a) / as_i(b)) as u128 } else { a / b }
                        } else if s {
                            (as_i(a) % as_i(b)) as u128
                        } else {
                            a % b
                        };
                        Some(K::Int(t, wrap(t, r)))
                    }
                    BitAnd => Some(K::Int(t, a & b)),
                    BitOr => Some(K::Int(t, a | b)),
                    BitXor => Some(K::Int(t, a ^ b)),
                    Shl => Some(K::Int(t, wrap(t, a << (b as u32 % t.bits())))),
                    Shr => Some(K::Int(
                        t,
                        if s { (as_i(a) >> (b as u32 % t.bits())) as u128 } else { a >> (b as u32 % t.bits()) },
                    )),
                    Eq => cmp(|o| o.is_eq()),
                    Ne => cmp(|o| o.is_ne()),
                    Lt => cmp(|o| o.is_lt()),
                    Le => cmp(|o| o.is_le()),
                    Gt => cmp(|o| o.is_gt()),
                    Ge => cmp(|o| o.is_ge()),
                    And | Or => None,
                }
            }
            _ => None,
        })
    }
}

/// Every expression in `e`, itself included.
pub fn walk(e: &Expr, f: &mut dyn FnMut(&Expr)) {
    f(e);
    let block = |b: &Block, f: &mut dyn FnMut(&Expr)| {
        for s in &b.stmts {
            match s {
                Stmt::Let { init, .. } => walk(init, f),
                Stmt::Expr { expr, .. } => walk(expr, f),
            }
        }
        if let Some(t) = &b.tail {
            walk(t, f);
        }
    };
    match &e.kind {
        ExprKind::Call(_, args) => args.iter().for_each(|a| walk(a, f)),
        ExprKind::BlackBox(a) | ExprKind::Unary(_, a) | ExprKind::Cast(a, _) => walk(a, f),
        ExprKind::Assign(_, a) | ExprKind::CompoundAssign(_, _, a) => walk(a, f),
        ExprKind::Binary(_, a, b) => {
            walk(a, f);
            walk(b, f);
        }
        ExprKind::If(c, t, els) => {
            walk(c, f);
            block(t, f);
            if let Some(e) = els {
                walk(e, f);
            }
        }
        ExprKind::While(c, b) => {
            walk(c, f);
            block(b, f);
        }
        ExprKind::Loop(b) | ExprKind::Block(b) => block(b, f),
        ExprKind::For { start, end, body, .. } => {
            walk(start, f);
            walk(end, f);
            block(body, f);
        }
        ExprKind::Return(Some(v)) => walk(v, f),
        ExprKind::Print(p) => p.args.iter().for_each(|a| walk(a, f)),
        _ => {}
    }
}
