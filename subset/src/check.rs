//! Names, types, and mutability: what `rustc` would check for the subset,
//! so that every program accepted here is one `rustc` accepts, with the
//! same types. Anything uncertain is a [`Fallback`].
//!
//! Types are inferred as `rustc` infers them for integers: an unsuffixed
//! literal is an integer variable, unified with whatever it meets, and
//! `i32` if it meets nothing.

use std::collections::HashMap;

use crate::syntax::{BinOp, Block, Expr, ExprKind, Func, IntConst, IntTy, Stmt, TyName, UnOp};
use crate::{Fallback, fallback};

/// A type during inference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum T {
    Int(IntTy),
    /// An integer of a type not known yet.
    Var(u32),
    Bool,
    Unit,
    Never,
}

/// A resolved type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    Int(IntTy),
    Bool,
    Unit,
    Never,
}

impl Ty {
    fn of(t: TyName) -> Ty {
        match t {
            TyName::Int(i) => Ty::Int(i),
            TyName::Bool => Ty::Bool,
            TyName::Unit => Ty::Unit,
        }
    }
}

/// A local variable or parameter.
#[derive(Clone, Debug)]
pub struct Binding {
    pub mutable: bool,
    pub ty: Ty,
}

/// What checking found, for linting and lowering: every expression's type,
/// and which binding each name refers to.
#[derive(Default)]
pub struct Typed {
    pub types: HashMap<*const Expr, Ty>,
    /// Each `Var`, `Assign`, `CompoundAssign` and `For` expression's binding.
    pub uses: HashMap<*const Expr, usize>,
    /// Each `let` statement's binding.
    pub lets: HashMap<*const Stmt, usize>,
    pub bindings: Vec<Binding>,
    /// Each function's parameters' bindings.
    pub params: HashMap<String, Vec<usize>>,
}

impl Typed {
    pub fn ty(&self, e: &Expr) -> Ty {
        self.types[&(e as *const Expr)]
    }
}

pub fn check(funcs: &[Func]) -> Result<Typed, Fallback> {
    let mut sigs: HashMap<&str, (Vec<Ty>, Ty)> = HashMap::new();
    for f in funcs {
        let sig = (f.params.iter().map(|p| Ty::of(p.ty)).collect(), Ty::of(f.ret));
        if sigs.insert(&f.name, sig).is_some() {
            return Err(fallback(format!("two functions named `{}`", f.name)));
        }
    }
    match sigs.get("main") {
        Some((p, Ty::Unit)) if p.is_empty() => {}
        _ => return Err(fallback("a `main` that is not `fn main()`")),
    }
    let mut cx = Cx {
        sigs: &sigs,
        vars: Vec::new(),
        types: HashMap::new(),
        uses: HashMap::new(),
        lets: HashMap::new(),
        bindings: Vec::new(),
        binding_types: Vec::new(),
        scopes: Vec::new(),
        loops: 0,
        ret: T::Unit,
        params: HashMap::new(),
    };
    for f in funcs {
        cx.func(f)?;
    }
    // Resolve: an integer variable that met nothing is `i32`.
    let resolve = |cx: &Cx, t: T| match cx.find(t) {
        T::Int(i) => Ty::Int(i),
        T::Var(_) => Ty::Int(IntTy::I32),
        T::Bool => Ty::Bool,
        T::Unit => Ty::Unit,
        T::Never => Ty::Never,
    };
    let mut typed = Typed { params: std::mem::take(&mut cx.params), ..Typed::default() };
    for (e, t) in &cx.types {
        typed.types.insert(*e, resolve(&cx, *t));
    }
    for (i, b) in cx.bindings.iter().enumerate() {
        typed.bindings.push(Binding { ty: resolve(&cx, cx.binding_types[i]), ..b.clone() });
    }
    typed.uses = std::mem::take(&mut cx.uses);
    typed.lets = std::mem::take(&mut cx.lets);
    Ok(typed)
}

struct Cx<'a> {
    sigs: &'a HashMap<&'a str, (Vec<Ty>, Ty)>,
    /// Integer variables: each points at another, or is resolved, or not.
    vars: Vec<VarState>,
    types: HashMap<*const Expr, T>,
    uses: HashMap<*const Expr, usize>,
    lets: HashMap<*const Stmt, usize>,
    bindings: Vec<Binding>,
    binding_types: Vec<T>,
    scopes: Vec<HashMap<String, usize>>,
    loops: u32,
    ret: T,
    params: HashMap<String, Vec<usize>>,
}

