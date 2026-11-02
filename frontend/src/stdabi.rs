//! What Athrad's own frontend (the `subset` crate) needs to know about the
//! pinned `std` to compile without `rustc`: the panic functions overflow and
//! division checks call, and how a `core::panic::Location` is laid out.
//! Read from `rustc` when Athrad itself is built, and shipped inside the
//! runtime the subset's programs link with.

use rustc_abi::FieldIdx;
use rustc_hir::LangItem;
use rustc_middle::ty::layout::{FnAbiOf, LayoutOf};
use rustc_middle::ty::{self, Instance, TyCtxt};
use rustc_span::DUMMY_SP;
use serde::{Deserialize, Serialize};

use crate::cx::{Cx, signature};
use crate::{Result, unsupported};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StdAbi {
    /// The symbol of each check's panic function, by the name of its lang
    /// item (`panic_const_add_overflow`, ...). Each takes a `&Location`.
    pub panics: Vec<(String, String)>,
    pub location: LocationLayout,
}

impl StdAbi {
    pub fn panic(&self, item: &str) -> Option<&str> {
        self.panics.iter().find(|(k, _)| k == item).map(|(_, s)| s.as_str())
    }
}

/// `core::panic::Location`: a file name, a line, and a column.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationLayout {
    pub size: u64,
    pub align: u64,
    /// Where the file name's data pointer and length go.
    pub file_ptr: u64,
    pub file_len: u64,
    /// Whether the file name is stored with a NUL after it, counted in its
    /// length.
    pub file_nul: bool,
    pub line: u64,
    pub col: u64,
}

pub(crate) fn std_abi(tcx: TyCtxt<'_>) -> Result<StdAbi> {
    let cx = Cx(tcx);
    let items = [
        (LangItem::PanicAddOverflow, "panic_const_add_overflow"),
        (LangItem::PanicSubOverflow, "panic_const_sub_overflow"),
        (LangItem::PanicMulOverflow, "panic_const_mul_overflow"),
        (LangItem::PanicDivOverflow, "panic_const_div_overflow"),
        (LangItem::PanicRemOverflow, "panic_const_rem_overflow"),
        (LangItem::PanicNegOverflow, "panic_const_neg_overflow"),
        (LangItem::PanicShlOverflow, "panic_const_shl_overflow"),
        (LangItem::PanicShrOverflow, "panic_const_shr_overflow"),
        (LangItem::PanicDivZero, "panic_const_div_by_zero"),
        (LangItem::PanicRemZero, "panic_const_rem_by_zero"),
    ];
    let mut panics = Vec::new();
    for (item, name) in items {
        let def = tcx.require_lang_item(item, DUMMY_SP);
        let inst = Instance::mono(tcx, def);
        let abi = cx.fn_abi_of_instance(inst, ty::List::empty());
        let sig = signature(cx, abi)?;
        if sig.param_types() != [ir::PTR] || !sig.returns.is_empty() {
            return Err(unsupported(format!("`{name}` with the signature {sig}")));
        }
        panics.push((name.to_owned(), tcx.symbol_name(inst).name.to_owned()));
    }

    let loc_ty = tcx.caller_location_ty().builtin_deref(true).expect("a reference");
    let layout = cx.layout_of(loc_ty);
    let ty::Adt(adt, _) = loc_ty.kind() else { return Err(unsupported("a `Location` that is not a struct")) };
    let mut loc = LocationLayout {
        size: layout.size.bytes(),
        align: layout.align.abi.bytes(),
        file_ptr: u64::MAX,
        file_len: u64::MAX,
        file_nul: false,
        line: u64::MAX,
        col: u64::MAX,
    };
    for (i, field) in adt.non_enum_variant().fields.iter().enumerate() {
        let name = field.name.as_str();
        let offset = layout.fields.offset(i).bytes();
        let f = layout.field(&cx, FieldIdx::from_usize(i).as_usize());
        match name {
            // Markers such as `PhantomData` take no space.
            _ if f.size.bytes() == 0 => {}
            "line" | "col" if f.size.bytes() == 4 => {
                if name == "line" {
                    loc.line = offset
                } else {
                    loc.col = offset
                }
            }
            _ if name.contains("file") && f.size.bytes() == 16 => {
                let rustc_abi::BackendRepr::ScalarPair(a, b) = f.backend_repr else {
                    return Err(unsupported(format!("`Location::{name}` of type `{}`", f.ty)));
                };
                loc.file_ptr = offset;
                loc.file_len = offset + crate::cx::pair_b_offset(cx, a, b).bytes();
                loc.file_nul = name.contains("nul");
            }
            _ => return Err(unsupported(format!("`Location::{name}` of type `{}`", f.ty))),
        }
    }
    if [loc.file_ptr, loc.line, loc.col].contains(&u64::MAX) {
        return Err(unsupported("this `Location` layout"));
    }
    Ok(StdAbi { panics, location: loc })
}
