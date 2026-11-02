//! What a panic may skip.
//!
//! Athrad's frames have no landing pads yet: a panic unwinds through them
//! without running their cleanup blocks. Athrad also rejects every
//! `catch_unwind` it would compile (`std::panic::catch_unwind`, thread
//! spawns), so a panic in an Athrad program always ends the process: the
//! panic hook prints the message, `lang_start` exits with 101. What a skipped
//! cleanup leaves behind (leaked memory, a lock never released, a slice a
//! sort left half-permuted) is never looked at again.
//!
//! What can still be seen is a destructor's effect outside the process:
//!
//! * a `Drop` impl in the program, which may print or write;
//! * the standard types whose `Drop` does I/O, such as `BufWriter`'s flush;
//! * a `dyn Trait` whose concrete type is either of those.
//!
//! A cleanup block that would run one of those makes the program one Athrad
//! cannot match, and it is rejected. So is a cleanup block that aborts (a
//! panic unwinding where it must not), since skipping it turns an abort into
//! exit code 101. Real landing pads lift all of this; see HANDOFF.md.

use std::collections::HashMap;

use rustc_hir::def_id::DefId;
use rustc_middle::mir::{BasicBlock, Body, TerminatorKind};
use rustc_middle::ty::{self, Ty, TyCtxt};
use serde::{Deserialize, Serialize};

/// Standard types whose `Drop` frees a buffer of their type parameter
/// without dropping any value in it.
const DEALLOCATE_ONLY: &[&str] = &["alloc::raw_vec::RawVec", "alloc::raw_vec::RawVecInner"];

/// Standard types whose `Drop` does I/O.
const EXTERNAL_EFFECTS: &[&str] = &["std::io::buffered::bufwriter::BufWriter"];

#[derive(Default)]
pub(crate) struct Skippable<'tcx> {
    cache: HashMap<Ty<'tcx>, Result<(), String>>,
    /// Trait objects some cleanup would drop, with where, to check once
    /// every vtable is known.
    dyn_drops: Vec<(Ty<'tcx>, String)>,
}

impl<'tcx> Skippable<'tcx> {
    /// Whether the cleanup block `bb` of `mir` may be skipped. `mono`
    /// monomorphizes a type of `mir`. `Err` names why not.
    pub(crate) fn cleanup_block(
        &mut self,
        tcx: TyCtxt<'tcx>,
        mir: &Body<'tcx>,
        bb: BasicBlock,
        mono: impl Fn(Ty<'tcx>) -> Ty<'tcx>,
        span: impl Fn() -> String,
    ) -> Result<(), String> {
        let data = &mir.basic_blocks[bb];
        let dropped = match &data.terminator().kind {
            TerminatorKind::Drop { place, .. } => mono(place.ty(mir, tcx).ty),
            // Drop glue calls a type's own `Drop::drop` from its cleanup:
            // `Box`'s, to free the box once dropping its contents panicked.
            TerminatorKind::Call { func, .. } => {
                let fn_ty = mono(func.ty(mir, tcx));
                let ty::FnDef(def_id, args) = *fn_ty.kind() else {
                    return Err("an indirect call while unwinding a panic".into());
                };
                let lang = tcx.lang_items();
                let is_drop = tcx.trait_of_assoc(def_id).is_some_and(|t| Some(t) == lang.drop_trait())
                    || Some(def_id) == lang.drop_glue_fn();
                if !is_drop {
                    return Err(format!("calling `{}` while unwinding a panic", tcx.def_path_str(def_id)));
                }
                args.type_at(0)
            }
            // Drop flags: branch on whether a value is still live.
            TerminatorKind::Goto { .. }
            | TerminatorKind::SwitchInt { .. }
            | TerminatorKind::UnwindResume
            | TerminatorKind::Unreachable => return Ok(()),
            TerminatorKind::UnwindTerminate(_) => {
                return Err("aborting when a panic unwinds where it must not".into());
            }
            other => return Err(format!("`{}` while unwinding a panic", other.name())),
        };
        let mut dyns = Vec::new();
        self.drop_of(tcx, dropped, &mut dyns)
            .map_err(|why| format!("dropping `{dropped}` while unwinding a panic: {why}"))?;
        self.dyn_drops.extend(dyns.into_iter().map(|d| (d, span())));
        Ok(())
    }

    /// What this codegen unit says about trait objects, for
    /// [`check_trait_objects`] to decide once every unit is lowered (or
    /// reused). `vtables` are the concrete type and principal trait of every
    /// vtable the unit builds.
    pub(crate) fn facts(&mut self, tcx: TyCtxt<'tcx>, vtables: &[(Ty<'tcx>, Option<DefId>)]) -> Facts {
        let mut facts = Facts::default();
        for &(concrete, principal) in vtables {
            let mut nested = Vec::new();
            let verdict = self.drop_of(tcx, concrete, &mut nested).and_then(|()| match nested.first() {
                Some(d) => Err(format!("`{d}` inside it")),
                None => Ok(()),
            });
            facts.vtables.push(VtableFact {
                principal: principal.map(|p| def_path(tcx, p)),
                concrete: concrete.to_string(),
                why_not: verdict.err(),
            });
        }
        for (dyn_ty, at) in std::mem::take(&mut self.dyn_drops) {
            let ty::Dynamic(preds, ..) = dyn_ty.kind() else { continue };
            facts.dyn_drops.push(DynDropFact {
                principal: preds.principal_def_id().map(|p| def_path(tcx, p)),
                ty: dyn_ty.to_string(),
                at,
            });
        }
        facts
    }

    /// `Ok` when skipping the drop of a `ty` cannot be seen. Trait objects
    /// are pushed to `dyns`, to be decided by [`check_trait_objects`].
    fn drop_of(&mut self, tcx: TyCtxt<'tcx>, ty: Ty<'tcx>, dyns: &mut Vec<Ty<'tcx>>) -> Result<(), String> {
        if let Some(r) = self.cache.get(&ty) {
            return r.clone();
        }
        // Recursive types (a list node holding a `Box` of itself) come back
        // here; assume yes while deciding, as a coinductive proof does.
        self.cache.insert(ty, Ok(()));
        let r = self.decide(tcx, ty, dyns);
        self.cache.insert(ty, r.clone());
        r
    }

    fn decide(&mut self, tcx: TyCtxt<'tcx>, ty: Ty<'tcx>, dyns: &mut Vec<Ty<'tcx>>) -> Result<(), String> {
        let env = ty::TypingEnv::fully_monomorphized();
        if !ty.needs_drop(tcx, env) {
            return Ok(());
        }
        match ty.kind() {
            ty::Adt(adt, args) => {
                if adt.has_dtor(tcx) {
                    if !is_standard(tcx, adt.did()) {
                        return Err(format!("`{}` implements `Drop`", tcx.def_path_str(adt.did())));
                    }
                    let path = def_path(tcx, adt.did());
                    if EXTERNAL_EFFECTS.contains(&path.as_str()) {
                        return Err(format!("`{path}`'s destructor does I/O"));
                    }
                    // A standard container drops values of its type
                    // parameters, wherever it keeps them.
                    if !DEALLOCATE_ONLY.contains(&path.as_str()) {
                        for arg in args.types() {
                            self.drop_of(tcx, arg, dyns)?;
                        }
                    }
                }
                for field in adt.all_fields() {
                    let fty = tcx.normalize_erasing_regions(env, field.ty(tcx, args));
                    self.drop_of(tcx, fty, dyns)?;
                }
                Ok(())
            }
            ty::Tuple(tys) => tys.iter().try_for_each(|t| self.drop_of(tcx, t, dyns)),
            ty::Array(elem, _) | ty::Slice(elem) | ty::Pat(elem, _) => self.drop_of(tcx, *elem, dyns),
            ty::Closure(_, args) => args.as_closure().upvar_tys().iter().try_for_each(|t| self.drop_of(tcx, t, dyns)),
            ty::Dynamic(..) => {
                dyns.push(ty);
                Ok(())
            }
            _ => Err(format!("`{ty}` has a destructor Athrad cannot see through")),
        }
    }
}

