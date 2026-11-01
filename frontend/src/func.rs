//! One monomorphized function's MIR, lowered to IR.
//!
//! Every local with a size lives in a stack slot. Operands are read out of
//! memory as scalars, scalar pairs, or (for anything bigger) an address, the
//! same three shapes `rustc`'s layouts and calling conventions use. Nothing is
//! optimized: this is the dev loop.

use rustc_abi::{BackendRepr, FIRST_VARIANT, FieldIdx, Size, TagEncoding, VariantIdx, Variants};
use rustc_hir::LangItem;
use rustc_index::IndexVec;
use rustc_middle::mir::interpret::Scalar as IScalar;
use rustc_middle::mir::{
    self, AggregateKind, AssertKind, BasicBlock, BinOp, Body, CastKind, ConstValue, Local, NonDivergingIntrinsic,
    Operand, Place, ProjectionElem, Rvalue, SourceInfo, StatementKind, TerminatorKind, UnOp,
};
use rustc_middle::ty::adjustment::PointerCoercion;
use rustc_middle::ty::layout::{FnAbiOf, LayoutOf, TyAndLayout};
use rustc_middle::ty::{self, Instance, InstanceKind, ShimKind, Ty, TyCtxt, TypeFoldable};
use rustc_span::Span;
use rustc_target::callconv::{FnAbi, PassMode};

use ir::{BinOp as IBin, BlockId, Cmp, OverflowOp, PTR, Type, Value};

use crate::consts::{Addr, ModCx, off};
use crate::cx::{Cx, cast_pieces, is_signed, pair_b_offset, scalar_type, signature};
use crate::{Result, unsupported};

/// A place in memory, with the metadata of an unsized one.
#[derive(Clone, Copy)]
struct PlaceRef<'tcx> {
    addr: Value,
    meta: Option<Value>,
    layout: TyAndLayout<'tcx>,
}

#[derive(Clone, Copy)]
enum V {
    Zst,
    Scalar(Value),
    Pair(Value, Value),
    /// In memory at this address.
    Ref(Value),
}

#[derive(Clone, Copy)]
struct Val<'tcx> {
    v: V,
    layout: TyAndLayout<'tcx>,
}

enum Callee {
    Direct(String),
    Indirect(Value),
    /// Through the vtable of the first argument, at this slot.
    Virtual(usize),
}

pub(crate) fn lower<'tcx>(m: &mut ModCx<'tcx>, instance: Instance<'tcx>, linkage: ir::Linkage) -> Result<()> {
    let tcx = m.tcx;
    let cx = m.cx();
    let mir = tcx.instance_mir(instance.def);
    let abi = cx.fn_abi_of_instance(instance, ty::List::empty());
    if abi.conv == rustc_abi::CanonAbi::RustCold {
        return Err(unsupported("defining `extern \"rust-cold\"` functions"));
    }
    let sig = signature(cx, abi)?;
    let (name, _) = m.func_symbol(instance)?;
    let b = ir::FunctionBuilder::new(&name, linkage, sig);
    let mut f = FnCx {
        m,
        tcx,
        cx,
        instance,
        mir,
        abi,
        b,
        blocks: IndexVec::new(),
        locals: IndexVec::new(),
        caller_location: None,
    };
    f.lower_body()?;
    let func = f.b.finish();
    m.module.push(func);
    Ok(())
}

struct FnCx<'a, 'tcx> {
    m: &'a mut ModCx<'tcx>,
    tcx: TyCtxt<'tcx>,
    cx: Cx<'tcx>,
    instance: Instance<'tcx>,
    mir: &'tcx Body<'tcx>,
    abi: &'tcx FnAbi<'tcx, Ty<'tcx>>,
    b: ir::FunctionBuilder,
    /// The IR block for each MIR block; `None` for cleanup blocks, which
    /// only unwinding reaches.
    blocks: IndexVec<BasicBlock, Option<BlockId>>,
    locals: IndexVec<Local, PlaceRef<'tcx>>,
    caller_location: Option<Value>,
}

