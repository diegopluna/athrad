//! Layouts and calling conventions, as `rustc` computes them.

use rustc_abi::{BackendRepr, HasDataLayout, Primitive, Reg, RegKind, Scalar, Size, TargetDataLayout};
use rustc_middle::ty::layout::{
    FnAbiError, FnAbiOfHelpers, FnAbiRequest, HasTyCtxt, HasTypingEnv, LayoutError, LayoutOfHelpers,
};
use rustc_middle::ty::{self, Ty, TyCtxt};
use rustc_span::Span;
use rustc_target::callconv::{ArgAbi, ArgAttributes, ArgExtension, CastTarget, FnAbi, PassMode};

use crate::{Result, unsupported};

/// Layouts for fully monomorphized code. Errors are fatal: the mono item
/// collector has already computed every layout codegen asks for.
#[derive(Clone, Copy)]
pub(crate) struct Cx<'tcx>(pub(crate) TyCtxt<'tcx>);

impl<'tcx> LayoutOfHelpers<'tcx> for Cx<'tcx> {
    fn handle_layout_err(&self, err: LayoutError<'tcx>, span: Span, ty: Ty<'tcx>) -> ! {
        self.0.dcx().span_fatal(span, format!("failed to get layout for `{ty}`: {err}"))
    }
}

impl<'tcx> FnAbiOfHelpers<'tcx> for Cx<'tcx> {
    fn handle_fn_abi_err(&self, err: FnAbiError<'tcx>, span: Span, req: FnAbiRequest<'tcx>) -> ! {
        self.0.dcx().span_fatal(span, format!("failed to get the ABI of {req:?}: {err:?}"))
    }
}

impl<'tcx> HasTyCtxt<'tcx> for Cx<'tcx> {
    fn tcx(&self) -> TyCtxt<'tcx> {
        self.0
    }
}

impl HasDataLayout for Cx<'_> {
    fn data_layout(&self) -> &TargetDataLayout {
        &self.0.data_layout
    }
}

impl<'tcx> HasTypingEnv<'tcx> for Cx<'tcx> {
    fn typing_env(&self) -> ty::TypingEnv<'tcx> {
        ty::TypingEnv::fully_monomorphized()
    }
}

/// The IR type of a scalar.
pub(crate) fn scalar_type(cx: Cx<'_>, s: Scalar) -> Result<ir::Type> {
    match s.primitive() {
        Primitive::Int(..) | Primitive::Pointer(_) => {
            let size = s.size(&cx).bytes();
            ir::Type::of_bytes(size).ok_or_else(|| unsupported(format!("a {size}-byte integer")))
        }
        Primitive::Float(_) => Err(unsupported("floating point")),
    }
}

/// Whether a scalar is a signed integer.
pub(crate) fn is_signed(s: Scalar) -> bool {
    matches!(s.primitive(), Primitive::Int(_, true))
}

/// The offset of the second half of a scalar pair.
pub(crate) fn pair_b_offset(cx: Cx<'_>, a: Scalar, b: Scalar) -> Size {
    a.size(&cx).align_to(b.default_align(&cx).abi)
}

fn ext(attrs: &ArgAttributes) -> ir::Ext {
    match attrs.arg_ext {
        ArgExtension::None => ir::Ext::None,
        ArgExtension::Zext => ir::Ext::Zero,
        ArgExtension::Sext => ir::Ext::Sign,
    }
}

fn reg_type(reg: Reg) -> Result<ir::Type> {
    match (reg.kind, reg.size.bytes()) {
        (RegKind::Integer, 1) => Ok(ir::Type::I8),
        (RegKind::Integer, 2) => Ok(ir::Type::I16),
        (RegKind::Integer, 3..=4) => Ok(ir::Type::I32),
        (RegKind::Integer, 5..=8) => Ok(ir::Type::I64),
        (RegKind::Integer, 9..=16) => Ok(ir::Type::I128),
        _ => Err(unsupported(format!("passing a value in a {reg:?} register"))),
    }
}

