//! Athrad's IR: a tiny instruction set we control.
//!
//! A [`Module`] holds functions defined here and functions imported from the
//! host C library. A function is a list of basic blocks in SSA form. Blocks
//! take parameters instead of phi nodes, the same shape Cranelift uses, so
//! lowering is a direct walk.
//!
//! Integer arithmetic wraps. Whatever produces this IR (today a hand-written
//! builder, later MIR) must insert its own overflow checks or reject the
//! program; the IR never guesses what `rustc` would have done.

use std::fmt;

mod verify;

pub use verify::{VerifyError, verify};

/// An integer type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    I8,
    I32,
    I64,
}

impl Type {
    pub fn bits(self) -> u32 {
        match self {
            Type::I8 => 8,
            Type::I32 => 32,
            Type::I64 => 64,
        }
    }
}

/// Parameter and return types of a function.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Signature {
    pub params: Vec<Type>,
    pub ret: Option<Type>,
}

impl Signature {
    pub fn new(params: impl Into<Vec<Type>>, ret: Option<Type>) -> Self {
        Signature { params: params.into(), ret }
    }
}

/// Who can see a defined function's symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Linkage {
    /// Visible only inside the object file.
    Local,
    /// Visible to the linker, as `main` must be.
    Export,
}

/// A function provided by the host C library, such as `putchar`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Extern {
    pub name: String,
    pub sig: Signature,
}

/// An SSA value, local to one function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Value(pub u32);

