//! Athrad's IR: a small instruction set we control.
//!
//! A [`Module`] holds functions and data defined here, and symbols imported
//! from elsewhere (`std`'s rlibs, the C library). A function is a list of
//! basic blocks in SSA form. Blocks take parameters instead of phi nodes, the
//! shape Cranelift uses, so lowering is a direct walk.
//!
//! The IR is for 64-bit little-endian hosts, which all three shores are: a
//! pointer is an [`Type::I64`].
//!
//! Integer arithmetic wraps. Whatever produces this IR must insert its own
//! overflow checks ([`Op::Overflow`] gives the flag) or reject the program;
//! the IR never guesses what `rustc` would have done.

use std::fmt;

mod verify;

pub use verify::{VerifyError, verify};

/// An integer type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    I8,
    I16,
    I32,
    I64,
    I128,
}

/// The type of a pointer.
pub const PTR: Type = Type::I64;

impl Type {
    pub fn bits(self) -> u32 {
        match self {
            Type::I8 => 8,
            Type::I16 => 16,
            Type::I32 => 32,
            Type::I64 => 64,
            Type::I128 => 128,
        }
    }

    pub fn bytes(self) -> u32 {
        self.bits() / 8
    }

    /// The integer type of exactly `bytes` bytes.
    pub fn of_bytes(bytes: u64) -> Option<Type> {
        Some(match bytes {
            1 => Type::I8,
            2 => Type::I16,
            4 => Type::I32,
            8 => Type::I64,
            16 => Type::I128,
            _ => return None,
        })
    }
}

/// How the calling convention widens a narrow integer argument or return.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Ext {
    #[default]
    None,
    Zero,
    Sign,
}

/// One parameter or return value at the machine level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Param {
    pub ty: Type,
    pub ext: Ext,
    /// A float in the calling convention: passed in a floating-point
    /// register. Inside a function it is the integer of its bits, as every
    /// float in Athrad IR is.
    pub float: bool,
    /// The pointer to memory the callee writes its return value into. The
    /// calling convention may give it a register of its own (x8 on aarch64).
    pub sret: bool,
}

impl Param {
    pub fn new(ty: Type) -> Self {
        Param { ty, ext: Ext::None, float: false, sret: false }
    }
}

/// Parameter and return values of a function, in the host C calling
/// convention, which is also how `rustc` lays out the Rust ABI after its own
/// argument lowering.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Signature {
    pub params: Vec<Param>,
    pub returns: Vec<Param>,
}

impl Signature {
    pub fn new(params: impl IntoIterator<Item = Type>, returns: impl IntoIterator<Item = Type>) -> Self {
        Signature {
            params: params.into_iter().map(Param::new).collect(),
            returns: returns.into_iter().map(Param::new).collect(),
        }
    }

    pub fn param_types(&self) -> Vec<Type> {
        self.params.iter().map(|p| p.ty).collect()
    }

    pub fn return_types(&self) -> Vec<Type> {
        self.returns.iter().map(|p| p.ty).collect()
    }
}

/// Who can see a defined symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Linkage {
    /// Visible only inside the object file.
    Local,
    /// Visible to the linker, but not exported from the executable.
    Hidden,
    /// Visible to the linker, as `main` must be.
    Export,
}

/// A symbol defined outside this module.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Import {
    pub name: String,
    pub kind: ImportKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ImportKind {
    Func(Signature),
    Data,
}

/// Bytes in the executable, with the addresses of other symbols written into
/// them at link time.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Data {
    pub name: String,
    pub linkage: Linkage,
    pub writable: bool,
    pub align: u64,
    pub bytes: Vec<u8>,
    pub relocs: Vec<Reloc>,
}

/// At `offset`, the 8-byte address of `symbol` plus `addend`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Reloc {
    pub offset: u32,
    pub symbol: String,
    pub addend: i64,
}

/// An SSA value, local to one function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Value(pub u32);

/// A basic block, local to one function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

/// A piece of the function's stack frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Slot(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SlotData {
    pub size: u32,
    pub align: u32,
}

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
    And,
    Or,
    Xor,
    /// Shifts take the amount modulo the bit width.
    Shl,
    UShr,
    SShr,
    /// Rotates take the amount modulo the bit width.
    Rotl,
    Rotr,
}