/// What one codegen unit knows about trait objects and their drops.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Facts {
    vtables: Vec<VtableFact>,
    dyn_drops: Vec<DynDropFact>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct VtableFact {
    principal: Option<String>,
    concrete: String,
    /// Why dropping a `concrete` may not be skipped, if it may not.
    why_not: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DynDropFact {
    principal: Option<String>,
    ty: String,
    at: String,
}

impl Facts {
    /// One crate's facts, from its units'.
    pub(crate) fn merged(all: &[Facts]) -> Facts {
        Facts {
            vtables: all.iter().flat_map(|f| f.vtables.iter().cloned()).collect(),
            dyn_drops: all.iter().flat_map(|f| f.dyn_drops.iter().cloned()).collect(),
        }
    }
}

/// Each trait object some cleanup would drop must only ever hold types whose
/// drop may be skipped, across every codegen unit's vtables, and those of the
/// crates this one depends on (given as more `Facts`).
pub(crate) fn check_trait_objects(all: &[Facts]) -> Result<(), String> {
    for d in all.iter().flat_map(|f| &f.dyn_drops) {
        for v in all.iter().flat_map(|f| &f.vtables) {
            if v.principal != d.principal {
                continue;
            }
            if let Some(why) = &v.why_not {
                return Err(format!(
                    "dropping `{}` while unwinding a panic, which may be a `{}`: {why} ({})",
                    d.ty, v.concrete, d.at
                ));
            }
        }
    }
    Ok(())
}

/// Whether `did` comes from the standard library: a crate in the sysroot.
/// A dependency from crates.io or a path is the program's own code here, and
/// its `Drop` impls count as the program's.
fn is_standard(tcx: TyCtxt<'_>, did: DefId) -> bool {
    if did.is_local() {
        return false;
    }
    tcx.used_crate_source(did.krate).paths().any(|p| crate::in_sysroot(tcx, p))
}

fn def_path(tcx: TyCtxt<'_>, did: DefId) -> String {
    format!("{}{}", tcx.crate_name(did.krate), tcx.def_path(did).to_string_no_crate_verbose())
}
