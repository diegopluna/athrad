//! One function's machine code, produced while compiling a module.
//!
//! Relocations name symbols. Cranelift's ids change as functions are added,
//! and the functions of one module are compiled at the same time, so the
//! object is filled in afterwards, in source order.

use cranelift_codegen::binemit::Reloc;
use cranelift_codegen::ir::{KnownSymbol, LibCall};
use cranelift_codegen::isa::unwind::UnwindInfo;

pub struct CallSite {
    pub ret_addr: u32,
    /// `(catch, landing pad offset)`; at most one.
    pub pad: Option<(bool, u32)>,
}

pub(crate) struct Function {
    pub align: u64,
    pub code: Vec<u8>,
    pub relocs: Vec<RelocEntry>,
    pub unwind: Option<UnwindInfo>,
    pub call_sites: Vec<CallSite>,
}

pub(crate) struct RelocEntry {
    pub offset: u32,
    pub kind: Reloc,
    pub target: Target,
    pub addend: i64,
}

pub(crate) enum Target {
    Func(String),
    Data(String),
    LibCall(LibCall),
    Known(KnownSymbol),
    /// An offset into the function itself.
    SelfOffset(u32),
}
