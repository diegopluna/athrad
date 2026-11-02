//! `simd_*` intrinsics, lane by lane. A vector lives in memory, its lanes at
//! consecutive offsets; each operation loads lanes, computes with scalar IR,
//! and stores lanes. Slow, and exactly what `rustc`'s code computes, which
//! is what the dev loop needs. `std::arch` and `core::simd` reach these
//! through `hashbrown`, `memchr`, and others.

use rustc_middle::mir::Operand;
use rustc_middle::ty::{self, Ty};

use ir::{BinOp as IBin, Cmp, FBinOp, FCmp, FUnOp, FloatTy, OverflowOp, PTR, Type, UnOp, Value};

use super::{FnCx, PlaceRef, V, Val, float_ty};
use crate::{Result, unsupported};

/// What a lane is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Signed,
    Unsigned,
    Float(FloatTy),
}

#[derive(Clone, Copy)]
pub(super) struct Lanes {
    pub n: u64,
    pub size: u64,
    pub ty: Type,
    pub kind: Kind,
}

impl<'tcx> FnCx<'_, 'tcx> {
    pub(super) fn lanes(&self, ty: Ty<'tcx>) -> Result<Lanes> {
        if !ty.is_simd() {
            return Err(unsupported(format!("`{ty}` as a SIMD vector")));
        }
        let (n, elem) = ty.simd_size_and_type(self.tcx);
        let layout = self.layout(elem);
        let kind = match elem.kind() {
            ty::Int(_) => Kind::Signed,
            ty::Uint(_) | ty::RawPtr(..) => Kind::Unsigned,
            ty::Float(_) => Kind::Float(float_ty(elem).ok_or_else(|| unsupported(format!("lanes of `{elem}`")))?),
            _ => return Err(unsupported(format!("lanes of `{elem}`"))),
        };
        let ty = self.scalar_of(layout)?;
        Ok(Lanes { n, size: layout.size.bytes(), ty, kind })
    }

    fn lane(&mut self, base: Value, l: Lanes, i: u64) -> Value {
        self.b.load(l.ty, base, (i * l.size) as i32)
    }

    fn set_lane(&mut self, base: Value, l: Lanes, i: u64, v: Value) {
        self.b.store(base, (i * l.size) as i32, v);
    }

    fn vector_arg(&mut self, op: &Operand<'tcx>) -> Result<(Value, Lanes)> {
        let ty = self.monomorphize(op.ty(self.mir, self.tcx));
        let l = self.lanes(ty)?;
        let v = self.operand(op)?;
        Ok((self.spill(v)?, l))
    }

    fn scalar_arg(&mut self, op: &Operand<'tcx>) -> Result<Value> {
        let v = self.operand(op)?;
        self.load_scalar(v)
    }

    /// All ones or all zeros, of type `t`, from a 0-or-1 `I8`.
    fn mask_of(&mut self, bit: Value, t: Type) -> Value {
        let w = self.b.resize(t, bit, false);
        let zero = self.b.iconst(t, 0);
        self.b.sub(zero, w)
    }

    /// Lower the `simd_*` intrinsic `name`; `false` if Athrad does not
    /// know it.
    pub(super) fn simd_intrinsic(
        &mut self,
        name: &str,
        inst: ty::Instance<'tcx>,
        args: &[rustc_span::Spanned<Operand<'tcx>>],
        dest: PlaceRef<'tcx>,
    ) -> Result<bool> {
        let d = dest.addr;
        match name {
            "simd_add"
            | "simd_sub"
            | "simd_mul"
            | "simd_div"
            | "simd_rem"
            | "simd_shl"
            | "simd_shr"
            | "simd_and"
            | "simd_or"
            | "simd_xor"
            | "simd_saturating_add"
            | "simd_saturating_sub"
            | "simd_minimum_number_nsz"
            | "simd_maximum_number_nsz"
            | "simd_fmin"
            | "simd_fmax" => {
                let (a, l) = self.vector_arg(&args[0].node)?;
                let (b, _) = self.vector_arg(&args[1].node)?;
                for i in 0..l.n {
                    let x = self.lane(a, l, i);
                    let y = self.lane(b, l, i);
                    let r = self.lane_binary(name, l, x, y)?;
                    self.set_lane(d, l, i, r);
                }
            }
            "simd_eq" | "simd_ne" | "simd_lt" | "simd_le" | "simd_gt" | "simd_ge" => {
                let (a, l) = self.vector_arg(&args[0].node)?;
                let (b, _) = self.vector_arg(&args[1].node)?;
                let out = self.lanes(dest.layout.ty)?;
                for i in 0..l.n {
                    let x = self.lane(a, l, i);
                    let y = self.lane(b, l, i);
                    let bit = match l.kind {
                        Kind::Float(ft) => {
                            let c = match name {
                                "simd_eq" => FCmp::Eq,
                                "simd_ne" => FCmp::Ne,
                                "simd_lt" => FCmp::Lt,
                                "simd_le" => FCmp::Le,
                                "simd_gt" => FCmp::Gt,
                                _ => FCmp::Ge,
                            };
                            self.b.fcmp(c, ft, x, y)
                        }
                        k => {
                            let s = k == Kind::Signed;
                            let c = match name {
                                "simd_eq" => Cmp::Eq,
                                "simd_ne" => Cmp::Ne,
                                "simd_lt" if s => Cmp::SLt,
                                "simd_lt" => Cmp::ULt,
                                "simd_le" if s => Cmp::SLe,
                                "simd_le" => Cmp::ULe,
                                "simd_gt" if s => Cmp::SGt,
                                "simd_gt" => Cmp::UGt,
                                _ if s => Cmp::SGe,
                                _ => Cmp::UGe,
                            };
                            self.b.cmp(c, x, y)
                        }
                    };
                    let m = self.mask_of(bit, out.ty);
                    self.set_lane(d, out, i, m);
                }
            }
            "simd_neg"
            | "simd_fabs"
            | "simd_fsqrt"
            | "simd_floor"
            | "simd_ceil"
            | "simd_trunc"
            | "simd_round_ties_even"
            | "simd_bswap"
            | "simd_bitreverse"
            | "simd_ctlz"
            | "simd_cttz"
            | "simd_ctpop" => {
                let (a, l) = self.vector_arg(&args[0].node)?;
                for i in 0..l.n {
                    let x = self.lane(a, l, i);
                    let r = match (name, l.kind) {
                        ("simd_neg", Kind::Float(ft)) => self.b.funary(FUnOp::Neg, ft, x),
                        ("simd_neg", _) => {
                            let z = self.b.iconst(l.ty, 0);
                            self.b.sub(z, x)
                        }
                        ("simd_fabs", Kind::Float(ft)) => self.b.funary(FUnOp::Abs, ft, x),
                        ("simd_fsqrt", Kind::Float(ft)) => self.b.funary(FUnOp::Sqrt, ft, x),
                        ("simd_floor", Kind::Float(ft)) => self.b.funary(FUnOp::Floor, ft, x),
                        ("simd_ceil", Kind::Float(ft)) => self.b.funary(FUnOp::Ceil, ft, x),
                        ("simd_trunc", Kind::Float(ft)) => self.b.funary(FUnOp::Trunc, ft, x),
                        ("simd_round_ties_even", Kind::Float(ft)) => self.b.funary(FUnOp::Nearest, ft, x),
                        ("simd_bswap", _) => self.b.unary(UnOp::Bswap, x),
                        ("simd_bitreverse", _) => self.b.unary(UnOp::Bitrev, x),
                        ("simd_ctlz", _) => self.b.unary(UnOp::Clz, x),
                        ("simd_cttz", _) => self.b.unary(UnOp::Ctz, x),
                        ("simd_ctpop", _) => self.b.unary(UnOp::Popcnt, x),
                        _ => return Err(unsupported(format!("`{name}` on these lanes"))),
                    };
                    self.set_lane(d, l, i, r);
                }
            }
            "simd_fma" | "simd_relaxed_fma" => {
                let (a, l) = self.vector_arg(&args[0].node)?;
                let (b, _) = self.vector_arg(&args[1].node)?;
                let (c, _) = self.vector_arg(&args[2].node)?;
                let Kind::Float(ft) = l.kind else { return Err(unsupported("`simd_fma` on integers")) };
                let fname = if ft == FloatTy::F32 { "fmaf" } else { "fma" };
                for i in 0..l.n {
                    let x = self.lane(a, l, i);
                    let y = self.lane(b, l, i);
                    let z = self.lane(c, l, i);
                    let r = self.libm(fname, ft, &[x, y, z]);
                    self.set_lane(d, l, i, r);
                }
            }
            "simd_cast" | "simd_as" => {
                let (a, from) = self.vector_arg(&args[0].node)?;
                let to = self.lanes(dest.layout.ty)?;
                for i in 0..from.n {
                    let x = self.lane(a, from, i);
                    let r = match (from.kind, to.kind) {
                        (Kind::Float(f), Kind::Float(t)) if f == t => x,
                        (Kind::Float(f), Kind::Float(t)) => self.b.fconvert(f, t, x),
                        (Kind::Float(f), k) => self.b.ftoint(f, to.ty, k == Kind::Signed, x),
                        (k, Kind::Float(t)) => self.b.inttof(t, k == Kind::Signed, x),
                        (k, _) => self.b.resize(to.ty, x, k == Kind::Signed),
                    };
                    self.set_lane(d, to, i, r);
                }
            }
            "simd_splat" => {
                let l = self.lanes(dest.layout.ty)?;
                let x = self.scalar_arg(&args[0].node)?;
                for i in 0..l.n {
                    self.set_lane(d, l, i, x);
                }
            }
            "simd_extract" | "simd_extract_dyn" => {
                let (a, l) = self.vector_arg(&args[0].node)?;
                let idx = self.scalar_arg(&args[1].node)?;
                let addr = self.lane_addr(a, l, idx);
                let v = self.b.load(l.ty, addr, 0);
                self.write(dest, Val { v: V::Scalar(v), layout: dest.layout })?;
            }
            "simd_insert" | "simd_insert_dyn" => {
                let (a, l) = self.vector_arg(&args[0].node)?;
                let idx = self.scalar_arg(&args[1].node)?;
                let x = self.scalar_arg(&args[2].node)?;
                self.copy(d, a, l.n * l.size);
                let addr = self.lane_addr(d, l, idx);
                self.b.store(addr, 0, x);
            }
            "simd_shuffle" => {
                let (a, l) = self.vector_arg(&args[0].node)?;
                let (b, _) = self.vector_arg(&args[1].node)?;
                let (idx, il) = self.vector_arg(&args[2].node)?;
                let out = self.lanes(dest.layout.ty)?;
                // Into a temporary: the destination may be an input.
                let tmp = self.alloc(dest.layout, 0)?;
                for i in 0..il.n {
                    let k = self.lane(idx, il, i);
                    let k = self.b.resize(PTR, k, false);
                    let n = self.b.iconst(PTR, l.n.into());
                    let in_a = self.b.cmp(Cmp::ULt, k, n);
                    let kb = self.b.sub(k, n);
                    let src = self.b.select(in_a, a, b);
                    let k = self.b.select(in_a, k, kb);
                    let addr = self.lane_addr(src, l, k);
                    let v = self.b.load(l.ty, addr, 0);
                    self.set_lane(tmp, out, i, v);
                }
                self.copy(d, tmp, out.n * out.size);
            }
            "simd_select" => {
                let (m, ml) = self.vector_arg(&args[0].node)?;
                let (a, l) = self.vector_arg(&args[1].node)?;
                let (b, _) = self.vector_arg(&args[2].node)?;
                for i in 0..l.n {
                    let mk = self.lane(m, ml, i);
                    let zero = self.b.iconst(ml.ty, 0);
                    let on = self.b.cmp(Cmp::Ne, mk, zero);
                    let x = self.lane(a, l, i);
                    let y = self.lane(b, l, i);
                    let r = self.b.select(on, x, y);
                    self.set_lane(d, l, i, r);
                }
            }
            "simd_select_bitmask" => {
                let bits_val = self.operand(&args[0].node)?;
                let (a, l) = self.vector_arg(&args[1].node)?;
                let (b, _) = self.vector_arg(&args[2].node)?;
                let bits = self.bitmask_bits(bits_val, l.n)?;
                for i in 0..l.n {
                    let one = self.b.iconst(Type::I64, 1);
                    let sh = self.b.iconst(Type::I64, i.into());
                    let bit = self.b.binary(IBin::UShr, bits, sh);
                    let bit = self.b.binary(IBin::And, bit, one);
                    let x = self.lane(a, l, i);
                    let y = self.lane(b, l, i);
                    let r = self.b.select(bit, x, y);
                    self.set_lane(d, l, i, r);
                }
            }
            "simd_bitmask" => {
                let (a, l) = self.vector_arg(&args[0].node)?;
                if l.n > 64 {
                    return Err(unsupported("`simd_bitmask` over 64 lanes"));
                }
                let mut acc = self.b.iconst(Type::I64, 0);
                for i in 0..l.n {
                    let x = self.lane(a, l, i);
                    let top = self.b.iconst(l.ty, i128::from(l.ty.bits() - 1));
                    let msb = self.b.binary(IBin::UShr, x, top);
                    let msb = self.b.resize(Type::I64, msb, false);
                    let sh = self.b.iconst(Type::I64, i.into());
                    let bit = self.b.binary(IBin::Shl, msb, sh);
                    acc = self.b.binary(IBin::Or, acc, bit);
                }
                // An integer, or an array of bytes, little-endian either way.
                let bytes = l.n.div_ceil(8);
                let mut tmp_bytes = Vec::new();
                for k in 0..bytes {
                    let sh = self.b.iconst(Type::I64, (k * 8).into());
                    let byte = self.b.binary(IBin::UShr, acc, sh);
                    tmp_bytes.push(self.b.trunc(Type::I8, byte));
                }
                for (k, byte) in tmp_bytes.into_iter().enumerate() {
                    if (k as u64) < dest.layout.size.bytes() {
                        self.b.store(d, k as i32, byte);
                    }
                }
            }
            "simd_reduce_and"
            | "simd_reduce_or"
            | "simd_reduce_xor"
            | "simd_reduce_add_unordered"
            | "simd_reduce_mul_unordered"
            | "simd_reduce_add_ordered"
            | "simd_reduce_mul_ordered"
            | "simd_reduce_max"
            | "simd_reduce_min"
            | "simd_reduce_all"
            | "simd_reduce_any" => {
                let (a, l) = self.vector_arg(&args[0].node)?;
                let mut acc = self.lane(a, l, 0);
                let start = if name.ends_with("_ordered") {
                    // A starting value, then every lane.
                    let s = self.scalar_arg(&args[1].node)?;
                    acc = s;
                    0
                } else {
                    1
                };
                for i in start..l.n {
                    let x = self.lane(a, l, i);
                    acc = match name {
                        "simd_reduce_and" | "simd_reduce_all" => self.b.binary(IBin::And, acc, x),
                        "simd_reduce_or" | "simd_reduce_any" => self.b.binary(IBin::Or, acc, x),
                        "simd_reduce_xor" => self.b.binary(IBin::Xor, acc, x),
                        "simd_reduce_add_unordered" | "simd_reduce_add_ordered" => {
                            self.lane_binary("simd_add", l, acc, x)?
                        }
                        "simd_reduce_mul_unordered" | "simd_reduce_mul_ordered" => {
                            self.lane_binary("simd_mul", l, acc, x)?
                        }
                        "simd_reduce_max" => self.lane_minmax(l, acc, x, true),
                        _ => self.lane_minmax(l, acc, x, false),
                    };
                }
                let v = if matches!(name, "simd_reduce_all" | "simd_reduce_any") {
                    let zero = self.b.iconst(l.ty, 0);
                    self.b.cmp(Cmp::Ne, acc, zero)
                } else {
                    acc
                };
                self.write(dest, Val { v: V::Scalar(v), layout: dest.layout })?;
            }
            _ => {
                let _ = inst;
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// LLVM intrinsics `core::arch` calls, by name after `llvm.`; `false`
    /// for ones Athrad does not know.
    pub(super) fn llvm_intrinsic(
        &mut self,
        name: &str,
        args: &[rustc_span::Spanned<Operand<'tcx>>],
        dest: PlaceRef<'tcx>,
    ) -> Result<bool> {
        let d = dest.addr;
        let parts: Vec<&str> = name.split('.').collect();
        match parts.as_slice() {
            // Pairwise: lane i of the result combines lanes 2i and 2i+1 of
            // `a` then of `b`.
            ["aarch64", "neon", op @ ("umaxp" | "uminp" | "smaxp" | "sminp" | "addp"), ..] => {
                let (a, l) = self.vector_arg(&args[0].node)?;
                let (b, _) = self.vector_arg(&args[1].node)?;
                let tmp = self.alloc(dest.layout, 0)?;
                for i in 0..l.n {
                    let (src, j) = if i < l.n / 2 { (a, 2 * i) } else { (b, 2 * (i - l.n / 2)) };
                    let x = self.lane(src, l, j);
                    let y = self.lane(src, l, j + 1);
                    let r = self.neon_op(op, l, x, y)?;
                    self.set_lane(tmp, l, i, r);
                }
                self.copy(d, tmp, l.n * l.size);
            }
            // Across all lanes, to a scalar.
            ["aarch64", "neon", op @ ("umaxv" | "uminv" | "smaxv" | "sminv" | "uaddv" | "saddv"), ..] => {
                let (a, l) = self.vector_arg(&args[0].node)?;
                let op = &op[..op.len() - 1];
                let mut acc = self.lane(a, l, 0);
                for i in 1..l.n {
                    let x = self.lane(a, l, i);
                    acc = self.neon_op(if op.ends_with("add") { "addp" } else { op }, l, acc, x)?;
                }
                let ty = self.scalar_of(dest.layout)?;
                let v = self.b.resize(ty, acc, op.starts_with('s'));
                self.write(dest, Val { v: V::Scalar(v), layout: dest.layout })?;
            }
            // Table lookup: each index picks a byte of the table, or 0.
            ["aarch64", "neon", "tbl1", ..] => {
                let (t, tl) = self.vector_arg(&args[0].node)?;
                let (idx, il) = self.vector_arg(&args[1].node)?;
                let tmp = self.alloc(dest.layout, 0)?;
                for i in 0..il.n {
                    let k = self.lane(idx, il, i);
                    let n = self.b.iconst(il.ty, tl.n.into());
                    let inside = self.b.cmp(Cmp::ULt, k, n);
                    let zero = self.b.iconst(il.ty, 0);
                    let k = self.b.select(inside, k, zero);
                    let addr = self.lane_addr(t, tl, k);
                    let v = self.b.load(tl.ty, addr, 0);
                    let v = self.b.select(inside, v, zero);
                    self.set_lane(tmp, il, i, v);
                }
                self.copy(d, tmp, il.n * il.size);
            }
            // Clearing the upper halves of AVX registers: nothing to do when
            // vectors live in memory.
            ["x86", "avx", "vzeroupper" | "vzeroall"] => {}
            // Barriers and spin-loop hints: a full fence is as strong as any.
            ["aarch64", "isb" | "dmb" | "dsb"]
            | ["x86", "sse2", "pause"]
            | ["x86", "sse" | "sse2", "lfence" | "mfence" | "sfence"] => {
                self.b.fence();
            }
            // Byte shuffle within each 16-byte lane: an index with its top bit
            // set gives 0, else the byte its low four bits name.
            ["x86", "ssse3" | "avx2" | "avx512", "pshuf", "b", ..] => {
                let (a, l) = self.vector_arg(&args[0].node)?;
                let (idx, _) = self.vector_arg(&args[1].node)?;
                let tmp = self.alloc(dest.layout, 0)?;
                for i in 0..l.n {
                    let k = self.lane(idx, l, i);
                    let mask = self.b.iconst(Type::I8, 0x0f);
                    let low = self.b.binary(IBin::And, k, mask);
                    let base = self.b.iconst(Type::I8, ((i / 16) * 16) as i128);
                    let at = self.b.add(low, base);
                    let addr = self.lane_addr(a, l, at);
                    let v = self.b.load(Type::I8, addr, 0);
                    let zero = self.b.iconst(Type::I8, 0);
                    let neg = self.b.cmp(Cmp::SLt, k, zero);
                    let v = self.b.select(neg, zero, v);
                    self.set_lane(tmp, l, i, v);
                }
                self.copy(d, tmp, l.n * l.size);
            }
            // The top bit of each byte, as an integer.
            ["x86", "sse2" | "avx2", "pmovmskb", ..] => {
                let (a, l) = self.vector_arg(&args[0].node)?;
                let mut acc = self.b.iconst(Type::I32, 0);
                for i in 0..l.n {
                    let x = self.lane(a, l, i);
                    let seven = self.b.iconst(l.ty, 7);
                    let msb = self.b.binary(IBin::UShr, x, seven);
                    let msb = self.b.resize(Type::I32, msb, false);
                    let sh = self.b.iconst(Type::I32, i.into());
                    let bit = self.b.binary(IBin::Shl, msb, sh);
                    acc = self.b.binary(IBin::Or, acc, bit);
                }
                self.write(dest, Val { v: V::Scalar(acc), layout: dest.layout })?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn neon_op(&mut self, op: &str, l: Lanes, x: Value, y: Value) -> Result<Value> {
        Ok(match op {
            "umaxp" | "umax" => {
                let c = self.b.cmp(Cmp::UGt, x, y);
                self.b.select(c, x, y)
            }
            "uminp" | "umin" => {
                let c = self.b.cmp(Cmp::ULt, x, y);
                self.b.select(c, x, y)
            }
            "smaxp" | "smax" => {
                let c = self.b.cmp(Cmp::SGt, x, y);
                self.b.select(c, x, y)
            }
            "sminp" | "smin" => {
                let c = self.b.cmp(Cmp::SLt, x, y);
                self.b.select(c, x, y)
            }
            "addp" => self.lane_binary("simd_add", l, x, y)?,
            _ => return Err(unsupported(format!("the NEON operation `{op}`"))),
        })
    }

    fn lane_addr(&mut self, base: Value, l: Lanes, idx: Value) -> Value {
        let idx = self.b.resize(PTR, idx, false);
        let size = self.b.iconst(PTR, l.size.into());
        let off = self.b.binary(IBin::Mul, idx, size);
        self.b.add(base, off)
    }

    /// The bits of a `simd_select_bitmask` mask, an integer or byte array.
    fn bitmask_bits(&mut self, v: Val<'tcx>, n: u64) -> Result<Value> {
        if n > 64 {
            return Err(unsupported("bitmasks over 64 lanes"));
        }
        let addr = self.spill(v)?;
        let bytes = v.layout.size.bytes().min(8);
        let mut acc = self.b.iconst(Type::I64, 0);
        for k in 0..bytes {
            let byte = self.b.load(Type::I8, addr, k as i32);
            let byte = self.b.zext(Type::I64, byte);
            let sh = self.b.iconst(Type::I64, (k * 8).into());
            let byte = self.b.binary(IBin::Shl, byte, sh);
            acc = self.b.binary(IBin::Or, acc, byte);
        }
        Ok(acc)
    }

    fn lane_minmax(&mut self, l: Lanes, x: Value, y: Value, max: bool) -> Value {
        match l.kind {
            Kind::Float(ft) => self.b.fbinary(if max { FBinOp::Max } else { FBinOp::Min }, ft, x, y),
            k => {
                let s = k == Kind::Signed;
                let c = match (max, s) {
                    (true, true) => Cmp::SGt,
                    (true, false) => Cmp::UGt,
                    (false, true) => Cmp::SLt,
                    (false, false) => Cmp::ULt,
                };
                let take_x = self.b.cmp(c, x, y);
                self.b.select(take_x, x, y)
            }
        }
    }

    fn lane_binary(&mut self, name: &str, l: Lanes, x: Value, y: Value) -> Result<Value> {
        if let Kind::Float(ft) = l.kind {
            let op = match name {
                "simd_add" => FBinOp::Add,
                "simd_sub" => FBinOp::Sub,
                "simd_mul" => FBinOp::Mul,
                "simd_div" => FBinOp::Div,
                "simd_rem" => return Ok(self.libm(if ft == FloatTy::F32 { "fmodf" } else { "fmod" }, ft, &[x, y])),
                "simd_minimum_number_nsz" | "simd_fmin" | "simd_maximum_number_nsz" | "simd_fmax" => {
                    let max = name.contains("max");
                    let m = self.b.fbinary(if max { FBinOp::Max } else { FBinOp::Min }, ft, x, y);
                    let x_nan = self.b.fcmp(FCmp::Uno, ft, x, x);
                    let y_nan = self.b.fcmp(FCmp::Uno, ft, y, y);
                    let m = self.b.select(y_nan, x, m);
                    return Ok(self.b.select(x_nan, y, m));
                }
                _ => return Err(unsupported(format!("`{name}` on floats"))),
            };
            return Ok(self.b.fbinary(op, ft, x, y));
        }
        let s = l.kind == Kind::Signed;
        Ok(match name {
            "simd_add" => self.b.add(x, y),
            "simd_sub" => self.b.sub(x, y),
            "simd_mul" => self.b.binary(IBin::Mul, x, y),
            "simd_div" => self.b.binary(if s { IBin::SDiv } else { IBin::UDiv }, x, y),
            "simd_rem" => self.b.binary(if s { IBin::SRem } else { IBin::URem }, x, y),
            "simd_shl" => self.b.binary(IBin::Shl, x, y),
            "simd_shr" => self.b.binary(if s { IBin::SShr } else { IBin::UShr }, x, y),
            "simd_and" => self.b.binary(IBin::And, x, y),
            "simd_or" => self.b.binary(IBin::Or, x, y),
            "simd_xor" => self.b.binary(IBin::Xor, x, y),
            "simd_saturating_add" | "simd_saturating_sub" => {
                let add = name == "simd_saturating_add";
                let (r, of) = self.b.overflow(if add { OverflowOp::Add } else { OverflowOp::Sub }, s, x, y);
                let bits = l.ty.bits();
                let clamp = if s {
                    let max = self.b.iconst(l.ty, (1i128 << (bits - 1)) - 1);
                    let min = self.b.iconst(l.ty, -(1i128 << (bits - 1)));
                    let zero = self.b.iconst(l.ty, 0);
                    let y_neg = self.b.cmp(Cmp::SLt, y, zero);
                    if add { self.b.select(y_neg, min, max) } else { self.b.select(y_neg, max, min) }
                } else if add {
                    self.b.iconst(l.ty, -1)
                } else {
                    self.b.iconst(l.ty, 0)
                };
                self.b.select(of, clamp, r)
            }
            _ => return Err(unsupported(format!("`{name}` on integers"))),
        })
    }
}
