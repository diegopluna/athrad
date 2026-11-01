//! What a panic may skip.
//!
//! Athrad's frames have no landing pads yet: a panic unwinds through them
//! without running their cleanup blocks. That is only right when skipping the
//! cleanup cannot be seen. The process either exits with 101 (and freed or
//! leaked memory look the same from outside), or a `catch_unwind` resumes it
//! (and leaked memory is still invisible to a program's output).
//!
//! So a cleanup block may only:
//!
//! * drop values whose destructors only free memory: types with no drop
//!   glue, and the standard containers below, recursively through their
//!   fields and element types;
//! * jump, branch on a drop flag, and resume unwinding.
//!
//! Anything else (a user `Drop`, a lock guard, a `RefCell` borrow, a
//! `BufWriter` that flushes, a `dyn Trait` whose destructor is unknown, a
//! call, an abort because unwinding reached a function that must not unwind)
//! makes the program one Athrad cannot match, and it is rejected. Real landing
//! pads lift this; see HANDOFF.md.

use std::collections::HashMap;

use rustc_middle::mir::{BasicBlock, Body, TerminatorKind};
use rustc_middle::ty::{self, Ty, TyCtxt};

/// Standard types whose `Drop` only frees their buffer: it drops no values.
const DEALLOCATE_ONLY: &[&str] = &["alloc::raw_vec::RawVec", "alloc::raw_vec::RawVecInner"];

/// Standard types whose `Drop` only frees memory and drops the values of
/// their type parameters. Paths are as `def_path` spells them, from the
/// defining crate.
const MEMORY_ONLY: &[&str] = &[
    "alloc::boxed::Box",
    "alloc::vec::Vec",
    "alloc::vec::into_iter::IntoIter",
    "alloc::collections::vec_deque::VecDeque",
    "alloc::collections::btree::map::BTreeMap",
    "alloc::collections::btree::map::IntoIter",
    "alloc::collections::btree::set::BTreeSet",
    "core::array::iter::IntoIter",
    "hashbrown::raw::RawTable",
    "hashbrown::raw::RawIntoIter",
    "hashbrown::raw::RawTableInner",
];

#[derive(Default)]
pub(crate) struct Skippable<'tcx> {
    cache: HashMap<Ty<'tcx>, Result<(), String>>,
}

impl<'tcx> Skippable<'tcx> {
    /// Whether the cleanup block `bb` of `mir` (types already
    /// monomorphized by `mono`) may be skipped. `Err` names why not.
    pub(crate) fn cleanup_block(
        &mut self,
        tcx: TyCtxt<'tcx>,
        mir: &Body<'tcx>,
        bb: BasicBlock,
        mono: impl Fn(Ty<'tcx>) -> Ty<'tcx>,
    ) -> Result<(), String> {
        let data = &mir.basic_blocks[bb];
        match &data.terminator().kind {
            TerminatorKind::Drop { place, .. } => {
                let ty = mono(place.ty(mir, tcx).ty);
                self.drop_of(tcx, ty).map_err(|why| format!("dropping `{ty}` while unwinding a panic: {why}"))
            }
            // Drop flags: branch on whether a value is still live.
            TerminatorKind::Goto { .. }
            | TerminatorKind::SwitchInt { .. }
            | TerminatorKind::UnwindResume
            | TerminatorKind::Unreachable => Ok(()),
            TerminatorKind::UnwindTerminate(_) => Err("aborting when a panic unwinds where it must not".into()),
            other => Err(format!("`{}` while unwinding a panic", other.name())),
        }
    }

    /// `Ok` when dropping a `ty` can be skipped unseen.
    fn drop_of(&mut self, tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> Result<(), String> {
        if let Some(r) = self.cache.get(&ty) {
            return r.clone();
        }
        // Recursive types (a list node holding a `Box` of itself) come back
        // here; assume yes while deciding, as a coinductive proof does.
        self.cache.insert(ty, Ok(()));
        let r = self.decide(tcx, ty);
        self.cache.insert(ty, r.clone());
        r
    }

    fn decide(&mut self, tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> Result<(), String> {
        let env = ty::TypingEnv::fully_monomorphized();
        if !ty.needs_drop(tcx, env) {
            return Ok(());
        }
        match ty.kind() {
            ty::Adt(adt, args) => {
                if adt.has_dtor(tcx) {
                    let path = def_path(tcx, adt.did());
                    if MEMORY_ONLY.contains(&path.as_str()) {
                        for arg in args.types() {
                            self.drop_of(tcx, arg)?;
                        }
                    } else if !DEALLOCATE_ONLY.contains(&path.as_str()) {
                        return Err(format!("`{path}` has a destructor"));
                    }
                }
                for field in adt.all_fields() {
                    let fty = tcx.normalize_erasing_regions(env, field.ty(tcx, args));
                    self.drop_of(tcx, fty)?;
                }
                Ok(())
            }
            ty::Tuple(tys) => tys.iter().try_for_each(|t| self.drop_of(tcx, t)),
            ty::Array(elem, _) | ty::Slice(elem) | ty::Pat(elem, _) => self.drop_of(tcx, *elem),
            ty::Closure(_, args) => args.as_closure().upvar_tys().iter().try_for_each(|t| self.drop_of(tcx, t)),
            _ => Err(format!("`{ty}` has a destructor Athrad cannot see through")),
        }
    }
}

fn def_path(tcx: TyCtxt<'_>, did: rustc_hir::def_id::DefId) -> String {
    format!("{}{}", tcx.crate_name(did.krate), tcx.def_path(did).to_string_no_crate_verbose())
}