/// Bit operations on one integer, giving the same type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnOp {
    /// Number of one bits.
    Popcnt,
    /// Leading zero bits; the bit width for zero.
    Clz,
    /// Trailing zero bits; the bit width for zero.
    Ctz,
    /// Reverse the bytes.
    Bswap,
    /// Reverse the bits.
    Bitrev,
}

/// An IEEE float format. Float values in the IR are integers of their bits
/// (`F32` in an `I32`, `F64` in an `I64`); float operations say how to read
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FloatTy {
    F32,
    F64,
}

impl FloatTy {
    /// The integer type that holds its bits.
    pub fn bits_type(self) -> Type {
        match self {
            FloatTy::F32 => Type::I32,
            FloatTy::F64 => Type::I64,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FBinOp {
    Add,
    Sub,
    Mul,
    Div,
    /// IEEE `minimum`/`maximum`: NaN if either is NaN.
    Min,
    Max,
    /// The magnitude of the first, the sign of the second.
    Copysign,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FUnOp {
    Neg,
    Abs,
    Sqrt,
    Floor,
    Ceil,
    Trunc,
    /// Round to nearest, ties to even.
    Nearest,
}

/// Float comparisons. `Ne` and `Uno` are true when either side is NaN; the
/// others are false then.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FCmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Uno,
}

/// Arithmetic that also reports whether the exact result fit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OverflowOp {
    Add,
    Sub,
    Mul,
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
    Const(Type, i128),
    Binary(BinOp, Value, Value),
    Unary(UnOp, Value),
    FBinary(FBinOp, FloatTy, Value, Value),
    FUnary(FUnOp, FloatTy, Value),
    /// An `I8` that is 0 or 1.
    FCmp(FCmp, FloatTy, Value, Value),
    /// Float to integer as Rust's `as` does it: rounding toward zero,
    /// saturating, NaN to 0.
    FToInt {
        from: FloatTy,
        to: Type,
        signed: bool,
        v: Value,
    },
    /// Integer to float, rounding to nearest.
    IntToF {
        to: FloatTy,
        signed: bool,
        v: Value,
    },
    /// Widen or narrow a float.
    FConvert {
        from: FloatTy,
        to: FloatTy,
        v: Value,
    },
    /// The wrapped result and an `I8` that is 1 if it overflowed, treating
    /// the operands as signed or unsigned.
    Overflow(OverflowOp, bool, Value, Value),
    /// Compare two values of the same type. The result is an `I8` that is 0 or 1.
    Cmp(Cmp, Value, Value),
    /// `a` if `cond` is not zero, else `b`.
    Select(Value, Value, Value),
    /// Zero-extend to a wider type.
    ZExt(Type, Value),
    /// Sign-extend to a wider type.
    SExt(Type, Value),
    /// Keep the low bits, to a narrower type.
    Trunc(Type, Value),
    /// The address of a stack slot.
    SlotAddr(Slot),
    /// The address of a function or data symbol.
    SymbolAddr(String),
    /// Load a value of this type from `addr + offset`. Need not be aligned.
    Load(Type, Value, i32),
    /// Store the second value at `addr + offset`. Need not be aligned.
    Store(Value, i32, Value),
    /// Call a function of this module or an import, by name.
    Call(String, Vec<Value>),
    /// Call the function at an address.
    CallIndirect(Signature, Value, Vec<Value>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Inst {
    /// The values this instruction defines: one for most, none for stores,
    /// as many as the callee returns for calls, two for [`Op::Overflow`].
    pub results: Vec<Value>,
    pub op: Op,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub block: BlockId,
    pub args: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Terminator {
    Return(Vec<Value>),
    Jump(Target),
    /// Go to `then` if `cond` is not zero, else to `otherwise`.
    Branch {
        cond: Value,
        then: Target,
        otherwise: Target,
    },
    /// Code that must never run, such as after a call that does not return.
    Trap,
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
    pub slots: Vec<SlotData>,
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
    pub imports: Vec<Import>,
    pub functions: Vec<Function>,
    pub data: Vec<Data>,
}

/// What a name in a module refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Symbol<'a> {
    Func(&'a Signature),
    Data,
}

impl Module {
    pub fn new() -> Self {
        Self::default()
    }

    /// Import a function. Importing the same one twice is a no-op.
    pub fn import_func(&mut self, name: &str, sig: Signature) {
        if !self.imports.iter().any(|e| e.name == name) {
            self.imports.push(Import { name: name.to_owned(), kind: ImportKind::Func(sig) });
        }
    }

    /// Import a data symbol. Importing the same one twice is a no-op.
    pub fn import_data(&mut self, name: &str) {
        if !self.imports.iter().any(|e| e.name == name) {
            self.imports.push(Import { name: name.to_owned(), kind: ImportKind::Data });
        }
    }

    pub fn push(&mut self, function: Function) {
        self.functions.push(function);
    }

    pub fn push_data(&mut self, data: Data) {
        self.data.push(data);
    }

    pub fn function(&self, name: &str) -> Option<&Function> {
        self.functions.iter().find(|f| f.name == name)
    }

    /// What `name` is, if this module defines or imports it.
    pub fn symbol(&self, name: &str) -> Option<Symbol<'_>> {
        if let Some(f) = self.function(name) {
            return Some(Symbol::Func(&f.sig));
        }
        if self.data.iter().any(|d| d.name == name) {
            return Some(Symbol::Data);
        }
        self.imports.iter().find(|i| i.name == name).map(|i| match &i.kind {
            ImportKind::Func(sig) => Symbol::Func(sig),
            ImportKind::Data => Symbol::Data,
        })
    }

    /// The signature of a defined or imported function.
    pub fn signature(&self, name: &str) -> Option<&Signature> {
        match self.symbol(name)? {
            Symbol::Func(sig) => Some(sig),
            Symbol::Data => None,
        }
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
        let mut func = Function {
            name: name.to_owned(),
            linkage,
            sig: sig.clone(),
            values: Vec::new(),
            slots: Vec::new(),
            blocks: Vec::new(),
        };
        func.blocks.push(Block { params: Vec::new(), insts: Vec::new(), term: None });
        for p in &sig.params {
            let v = new_value(&mut func, p.ty);
            func.blocks[0].params.push(v);
        }
        FunctionBuilder { func, current: BlockId(0) }
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

    pub fn current(&self) -> BlockId {
        self.current
    }

    /// Whether the current block has its terminator already.
    pub fn is_terminated(&self) -> bool {
        self.func.blocks[self.current.0 as usize].term.is_some()
    }

    pub fn slot(&mut self, size: u32, align: u32) -> Slot {
        let s = Slot(self.func.slots.len() as u32);
        self.func.slots.push(SlotData { size, align });
        s
    }

    pub fn value_type(&self, v: Value) -> Type {
        self.func.values[v.0 as usize]
    }

    pub fn iconst(&mut self, ty: Type, n: i128) -> Value {
        self.def(ty, Op::Const(ty, n))
    }

    pub fn binary(&mut self, op: BinOp, a: Value, b: Value) -> Value {
        let ty = self.value_type(a);
        self.def(ty, Op::Binary(op, a, b))
    }

    pub fn unary(&mut self, op: UnOp, a: Value) -> Value {
        let ty = self.value_type(a);
        self.def(ty, Op::Unary(op, a))
    }

    pub fn fbinary(&mut self, op: FBinOp, ft: FloatTy, a: Value, b: Value) -> Value {
        self.def(ft.bits_type(), Op::FBinary(op, ft, a, b))
    }

    pub fn funary(&mut self, op: FUnOp, ft: FloatTy, a: Value) -> Value {
        self.def(ft.bits_type(), Op::FUnary(op, ft, a))
    }

    pub fn fcmp(&mut self, cmp: FCmp, ft: FloatTy, a: Value, b: Value) -> Value {
        self.def(Type::I8, Op::FCmp(cmp, ft, a, b))
    }

    pub fn ftoint(&mut self, from: FloatTy, to: Type, signed: bool, v: Value) -> Value {
        self.def(to, Op::FToInt { from, to, signed, v })
    }

    pub fn inttof(&mut self, to: FloatTy, signed: bool, v: Value) -> Value {
        self.def(to.bits_type(), Op::IntToF { to, signed, v })
    }

    pub fn fconvert(&mut self, from: FloatTy, to: FloatTy, v: Value) -> Value {
        self.def(to.bits_type(), Op::FConvert { from, to, v })
    }

    pub fn add(&mut self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Add, a, b)
    }

    pub fn sub(&mut self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Sub, a, b)
    }

    /// The wrapped result, and an `I8` overflow flag.
    pub fn overflow(&mut self, op: OverflowOp, signed: bool, a: Value, b: Value) -> (Value, Value) {
        let ty = self.value_type(a);
        let r = new_value(&mut self.func, ty);
        let flag = new_value(&mut self.func, Type::I8);
        self.push(Inst { results: vec![r, flag], op: Op::Overflow(op, signed, a, b) });
        (r, flag)
    }

    pub fn cmp(&mut self, cmp: Cmp, a: Value, b: Value) -> Value {
        self.def(Type::I8, Op::Cmp(cmp, a, b))
    }

    pub fn select(&mut self, cond: Value, a: Value, b: Value) -> Value {
        let ty = self.value_type(a);
        self.def(ty, Op::Select(cond, a, b))
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

    /// Convert `v` to `ty`: truncate, extend (signed or not), or nothing.
    pub fn resize(&mut self, ty: Type, v: Value, signed: bool) -> Value {
        let from = self.value_type(v);
        match from.bits().cmp(&ty.bits()) {
            std::cmp::Ordering::Equal => v,
            std::cmp::Ordering::Greater => self.trunc(ty, v),
            std::cmp::Ordering::Less if signed => self.sext(ty, v),
            std::cmp::Ordering::Less => self.zext(ty, v),
        }
    }

    pub fn slot_addr(&mut self, slot: Slot) -> Value {
        self.def(PTR, Op::SlotAddr(slot))
    }

    pub fn symbol_addr(&mut self, name: &str) -> Value {
        self.def(PTR, Op::SymbolAddr(name.to_owned()))
    }

    pub fn load(&mut self, ty: Type, addr: Value, offset: i32) -> Value {
        self.def(ty, Op::Load(ty, addr, offset))
    }

    pub fn store(&mut self, addr: Value, offset: i32, v: Value) {
        self.push(Inst { results: Vec::new(), op: Op::Store(addr, offset, v) });
    }

    /// `addr + offset`, or `addr` itself for a zero offset.
    pub fn offset(&mut self, addr: Value, offset: i64) -> Value {
        if offset == 0 {
            return addr;
        }
        let o = self.iconst(PTR, offset.into());
        self.add(addr, o)
    }

    /// Call a function by name. `returns` must match the callee's return
    /// types; [`verify`] checks it against the module.
    pub fn call(&mut self, callee: &str, args: &[Value], returns: &[Type]) -> Vec<Value> {
        let results: Vec<Value> = returns.iter().map(|&t| new_value(&mut self.func, t)).collect();
        self.push(Inst { results: results.clone(), op: Op::Call(callee.to_owned(), args.to_vec()) });
        results
    }

    pub fn call_indirect(&mut self, sig: Signature, callee: Value, args: &[Value]) -> Vec<Value> {
        let results: Vec<Value> = sig.returns.iter().map(|p| new_value(&mut self.func, p.ty)).collect();
        self.push(Inst { results: results.clone(), op: Op::CallIndirect(sig, callee, args.to_vec()) });
        results
    }

    pub fn ret(&mut self, values: &[Value]) {
        self.terminate(Terminator::Return(values.to_vec()));
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

    pub fn trap(&mut self) {
        self.terminate(Terminator::Trap);
    }

    pub fn finish(self) -> Function {
        self.func
    }

    fn def(&mut self, ty: Type, op: Op) -> Value {
        let v = new_value(&mut self.func, ty);
        self.push(Inst { results: vec![v], op });
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

impl fmt::Display for Param {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.ty)?;
        match self.ext {
            Ext::None => {}
            Ext::Zero => f.write_str(" zext")?,
            Ext::Sign => f.write_str(" sext")?,
        }
        if self.sret {
            f.write_str(" sret")?;
        }
        Ok(())
    }
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({})", list(&self.params))?;
        if !self.returns.is_empty() {
            write!(f, " -> {}", list(&self.returns))?;
        }
        Ok(())
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

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "slot{}", self.0)
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

fn linkage_prefix(l: Linkage) -> &'static str {
    match l {
        Linkage::Local => "",
        Linkage::Hidden => "hidden ",
        Linkage::Export => "export ",
    }
}

impl fmt::Display for Module {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for i in &self.imports {
            match &i.kind {
                ImportKind::Func(sig) => writeln!(f, "import fn {}{sig}", i.name)?,
                ImportKind::Data => writeln!(f, "import data {}", i.name)?,
            }
        }
        for d in &self.data {
            let rw = if d.writable { "mut " } else { "" };
            write!(f, "{}data {rw}{} align {} = {:02x?}", linkage_prefix(d.linkage), d.name, d.align, d.bytes)?;
            for r in &d.relocs {
                write!(f, " [{}: {}+{}]", r.offset, r.symbol, r.addend)?;
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
        writeln!(f, "{}fn {}{} {{", linkage_prefix(self.linkage), self.name, self.sig)?;
        for (i, s) in self.slots.iter().enumerate() {
            writeln!(f, "    {} = {} bytes align {}", Slot(i as u32), s.size, s.align)?;
        }
        for (i, block) in self.blocks.iter().enumerate() {
            let params: Vec<String> =
                block.params.iter().map(|&p| format!("{p}: {}", self.values[p.0 as usize])).collect();
            writeln!(f, "{}({}):", BlockId(i as u32), params.join(", "))?;
            for inst in &block.insts {
                write!(f, "    ")?;
                if !inst.results.is_empty() {
                    write!(f, "{} = ", list(&inst.results))?;
                }
                let lower = |x: &dyn fmt::Debug| format!("{x:?}").to_lowercase();
                match &inst.op {
                    Op::Const(ty, n) => writeln!(f, "const.{ty} {n}")?,
                    Op::Binary(op, a, b) => writeln!(f, "{} {a}, {b}", lower(op))?,
                    Op::Unary(op, a) => writeln!(f, "{} {a}", lower(op))?,
                    Op::FBinary(op, ft, a, b) => writeln!(f, "f{}.{} {a}, {b}", lower(op), lower(ft))?,
                    Op::FUnary(op, ft, a) => writeln!(f, "f{}.{} {a}", lower(op), lower(ft))?,
                    Op::FCmp(c, ft, a, b) => writeln!(f, "fcmp.{}.{} {a}, {b}", lower(c), lower(ft))?,
                    Op::FToInt { from, to, signed, v } => {
                        writeln!(f, "{}.{}.{to} {v}", if *signed { "ftosi" } else { "ftoui" }, lower(from))?
                    }
                    Op::IntToF { to, signed, v } => {
                        writeln!(f, "{}.{} {v}", if *signed { "sitof" } else { "uitof" }, lower(to))?
                    }
                    Op::FConvert { from, to, v } => writeln!(f, "fconvert.{}.{} {v}", lower(from), lower(to))?,
                    Op::Overflow(op, signed, a, b) => {
                        writeln!(f, "{}{}_overflow {a}, {b}", if *signed { "s" } else { "u" }, lower(op))?
                    }
                    Op::Cmp(c, a, b) => writeln!(f, "cmp.{} {a}, {b}", lower(c))?,
                    Op::Select(c, a, b) => writeln!(f, "select {c}, {a}, {b}")?,
                    Op::ZExt(ty, v) => writeln!(f, "zext.{ty} {v}")?,
                    Op::SExt(ty, v) => writeln!(f, "sext.{ty} {v}")?,
                    Op::Trunc(ty, v) => writeln!(f, "trunc.{ty} {v}")?,
                    Op::SlotAddr(s) => writeln!(f, "slot_addr {s}")?,
                    Op::SymbolAddr(name) => writeln!(f, "symbol_addr {name}")?,
                    Op::Load(ty, a, o) => writeln!(f, "load.{ty} {a}+{o}")?,
                    Op::Store(a, o, v) => writeln!(f, "store {a}+{o}, {v}")?,
                    Op::Call(name, args) => writeln!(f, "call {name}({})", list(args))?,
                    Op::CallIndirect(sig, callee, args) => writeln!(f, "call_indirect {sig} {callee}({})", list(args))?,
                }
            }
            match &block.term {
                None => writeln!(f, "    <unterminated>")?,
                Some(Terminator::Return(v)) => writeln!(f, "    return {}", list(v))?,
                Some(Terminator::Jump(t)) => writeln!(f, "    jump {t}")?,
                Some(Terminator::Branch { cond, then, otherwise }) => {
                    writeln!(f, "    branch {cond}, {then}, {otherwise}")?
                }
                Some(Terminator::Trap) => writeln!(f, "    trap")?,
            }
        }
        writeln!(f, "}}")
    }
}