impl<'a, 'tcx> FnCx<'a, 'tcx> {
    fn monomorphize<T: TypeFoldable<TyCtxt<'tcx>>>(&self, v: T) -> T {
        self.instance.instantiate_mir_and_normalize_erasing_regions(
            self.tcx,
            ty::TypingEnv::fully_monomorphized(),
            ty::EarlyBinder::bind(self.tcx, v),
        )
    }

    fn layout(&self, ty: Ty<'tcx>) -> TyAndLayout<'tcx> {
        self.cx.layout_of(ty)
    }

    fn span_str(&self, span: Span) -> String {
        self.tcx.sess.source_map().span_to_diagnostic_string(span)
    }

    fn lower_body(&mut self) -> Result<()> {
        let mir = self.mir;
        for (bb, data) in mir.basic_blocks.iter_enumerated() {
            if data.is_cleanup {
                self.check_cleanup(bb)?;
                self.blocks.push(None);
            } else {
                let (id, _) = self.b.block(&[]);
                self.blocks.push(Some(id));
            }
        }
        self.prologue().map_err(|e| e.context(format!("{}: in `{}`", self.span_str(mir.span), self.instance)))?;
        for (bb, data) in mir.basic_blocks.iter_enumerated() {
            let Some(id) = self.blocks[bb] else { continue };
            self.b.switch_to(id);
            for stmt in &data.statements {
                self.statement(stmt).map_err(|e| self.at(e, stmt.source_info.span))?;
            }
            let term = data.terminator();
            self.terminator(term).map_err(|e| self.at(e, term.source_info.span))?;
        }
        Ok(())
    }

    fn at(&self, e: anyhow::Error, span: Span) -> anyhow::Error {
        e.context(format!("{}: in `{}`", self.span_str(span), self.instance))
    }

    /// Unwinding skips Athrad's frames without running their cleanup, so
    /// each cleanup block must be one whose skipping cannot be seen.
    fn check_cleanup(&mut self, bb: BasicBlock) -> Result<()> {
        let (tcx, mir, instance) = (self.tcx, self.mir, self.instance);
        let mono = |ty| {
            instance.instantiate_mir_and_normalize_erasing_regions(
                tcx,
                ty::TypingEnv::fully_monomorphized(),
                ty::EarlyBinder::bind(tcx, ty),
            )
        };
        self.m.skippable.cleanup_block(tcx, mir, bb, mono).map_err(|why| {
            let span = self.span_str(mir.basic_blocks[bb].terminator().source_info.span);
            unsupported(format!("{why} ({span})"))
        })
    }

    fn block(&self, bb: BasicBlock) -> BlockId {
        self.blocks[bb].expect("normal control flow never enters a cleanup block")
    }

    // ---- memory ----

    fn alloc(&mut self, layout: TyAndLayout<'tcx>, min_size: u64) -> Result<Value> {
        if layout.is_unsized() {
            return Err(unsupported("unsized locals"));
        }
        let size = layout.size.bytes().max(min_size);
        let align = layout.align.abi.bytes();
        if size == 0 {
            return Ok(self.b.iconst(PTR, align.into()));
        }
        let size = u32::try_from(size).map_err(|_| unsupported("stack values over 4 GiB"))?;
        let slot = self.b.slot(size, align as u32);
        Ok(self.b.slot_addr(slot))
    }

    fn copy(&mut self, dst: Value, src: Value, size: u64) {
        if size > 64 {
            let n = self.b.iconst(PTR, size.into());
            let sig = ir::Signature::new([PTR, PTR, PTR], [PTR]);
            self.m.use_func("memcpy", sig);
            self.b.call("memcpy", &[dst, src, n], &[PTR]);
            return;
        }
        let mut at = 0u64;
        for chunk in [8u64, 4, 2, 1] {
            while size - at >= chunk {
                let ty = Type::of_bytes(chunk).expect("chunk sizes are integer sizes");
                let v = self.b.load(ty, src, at as i32);
                self.b.store(dst, at as i32, v);
                at += chunk;
            }
        }
    }

    fn scalar_of(&self, layout: TyAndLayout<'tcx>) -> Result<Type> {
        match layout.backend_repr {
            BackendRepr::Scalar(s) => scalar_type(self.cx, s),
            _ => Err(unsupported(format!("`{}` as a scalar", layout.ty))),
        }
    }

    fn load_scalar(&mut self, val: Val<'tcx>) -> Result<Value> {
        match val.v {
            V::Scalar(x) => Ok(x),
            V::Ref(addr) => {
                let ty = self.scalar_of(val.layout)?;
                Ok(self.b.load(ty, addr, 0))
            }
            V::Pair(..) | V::Zst => Err(unsupported(format!("reading `{}` as one scalar", val.layout.ty))),
        }
    }

    fn load_pair(&mut self, val: Val<'tcx>) -> Result<(Value, Value)> {
        match val.v {
            V::Pair(a, b) => Ok((a, b)),
            V::Ref(addr) => {
                let BackendRepr::ScalarPair(a, b) = val.layout.backend_repr else {
                    return Err(unsupported(format!("reading `{}` as a scalar pair", val.layout.ty)));
                };
                let (ta, tb) = (scalar_type(self.cx, a)?, scalar_type(self.cx, b)?);
                let boff = off(pair_b_offset(self.cx, a, b)) as i32;
                let x = self.b.load(ta, addr, 0);
                let y = self.b.load(tb, addr, boff);
                Ok((x, y))
            }
            V::Scalar(_) | V::Zst => Err(unsupported(format!("reading `{}` as a scalar pair", val.layout.ty))),
        }
    }

    /// The value in memory: its own address, or a copy in a new slot.
    fn spill(&mut self, val: Val<'tcx>) -> Result<Value> {
        if let V::Ref(addr) = val.v {
            return Ok(addr);
        }
        let addr = self.alloc(val.layout, 0)?;
        self.write_raw(addr, val)?;
        Ok(addr)
    }

    /// Store `val` at `addr` in `val`'s own representation.
    fn write_raw(&mut self, addr: Value, val: Val<'tcx>) -> Result<()> {
        match val.v {
            V::Zst => {}
            V::Scalar(x) => self.b.store(addr, 0, x),
            V::Pair(x, y) => {
                let BackendRepr::ScalarPair(a, b) = val.layout.backend_repr else { unreachable!() };
                let boff = off(pair_b_offset(self.cx, a, b)) as i32;
                self.b.store(addr, 0, x);
                self.b.store(addr, boff, y);
            }
            V::Ref(src) => self.copy(addr, src, val.layout.size.bytes()),
        }
        Ok(())
    }

    fn write(&mut self, place: PlaceRef<'tcx>, val: Val<'tcx>) -> Result<()> {
        if place.layout.size.bytes() == 0 {
            return Ok(());
        }
        match (place.layout.backend_repr, val.v) {
            (BackendRepr::Scalar(_), _) => {
                let x = self.load_scalar(val)?;
                self.b.store(place.addr, 0, x);
            }
            (BackendRepr::ScalarPair(..), _) => {
                let (x, y) = self.load_pair(val)?;
                let v = Val { v: V::Pair(x, y), layout: place.layout };
                self.write_raw(place.addr, v)?;
            }
            _ => {
                let src = self.spill(val)?;
                self.copy(place.addr, src, place.layout.size.bytes());
            }
        }
        Ok(())
    }

    /// Write `val` into a place of another type of the same size.
    fn write_transmute(&mut self, place: PlaceRef<'tcx>, val: Val<'tcx>) -> Result<()> {
        if val.layout.size != place.layout.size {
            return Err(unsupported("a transmute between types of different sizes"));
        }
        let src = self.spill(val)?;
        self.copy(place.addr, src, place.layout.size.bytes());
        Ok(())
    }

    fn read(&mut self, place: PlaceRef<'tcx>) -> Result<Val<'tcx>> {
        if place.meta.is_some() {
            return Err(unsupported("reading an unsized place by value"));
        }
        let layout = place.layout;
        let v = if layout.size.bytes() == 0 {
            V::Zst
        } else {
            match layout.backend_repr {
                BackendRepr::Scalar(s) => {
                    let ty = scalar_type(self.cx, s)?;
                    V::Scalar(self.b.load(ty, place.addr, 0))
                }
                BackendRepr::ScalarPair(..) => {
                    let (x, y) = self.load_pair(Val { v: V::Ref(place.addr), layout })?;
                    V::Pair(x, y)
                }
                _ => V::Ref(place.addr),
            }
        };
        Ok(Val { v, layout })
    }

    /// `val` reinterpreted as `layout`, which has the same size.
    fn transmute(&mut self, val: Val<'tcx>, layout: TyAndLayout<'tcx>) -> Result<Val<'tcx>> {
        let addr = self.alloc(layout, 0)?;
        self.write_transmute(PlaceRef { addr, meta: None, layout }, val)?;
        self.read(PlaceRef { addr, meta: None, layout })
    }

    // ---- places ----

    fn place(&mut self, place: Place<'tcx>) -> Result<PlaceRef<'tcx>> {
        let mut p = self.locals[place.local];
        for elem in place.projection {
            p = match elem {
                ProjectionElem::Deref => {
                    let ptr = self.read(p)?;
                    let pointee = p
                        .layout
                        .ty
                        .builtin_deref(true)
                        .ok_or_else(|| unsupported(format!("dereferencing `{}`", p.layout.ty)))?;
                    let layout = self.layout(pointee);
                    match ptr.v {
                        V::Pair(addr, meta) => PlaceRef { addr, meta: Some(meta), layout },
                        _ => PlaceRef { addr: self.load_scalar(ptr)?, meta: None, layout },
                    }
                }
                ProjectionElem::Field(field, _) => {
                    let layout = p.layout.field(&self.cx, field.as_usize());
                    if layout.is_unsized() {
                        return Err(unsupported("fields of unsized structs"));
                    }
                    let addr = self.b.offset(p.addr, off(p.layout.fields.offset(field.as_usize())));
                    PlaceRef { addr, meta: None, layout }
                }
                ProjectionElem::Index(local) => {
                    let i = self.read(self.locals[local])?;
                    let i = self.load_scalar(i)?;
                    self.index(p, i)?
                }
                ProjectionElem::ConstantIndex { offset, from_end, .. } => {
                    let i = if from_end {
                        let len = self.len_of(p)?;
                        let k = self.b.iconst(PTR, offset.into());
                        self.b.sub(len, k)
                    } else {
                        self.b.iconst(PTR, offset.into())
                    };
                    self.index(p, i)?
                }
                ProjectionElem::Downcast(_, variant) => {
                    PlaceRef { layout: p.layout.for_variant(&self.cx, variant), ..p }
                }
                ProjectionElem::UnwrapUnsafeBinder(ty) => PlaceRef { layout: self.layout(self.monomorphize(ty)), ..p },
                ProjectionElem::Subslice { .. } => return Err(unsupported("subslice patterns")),
                ProjectionElem::OpaqueCast(_) => return Err(unsupported("opaque casts")),
            };
        }
        Ok(p)
    }

    fn len_of(&mut self, p: PlaceRef<'tcx>) -> Result<Value> {
        match p.layout.ty.kind() {
            ty::Array(_, len) => {
                let n = self.monomorphize(*len).try_to_target_usize(self.tcx).expect("monomorphic array length");
                Ok(self.b.iconst(PTR, n.into()))
            }
            ty::Slice(_) | ty::Str => p.meta.ok_or_else(|| unsupported("a slice without a length")),
            _ => Err(unsupported(format!("the length of `{}`", p.layout.ty))),
        }
    }

    fn index(&mut self, p: PlaceRef<'tcx>, i: Value) -> Result<PlaceRef<'tcx>> {
        let elem = p.layout.field(&self.cx, 0);
        let size = self.b.iconst(PTR, elem.size.bytes().into());
        let i = self.b.resize(PTR, i, false);
        let o = self.b.binary(IBin::Mul, i, size);
        let addr = self.b.add(p.addr, o);
        Ok(PlaceRef { addr, meta: None, layout: elem })
    }

    // ---- operands and constants ----

    fn operand(&mut self, op: &Operand<'tcx>) -> Result<Val<'tcx>> {
        match op {
            Operand::Copy(place) | Operand::Move(place) => {
                let p = self.place(*place)?;
                self.read(p)
            }
            Operand::Constant(c) => {
                let cv = self.monomorphize(c.const_);
                let value = cv
                    .eval(self.tcx, ty::TypingEnv::fully_monomorphized(), c.span)
                    .map_err(|_| unsupported("a constant that failed to evaluate"))?;
                self.const_value(value, cv.ty())
            }
            Operand::RuntimeChecks(checks) => {
                let on = checks.value(self.tcx.sess);
                let v = self.b.iconst(Type::I8, on.into());
                Ok(Val { v: V::Scalar(v), layout: self.layout(self.tcx.types.bool) })
            }
        }
    }

    fn addr_value(&mut self, addr: Addr, offset: Size) -> Value {
        match addr {
            Addr::Symbol(name, base) => {
                let a = self.b.symbol_addr(&name);
                self.b.offset(a, base + off(offset))
            }
            Addr::Int(n) => self.b.iconst(PTR, (n + offset.bytes()).into()),
        }
    }

    fn const_value(&mut self, value: ConstValue, ty: Ty<'tcx>) -> Result<Val<'tcx>> {
        let layout = self.layout(ty);
        if layout.is_zst() {
            return Ok(Val { v: V::Zst, layout });
        }
        let scalar_val = |s: Value| Val { v: V::Scalar(s), layout };
        match value {
            ConstValue::ZeroSized => Ok(Val { v: V::Zst, layout }),
            ConstValue::Scalar(IScalar::Int(int)) => {
                let bits = int.to_bits(int.size()) as i128;
                let ty = Type::of_bytes(int.size().bytes()).expect("integer sizes");
                let x = self.b.iconst(ty, bits);
                let v = Val { v: V::Scalar(x), layout };
                match layout.backend_repr {
                    BackendRepr::Scalar(_) => Ok(v),
                    _ => {
                        let addr = self.alloc(layout, 0)?;
                        self.b.store(addr, 0, x);
                        Ok(Val { v: V::Ref(addr), layout })
                    }
                }
            }
            ConstValue::Scalar(IScalar::Ptr(ptr, _)) => {
                let (prov, offset) = ptr.prov_and_relative_offset();
                let addr = self.m.alloc_addr(prov.alloc_id())?;
                let a = self.addr_value(addr, offset);
                Ok(scalar_val(a))
            }
            ConstValue::Indirect { alloc_id, offset } => {
                let addr = self.m.alloc_addr(alloc_id)?;
                let a = self.addr_value(addr, offset);
                Ok(Val { v: V::Ref(a), layout })
            }
            ConstValue::Slice { alloc_id, meta } => {
                let addr = self.m.alloc_addr(alloc_id)?;
                let a = self.addr_value(addr, Size::ZERO);
                let len = self.b.iconst(PTR, meta.into());
                Ok(Val { v: V::Pair(a, len), layout })
            }
        }
    }

    /// `&'static Location` for a panic at `span`, or this function's own
    /// caller's if it is `#[track_caller]`.
    fn caller_location(&mut self, si: SourceInfo) -> Result<Value> {
        let mir = self.mir;
        let tcx = self.tcx;
        let own = self.caller_location.map(Ok);
        mir.caller_location_span(si, own, tcx, |span| {
            let loc = tcx.span_as_caller_location(span);
            let v = self.const_value(loc, tcx.caller_location_ty())?;
            self.load_scalar(v)
        })
    }

    fn int_const(&mut self, layout: TyAndLayout<'tcx>, n: u128) -> Result<Value> {
        let ty = self.scalar_of(layout)?;
        Ok(self.b.iconst(ty, n as i128))
    }

    // ---- prologue ----

    fn prologue(&mut self) -> Result<()> {
        let mir = self.mir;
        let abi = self.abi;
        let params = self.b.params();
        let mut params = params.into_iter();

        // The return place: the caller's memory, or a slot of our own.
        let ret_layout = self.layout(self.monomorphize(mir.local_decls[mir::RETURN_PLACE].ty));
        let ret_addr = match &abi.ret.mode {
            PassMode::Indirect { .. } => params.next().expect("sret parameter"),
            PassMode::Cast { cast, .. } => {
                let size = cast_pieces(cast)?.iter().map(|(o, t)| o.bytes() + u64::from(t.bytes())).max();
                self.alloc(ret_layout, size.unwrap_or(0))?
            }
            _ => self.alloc(ret_layout, 0)?,
        };
        self.locals.push(PlaceRef { addr: ret_addr, meta: None, layout: ret_layout });

        let mut arg_abis = abi.args.iter();
        for local in mir.args_iter() {
            let layout = self.layout(self.monomorphize(mir.local_decls[local].ty));
            if Some(local) == mir.spread_arg {
                let ty::Tuple(fields) = layout.ty.kind() else { unreachable!("spread argument is a tuple") };
                let addr = self.alloc(layout, 0)?;
                let place = PlaceRef { addr, meta: None, layout };
                for i in 0..fields.len() {
                    let arg = arg_abis.next().expect("an ABI for every spread field");
                    let field = self.place_field(place, i);
                    self.store_param(field, &arg.mode, &mut params)?;
                }
                self.locals.push(place);
                continue;
            }
            let arg = arg_abis.next().expect("an ABI for every argument");
            if let PassMode::Indirect { .. } = arg.mode {
                // The caller hands over ownership of the memory itself.
                let addr = params.next().expect("indirect argument pointer");
                self.locals.push(PlaceRef { addr, meta: None, layout });
                continue;
            }
            let addr = self.alloc(layout, 0)?;
            let place = PlaceRef { addr, meta: None, layout };
            self.store_param(place, &arg.mode, &mut params)?;
            self.locals.push(place);
        }
        if self.instance.def.requires_caller_location(self.tcx) {
            let arg = arg_abis.next().expect("an ABI for the caller location");
            let PassMode::Direct(_) = arg.mode else { unreachable!("the location is a pointer") };
            self.caller_location = Some(params.next().expect("caller location parameter"));
        }
        assert!(arg_abis.next().is_none() && params.next().is_none(), "every parameter has a home");

        for local in mir.vars_and_temps_iter() {
            let layout = self.layout(self.monomorphize(mir.local_decls[local].ty));
            let addr = self.alloc(layout, 0)?;
            self.locals.push(PlaceRef { addr, meta: None, layout });
        }
        let start = self.block(mir::START_BLOCK);
        self.b.jump(start, &[]);
        Ok(())
    }

    fn place_field(&mut self, p: PlaceRef<'tcx>, i: usize) -> PlaceRef<'tcx> {
        let layout = p.layout.field(&self.cx, i);
        let addr = self.b.offset(p.addr, off(p.layout.fields.offset(i)));
        PlaceRef { addr, meta: None, layout }
    }

    fn store_param(
        &mut self,
        place: PlaceRef<'tcx>,
        mode: &PassMode,
        params: &mut impl Iterator<Item = Value>,
    ) -> Result<()> {
        let mut next = || params.next().expect("a parameter for each ABI piece");
        match mode {
            PassMode::Ignore => {}
            PassMode::Direct(_) => {
                let x = next();
                self.b.store(place.addr, 0, x);
            }
            PassMode::Pair(..) => {
                let (x, y) = (next(), next());
                self.write_raw(place.addr, Val { v: V::Pair(x, y), layout: place.layout })?;
            }
            PassMode::Cast { cast, .. } => {
                // The slot must hold every register's bytes, which may be
                // more than the type's size.
                let pieces = cast_pieces(cast)?;
                let size = pieces.iter().map(|(o, t)| o.bytes() + u64::from(t.bytes())).max().unwrap_or(0);
                let tmp = self.alloc(place.layout, size)?;
                for (o, _) in pieces {
                    let x = next();
                    self.b.store(tmp, off(o) as i32, x);
                }
                self.copy(place.addr, tmp, place.layout.size.bytes());
            }
            PassMode::Indirect { .. } => {
                let src = next();
                self.copy(place.addr, src, place.layout.size.bytes());
            }
        }
        Ok(())
    }

    // ---- statements ----

    fn statement(&mut self, stmt: &mir::Statement<'tcx>) -> Result<()> {
        match &stmt.kind {
            StatementKind::Assign(assign) => {
                let (place, rvalue) = &**assign;
                let dest = self.place(*place)?;
                self.rvalue(dest, rvalue)
            }
            StatementKind::SetDiscriminant { place, variant_index } => {
                let p = self.place(**place)?;
                self.set_discriminant(p, *variant_index)
            }
            StatementKind::StorageLive(_)
            | StatementKind::StorageDead(_)
            | StatementKind::Nop
            | StatementKind::FakeRead(..)
            | StatementKind::PlaceMention(..)
            | StatementKind::AscribeUserType(..)
            | StatementKind::ConstEvalCounter
            | StatementKind::BackwardIncompatibleDropHint { .. }
            | StatementKind::Coverage(..) => Ok(()),
            StatementKind::Intrinsic(i) => match &**i {
                NonDivergingIntrinsic::Assume(_) => Ok(()),
                NonDivergingIntrinsic::CopyNonOverlapping(c) => {
                    let dst = self.operand(&c.dst)?;
                    let pointee = dst.layout.ty.builtin_deref(true).expect("copy to a pointer");
                    let size = self.layout(pointee).size.bytes();
                    let dst = self.load_scalar(dst)?;
                    let src = self.operand(&c.src)?;
                    let src = self.load_scalar(src)?;
                    let count = self.operand(&c.count)?;
                    let count = self.load_scalar(count)?;
                    let k = self.b.iconst(PTR, size.into());
                    let n = self.b.binary(IBin::Mul, count, k);
                    self.m.use_func("memcpy", ir::Signature::new([PTR, PTR, PTR], [PTR]));
                    self.b.call("memcpy", &[dst, src, n], &[PTR]);
                    Ok(())
                }
            },
        }
    }

    fn rvalue(&mut self, dest: PlaceRef<'tcx>, rvalue: &Rvalue<'tcx>) -> Result<()> {
        match rvalue {
            Rvalue::Use(op, _) => {
                let v = self.operand(op)?;
                self.write(dest, v)
            }
            Rvalue::CopyForDeref(place) | Rvalue::Reborrow(_, _, place) => {
                let p = self.place(*place)?;
                let v = self.read(p)?;
                self.write(dest, v)
            }
            Rvalue::Ref(_, _, place) | Rvalue::RawPtr(_, place) => {
                let p = self.place(*place)?;
                let v = match p.meta {
                    Some(meta) => V::Pair(p.addr, meta),
                    None => V::Scalar(p.addr),
                };
                self.write(dest, Val { v, layout: dest.layout })
            }
            Rvalue::ThreadLocalRef(_) => Err(unsupported("thread-local statics")),
            Rvalue::Cast(kind, op, to) => {
                let to = self.layout(self.monomorphize(*to));
                let v = self.cast(*kind, op, to)?;
                self.write(dest, v)
            }
            Rvalue::BinaryOp(op, ops) => {
                let a = self.operand(&ops.0)?;
                let b = self.operand(&ops.1)?;
                let v = self.binop(*op, a, b, dest.layout)?;
                self.write(dest, v)
            }
            Rvalue::UnaryOp(op, operand) => {
                let a = self.operand(operand)?;
                let v = match op {
                    UnOp::Not => {
                        let x = self.load_scalar(a)?;
                        let mask = if a.layout.ty.is_bool() { 1 } else { -1 };
                        let ty = self.b.value_type(x);
                        let m = self.b.iconst(ty, mask);
                        V::Scalar(self.b.binary(IBin::Xor, x, m))
                    }
                    UnOp::Neg => {
                        if !matches!(a.layout.ty.kind(), ty::Int(_)) {
                            return Err(unsupported(format!("negating `{}`", a.layout.ty)));
                        }
                        let x = self.load_scalar(a)?;
                        let ty = self.b.value_type(x);
                        let z = self.b.iconst(ty, 0);
                        V::Scalar(self.b.sub(z, x))
                    }
                    UnOp::PtrMetadata => match a.layout.backend_repr {
                        BackendRepr::ScalarPair(..) => V::Scalar(self.load_pair(a)?.1),
                        _ => V::Zst,
                    },
                };
                self.write(dest, Val { v, layout: dest.layout })
            }
            Rvalue::Discriminant(place) => {
                let p = self.place(*place)?;
                let d = self.discriminant(p, dest.layout)?;
                self.write(dest, Val { v: V::Scalar(d), layout: dest.layout })
            }
            Rvalue::Repeat(op, times) => {
                let v = self.operand(op)?;
                let n = self.monomorphize(*times).try_to_target_usize(self.tcx).expect("monomorphic repeat count");
                if v.layout.size.bytes() == 0 || n == 0 {
                    return Ok(());
                }
                let (head, i) = {
                    let (head, ps) = self.b.block(&[PTR]);
                    (head, ps[0])
                };
                let (body, _) = self.b.block(&[]);
                let (done, _) = self.b.block(&[]);
                let zero = self.b.iconst(PTR, 0);
                self.b.jump(head, &[zero]);
                self.b.switch_to(head);
                let end = self.b.iconst(PTR, n.into());
                let finished = self.b.cmp(Cmp::Eq, i, end);
                self.b.branch(finished, (done, &[]), (body, &[]));
                self.b.switch_to(body);
                let elem = self.index(dest, i)?;
                self.write(elem, v)?;
                let one = self.b.iconst(PTR, 1);
                let next = self.b.add(i, one);
                self.b.jump(head, &[next]);
                self.b.switch_to(done);
                Ok(())
            }
            Rvalue::Aggregate(kind, ops) => self.aggregate(dest, kind, ops),
            Rvalue::WrapUnsafeBinder(op, _) => {
                let v = self.operand(op)?;
                self.write_transmute(dest, v)
            }
        }
    }

    fn aggregate(
        &mut self,
        dest: PlaceRef<'tcx>,
        kind: &AggregateKind<'tcx>,
        ops: &IndexVec<FieldIdx, Operand<'tcx>>,
    ) -> Result<()> {
        if let AggregateKind::RawPtr(..) = kind {
            let data = self.operand(&ops[FieldIdx::from_u32(0)])?;
            let meta = self.operand(&ops[FieldIdx::from_u32(1)])?;
            let d = self.load_scalar(data)?;
            let v = if meta.layout.is_zst() { V::Scalar(d) } else { V::Pair(d, self.load_scalar(meta)?) };
            return self.write(dest, Val { v, layout: dest.layout });
        }
        let (variant, active) = match kind {
            AggregateKind::Adt(_, variant, _, _, active) => (*variant, *active),
            AggregateKind::Coroutine(..) | AggregateKind::CoroutineClosure(..) => {
                return Err(unsupported("coroutines"));
            }
            _ => (FIRST_VARIANT, None),
        };
        let variant_dest = PlaceRef { layout: dest.layout.for_variant(&self.cx, variant), ..dest };
        for (i, op) in ops.iter_enumerated() {
            let v = self.operand(op)?;
            let field = active.unwrap_or(i);
            let to = if let AggregateKind::Array(_) = kind {
                let idx = self.b.iconst(PTR, i.as_u32().into());
                self.index(variant_dest, idx)?
            } else {
                self.place_field(variant_dest, field.as_usize())
            };
            self.write(to, v)?;
        }
        self.set_discriminant(dest, variant)
    }

    // ---- enums ----

    fn set_discriminant(&mut self, p: PlaceRef<'tcx>, variant: VariantIdx) -> Result<()> {
        let layout = p.layout;
        if layout.for_variant(&self.cx, variant).is_uninhabited() {
            return Ok(());
        }
        match &layout.variants {
            Variants::Empty => unreachable!("uninhabited"),
            Variants::Single { .. } => Ok(()),
            Variants::Multiple { tag_field, tag_encoding: TagEncoding::Direct, .. } => {
                let tag = self.place_field(p, tag_field.as_usize());
                let d = layout.ty.discriminant_for_variant(self.tcx, variant).expect("an enum").val;
                let x = self.int_const(tag.layout, d)?;
                self.b.store(tag.addr, 0, x);
                Ok(())
            }
            Variants::Multiple {
                tag_field,
                tag_encoding: TagEncoding::Niche { untagged_variant, niche_variants, niche_start },
                ..
            } => {
                if variant != *untagged_variant {
                    let niche = self.place_field(p, tag_field.as_usize());
                    let value = u128::from(variant.as_u32() - niche_variants.start.as_u32()).wrapping_add(*niche_start);
                    let x = self.int_const(niche.layout, value)?;
                    self.b.store(niche.addr, 0, x);
                }
                Ok(())
            }
        }
    }

    fn discriminant(&mut self, p: PlaceRef<'tcx>, dest: TyAndLayout<'tcx>) -> Result<Value> {
        let layout = p.layout;
        let dest_ty = self.scalar_of(dest)?;
        if layout.is_uninhabited() {
            return Ok(self.b.iconst(dest_ty, 0));
        }
        let (tag_scalar, tag_field, encoding) = match &layout.variants {
            Variants::Empty => unreachable!("uninhabited"),
            Variants::Single { index } => {
                let d =
                    layout.ty.discriminant_for_variant(self.tcx, *index).map_or(u128::from(index.as_u32()), |d| d.val);
                return Ok(self.b.iconst(dest_ty, d as i128));
            }
            Variants::Multiple { tag, tag_field, tag_encoding, .. } => (*tag, *tag_field, tag_encoding),
        };
        let tag_place = self.place_field(p, tag_field.as_usize());
        let tag = self.read(tag_place)?;
        let tag = self.load_scalar(tag)?;
        match encoding {
            TagEncoding::Direct => Ok(self.b.resize(dest_ty, tag, is_signed(tag_scalar))),
            TagEncoding::Niche { untagged_variant, niche_variants, niche_start } => {
                let tag_ty = self.b.value_type(tag);
                let relative_max = niche_variants.last.as_u32() - niche_variants.start.as_u32();
                let start = self.b.iconst(tag_ty, *niche_start as i128);
                let relative = self.b.sub(tag, start);
                let max = self.b.iconst(tag_ty, relative_max.into());
                let is_niche = self.b.cmp(Cmp::ULe, relative, max);
                let relative = self.b.resize(dest_ty, relative, false);
                let first = self.b.iconst(dest_ty, niche_variants.start.as_u32().into());
                let tagged = self.b.add(relative, first);
                let untagged = self.b.iconst(dest_ty, untagged_variant.as_u32().into());
                Ok(self.b.select(is_niche, tagged, untagged))
            }
        }
    }

    // ---- casts and arithmetic ----

    fn cast(&mut self, kind: CastKind, op: &Operand<'tcx>, to: TyAndLayout<'tcx>) -> Result<Val<'tcx>> {
        let scalar = |v: Value| Val { v: V::Scalar(v), layout: to };
        match kind {
            CastKind::PointerCoercion(PointerCoercion::ReifyFnPointer(_), _) => {
                let from = self.monomorphize(op.ty(self.mir, self.tcx));
                let ty::FnDef(def_id, args) = *from.kind() else { unreachable!("reify of a non-fn-item") };
                let inst = Instance::resolve_for_fn_ptr(self.tcx, ty::TypingEnv::fully_monomorphized(), def_id, args)
                    .ok_or_else(|| unsupported("this function pointer"))?;
                let (name, _) = self.m.func_symbol(inst)?;
                Ok(scalar(self.b.symbol_addr(&name)))
            }
            CastKind::PointerCoercion(PointerCoercion::ClosureFnPointer(_), _) => {
                let from = self.monomorphize(op.ty(self.mir, self.tcx));
                let ty::Closure(def_id, args) = *from.kind() else { unreachable!("closure cast of a non-closure") };
                let inst = Instance::resolve_closure(self.tcx, def_id, args, ty::ClosureKind::FnOnce);
                let (name, _) = self.m.func_symbol(inst)?;
                Ok(scalar(self.b.symbol_addr(&name)))
            }
            CastKind::PointerCoercion(PointerCoercion::UnsafeFnPointer, _) => {
                let v = self.operand(op)?;
                Ok(scalar(self.load_scalar(v)?))
            }
            CastKind::PointerCoercion(PointerCoercion::Unsize, _) => {
                let v = self.operand(op)?;
                self.unsize(v, to)
            }
            CastKind::PointerCoercion(..) => Err(unsupported("this pointer coercion")),
            CastKind::IntToInt => {
                let v = self.operand(op)?;
                let x = self.load_scalar(v)?;
                let signed = matches!(v.layout.ty.kind(), ty::Int(_));
                let ty = self.scalar_of(to)?;
                Ok(scalar(self.b.resize(ty, x, signed)))
            }
            CastKind::PtrToPtr
            | CastKind::FnPtrToPtr
            | CastKind::PointerExposeProvenance
            | CastKind::PointerWithExposedProvenance => {
                let v = self.operand(op)?;
                match (v.layout.backend_repr, to.backend_repr) {
                    (BackendRepr::ScalarPair(..), BackendRepr::ScalarPair(..)) => {
                        let (a, b) = self.load_pair(v)?;
                        Ok(Val { v: V::Pair(a, b), layout: to })
                    }
                    (BackendRepr::ScalarPair(..), _) => Ok(scalar(self.load_pair(v)?.0)),
                    _ => {
                        let x = self.load_scalar(v)?;
                        let ty = self.scalar_of(to)?;
                        Ok(scalar(self.b.resize(ty, x, false)))
                    }
                }
            }
            CastKind::Transmute | CastKind::Subtype => {
                let v = self.operand(op)?;
                match (v.layout.backend_repr, to.backend_repr, v.v) {
                    (BackendRepr::Scalar(a), BackendRepr::Scalar(b), _)
                        if scalar_type(self.cx, a)? == scalar_type(self.cx, b)? =>
                    {
                        Ok(scalar(self.load_scalar(v)?))
                    }
                    _ if v.layout.size == to.size && to.is_zst() => Ok(Val { v: V::Zst, layout: to }),
                    _ => self.transmute(v, to),
                }
            }
            CastKind::FloatToInt | CastKind::FloatToFloat | CastKind::IntToFloat => Err(unsupported("floating point")),
        }
    }

    /// `&T` to `&[T]` or `&dyn Trait`, and the same for raw pointers and `Box`.
    fn unsize(&mut self, v: Val<'tcx>, to: TyAndLayout<'tcx>) -> Result<Val<'tcx>> {
        let pointee = |ty: Ty<'tcx>| ty.builtin_deref(true).or_else(|| ty.boxed_ty());
        let (Some(src), Some(dst)) = (pointee(v.layout.ty), pointee(to.ty)) else {
            return Err(unsupported(format!("unsizing `{}` to `{}`", v.layout.ty, to.ty)));
        };
        let (src, dst) = self.tcx.struct_lockstep_tails_for_codegen(src, dst, ty::TypingEnv::fully_monomorphized());
        let pair = |data, meta| Val { v: V::Pair(data, meta), layout: to };
        match (src.kind(), dst.kind()) {
            (ty::Array(_, len), ty::Slice(_)) => {
                let data = self.load_scalar(v)?;
                let n = len.try_to_target_usize(self.tcx).expect("monomorphic array length");
                let len = self.b.iconst(PTR, n.into());
                Ok(pair(data, len))
            }
            (ty::Dynamic(a, _), ty::Dynamic(b, _)) => {
                if a.principal_def_id() != b.principal_def_id() && b.principal_def_id().is_some() {
                    return Err(unsupported("trait upcasting"));
                }
                let (data, meta) = self.load_pair(v)?;
                Ok(pair(data, meta))
            }
            (_, ty::Dynamic(data, _)) => {
                let ptr = self.load_scalar(v)?;
                let principal = data.principal().map(|p| self.tcx.instantiate_bound_regions_with_erased(p));
                let vtable = self.tcx.vtable_allocation((src, principal));
                let addr = self.m.alloc_addr(vtable)?;
                let meta = self.addr_value(addr, Size::ZERO);
                Ok(pair(ptr, meta))
            }
            _ => Err(unsupported(format!("unsizing `{src}` to `{dst}`"))),
        }
    }

    fn binop(&mut self, op: BinOp, a: Val<'tcx>, b: Val<'tcx>, dest: TyAndLayout<'tcx>) -> Result<Val<'tcx>> {
        let ty = a.layout.ty;
        if ty.is_floating_point() {
            return Err(unsupported("floating point"));
        }
        if matches!(a.layout.backend_repr, BackendRepr::ScalarPair(..)) {
            return Err(unsupported("operators on wide pointers"));
        }
        let signed = matches!(ty.kind(), ty::Int(_));
        let x = self.load_scalar(a)?;
        let mut y = self.load_scalar(b)?;
        let scalar = |v: Value| Val { v: V::Scalar(v), layout: dest };
        let cmp = |s: Cmp, u: Cmp| if signed { s } else { u };
        let checked = |o: OverflowOp| -> (OverflowOp, bool) { (o, signed) };
        let r = match op {
            BinOp::Add | BinOp::AddUnchecked => self.b.binary(IBin::Add, x, y),
            BinOp::Sub | BinOp::SubUnchecked => self.b.binary(IBin::Sub, x, y),
            BinOp::Mul | BinOp::MulUnchecked => self.b.binary(IBin::Mul, x, y),
            BinOp::Div => self.b.binary(if signed { IBin::SDiv } else { IBin::UDiv }, x, y),
            BinOp::Rem => self.b.binary(if signed { IBin::SRem } else { IBin::URem }, x, y),
            BinOp::BitXor => self.b.binary(IBin::Xor, x, y),
            BinOp::BitAnd => self.b.binary(IBin::And, x, y),
            BinOp::BitOr => self.b.binary(IBin::Or, x, y),
            BinOp::Shl | BinOp::ShlUnchecked | BinOp::Shr | BinOp::ShrUnchecked => {
                let xt = self.b.value_type(x);
                y = self.b.resize(xt, y, false);
                let o = match op {
                    BinOp::Shl | BinOp::ShlUnchecked => IBin::Shl,
                    _ if signed => IBin::SShr,
                    _ => IBin::UShr,
                };
                self.b.binary(o, x, y)
            }
            BinOp::AddWithOverflow | BinOp::SubWithOverflow | BinOp::MulWithOverflow => {
                let (o, s) = checked(match op {
                    BinOp::AddWithOverflow => OverflowOp::Add,
                    BinOp::SubWithOverflow => OverflowOp::Sub,
                    _ => OverflowOp::Mul,
                });
                if self.b.value_type(x) == Type::I128 {
                    return Err(unsupported("overflow checks on 128-bit integers"));
                }
                let (r, of) = self.b.overflow(o, s, x, y);
                return Ok(Val { v: V::Pair(r, of), layout: dest });
            }
            BinOp::Eq => self.b.cmp(Cmp::Eq, x, y),
            BinOp::Ne => self.b.cmp(Cmp::Ne, x, y),
            BinOp::Lt => self.b.cmp(cmp(Cmp::SLt, Cmp::ULt), x, y),
            BinOp::Le => self.b.cmp(cmp(Cmp::SLe, Cmp::ULe), x, y),
            BinOp::Gt => self.b.cmp(cmp(Cmp::SGt, Cmp::UGt), x, y),
            BinOp::Ge => self.b.cmp(cmp(Cmp::SGe, Cmp::UGe), x, y),
            BinOp::Cmp => {
                let lt = self.b.cmp(cmp(Cmp::SLt, Cmp::ULt), x, y);
                let gt = self.b.cmp(cmp(Cmp::SGt, Cmp::UGt), x, y);
                self.b.sub(gt, lt)
            }
            BinOp::Offset => {
                let pointee = ty.builtin_deref(true).expect("offset of a pointer");
                let size = self.layout(pointee).size.bytes();
                let y = self.b.resize(PTR, y, true);
                let k = self.b.iconst(PTR, size.into());
                let o = self.b.binary(IBin::Mul, y, k);
                self.b.add(x, o)
            }
        };
        Ok(scalar(r))
    }

    // ---- terminators ----

    fn terminator(&mut self, term: &mir::Terminator<'tcx>) -> Result<()> {
        let si = term.source_info;
        match &term.kind {
            TerminatorKind::Goto { target } => {
                let t = self.block(*target);
                self.b.jump(t, &[]);
            }
            TerminatorKind::Return => self.ret()?,
            TerminatorKind::Unreachable | TerminatorKind::UnwindResume | TerminatorKind::UnwindTerminate(_) => {
                self.b.trap()
            }
            TerminatorKind::SwitchInt { discr, targets } => {
                let d = self.operand(discr)?;
                let d = self.load_scalar(d)?;
                let ty = self.b.value_type(d);
                for (value, target) in targets.iter() {
                    let k = self.b.iconst(ty, value as i128);
                    let hit = self.b.cmp(Cmp::Eq, d, k);
                    let (next, _) = self.b.block(&[]);
                    let t = self.block(target);
                    self.b.branch(hit, (t, &[]), (next, &[]));
                    self.b.switch_to(next);
                }
                let other = self.block(targets.otherwise());
                self.b.jump(other, &[]);
            }
            TerminatorKind::Assert { cond, expected, msg, target, .. } => {
                let ok = self.block(*target);
                if !self.tcx.sess.overflow_checks() && msg.is_optional_overflow_check() {
                    self.b.jump(ok, &[]);
                    return Ok(());
                }
                let c = self.operand(cond)?;
                let c = self.load_scalar(c)?;
                let (fail, _) = self.b.block(&[]);
                if *expected {
                    self.b.branch(c, (ok, &[]), (fail, &[]));
                } else {
                    self.b.branch(c, (fail, &[]), (ok, &[]));
                }
                self.b.switch_to(fail);
                self.assert_failure(si, msg)?;
            }
            TerminatorKind::Call { func, args, destination, target, fn_span, .. } => {
                let si = SourceInfo { span: *fn_span, ..si };
                self.call(si, func, args, *destination, *target)?;
            }
            TerminatorKind::Drop { place, target, .. } => {
                let p = self.place(*place)?;
                self.drop(p, *target)?;
            }
            TerminatorKind::TailCall { .. } => return Err(unsupported("tail calls")),
            TerminatorKind::InlineAsm { .. } => return Err(unsupported("inline assembly")),
            TerminatorKind::Yield { .. }
            | TerminatorKind::CoroutineDrop
            | TerminatorKind::FalseEdge { .. }
            | TerminatorKind::FalseUnwind { .. } => return Err(unsupported("coroutines")),
        }
        Ok(())
    }

    fn ret(&mut self) -> Result<()> {
        let place = self.locals[mir::RETURN_PLACE];
        let values = match &self.abi.ret.mode {
            PassMode::Ignore | PassMode::Indirect { .. } => vec![],
            PassMode::Direct(_) => {
                let v = self.read(place)?;
                vec![self.load_scalar(v)?]
            }
            PassMode::Pair(..) => {
                let v = self.read(place)?;
                let (a, b) = self.load_pair(v)?;
                vec![a, b]
            }
            PassMode::Cast { cast, .. } => {
                let pieces = cast_pieces(cast)?;
                pieces.into_iter().map(|(o, ty)| self.b.load(ty, place.addr, off(o) as i32)).collect()
            }
        };
        self.b.ret(&values);
        Ok(())
    }

    fn assert_failure(&mut self, si: SourceInfo, msg: &AssertKind<Operand<'tcx>>) -> Result<()> {
        let mut args = Vec::new();
        let item = match msg {
            AssertKind::BoundsCheck { len, index } => {
                for op in [index, len] {
                    let v = self.operand(op)?;
                    args.push(self.load_scalar(v)?);
                }
                LangItem::PanicBoundsCheck
            }
            AssertKind::MisalignedPointerDereference { required, found } => {
                for op in [required, found] {
                    let v = self.operand(op)?;
                    args.push(self.load_scalar(v)?);
                }
                LangItem::PanicMisalignedPointerDereference
            }
            AssertKind::NullPointerDereference => LangItem::PanicNullPointerDereference,
            AssertKind::InvalidEnumConstruction(source) => {
                let v = self.operand(source)?;
                args.push(self.load_scalar(v)?);
                LangItem::PanicInvalidEnumConstruction
            }
            _ => msg.panic_function(),
        };
        args.push(self.caller_location(si)?);
        let def_id = self.tcx.require_lang_item(item, si.span);
        let inst = Instance::mono(self.tcx, def_id);
        let name = self.tcx.symbol_name(inst).name.to_owned();
        let params: Vec<Type> = args.iter().map(|&a| self.b.value_type(a)).collect();
        self.m.use_func(&name, ir::Signature::new(params, []));
        self.b.call(&name, &args, &[]);
        self.b.trap();
        Ok(())
    }

    fn drop(&mut self, p: PlaceRef<'tcx>, target: BasicBlock) -> Result<()> {
        let ty = p.layout.ty;
        let inst = Instance::resolve_drop_glue(self.tcx, ty);
        let t = self.block(target);
        if let InstanceKind::Shim(ShimKind::DropGlue(_, None)) = inst.def {
            self.b.jump(t, &[]);
            return Ok(());
        }
        if let ty::Dynamic(..) = ty.kind() {
            return Err(unsupported("dropping trait objects"));
        }
        let ptr_layout = self.layout(Ty::new_mut_ptr(self.tcx, ty));
        let v = match p.meta {
            Some(meta) => V::Pair(p.addr, meta),
            None => V::Scalar(p.addr),
        };
        let abi = self.cx.fn_abi_of_instance(inst, ty::List::empty());
        let (name, _) = self.m.func_symbol(inst)?;
        self.emit_call(Callee::Direct(name), abi, vec![Val { v, layout: ptr_layout }], None)?;
        self.b.jump(t, &[]);
        Ok(())
    }

    fn call(
        &mut self,
        si: SourceInfo,
        func: &Operand<'tcx>,
        args: &[rustc_span::Spanned<Operand<'tcx>>],
        destination: Place<'tcx>,
        target: Option<BasicBlock>,
    ) -> Result<()> {
        let fn_ty = self.monomorphize(func.ty(self.mir, self.tcx));
        let fn_sig = fn_ty.fn_sig(self.tcx);
        let dest = self.place(destination)?;

        let instance = if let ty::FnDef(def_id, fn_args) = *fn_ty.kind() {
            let inst =
                Instance::expect_resolve(self.tcx, ty::TypingEnv::fully_monomorphized(), def_id, fn_args, si.span);
            match inst.def {
                InstanceKind::Intrinsic(def_id) => {
                    if self.intrinsic(def_id, args, dest, target)? {
                        return Ok(());
                    }
                    // Not one Athrad implements: call its fallback body.
                    Some(Instance::new_raw(def_id, inst.args))
                }
                InstanceKind::Shim(ShimKind::DropGlue(_, None)) => {
                    let t = self.block(target.expect("drop glue returns"));
                    self.b.jump(t, &[]);
                    return Ok(());
                }
                _ => Some(inst),
            }
        } else {
            None
        };

        let extra: Vec<Ty<'tcx>> = args[fn_sig.inputs().skip_binder().len()..]
            .iter()
            .map(|a| self.monomorphize(a.node.ty(self.mir, self.tcx)))
            .collect();
        let extra = self.tcx.mk_type_list(&extra);
        let abi = match instance {
            Some(inst) => self.cx.fn_abi_of_instance(inst, extra),
            None => self.cx.fn_abi_of_fn_ptr(fn_sig, extra),
        };

        let mut vals = Vec::new();
        if fn_sig.abi() == rustc_abi::ExternAbi::RustCall {
            let (pack, rest) = args.split_last().expect("rust-call takes a tuple");
            for a in rest {
                vals.push(self.operand(&a.node)?);
            }
            let tuple = self.operand(&pack.node)?;
            let ty::Tuple(fields) = tuple.layout.ty.kind() else { unreachable!("rust-call takes a tuple") };
            if !fields.is_empty() {
                let addr = self.spill(tuple)?;
                let place = PlaceRef { addr, meta: None, layout: tuple.layout };
                for i in 0..fields.len() {
                    let f = self.place_field(place, i);
                    vals.push(self.read(f)?);
                }
            }
        } else {
            for a in args {
                vals.push(self.operand(&a.node)?);
            }
        }
        if let Some(inst) = instance
            && inst.def.requires_caller_location(self.tcx)
        {
            let loc = self.caller_location(si)?;
            vals.push(Val { v: V::Scalar(loc), layout: self.layout(self.tcx.caller_location_ty()) });
        }

        let callee = match instance {
            Some(Instance { def: InstanceKind::Virtual(_, idx), .. }) => Callee::Virtual(idx),
            Some(inst) => Callee::Direct(self.m.func_symbol(inst)?.0),
            None => {
                let f = self.operand(func)?;
                Callee::Indirect(self.load_scalar(f)?)
            }
        };
        self.emit_call(callee, abi, vals, Some(dest))?;
        match target {
            Some(t) => {
                let t = self.block(t);
                self.b.jump(t, &[]);
            }
            None => self.b.trap(),
        }
        Ok(())
    }

    /// Lower a call to an intrinsic Athrad implements itself. `false` means
    /// call its fallback body instead.
    fn intrinsic(
        &mut self,
        def_id: rustc_hir::def_id::DefId,
        args: &[rustc_span::Spanned<Operand<'tcx>>],
        dest: PlaceRef<'tcx>,
        target: Option<BasicBlock>,
    ) -> Result<bool> {
        let def = self.tcx.intrinsic(def_id).expect("an intrinsic");
        let name = def.name.as_str();
        match name {
            "black_box" => {
                let v = self.operand(&args[0].node)?;
                self.write(dest, v)?;
            }
            "assume" | "cold_path" => {}
            "abort" | "unreachable" => {
                self.b.trap();
                return Ok(true);
            }
            _ if !def.must_be_overridden => return Ok(false),
            _ => return Err(unsupported(format!("the `{name}` intrinsic"))),
        }
        match target {
            Some(t) => {
                let t = self.block(t);
                self.b.jump(t, &[]);
            }
            None => self.b.trap(),
        }
        Ok(true)
    }

    fn emit_call(
        &mut self,
        callee: Callee,
        abi: &FnAbi<'tcx, Ty<'tcx>>,
        vals: Vec<Val<'tcx>>,
        dest: Option<PlaceRef<'tcx>>,
    ) -> Result<()> {
        assert_eq!(abi.args.len(), vals.len(), "one value per ABI argument");
        let sig = signature(self.cx, abi)?;
        let mut call_args = Vec::new();

        let ret_mem = match &abi.ret.mode {
            PassMode::Indirect { .. } => {
                let addr = match dest {
                    Some(d) if d.layout.size == abi.ret.layout.size => d.addr,
                    _ => self.alloc(abi.ret.layout, 0)?,
                };
                call_args.push(addr);
                Some(addr)
            }
            _ => None,
        };

        let mut fn_ptr = None;
        for (i, (val, arg)) in vals.into_iter().zip(abi.args.iter()).enumerate() {
            if i == 0
                && let Callee::Virtual(idx) = callee
            {
                let (data, vtable) = self.load_pair(val)?;
                fn_ptr = Some(self.b.load(PTR, vtable, (idx * 8) as i32));
                call_args.push(data);
                continue;
            }
            match &arg.mode {
                PassMode::Ignore => {}
                PassMode::Direct(_) => call_args.push(self.load_scalar(val)?),
                PassMode::Pair(..) => {
                    let (a, b) = self.load_pair(val)?;
                    call_args.extend([a, b]);
                }
                PassMode::Cast { cast, .. } => {
                    let pieces = cast_pieces(cast)?;
                    let size = pieces.iter().map(|(o, t)| o.bytes() + u64::from(t.bytes())).max().unwrap_or(0);
                    let tmp = self.alloc(val.layout, size)?;
                    self.write_raw(tmp, val)?;
                    for (o, ty) in pieces {
                        call_args.push(self.b.load(ty, tmp, off(o) as i32));
                    }
                }
                PassMode::Indirect { .. } => {
                    // A copy the callee may consume or change.
                    let tmp = self.alloc(val.layout, 0)?;
                    self.write_raw(tmp, val)?;
                    call_args.push(tmp);
                }
            }
        }

        let results = match callee {
            Callee::Direct(name) => {
                self.m.use_func(&name, sig.clone());
                let rets = sig.return_types();
                self.b.call(&name, &call_args, &rets)
            }
            Callee::Indirect(ptr) => self.b.call_indirect(sig, ptr, &call_args),
            Callee::Virtual(_) => self.b.call_indirect(sig, fn_ptr.expect("a receiver"), &call_args),
        };

        let ret_layout = abi.ret.layout;
        let ret = match &abi.ret.mode {
            PassMode::Ignore => return Ok(()),
            PassMode::Indirect { .. } => {
                let mem = ret_mem.expect("indirect return memory");
                if dest.is_some_and(|d| d.addr == mem) {
                    return Ok(());
                }
                Val { v: V::Ref(mem), layout: ret_layout }
            }
            PassMode::Direct(_) => Val { v: V::Scalar(results[0]), layout: ret_layout },
            PassMode::Pair(..) => Val { v: V::Pair(results[0], results[1]), layout: ret_layout },
            PassMode::Cast { cast, .. } => {
                let pieces = cast_pieces(cast)?;
                let size = pieces.iter().map(|(o, t)| o.bytes() + u64::from(t.bytes())).max().unwrap_or(0);
                let tmp = self.alloc(ret_layout, size)?;
                for ((o, _), r) in pieces.iter().zip(&results) {
                    self.b.store(tmp, off(*o) as i32, *r);
                }
                Val { v: V::Ref(tmp), layout: ret_layout }
            }
        };
        if let Some(d) = dest {
            self.write(d, ret)?;
        }
        Ok(())
    }
}