#[derive(Clone, Copy)]
enum VarState {
    Unknown,
    Is(IntTy),
    Same(u32),
}

fn t_of(t: TyName) -> T {
    match t {
        TyName::Int(i) => T::Int(i),
        TyName::Bool => T::Bool,
        TyName::Unit => T::Unit,
    }
}

impl Cx<'_> {
    fn find(&self, t: T) -> T {
        let T::Var(mut v) = t else { return t };
        loop {
            match self.vars[v as usize] {
                VarState::Same(w) => v = w,
                VarState::Is(i) => return T::Int(i),
                VarState::Unknown => return T::Var(v),
            }
        }
    }

    fn new_var(&mut self) -> T {
        self.vars.push(VarState::Unknown);
        T::Var(self.vars.len() as u32 - 1)
    }

    /// Make `a` and `b` the same type. `Never` fits anything.
    fn unify(&mut self, a: T, b: T) -> Result<T, Fallback> {
        let (a, b) = (self.find(a), self.find(b));
        Ok(match (a, b) {
            (T::Never, x) | (x, T::Never) => x,
            (x, y) if x == y => x,
            (T::Var(v), T::Var(w)) => {
                self.vars[v as usize] = VarState::Same(w);
                b
            }
            (T::Var(v), T::Int(i)) | (T::Int(i), T::Var(v)) => {
                self.vars[v as usize] = VarState::Is(i);
                T::Int(i)
            }
            _ => return Err(fallback(format!("a type mismatch ({a:?} and {b:?})"))),
        })
    }

    fn is_int(&self, t: T) -> bool {
        matches!(self.find(t), T::Int(_) | T::Var(_))
    }

    fn func(&mut self, f: &Func) -> Result<(), Fallback> {
        self.scopes.push(HashMap::new());
        let mut ids = Vec::new();
        for p in &f.params {
            let id = self.bind(&p.name, p.mutable, t_of(p.ty));
            ids.push(id);
        }
        self.params.insert(f.name.clone(), ids);
        self.ret = t_of(f.ret);
        let body = self.block(&f.body)?;
        self.unify(body, self.ret)?;
        self.scopes.pop();
        Ok(())
    }

    fn bind(&mut self, name: &str, mutable: bool, t: T) -> usize {
        let id = self.bindings.len();
        // The type is filled in when resolving.
        self.bindings.push(Binding { mutable, ty: Ty::Unit });
        self.binding_types.push(t);
        self.scopes.last_mut().expect("a scope").insert(name.to_owned(), id);
        id
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        self.scopes.iter().rev().find_map(|s| s.get(name).copied())
    }

    fn block(&mut self, b: &Block) -> Result<T, Fallback> {
        self.scopes.push(HashMap::new());
        let mut diverges = false;
        for s in &b.stmts {
            match s {
                Stmt::Let { name, mutable, ty, init } => {
                    let t = self.expr(init)?;
                    let t = match ty {
                        Some(ty) => self.unify(t, t_of(*ty))?,
                        None => t,
                    };
                    diverges |= self.find(t) == T::Never;
                    // A `let` of a diverging value has a type of its own in
                    // rustc; keep the subset away from that.
                    if self.find(t) == T::Never {
                        return Err(fallback("`let` of a value that never arrives"));
                    }
                    if self.find(t) == T::Unit {
                        return Err(fallback("a `()` variable"));
                    }
                    let id = self.bind(name, *mutable, t);
                    self.lets.insert(s as *const Stmt, id);
                }
                Stmt::Expr { expr, semi } => {
                    let t = self.expr(expr)?;
                    if !semi && !matches!(self.find(t), T::Unit | T::Never) {
                        return Err(fallback("a block-like statement that is not `()`"));
                    }
                    diverges |= self.find(t) == T::Never;
                }
            }
        }
        let t = match &b.tail {
            Some(e) => self.expr(e)?,
            None if diverges => T::Never,
            None => T::Unit,
        };
        self.scopes.pop();
        Ok(t)
    }

    fn expr(&mut self, e: &Expr) -> Result<T, Fallback> {
        let t = self.expr_inner(e)?;
        self.types.insert(e as *const Expr, t);
        Ok(t)
    }

    fn expr_inner(&mut self, e: &Expr) -> Result<T, Fallback> {
        Ok(match &e.kind {
            ExprKind::Int(_, Some(t)) => T::Int(*t),
            ExprKind::Int(_, None) => self.new_var(),
            ExprKind::Bool(_) => T::Bool,
            ExprKind::Var(name) => {
                let id = self.lookup(name).ok_or_else(|| fallback(format!("the name `{name}`")))?;
                self.uses.insert(e as *const Expr, id);
                self.binding_types[id]
            }
            ExprKind::IntConst(t, IntConst::Bits) => {
                let _ = t;
                T::Int(IntTy::U32)
            }
            ExprKind::IntConst(t, _) => T::Int(*t),
            ExprKind::Call(name, args) => {
                if self.lookup(name).is_some() {
                    return Err(fallback(format!("calling the variable `{name}`")));
                }
                let (params, ret) =
                    self.sigs.get(name.as_str()).ok_or_else(|| fallback(format!("calling `{name}`")))?;
                let (params, ret) = (params.clone(), *ret);
                if params.len() != args.len() {
                    return Err(fallback(format!("a call to `{name}` with the wrong number of arguments")));
                }
                for (a, p) in args.iter().zip(params) {
                    let t = self.expr(a)?;
                    self.unify(t, ty_t(p))?;
                }
                ty_t(ret)
            }
            ExprKind::BlackBox(inner) => self.expr(inner)?,
            ExprKind::Unary(op, inner) => {
                let t = self.expr(inner)?;
                match (op, self.find(t)) {
                    (UnOp::Neg, T::Int(i)) if i.signed() => t,
                    // An integer variable met by `-` must be signed; leave
                    // that to rustc.
                    (UnOp::Neg, T::Var(_)) => t,
                    (UnOp::Not, T::Int(_) | T::Var(_) | T::Bool) => t,
                    (_, T::Never) => T::Never,
                    _ => return Err(fallback(format!("`{op:?}` on {t:?}"))),
                }
            }
            ExprKind::Binary(op, a, b) => {
                let ta = self.expr(a)?;
                let tb = self.expr(b)?;
                self.binary(*op, ta, tb)?
            }
            ExprKind::Assign(name, rhs) => {
                let id = self.assignable(name, e)?;
                let t = self.expr(rhs)?;
                self.unify(t, self.binding_types[id])?;
                T::Unit
            }
            ExprKind::CompoundAssign(op, name, rhs) => {
                let id = self.assignable(name, e)?;
                let t = self.expr(rhs)?;
                let var = self.binding_types[id];
                if !self.is_int(var)
                    && !(self.find(var) == T::Bool && matches!(op, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor))
                {
                    return Err(fallback("a compound assignment on this type"));
                }
                if matches!(op, BinOp::Shl | BinOp::Shr) {
                    if !self.is_int(t) {
                        return Err(fallback("a shift by a non-integer"));
                    }
                } else {
                    self.unify(t, var)?;
                }
                T::Unit
            }
            ExprKind::Cast(inner, to) => {
                let t = self.expr(inner)?;
                match self.find(t) {
                    T::Int(_) | T::Bool => {}
                    // rustc types a literal by what it is cast to, through
                    // `-`, blocks and `if`s; which literal, and what then
                    // overflows, are its to decide.
                    T::Var(_) if ends_in_literal(inner) => {
                        return Err(fallback("a cast of an integer literal without a suffix"));
                    }
                    T::Var(_) => {}
                    T::Never => {}
                    _ => return Err(fallback("this cast")),
                }
                T::Int(*to)
            }
            ExprKind::If(cond, then, els) => {
                let c = self.expr(cond)?;
                self.unify(c, T::Bool)?;
                let t = self.block(then)?;
                match els {
                    Some(e) => {
                        let u = self.expr(e)?;
                        self.unify(t, u)?
                    }
                    None => self.unify(t, T::Unit)?,
                }
            }
            ExprKind::While(cond, body) => {
                let c = self.expr(cond)?;
                self.unify(c, T::Bool)?;
                self.loops += 1;
                let t = self.block(body)?;
                self.loops -= 1;
                self.unify(t, T::Unit)?;
                T::Unit
            }
            ExprKind::Loop(body) => {
                self.loops += 1;
                let t = self.block(body)?;
                self.loops -= 1;
                self.unify(t, T::Unit)?;
                // A `loop` without `break` never ends; with one, it is `()`.
                if contains_break(body) { T::Unit } else { T::Never }
            }
            ExprKind::For { var, start, end, body, .. } => {
                let s = self.expr(start)?;
                let t = self.expr(end)?;
                let t = self.unify(s, t)?;
                if !self.is_int(t) {
                    return Err(fallback("a range of non-integers"));
                }
                self.scopes.push(HashMap::new());
                let id = self.bind(var, false, t);
                self.uses.insert(e as *const Expr, id);
                self.loops += 1;
                let b = self.block(body)?;
                self.loops -= 1;
                self.unify(b, T::Unit)?;
                self.scopes.pop();
                T::Unit
            }
            ExprKind::Block(b) => self.block(b)?,
            ExprKind::Break | ExprKind::Continue => {
                if self.loops == 0 {
                    return Err(fallback("`break` or `continue` outside a loop"));
                }
                T::Never
            }
            ExprKind::Return(v) => {
                let t = match v {
                    Some(v) => self.expr(v)?,
                    None => T::Unit,
                };
                self.unify(t, self.ret)?;
                T::Never
            }
            ExprKind::Print(p) => {
                for a in &p.args {
                    let t = self.expr(a)?;
                    if !matches!(self.find(t), T::Int(_) | T::Var(_) | T::Bool) {
                        return Err(fallback("printing this type"));
                    }
                }
                T::Unit
            }
        })
    }

    fn assignable(&mut self, name: &str, e: &Expr) -> Result<usize, Fallback> {
        let id = self.lookup(name).ok_or_else(|| fallback(format!("the name `{name}`")))?;
        if !self.bindings[id].mutable {
            return Err(fallback(format!("assigning to `{name}`, which is not `mut`")));
        }
        self.uses.insert(e as *const Expr, id);
        Ok(id)
    }

    fn binary(&mut self, op: BinOp, a: T, b: T) -> Result<T, Fallback> {
        use BinOp::*;
        Ok(match op {
            Add | Sub | Mul | Div | Rem => {
                let t = self.unify(a, b)?;
                if !self.is_int(t) && self.find(t) != T::Never {
                    return Err(fallback(format!("`{op:?}` on {t:?}")));
                }
                t
            }
            BitAnd | BitOr | BitXor => {
                let t = self.unify(a, b)?;
                if !self.is_int(t) && !matches!(self.find(t), T::Bool | T::Never) {
                    return Err(fallback(format!("`{op:?}` on {t:?}")));
                }
                t
            }
            // A shift's two sides have types of their own.
            Shl | Shr => {
                if !self.is_int(a) || !self.is_int(b) {
                    return Err(fallback("a shift of non-integers"));
                }
                a
            }
            Eq | Ne | Lt | Le | Gt | Ge => {
                let t = self.unify(a, b)?;
                if !self.is_int(t) && !matches!(self.find(t), T::Bool | T::Never) {
                    return Err(fallback(format!("comparing {t:?}")));
                }
                T::Bool
            }
            And | Or => {
                self.unify(a, T::Bool)?;
                self.unify(b, T::Bool)?;
                T::Bool
            }
        })
    }
}