/// A basic block, local to one function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    /// Unsigned division. Division by zero traps.
    UDiv,
    /// Unsigned remainder. Division by zero traps.
    URem,
    /// Signed division. Division by zero, or `MIN / -1`, traps.
    SDiv,
    /// Signed remainder. Division by zero traps.
    SRem,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cmp {
    Eq,
    Ne,
    SLt,
    SLe,
    SGt,
    SGe,
    ULt,
    ULe,
    UGt,
    UGe,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    /// An integer constant. The bits are truncated to the type.
    Const(Type, i64),
    Binary(BinOp, Value, Value),
    /// Compare two values of the same type. The result is an `I8` that is 0 or 1.
    Cmp(Cmp, Value, Value),
    /// Zero-extend to a wider type.
    ZExt(Type, Value),
    /// Sign-extend to a wider type.
    SExt(Type, Value),
    /// Keep the low bits, to a narrower type.
    Trunc(Type, Value),
    /// Call a function of this module or an extern, by name.
    Call(String, Vec<Value>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Inst {
    /// The value this instruction defines. `None` only for calls to
    /// functions that return nothing.
    pub result: Option<Value>,
    pub op: Op,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub block: BlockId,
    pub args: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Terminator {
    Return(Option<Value>),
    Jump(Target),
    /// Go to `then` if `cond` is not zero, else to `otherwise`.
    Branch {
        cond: Value,
        then: Target,
        otherwise: Target,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Block {
    pub params: Vec<Value>,
    pub insts: Vec<Inst>,
    /// `None` only while the block is being built.
    pub term: Option<Terminator>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Function {
    pub name: String,
    pub linkage: Linkage,
    pub sig: Signature,
    /// The type of each value, indexed by [`Value`].
    pub values: Vec<Type>,
    /// The first block is the entry. Its parameters are the function's.
    pub blocks: Vec<Block>,
}

impl Function {
    pub fn value_type(&self, v: Value) -> Option<Type> {
        self.values.get(v.0 as usize).copied()
    }

    pub fn block(&self, b: BlockId) -> Option<&Block> {
        self.blocks.get(b.0 as usize)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Module {
    pub externs: Vec<Extern>,
    pub functions: Vec<Function>,
}

impl Module {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a host C function. Declaring the same one twice is a no-op.
    pub fn declare_extern(&mut self, name: &str, sig: Signature) {
        if !self.externs.iter().any(|e| e.name == name) {
            self.externs.push(Extern { name: name.to_owned(), sig });
        }
    }

    pub fn push(&mut self, function: Function) {
        self.functions.push(function);
    }

    pub fn function(&self, name: &str) -> Option<&Function> {
        self.functions.iter().find(|f| f.name == name)
    }

    /// The signature of a defined function or an extern.
    pub fn signature(&self, name: &str) -> Option<&Signature> {
        self.function(name).map(|f| &f.sig).or_else(|| self.externs.iter().find(|e| e.name == name).map(|e| &e.sig))
    }
}

/// Builds one [`Function`]. The entry block exists from the start and holds
/// the function's parameters.
pub struct FunctionBuilder {
    func: Function,
    current: BlockId,
}

impl FunctionBuilder {
    pub fn new(name: &str, linkage: Linkage, sig: Signature) -> Self {
        let mut func =
            Function { name: name.to_owned(), linkage, sig: sig.clone(), values: Vec::new(), blocks: Vec::new() };
        let entry = BlockId(0);
        func.blocks.push(Block { params: Vec::new(), insts: Vec::new(), term: None });
        for ty in sig.params {
            let v = new_value(&mut func, ty);
            func.blocks[0].params.push(v);
        }
        FunctionBuilder { func, current: entry }
    }

    /// The function's parameters, which are the entry block's.
    pub fn params(&self) -> Vec<Value> {
        self.func.blocks[0].params.clone()
    }

    /// Make a new empty block with parameters of these types.
    pub fn block(&mut self, params: &[Type]) -> (BlockId, Vec<Value>) {
        let id = BlockId(self.func.blocks.len() as u32);
        let params: Vec<Value> = params.iter().map(|&ty| new_value(&mut self.func, ty)).collect();
        self.func.blocks.push(Block { params: params.clone(), insts: Vec::new(), term: None });
        (id, params)
    }

    /// Append instructions to this block from now on.
    pub fn switch_to(&mut self, block: BlockId) {
        self.current = block;
    }

    pub fn iconst(&mut self, ty: Type, n: i64) -> Value {
        self.def(ty, Op::Const(ty, n))
    }

    pub fn binary(&mut self, op: BinOp, a: Value, b: Value) -> Value {
        let ty = self.ty(a);
        self.def(ty, Op::Binary(op, a, b))
    }

    pub fn add(&mut self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Add, a, b)
    }

    pub fn sub(&mut self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Sub, a, b)
    }

    pub fn cmp(&mut self, cmp: Cmp, a: Value, b: Value) -> Value {
        self.def(Type::I8, Op::Cmp(cmp, a, b))
    }

    pub fn zext(&mut self, ty: Type, v: Value) -> Value {
        self.def(ty, Op::ZExt(ty, v))
    }

    pub fn sext(&mut self, ty: Type, v: Value) -> Value {
        self.def(ty, Op::SExt(ty, v))
    }

    pub fn trunc(&mut self, ty: Type, v: Value) -> Value {
        self.def(ty, Op::Trunc(ty, v))
    }

    /// Call a function by name. `ret` must match the callee's return type;
    /// [`verify`] checks it against the module.
    pub fn call(&mut self, callee: &str, args: &[Value], ret: Option<Type>) -> Option<Value> {
        let op = Op::Call(callee.to_owned(), args.to_vec());
        match ret {
            Some(ty) => Some(self.def(ty, op)),
            None => {
                self.push(Inst { result: None, op });
                None
            }
        }
    }

    pub fn ret(&mut self, v: Option<Value>) {
        self.terminate(Terminator::Return(v));
    }

    pub fn jump(&mut self, block: BlockId, args: &[Value]) {
        self.terminate(Terminator::Jump(Target { block, args: args.to_vec() }));
    }

    pub fn branch(&mut self, cond: Value, then: (BlockId, &[Value]), otherwise: (BlockId, &[Value])) {
        self.terminate(Terminator::Branch {
            cond,
            then: Target { block: then.0, args: then.1.to_vec() },
            otherwise: Target { block: otherwise.0, args: otherwise.1.to_vec() },
        });
    }

    pub fn finish(self) -> Function {
        self.func
    }

    fn ty(&self, v: Value) -> Type {
        self.func.values[v.0 as usize]
    }

    fn def(&mut self, ty: Type, op: Op) -> Value {
        let v = new_value(&mut self.func, ty);
        self.push(Inst { result: Some(v), op });
        v
    }

    fn push(&mut self, inst: Inst) {
        let block = &mut self.func.blocks[self.current.0 as usize];
        assert!(block.term.is_none(), "block{} is already terminated", self.current.0);
        block.insts.push(inst);
    }

    fn terminate(&mut self, term: Terminator) {
        let block = &mut self.func.blocks[self.current.0 as usize];
        assert!(block.term.is_none(), "block{} is already terminated", self.current.0);
        block.term = Some(term);
    }
}

fn new_value(func: &mut Function, ty: Type) -> Value {
    let v = Value(func.values.len() as u32);
    func.values.push(ty);
    v
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "i{}", self.bits())
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

impl fmt::Display for BlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "block{}", self.0)
    }
}

fn list<T: fmt::Display>(items: &[T]) -> String {
    items.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.block, list(&self.args))
    }
}

impl fmt::Display for Module {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for e in &self.externs {
            write!(f, "extern fn {}({})", e.name, list(&e.sig.params))?;
            if let Some(ret) = e.sig.ret {
                write!(f, " -> {ret}")?;
            }
            writeln!(f)?;
        }
        for func in &self.functions {
            writeln!(f)?;
            write!(f, "{func}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let export = if self.linkage == Linkage::Export { "export " } else { "" };
        write!(f, "{export}fn {}({})", self.name, list(&self.sig.params))?;
        if let Some(ret) = self.sig.ret {
            write!(f, " -> {ret}")?;
        }
        writeln!(f, " {{")?;
        for (i, block) in self.blocks.iter().enumerate() {
            let params: Vec<String> =
                block.params.iter().map(|&p| format!("{p}: {}", self.values[p.0 as usize])).collect();
            writeln!(f, "{}({}):", BlockId(i as u32), params.join(", "))?;
            for inst in &block.insts {
                write!(f, "    ")?;
                if let Some(r) = inst.result {
                    write!(f, "{r} = ")?;
                }
                match &inst.op {
                    Op::Const(ty, n) => writeln!(f, "const.{ty} {n}")?,
                    Op::Binary(op, a, b) => writeln!(f, "{} {a}, {b}", format!("{op:?}").to_lowercase())?,
                    Op::Cmp(c, a, b) => writeln!(f, "cmp.{} {a}, {b}", format!("{c:?}").to_lowercase())?,
                    Op::ZExt(ty, v) => writeln!(f, "zext.{ty} {v}")?,
                    Op::SExt(ty, v) => writeln!(f, "sext.{ty} {v}")?,
                    Op::Trunc(ty, v) => writeln!(f, "trunc.{ty} {v}")?,
                    Op::Call(name, args) => writeln!(f, "call {name}({})", list(args))?,
                }
            }
            match &block.term {
                None => writeln!(f, "    <unterminated>")?,
                Some(Terminator::Return(None)) => writeln!(f, "    return")?,
                Some(Terminator::Return(Some(v))) => writeln!(f, "    return {v}")?,
                Some(Terminator::Jump(t)) => writeln!(f, "    jump {t}")?,
                Some(Terminator::Branch { cond, then, otherwise }) => {
                    writeln!(f, "    branch {cond}, {then}, {otherwise}")?
                }
            }
        }
        writeln!(f, "}}")
    }
}
