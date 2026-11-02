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

mod simd;

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
    if abi.c_variadic {
        return Err(unsupported("defining C-variadic functions"));
    }
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
        pending_unwind: None,
        exn_slot: None,
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
    /// What the call the current terminator makes does when the callee
    /// unwinds; taken by the first call made.
    pending_unwind: Option<mir::UnwindAction>,
    /// Where a landing pad keeps the exception for `UnwindResume`.
    exn_slot: Option<ir::Slot>,
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
        // Blocks a constant `switchInt` rules out once types are known are
        // dead, and may name items the mono collector never saw, as `rustc`
        // skips them. They become traps.
        let reachable = rustc_middle::mir::traversal::mono_reachable_as_bitset(mir, self.tcx, self.instance);
        for (bb, data) in mir.basic_blocks.iter_enumerated() {
            if !reachable.contains(bb) {
                let (id, _) = self.b.block(&[]);
                self.blocks.push(Some(id));
            } else if data.is_cleanup && !landing_pads(self.tcx) {
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
            if !reachable.contains(bb) {
                self.b.trap();
                continue;
            }
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
        let span =
            || tcx.sess.source_map().span_to_diagnostic_string(mir.basic_blocks[bb].terminator().source_info.span);
        self.m
            .skippable
            .cleanup_block(tcx, mir, bb, mono, span)
            .map_err(|why| unsupported(format!("{why} ({})", span())))
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
                    let offset = off(p.layout.fields.offset(field.as_usize()));
                    if !layout.is_unsized() {
                        PlaceRef { addr: self.b.offset(p.addr, offset), meta: None, layout }
                    } else {
                        // The unsized tail keeps the metadata. Its offset is
                        // static unless its alignment is known only at run
                        // time, as a trait object's is.
                        let addr = if let ty::Dynamic(..) = self.struct_tail(layout.ty).kind() {
                            let (_, align) = self.size_align_of(layout, p.meta)?;
                            let o = self.b.iconst(PTR, offset.into());
                            let o = self.round_up(o, align);
                            self.b.add(p.addr, o)
                        } else {
                            self.b.offset(p.addr, offset)
                        };
                        PlaceRef { addr, meta: p.meta, layout }
                    }
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
                ProjectionElem::Subslice { from, to, from_end } => {
                    let elem = p.layout.field(&self.cx, 0);
                    let addr = self.b.offset(p.addr, (from * elem.size.bytes()) as i64);
                    if from_end {
                        let layout = p.layout;
                        // A slice: `[from..len - to]`.
                        let len = p.meta.ok_or_else(|| unsupported("a slice without a length"))?;
                        let k = self.b.iconst(PTR, (from + to).into());
                        let meta = self.b.sub(len, k);
                        PlaceRef { addr, meta: Some(meta), layout }
                    } else {
                        // An array: `[from..to]`.
                        let layout = self.layout(Ty::new_array(self.tcx, elem.ty, to - from));
                        PlaceRef { addr, meta: None, layout }
                    }
                }
                ProjectionElem::OpaqueCast(_) => return Err(unsupported("opaque casts")),
            };
        }
        Ok(p)
    }

    fn struct_tail(&self, ty: Ty<'tcx>) -> Ty<'tcx> {
        self.tcx.struct_tail_for_codegen(ty, ty::TypingEnv::fully_monomorphized())
    }

    /// `n` rounded up to a multiple of `align`, a power of two.
    fn round_up(&mut self, n: Value, align: Value) -> Value {
        let one = self.b.iconst(PTR, 1);
        let mask = self.b.sub(align, one);
        let n = self.b.add(n, mask);
        let zero = self.b.iconst(PTR, 0);
        let not_mask = self.b.sub(zero, align);
        self.b.binary(IBin::And, n, not_mask)
    }

    /// The size and alignment of a value of `layout` with metadata `meta`,
    /// as `size_of_val` and `align_of_val` give them.
    fn size_align_of(&mut self, layout: TyAndLayout<'tcx>, meta: Option<Value>) -> Result<(Value, Value)> {
        if !layout.is_unsized() {
            let size = self.b.iconst(PTR, layout.size.bytes().into());
            let align = self.b.iconst(PTR, layout.align.abi.bytes().into());
            return Ok((size, align));
        }
        let meta = meta.ok_or_else(|| unsupported(format!("`{}` without metadata", layout.ty)))?;
        match layout.ty.kind() {
            ty::Slice(_) | ty::Str => {
                let elem = layout.field(&self.cx, 0);
                let k = self.b.iconst(PTR, elem.size.bytes().into());
                let size = self.b.binary(IBin::Mul, meta, k);
                let align = self.b.iconst(PTR, elem.align.abi.bytes().into());
                Ok((size, align))
            }
            ty::Dynamic(..) => {
                // A vtable starts with drop_in_place, size, align.
                let size = self.b.load(PTR, meta, 8);
                let align = self.b.load(PTR, meta, 16);
                Ok((size, align))
            }
            ty::Adt(..) | ty::Tuple(..) => {
                let i = layout.fields.count() - 1;
                let tail = layout.field(&self.cx, i);
                let (tsize, talign) = self.size_align_of(tail, Some(meta))?;
                let offset = self.b.iconst(PTR, layout.fields.offset(i).bytes().into());
                let sized_align = self.b.iconst(PTR, layout.align.abi.bytes().into());
                let wider = self.b.cmp(Cmp::UGt, sized_align, talign);
                let align = self.b.select(wider, sized_align, talign);
                let offset = self.round_up(offset, talign);
                let unrounded = self.b.add(offset, tsize);
                Ok((self.round_up(unrounded, align), align))
            }
            _ => Err(unsupported(format!("the size of `{}`", layout.ty))),
        }
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
            Rvalue::ThreadLocalRef(def_id) => {
                let name = self.m.static_symbol(*def_id)?;
                let addr = self.b.tls_addr(&name);
                self.write(dest, Val { v: V::Scalar(addr), layout: dest.layout })
            }
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
                    UnOp::Neg if float_ty(a.layout.ty).is_some() => {
                        let ft = float_ty(a.layout.ty).expect("a float");
                        let x = self.load_scalar(a)?;
                        V::Scalar(self.b.funary(ir::FUnOp::Neg, ft, x))
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
            CastKind::FloatToInt | CastKind::FloatToFloat | CastKind::IntToFloat => {
                let v = self.operand(op)?;
                let x = self.load_scalar(v)?;
                let from = v.layout.ty;
                let r = match kind {
                    CastKind::FloatToInt => {
                        let ft = float_ty(from).expect("a float");
                        let ty = self.scalar_of(to)?;
                        if ty == Type::I128 {
                            return Err(unsupported("float to 128-bit integer casts"));
                        }
                        self.b.ftoint(ft, ty, matches!(to.ty.kind(), ty::Int(_)), x)
                    }
                    CastKind::IntToFloat => {
                        let ft = float_ty(to.ty).ok_or_else(|| unsupported(format!("casting to `{}`", to.ty)))?;
                        if self.b.value_type(x) == Type::I128 {
                            return Err(unsupported("128-bit integer to float casts"));
                        }
                        self.b.inttof(ft, matches!(from.kind(), ty::Int(_)), x)
                    }
                    _ => {
                        let (a, b) = (float_ty(from).expect("a float"), float_ty(to.ty).expect("a float"));
                        if a == b { x } else { self.b.fconvert(a, b, x) }
                    }
                };
                Ok(scalar(r))
            }
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
                self.m.vtables.push((src, data.principal_def_id()));
                let addr = self.m.alloc_addr(vtable)?;
                let meta = self.addr_value(addr, Size::ZERO);
                Ok(pair(ptr, meta))
            }
            _ => Err(unsupported(format!("unsizing `{src}` to `{dst}`"))),
        }
    }

    fn binop(&mut self, op: BinOp, a: Val<'tcx>, b: Val<'tcx>, dest: TyAndLayout<'tcx>) -> Result<Val<'tcx>> {
        let ty = a.layout.ty;
        if let Some(ft) = float_ty(ty) {
            return self.float_binop(op, ft, a, b, dest);
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

    fn float_binop(
        &mut self,
        op: BinOp,
        ft: ir::FloatTy,
        a: Val<'tcx>,
        b: Val<'tcx>,
        dest: TyAndLayout<'tcx>,
    ) -> Result<Val<'tcx>> {
        let x = self.load_scalar(a)?;
        let y = self.load_scalar(b)?;
        let r = match op {
            BinOp::Add => self.b.fbinary(ir::FBinOp::Add, ft, x, y),
            BinOp::Sub => self.b.fbinary(ir::FBinOp::Sub, ft, x, y),
            BinOp::Mul => self.b.fbinary(ir::FBinOp::Mul, ft, x, y),
            BinOp::Div => self.b.fbinary(ir::FBinOp::Div, ft, x, y),
            BinOp::Rem => self.libm(if ft == ir::FloatTy::F32 { "fmodf" } else { "fmod" }, ft, &[x, y]),
            BinOp::Eq => self.b.fcmp(ir::FCmp::Eq, ft, x, y),
            BinOp::Ne => self.b.fcmp(ir::FCmp::Ne, ft, x, y),
            BinOp::Lt => self.b.fcmp(ir::FCmp::Lt, ft, x, y),
            BinOp::Le => self.b.fcmp(ir::FCmp::Le, ft, x, y),
            BinOp::Gt => self.b.fcmp(ir::FCmp::Gt, ft, x, y),
            BinOp::Ge => self.b.fcmp(ir::FCmp::Ge, ft, x, y),
            _ => return Err(unsupported(format!("`{op:?}` on floats"))),
        };
        Ok(Val { v: V::Scalar(r), layout: dest })
    }

    /// Call a C math library function whose parameters and result are all
    /// `ft`.
    fn libm(&mut self, name: &str, ft: ir::FloatTy, args: &[Value]) -> Value {
        let p = ir::Param { float: true, ..ir::Param::new(ft.bits_type()) };
        let sig = ir::Signature { params: vec![p; args.len()], returns: vec![p] };
        self.m.use_func(name, sig);
        self.b.call(name, args, &[ft.bits_type()])[0]
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
            TerminatorKind::Unreachable => self.b.trap(),
            TerminatorKind::UnwindResume if landing_pads(self.tcx) => {
                let slot = self.exn_slot();
                let a = self.b.slot_addr(slot);
                let exn = self.b.load(PTR, a, 0);
                self.m.use_func("_Unwind_Resume", ir::Signature::new([PTR], []));
                self.b.call("_Unwind_Resume", &[exn], &[]);
                self.b.trap();
            }
            TerminatorKind::UnwindTerminate(reason) if landing_pads(self.tcx) => {
                self.terminate(*reason)?;
            }
            TerminatorKind::UnwindResume | TerminatorKind::UnwindTerminate(_) => self.b.trap(),
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
            TerminatorKind::Assert { cond, expected, msg, target, unwind } => {
                self.pending_unwind = Some(*unwind);
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
            TerminatorKind::Call { func, args, destination, target, fn_span, unwind, .. } => {
                self.pending_unwind = Some(*unwind);
                let si = SourceInfo { span: *fn_span, ..si };
                self.call(si, func, args, *destination, *target)?;
            }
            TerminatorKind::Drop { place, target, unwind, .. } => {
                self.pending_unwind = Some(*unwind);
                let p = self.place(*place)?;
                self.drop(p, *target)?;
            }
            TerminatorKind::TailCall { .. } => return Err(unsupported("tail calls")),
            TerminatorKind::InlineAsm { template, operands, .. } => self.inline_asm(template, operands)?,
            TerminatorKind::Yield { .. }
            | TerminatorKind::CoroutineDrop
            | TerminatorKind::FalseEdge { .. }
            | TerminatorKind::FalseUnwind { .. } => return Err(unsupported("coroutines")),
        }
        self.pending_unwind = None;
        Ok(())
    }

    /// Inline assembly Athrad knows: only `int $0x29` with the code in `ecx`,
    /// Windows' `__fastfail`, which `std::process::abort` uses there. It
    /// becomes a call to two bytes of machine code that never return.
    fn inline_asm(
        &mut self,
        template: &[rustc_ast::InlineAsmTemplatePiece],
        operands: &[mir::InlineAsmOperand<'tcx>],
    ) -> Result<()> {
        let text: String = template.iter().map(|p| p.to_string()).collect();
        let target = &self.tcx.sess.target;
        let windows_x64 = target.is_like_windows && target.arch == rustc_target::spec::Arch::X86_64;
        if windows_x64
            && text.replace('$', "").trim() == "int 0x29"
            && let [mir::InlineAsmOperand::In { reg, value }] = operands
            && format!("{reg}").contains("cx")
        {
            let v = self.operand(value)?;
            let code = self.load_scalar(v)?;
            let code = self.b.resize(Type::I32, code, false);
            let name = "__athrad_fastfail";
            if !self.m.module.raw.iter().any(|f| f.name == name) {
                // `int 0x29` (the code is already in ecx, the first
                // argument's register), then `ud2`.
                self.m.module.raw.push(ir::RawFunction {
                    name: name.into(),
                    sig: ir::Signature::new([Type::I32], []),
                    bytes: vec![0xcd, 0x29, 0x0f, 0x0b],
                });
            }
            self.b.call(name, &[code], &[]);
            self.b.trap();
            return Ok(());
        }
        Err(unsupported(format!("inline assembly (`{text}`)")))
    }

    fn exn_slot(&mut self) -> ir::Slot {
        match self.exn_slot {
            Some(s) => s,
            None => {
                let s = self.b.slot(8, 8);
                self.exn_slot = Some(s);
                s
            }
        }
    }

    /// Abort, as unwinding out of where it may not (a function that cannot
    /// unwind, or a destructor during unwinding) does: `core`'s nounwind
    /// panic for the reason.
    fn terminate(&mut self, reason: mir::UnwindTerminateReason) -> Result<()> {
        let item = match reason {
            mir::UnwindTerminateReason::Abi => LangItem::PanicCannotUnwind,
            mir::UnwindTerminateReason::InCleanup => LangItem::PanicInCleanup,
        };
        let def_id = self.tcx.require_lang_item(item, rustc_span::DUMMY_SP);
        let inst = Instance::mono(self.tcx, def_id);
        let abi = self.cx.fn_abi_of_instance(inst, ty::List::empty());
        let (name, _) = self.m.func_symbol(inst)?;
        self.pending_unwind = None;
        self.emit_call(Callee::Direct(name), abi, Vec::new(), None)?;
        if !self.b.is_terminated() {
            self.b.trap();
        }
        Ok(())
    }

    /// Call, with a landing pad when the current terminator's unwind action
    /// asks for one: to its cleanup block, or to abort. Returns the results.
    fn call_unwinding(&mut self, callee: ir::Callee, sig: ir::Signature, args: &[Value]) -> Result<Vec<Value>> {
        let action = self.pending_unwind.take();
        let pad = match action {
            Some(a @ (mir::UnwindAction::Cleanup(_) | mir::UnwindAction::Terminate(_))) if landing_pads(self.tcx) => a,
            _ => {
                return Ok(match callee {
                    ir::Callee::Direct(name) => {
                        let rets = sig.return_types();
                        self.m.use_func(&name, sig);
                        self.b.call(&name, args, &rets)
                    }
                    ir::Callee::Indirect(sig, ptr) => self.b.call_indirect(sig, ptr, args),
                });
            }
        };
        if let ir::Callee::Direct(name) = &callee {
            self.m.use_func(name, sig.clone());
        }
        let (normal, results) = self.b.block(&sig.return_types());
        let (unwind, exn) = self.b.block(&[PTR]);
        self.b.try_call(callee, args, normal, unwind, false);
        self.b.switch_to(unwind);
        match pad {
            mir::UnwindAction::Cleanup(bb) => {
                let slot = self.exn_slot();
                let a = self.b.slot_addr(slot);
                self.b.store(a, 0, exn[0]);
                let t = self.block(bb);
                self.b.jump(t, &[]);
            }
            mir::UnwindAction::Terminate(reason) => self.terminate(reason)?,
            _ => unreachable!("matched above"),
        }
        self.b.switch_to(normal);
        Ok(results)
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
        self.call_unwinding(ir::Callee::Direct(name), ir::Signature::new(params, []), &args)?;
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
            // The vtable's first entry is the concrete type's drop glue, or
            // null when it has none.
            let vtable = p.meta.ok_or_else(|| unsupported("a trait object without a vtable"))?;
            let glue = self.b.load(PTR, vtable, 0);
            let zero = self.b.iconst(PTR, 0);
            let has_glue = self.b.cmp(Cmp::Ne, glue, zero);
            let (call, _) = self.b.block(&[]);
            self.b.branch(has_glue, (call, &[]), (t, &[]));
            self.b.switch_to(call);
            let sig = ir::Signature::new([PTR], []);
            self.call_unwinding(ir::Callee::Indirect(sig.clone(), glue), sig, &[p.addr])?;
            self.b.jump(t, &[]);
            return Ok(());
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
                    if self.intrinsic(inst, si, args, dest, target)? {
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
        inst: Instance<'tcx>,
        si: SourceInfo,
        args: &[rustc_span::Spanned<Operand<'tcx>>],
        dest: PlaceRef<'tcx>,
        target: Option<BasicBlock>,
    ) -> Result<bool> {
        let def = self.tcx.intrinsic(inst.def_id()).expect("an intrinsic");
        let name = def.name.as_str();
        let tcx = self.tcx;
        let arg_ty = |f: &Self, i: usize| f.monomorphize(args[i].node.ty(f.mir, tcx));
        let pointee = |f: &Self, i: usize| -> Result<TyAndLayout<'tcx>> {
            let t = arg_ty(f, i);
            let p = t.builtin_deref(true).ok_or_else(|| unsupported(format!("`{name}` of `{t}`")))?;
            Ok(f.layout(p))
        };
        let signed_arg = |f: &Self, i: usize| matches!(arg_ty(f, i).kind(), ty::Int(_));
        // Write a scalar result, resized to the destination's type.
        let put = |f: &mut Self, v: Value, signed: bool| -> Result<()> {
            if dest.layout.size.bytes() == 0 {
                return Ok(());
            }
            let ty = f.scalar_of(dest.layout)?;
            let v = f.b.resize(ty, v, signed);
            f.write(dest, Val { v: V::Scalar(v), layout: dest.layout })
        };
        let mem = |f: &mut Self, func: &str, params: &[Type], ret: Type| {
            f.m.use_func(func, ir::Signature::new(params.iter().copied(), [ret]));
        };

        match name {
            "black_box" => {
                let v = self.operand(&args[0].node)?;
                self.write(dest, v)?;
            }
            "assume" | "cold_path" => {}
            "abort" | "unreachable" | "breakpoint" => {
                self.b.trap();
                return Ok(true);
            }
            "assert_inhabited" | "assert_zero_valid" | "assert_mem_uninitialized_valid" => {
                use rustc_middle::ty::layout::ValidityRequirement;
                let req = ValidityRequirement::from_intrinsic(def.name).expect("an assert intrinsic");
                let ty = inst.args.type_at(0);
                let env = ty::TypingEnv::fully_monomorphized();
                if !tcx.check_validity_requirement((req, env.as_query_input(ty))).unwrap_or(false) {
                    return Err(unsupported(format!("`{name}::<{ty}>`, which panics")));
                }
            }
            "ctpop" | "ctlz" | "ctlz_nonzero" | "cttz" | "cttz_nonzero" | "bswap" | "bitreverse" => {
                let a = self.operand(&args[0].node)?;
                let x = self.load_scalar(a)?;
                let op = match name {
                    "ctpop" => ir::UnOp::Popcnt,
                    "ctlz" | "ctlz_nonzero" => ir::UnOp::Clz,
                    "cttz" | "cttz_nonzero" => ir::UnOp::Ctz,
                    "bswap" => ir::UnOp::Bswap,
                    _ => ir::UnOp::Bitrev,
                };
                let r = self.b.unary(op, x);
                put(self, r, false)?;
            }
            "rotate_left" | "rotate_right" => {
                let (a, b) = (self.operand(&args[0].node)?, self.operand(&args[1].node)?);
                let x = self.load_scalar(a)?;
                let n = self.load_scalar(b)?;
                let n = self.b.resize(self.b.value_type(x), n, false);
                let op = if name == "rotate_left" { IBin::Rotl } else { IBin::Rotr };
                let r = self.b.binary(op, x, n);
                put(self, r, false)?;
            }
            "saturating_add" | "saturating_sub" => {
                let signed = signed_arg(self, 0);
                let (a, b) = (self.operand(&args[0].node)?, self.operand(&args[1].node)?);
                let x = self.load_scalar(a)?;
                let y = self.load_scalar(b)?;
                let ty = self.b.value_type(x);

                let add = name == "saturating_add";
                let (r, of) = self.b.overflow(if add { OverflowOp::Add } else { OverflowOp::Sub }, signed, x, y);
                let bits = ty.bits();
                let clamp = if signed {
                    // Overflow goes toward the sign of y (add) or away from it (sub).
                    let max = self.b.iconst(ty, (1i128 << (bits - 1)) - 1);
                    let min = self.b.iconst(ty, -(1i128 << (bits - 1)));
                    let zero = self.b.iconst(ty, 0);
                    let y_neg = self.b.cmp(Cmp::SLt, y, zero);
                    if add { self.b.select(y_neg, min, max) } else { self.b.select(y_neg, max, min) }
                } else if add {
                    self.b.iconst(ty, -1)
                } else {
                    self.b.iconst(ty, 0)
                };
                let v = self.b.select(of, clamp, r);
                put(self, v, signed)?;
            }
            "exact_div" | "unchecked_div" | "unchecked_rem" | "unchecked_add" | "unchecked_sub" | "unchecked_mul"
            | "wrapping_add" | "wrapping_sub" | "wrapping_mul" | "unchecked_shl" | "unchecked_shr"
            | "add_with_overflow" | "sub_with_overflow" | "mul_with_overflow" | "three_way_compare" => {
                let op = match name {
                    "exact_div" | "unchecked_div" => BinOp::Div,
                    "unchecked_rem" => BinOp::Rem,
                    "unchecked_add" | "wrapping_add" => BinOp::Add,
                    "unchecked_sub" | "wrapping_sub" => BinOp::Sub,
                    "unchecked_mul" | "wrapping_mul" => BinOp::Mul,
                    "unchecked_shl" => BinOp::Shl,
                    "unchecked_shr" => BinOp::Shr,
                    "add_with_overflow" => BinOp::AddWithOverflow,
                    "sub_with_overflow" => BinOp::SubWithOverflow,
                    "mul_with_overflow" => BinOp::MulWithOverflow,
                    _ => BinOp::Cmp,
                };
                let (a, b) = (self.operand(&args[0].node)?, self.operand(&args[1].node)?);
                let v = self.binop(op, a, b, dest.layout)?;
                self.write(dest, v)?;
            }
            "size_of_val" | "align_of_val" => {
                let layout = pointee(self, 0)?;
                let a = self.operand(&args[0].node)?;
                let meta = match a.v {
                    V::Pair(..) => Some(self.load_pair(a)?.1),
                    _ => None,
                };
                let (size, align) = self.size_align_of(layout, meta)?;
                put(self, if name == "size_of_val" { size } else { align }, false)?;
            }
            "vtable_size" | "vtable_align" => {
                let a = self.operand(&args[0].node)?;
                let vt = self.load_scalar(a)?;
                let v = self.b.load(PTR, vt, if name == "vtable_size" { 8 } else { 16 });
                put(self, v, false)?;
            }
            "write_bytes" | "volatile_set_memory" => {
                let elem = pointee(self, 0)?;
                let d = self.operand(&args[0].node)?;
                let d = self.load_scalar(d)?;
                let v = self.operand(&args[1].node)?;
                let v = self.load_scalar(v)?;
                let v = self.b.resize(Type::I32, v, false);
                let n = self.operand(&args[2].node)?;
                let n = self.load_scalar(n)?;
                let k = self.b.iconst(PTR, elem.size.bytes().into());
                let bytes = self.b.binary(IBin::Mul, n, k);
                mem(self, "memset", &[PTR, Type::I32, PTR], PTR);
                self.b.call("memset", &[d, v, bytes], &[PTR]);
            }
            "copy" | "copy_nonoverlapping" | "volatile_copy_memory" | "volatile_copy_nonoverlapping_memory" => {
                let elem = pointee(self, 0)?;
                // `copy(src, dst, n)`; the volatile ones are `(dst, src, n)`.
                let (si_, di) = if name.starts_with("volatile") { (1, 0) } else { (0, 1) };
                let src = self.operand(&args[si_].node)?;
                let src = self.load_scalar(src)?;
                let dst = self.operand(&args[di].node)?;
                let dst = self.load_scalar(dst)?;
                let n = self.operand(&args[2].node)?;
                let n = self.load_scalar(n)?;
                let k = self.b.iconst(PTR, elem.size.bytes().into());
                let bytes = self.b.binary(IBin::Mul, n, k);
                mem(self, "memmove", &[PTR, PTR, PTR], PTR);
                self.b.call("memmove", &[dst, src, bytes], &[PTR]);
            }
            "volatile_load" | "unaligned_volatile_load" => {
                let layout = pointee(self, 0)?;
                let a = self.operand(&args[0].node)?;
                let addr = self.load_scalar(a)?;
                let v = self.read(PlaceRef { addr, meta: None, layout })?;
                self.write(dest, v)?;
            }
            "volatile_store" | "unaligned_volatile_store" | "nontemporal_store" => {
                let layout = pointee(self, 0)?;
                let a = self.operand(&args[0].node)?;
                let addr = self.load_scalar(a)?;
                let v = self.operand(&args[1].node)?;
                self.write(PlaceRef { addr, meta: None, layout }, v)?;
            }
            "compare_bytes" => {
                let mut vs = Vec::new();
                for a in &args[..3] {
                    let v = self.operand(&a.node)?;
                    vs.push(self.load_scalar(v)?);
                }
                mem(self, "memcmp", &[PTR, PTR, PTR], Type::I32);
                let r = self.b.call("memcmp", &vs, &[Type::I32])[0];
                put(self, r, true)?;
            }
            "raw_eq" => {
                let layout = pointee(self, 0)?;
                let a = self.operand(&args[0].node)?;
                let a = self.load_scalar(a)?;
                let b = self.operand(&args[1].node)?;
                let b = self.load_scalar(b)?;
                let n = self.b.iconst(PTR, layout.size.bytes().into());
                mem(self, "memcmp", &[PTR, PTR, PTR], Type::I32);
                let r = self.b.call("memcmp", &[a, b, n], &[Type::I32])[0];
                let zero = self.b.iconst(Type::I32, 0);
                let eq = self.b.cmp(Cmp::Eq, r, zero);
                put(self, eq, false)?;
            }
            "ptr_offset_from" | "ptr_offset_from_unsigned" => {
                let elem = pointee(self, 0)?;
                let a = self.operand(&args[0].node)?;
                let a = self.load_scalar(a)?;
                let b = self.operand(&args[1].node)?;
                let b = self.load_scalar(b)?;
                let d = self.b.sub(a, b);
                let k = self.b.iconst(PTR, elem.size.bytes().max(1).into());
                let op = if name == "ptr_offset_from" { IBin::SDiv } else { IBin::UDiv };
                let r = self.b.binary(op, d, k);
                put(self, r, true)?;
            }
            "arith_offset" => {
                let elem = pointee(self, 0)?;
                let p = self.operand(&args[0].node)?;
                let p = self.load_scalar(p)?;
                let n = self.operand(&args[1].node)?;
                let n = self.load_scalar(n)?;
                let k = self.b.iconst(PTR, elem.size.bytes().into());
                let o = self.b.binary(IBin::Mul, n, k);
                let r = self.b.add(p, o);
                put(self, r, false)?;
            }
            "ptr_mask" => {
                let p = self.operand(&args[0].node)?;
                let p = self.load_scalar(p)?;
                let m = self.operand(&args[1].node)?;
                let m = self.load_scalar(m)?;
                let r = self.b.binary(IBin::And, p, m);
                put(self, r, false)?;
            }
            "discriminant_value" => {
                let layout = pointee(self, 0)?;
                let a = self.operand(&args[0].node)?;
                let addr = self.load_scalar(a)?;
                let d = self.discriminant(PlaceRef { addr, meta: None, layout }, dest.layout)?;
                put(self, d, false)?;
            }
            "catch_unwind" if !landing_pads(tcx) => return Err(unsupported("catching panics on this target")),
            "catch_unwind" => {
                // `try_fn(data)`; if it unwinds, `catch_fn(data, exception)`.
                let mut v = Vec::new();
                for a in &args[..3] {
                    let x = self.operand(&a.node)?;
                    v.push(self.load_scalar(x)?);
                }
                let (try_fn, data, catch_fn) = (v[0], v[1], v[2]);
                let (normal, _) = self.b.block(&[]);
                let (caught, exn) = self.b.block(&[PTR]);
                let (join, r) = self.b.block(&[Type::I32]);
                let sig = ir::Signature::new([PTR], []);
                self.b.try_call(ir::Callee::Indirect(sig, try_fn), &[data], normal, caught, true);
                self.b.switch_to(normal);
                let zero = self.b.iconst(Type::I32, 0);
                self.b.jump(join, &[zero]);
                self.b.switch_to(caught);
                self.b.call_indirect(ir::Signature::new([PTR, PTR], []), catch_fn, &[data, exn[0]]);
                let one = self.b.iconst(Type::I32, 1);
                self.b.jump(join, &[one]);
                self.b.switch_to(join);
                put(self, r[0], false)?;
            }
            _ if name.starts_with("simd_") => {
                if !self.simd_intrinsic(name, inst, args, dest)? {
                    return Err(unsupported(format!("the `{name}` intrinsic")));
                }
            }
            // Atomics. Cranelift's are all sequentially consistent, as
            // strong as any ordering the program asks for.
            "atomic_fence" | "atomic_singlethreadfence" => self.b.fence(),
            "atomic_load" => {
                let layout = pointee(self, 0)?;
                let ty = self.atomic_type(layout)?;
                let p = self.operand(&args[0].node)?;
                let p = self.load_scalar(p)?;
                let v = self.b.atomic_load(ty, p);
                put(self, v, false)?;
            }
            "atomic_store" => {
                let layout = pointee(self, 0)?;
                self.atomic_type(layout)?;
                let p = self.operand(&args[0].node)?;
                let p = self.load_scalar(p)?;
                let v = self.operand(&args[1].node)?;
                let v = self.load_scalar(v)?;
                self.b.atomic_store(p, v);
            }
            "atomic_xchg" | "atomic_xadd" | "atomic_xsub" | "atomic_and" | "atomic_nand" | "atomic_or"
            | "atomic_xor" | "atomic_max" | "atomic_min" | "atomic_umax" | "atomic_umin" => {
                let layout = pointee(self, 0)?;
                let ty = self.atomic_type(layout)?;
                let op = match name {
                    "atomic_xchg" => ir::RmwOp::Xchg,
                    "atomic_xadd" => ir::RmwOp::Add,
                    "atomic_xsub" => ir::RmwOp::Sub,
                    "atomic_and" => ir::RmwOp::And,
                    "atomic_nand" => ir::RmwOp::Nand,
                    "atomic_or" => ir::RmwOp::Or,
                    "atomic_xor" => ir::RmwOp::Xor,
                    "atomic_max" => ir::RmwOp::Smax,
                    "atomic_min" => ir::RmwOp::Smin,
                    "atomic_umax" => ir::RmwOp::Umax,
                    _ => ir::RmwOp::Umin,
                };
                let p = self.operand(&args[0].node)?;
                let p = self.load_scalar(p)?;
                // The operand may be another type of the same width (`usize`
                // for an `AtomicPtr`'s byte offset).
                let v = self.operand(&args[1].node)?;
                let v = self.load_scalar(v)?;
                let v = self.b.resize(ty, v, false);
                let old = self.b.atomic_rmw(op, p, v);
                put(self, old, false)?;
            }
            "atomic_cxchg" | "atomic_cxchgweak" => {
                let layout = pointee(self, 0)?;
                let ty = self.atomic_type(layout)?;
                let p = self.operand(&args[0].node)?;
                let p = self.load_scalar(p)?;
                let e = self.operand(&args[1].node)?;
                let e = self.load_scalar(e)?;
                let n = self.operand(&args[2].node)?;
                let n = self.load_scalar(n)?;
                let (e, n) = (self.b.resize(ty, e, false), self.b.resize(ty, n, false));
                let old = self.b.atomic_cas(p, e, n);
                let ok = self.b.cmp(Cmp::Eq, old, e);
                self.write(dest, Val { v: V::Pair(old, ok), layout: dest.layout })?;
            }
            _ if float_intrinsic(name).is_some() => {
                let (op, ft) = float_intrinsic(name).expect("a float intrinsic");
                let ft = ft.or_else(|| float_ty(inst.args.type_at(0)));
                let ft = ft.ok_or_else(|| unsupported(format!("the `{name}` intrinsic on this type")))?;
                let mut xs = Vec::new();
                for a in args {
                    let v = self.operand(&a.node)?;
                    xs.push(self.load_scalar(v)?);
                }
                let f32 = ft == ir::FloatTy::F32;
                let r = match op {
                    FloatIntrinsic::Un(op) => self.b.funary(op, ft, xs[0]),
                    FloatIntrinsic::Bin(op) => self.b.fbinary(op, ft, xs[0], xs[1]),
                    FloatIntrinsic::MinMaxNum(max) => {
                        // The other operand when one is NaN.
                        let m = self.b.fbinary(if max { ir::FBinOp::Max } else { ir::FBinOp::Min }, ft, xs[0], xs[1]);
                        let x_nan = self.b.fcmp(ir::FCmp::Uno, ft, xs[0], xs[0]);
                        let y_nan = self.b.fcmp(ir::FCmp::Uno, ft, xs[1], xs[1]);
                        let m = self.b.select(y_nan, xs[0], m);
                        self.b.select(x_nan, xs[1], m)
                    }
                    FloatIntrinsic::Libm(base) => {
                        let name = if f32 { format!("{base}f") } else { base.to_owned() };
                        self.libm(&name, ft, &xs)
                    }
                    FloatIntrinsic::Powi => {
                        let name = if f32 { "__powisf2" } else { "__powidf2" };
                        let fp = ir::Param { float: true, ..ir::Param::new(ft.bits_type()) };
                        let sig = ir::Signature { params: vec![fp, ir::Param::new(Type::I32)], returns: vec![fp] };
                        self.m.use_func(name, sig);
                        self.b.call(name, &xs, &[ft.bits_type()])[0]
                    }
                    FloatIntrinsic::ToInt => {
                        let ty = self.scalar_of(dest.layout)?;
                        self.b.ftoint(ft, ty, matches!(dest.layout.ty.kind(), ty::Int(_)), xs[0])
                    }
                };
                put(self, r, false)?;
            }
            // Whether the crate is built with debug assertions in std
            // (`ub_checks`), overflow checks, and contract checks: the
            // session says, as it does for rustc's backends.
            "ub_checks" | "overflow_checks" | "contract_checks" => {
                let on = match name {
                    "ub_checks" => tcx.sess.ub_checks(),
                    "overflow_checks" => tcx.sess.overflow_checks(),
                    _ => tcx.sess.contract_checks(),
                };
                let v = self.b.iconst(Type::I8, i128::from(on));
                put(self, v, false)?;
            }
            // Intrinsics that are constants once their types are known.
            _ if args.is_empty()
                && matches!(
                    name,
                    "type_name" | "type_id" | "needs_drop" | "variant_count" | "size_of" | "align_of" | "pref_align_of"
                ) =>
            {
                let env = ty::TypingEnv::fully_monomorphized();
                let value = tcx
                    .const_eval_instance(env, inst, si.span)
                    .map_err(|_| unsupported(format!("the `{name}` intrinsic")))?;
                let v = self.const_value(value, dest.layout.ty)?;
                self.write(dest, v)?;
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

    /// The signature of one call to a C-variadic function: the fixed
    /// parameters as usual, then each variadic argument as a 64-bit integer
    /// (C promotes narrower ones; Athrad passes no floats). On Apple's
    /// aarch64 variadic arguments go on the stack, so the integer registers
    /// before them are filled with zeros, as `rustc_codegen_cranelift` does.
    fn variadic_signature(
        &mut self,
        abi: &FnAbi<'tcx, Ty<'tcx>>,
        call_args: &mut Vec<Value>,
        fixed_end: usize,
    ) -> Result<ir::Signature> {
        if abi.conv != rustc_abi::CanonAbi::C {
            return Err(unsupported("variadic calls outside the C ABI"));
        }
        let mut sig = ir::Signature::default();
        match &abi.ret.mode {
            PassMode::Indirect { .. } => sig.params.push(ir::Param { sret: true, ..ir::Param::new(PTR) }),
            _ => sig.returns = crate::cx::arg_params(self.cx, &abi.ret)?,
        }
        for arg in &abi.args[..abi.fixed_count as usize] {
            sig.params.extend(crate::cx::arg_params(self.cx, arg)?);
        }
        if sig.params.iter().any(|p| p.float) {
            return Err(unsupported("variadic calls with float parameters"));
        }
        let mut extra = Vec::new();
        for (arg, &v) in abi.args[abi.fixed_count as usize..].iter().zip(&call_args[fixed_end..]) {
            if !matches!(arg.mode, PassMode::Direct(_))
                || !matches!(arg.layout.ty.kind(), ty::Int(_) | ty::Uint(_) | ty::RawPtr(..) | ty::Ref(..))
            {
                return Err(unsupported(format!("passing `{}` to a variadic function", arg.layout.ty)));
            }
            let signed = matches!(arg.layout.ty.kind(), ty::Int(_));
            if self.b.value_type(v) == Type::I128 {
                return Err(unsupported("passing a 128-bit integer to a variadic function"));
            }
            extra.push(self.b.resize(Type::I64, v, signed));
        }
        call_args.truncate(fixed_end);
        let target = &self.tcx.sess.target;
        if !extra.is_empty() && target.is_like_darwin && target.arch == rustc_target::spec::Arch::AArch64 {
            let sret = usize::from(sig.params.first().is_some_and(|p| p.sret));
            let used: usize = sig.params[sret..].iter().map(|p| if p.ty == Type::I128 { 2 } else { 1 }).sum();
            for _ in used..8 {
                sig.params.push(ir::Param::new(Type::I64));
                call_args.push(self.b.iconst(Type::I64, 0));
            }
        }
        for v in extra {
            sig.params.push(ir::Param::new(Type::I64));
            call_args.push(v);
        }
        Ok(sig)
    }

    /// The IR type of an atomic's value: an integer or pointer up to 64
    /// bits.
    fn atomic_type(&self, layout: TyAndLayout<'tcx>) -> Result<Type> {
        match layout.ty.kind() {
            ty::Int(_) | ty::Uint(_) | ty::RawPtr(..) | ty::Bool if layout.size.bytes() <= 8 => self.scalar_of(layout),
            _ => Err(unsupported(format!("atomics on `{}`", layout.ty))),
        }
    }

    fn emit_call(
        &mut self,
        callee: Callee,
        abi: &FnAbi<'tcx, Ty<'tcx>>,
        vals: Vec<Val<'tcx>>,
        dest: Option<PlaceRef<'tcx>>,
    ) -> Result<()> {
        assert_eq!(abi.args.len(), vals.len(), "one value per ABI argument");
        let variadic = abi.c_variadic;
        let mut sig = if variadic { ir::Signature::default() } else { signature(self.cx, abi)? };
        let mut call_args = Vec::new();
        let mut fixed_end = None;

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
            if variadic && i == abi.fixed_count as usize {
                fixed_end = Some(call_args.len());
            }
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

        let mut callee = callee;
        if variadic {
            let fixed_end = fixed_end.unwrap_or(call_args.len());
            sig = self.variadic_signature(abi, &mut call_args, fixed_end)?;
            // Each call site's signature is its own; call through the
            // symbol's address, declared with the fixed parameters alone.
            if let Callee::Direct(name) = &callee {
                let mut fixed = sig.clone();
                fixed.params.truncate(fixed_end);
                self.m.use_func(name, fixed);
                let addr = self.b.symbol_addr(name);
                callee = Callee::Indirect(addr);
            }
        }

        let ir_callee = match callee {
            Callee::Direct(name) => ir::Callee::Direct(name),
            Callee::Indirect(ptr) => ir::Callee::Indirect(sig.clone(), ptr),
            Callee::Virtual(_) => ir::Callee::Indirect(sig.clone(), fn_ptr.expect("a receiver")),
        };
        let results = self.call_unwinding(ir_callee, sig, &call_args)?;

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

/// Whether Athrad runs cleanups and catches panics on this target: with
/// DWARF unwinding (ELF and Mach-O). On Windows, where unwinding is SEH with
/// funclets Cranelift cannot express, cleanups are skipped where that
/// cannot be seen and programs are rejected otherwise (`unwind.rs`).
pub(crate) fn landing_pads(tcx: TyCtxt<'_>) -> bool {
    !tcx.sess.target.is_like_windows
}

/// The float format of a type, if it is `f32` or `f64`.
fn float_ty(ty: Ty<'_>) -> Option<ir::FloatTy> {
    match ty.kind() {
        ty::Float(ty::FloatTy::F32) => Some(ir::FloatTy::F32),
        ty::Float(ty::FloatTy::F64) => Some(ir::FloatTy::F64),
        _ => None,
    }
}

enum FloatIntrinsic {
    Un(ir::FUnOp),
    Bin(ir::FBinOp),
    /// `minimum_number_nsz` and `maximum_number_nsz`: a NaN loses.
    MinMaxNum(bool),
    /// The C math library function of this name, with an `f` for `f32`.
    Libm(&'static str),
    Powi,
    ToInt,
}

/// The float intrinsics, and their format when the name says it
/// (`sqrtf64`); `None` takes it from the type argument (`fabs::<f32>`).
fn float_intrinsic(name: &str) -> Option<(FloatIntrinsic, Option<ir::FloatTy>)> {
    use FloatIntrinsic::*;
    let (base, ft) = if let Some(b) = name.strip_suffix("f32").or_else(|| name.strip_suffix("_f32")) {
        (b, Some(ir::FloatTy::F32))
    } else if let Some(b) = name.strip_suffix("f64").or_else(|| name.strip_suffix("_f64")) {
        (b, Some(ir::FloatTy::F64))
    } else {
        (name, None)
    };
    let base = base.strip_suffix('_').unwrap_or(base);
    let op = match base {
        "sqrt" => Un(ir::FUnOp::Sqrt),
        "floor" => Un(ir::FUnOp::Floor),
        "ceil" => Un(ir::FUnOp::Ceil),
        "trunc" => Un(ir::FUnOp::Trunc),
        "round_ties_even" => Un(ir::FUnOp::Nearest),
        "fabs" => Un(ir::FUnOp::Abs),
        "copysign" => Bin(ir::FBinOp::Copysign),
        "minimum" => Bin(ir::FBinOp::Min),
        "maximum" => Bin(ir::FBinOp::Max),
        "minimum_number_nsz" => MinMaxNum(false),
        "maximum_number_nsz" => MinMaxNum(true),
        "fadd_fast" | "fadd_algebraic" => Bin(ir::FBinOp::Add),
        "fsub_fast" | "fsub_algebraic" => Bin(ir::FBinOp::Sub),
        "fmul_fast" | "fmul_algebraic" => Bin(ir::FBinOp::Mul),
        "fdiv_fast" | "fdiv_algebraic" => Bin(ir::FBinOp::Div),
        "frem_fast" | "frem_algebraic" => Libm("fmod"),
        "round" => Libm("round"),
        "pow" => Libm("pow"),
        "exp" => Libm("exp"),
        "exp2" => Libm("exp2"),
        "log" => Libm("log"),
        "log2" => Libm("log2"),
        "log10" => Libm("log10"),
        "sin" => Libm("sin"),
        "cos" => Libm("cos"),
        "fma" | "fmuladd" => Libm("fma"),
        "powi" => Powi,
        "float_to_int_unchecked" => ToInt,
        _ => return None,
    };
    // The fast and algebraic ones, and `fabs`, `float_to_int_unchecked`,
    // take their format from the type argument.
    Some((op, ft))
}