fn ty_t(t: Ty) -> T {
    match t {
        Ty::Int(i) => T::Int(i),
        Ty::Bool => T::Bool,
        Ty::Unit => T::Unit,
        Ty::Never => T::Never,
    }
}

/// Whether `e`'s value is a literal, as rustc's expectations see through.
fn ends_in_literal(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Int(..) => true,
        ExprKind::Unary(_, a) => ends_in_literal(a),
        ExprKind::Block(b) => b.tail.as_deref().is_some_and(ends_in_literal),
        ExprKind::If(_, t, els) => {
            t.tail.as_deref().is_some_and(ends_in_literal) || els.as_deref().is_some_and(ends_in_literal)
        }
        _ => false,
    }
}

/// Whether a `break` in `b` leaves this loop (not one nested in it).
fn contains_break(b: &Block) -> bool {
    fn expr(e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Break => true,
            ExprKind::While(..) | ExprKind::Loop(..) | ExprKind::For { .. } => false,
            ExprKind::If(c, t, els) => expr(c) || block(t) || els.as_deref().is_some_and(expr),
            ExprKind::Block(b) => block(b),
            ExprKind::Unary(_, a) | ExprKind::BlackBox(a) | ExprKind::Cast(a, _) => expr(a),
            ExprKind::Binary(_, a, b) => expr(a) || expr(b),
            ExprKind::Assign(_, a) | ExprKind::CompoundAssign(_, _, a) => expr(a),
            ExprKind::Return(Some(a)) => expr(a),
            ExprKind::Call(_, args) => args.iter().any(expr),
            ExprKind::Print(p) => p.args.iter().any(expr),
            _ => false,
        }
    }
    fn block(b: &Block) -> bool {
        b.stmts.iter().any(|s| match s {
            Stmt::Let { init, .. } => expr(init),
            Stmt::Expr { expr: e, .. } => expr(e),
        }) || b.tail.as_deref().is_some_and(expr)
    }
    block(b)
}