/// The registers a cast argument is passed in, and where each one's bytes
/// sit in the value's memory.
pub(crate) fn cast_pieces(cast: &CastTarget) -> Result<Vec<(Size, ir::Type)>> {
    if let Some(rest_offset) = cast.rest_offset {
        if cast.prefix.len() != 1 || cast.rest.unit.size != cast.rest.total {
            return Err(unsupported("this argument cast"));
        }
        return Ok(vec![(Size::ZERO, reg_type(cast.prefix[0])?), (rest_offset, reg_type(cast.rest.unit)?)]);
    }
    let unit = cast.rest.unit.size.bytes();
    let total = cast.rest.total.bytes();
    let (count, rem) = total.checked_div(unit).map_or((0, 0), |n| (n, total % unit));
    let mut out = Vec::new();
    let mut offset = Size::ZERO;
    for reg in cast.prefix.iter().copied().chain((0..count).map(|_| cast.rest.unit)) {
        let ty = reg_type(reg)?;
        out.push((offset, ty));
        offset += Size::from_bytes(ty.bytes());
    }
    if rem != 0 {
        if cast.rest.unit.kind != RegKind::Integer {
            return Err(unsupported("a non-integer argument cast with a remainder"));
        }
        out.push((offset, reg_type(Reg { kind: RegKind::Integer, size: Size::from_bytes(rem) })?));
    }
    Ok(out)
}

/// The machine parameters for one Rust argument.
pub(crate) fn arg_params<'tcx>(cx: Cx<'tcx>, arg: &ArgAbi<'tcx, Ty<'tcx>>) -> Result<Vec<ir::Param>> {
    let p = |ty, attrs: &ArgAttributes| ir::Param { ty, ext: ext(attrs), sret: false };
    Ok(match &arg.mode {
        PassMode::Ignore => vec![],
        PassMode::Direct(attrs) => match arg.layout.backend_repr {
            BackendRepr::Scalar(s) => vec![p(scalar_type(cx, s)?, attrs)],
            _ => return Err(unsupported("passing a vector")),
        },
        PassMode::Pair(a_attrs, b_attrs) => match arg.layout.backend_repr {
            BackendRepr::ScalarPair(a, b) => vec![p(scalar_type(cx, a)?, a_attrs), p(scalar_type(cx, b)?, b_attrs)],
            _ => unreachable!("pair pass mode for a non-pair layout"),
        },
        PassMode::Cast { cast, pad_i32 } => {
            if *pad_i32 {
                return Err(unsupported("padded argument casts"));
            }
            cast_pieces(cast)?.into_iter().map(|(_, ty)| ir::Param::new(ty)).collect()
        }
        PassMode::Indirect { attrs, meta_attrs: None, on_stack: false } => vec![p(ir::PTR, attrs)],
        PassMode::Indirect { on_stack: true, .. } => return Err(unsupported("arguments passed by value on the stack")),
        PassMode::Indirect { meta_attrs: Some(_), .. } => return Err(unsupported("unsized arguments")),
    })
}

/// The IR signature for a Rust function's ABI.
pub(crate) fn signature<'tcx>(cx: Cx<'tcx>, abi: &FnAbi<'tcx, Ty<'tcx>>) -> Result<ir::Signature> {
    if abi.c_variadic {
        return Err(unsupported("C-variadic functions"));
    }
    let mut sig = ir::Signature::default();
    match &abi.ret.mode {
        PassMode::Indirect { attrs, meta_attrs: None, on_stack: false } => {
            sig.params.push(ir::Param { ty: ir::PTR, ext: ext(attrs), sret: true })
        }
        PassMode::Indirect { .. } => return Err(unsupported("this return value")),
        _ => sig.returns = arg_params(cx, &abi.ret)?,
    }
    for arg in abi.args.iter() {
        sig.params.extend(arg_params(cx, arg)?);
    }
    Ok(sig)
}
