//! Release builds split by a run profile: LLVM optimizes the functions the
//! program spends its time in, and Athrad compiles the rest with Cranelift.
//!
//! LLVM's optimizer is most of a release build, and it is what makes the
//! binary fast, but only where the binary runs. In a profile of `regex`, a
//! third of its functions ran at all, and 4.6% of them held 99.9% of the
//! executed blocks. Those, and anything Athrad cannot compile itself, go to
//! LLVM as `rustc` would send them; the rest never reach LLVM.
//!
//! This is `rustc`'s partitioning query, replaced: `rustc` partitions as it
//! always does, then every cold function is lowered to IR (in parallel, each
//! on its own), and taken out of LLVM's codegen units. Cranelift compiles
//! the cold module on a thread of its own while LLVM runs, and its object
//! joins LLVM's. A cold function that calls a `#[inline]` function gets its
//! own copy, as LLVM's units do; a cold function that cannot be lowered, or
//! whose copies cannot, stays with LLVM. Functions the two halves call
//! across are given hidden external linkage, so they link inside the
//! program and nowhere else.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;

use rustc_hir::attrs::Linkage as RLinkage;
use rustc_middle::middle::exported_symbols::{ExportedSymbol, SymbolExportInfo, SymbolExportKind, SymbolExportLevel};
use rustc_middle::mono::{CodegenUnit, MonoItem, MonoItemData, MonoItemPartitions, Visibility};
use rustc_middle::query::LocalCrate;
use rustc_middle::ty::{Instance, TyCtxt};

use crate::consts::ModCx;
use crate::{Codegen, Result, func, unwind};

/// The functions a profile found hot, by symbol, and how to compile the
/// rest: set once per process, before `rustc` runs.
pub(crate) struct Tier {
    pub(crate) hot: HashSet<String>,
    pub(crate) codegen: Codegen,
}

pub(crate) static TIER: OnceLock<Tier> = OnceLock::new();

/// A hot `#[inline]` helper with at most this many callers is taken to run
/// its hot code for them (see `split`).
const FEW_CALLERS: usize = 2;

type PartitionFn = for<'tcx> fn(TyCtxt<'tcx>, ()) -> MonoItemPartitions<'tcx>;

/// `rustc`'s own partitioning, which [`partition`] wraps.
pub(crate) static DEFAULT_PARTITION: OnceLock<PartitionFn> = OnceLock::new();

/// The cold half, compiling: its object, once Cranelift is done, and its
/// trait-object facts. `None` when nothing was cold.
pub(crate) struct Cold {
    pub(crate) object: JoinHandle<Result<PathBuf, String>>,
    pub(crate) facts: unwind::Facts,
    pub(crate) functions: usize,
    pub(crate) llvm_functions: usize,
}

pub(crate) static COLD: Mutex<Option<Result<Cold, String>>> = Mutex::new(None);

/// Symbols in LLVM's half that the cold half calls or reads, and whether
/// each is data. LLVM's ThinLTO across this crate's units internalizes and
/// drops every symbol the crate does not export, so these are exported, as
/// `#[used]` items are: see [`exported`].
static CROSSING: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());

type ExportedFn = for<'tcx> fn(TyCtxt<'tcx>, LocalCrate) -> &'tcx [(ExportedSymbol<'tcx>, SymbolExportInfo)];

/// `rustc`'s own `exported_non_generic_symbols`, which [`exported`] wraps.
pub(crate) static DEFAULT_EXPORTED: OnceLock<ExportedFn> = OnceLock::new();

/// The replacement for `exported_non_generic_symbols`: `rustc`'s, and for
/// this crate, the symbols the cold half needs from LLVM's.
pub(crate) fn exported<'tcx>(tcx: TyCtxt<'tcx>, local: LocalCrate) -> &'tcx [(ExportedSymbol<'tcx>, SymbolExportInfo)] {
    let default = DEFAULT_EXPORTED.get().expect("the default is saved before it is replaced");
    let base = default(tcx, local);
    if TIER.get().is_none() {
        return base;
    }
    // The split decides what crosses.
    let _ = tcx.collect_and_partition_mono_items(());
    let crossing = CROSSING.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if crossing.is_empty() {
        return base;
    }
    let extra = crossing.into_iter().map(|(name, data)| {
        let info = SymbolExportInfo {
            level: SymbolExportLevel::Rust,
            kind: if data { SymbolExportKind::Data } else { SymbolExportKind::Text },
            used: true,
            rustc_std_internal_symbol: false,
        };
        (ExportedSymbol::NoDefId(rustc_middle::ty::SymbolName::new(tcx, &name)), info)
    });
    tcx.arena.alloc_from_iter(base.iter().copied().chain(extra))
}

/// The hot list in `ATHRAD_HOT`: one symbol per line.
pub(crate) fn load_hot(path: &std::path::Path) -> std::io::Result<HashSet<String>> {
    Ok(std::fs::read_to_string(path)?.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect())
}

/// The replacement for `collect_and_partition_mono_items`.
pub(crate) fn partition<'tcx>(tcx: TyCtxt<'tcx>, (): ()) -> MonoItemPartitions<'tcx> {
    let default = DEFAULT_PARTITION.get().expect("the default partition is saved before it is replaced");
    let parts = default(tcx, ());
    let Some(tier) = TIER.get() else { return parts };
    let _timer = tcx.prof.generic_activity("athrad_tier_split");
    match split(tcx, &parts, tier) {
        Ok((parts, cold)) => {
            *COLD.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ok(cold));
            parts
        }
        Err(e) => {
            *COLD.lock().unwrap_or_else(|e| e.into_inner()) = Some(Err(e));
            parts
        }
    }
}

/// One lowered function and what it needs: `None` if Athrad could not
/// lower it.
type Lowered = Option<(ir::Module, unwind::Facts)>;

fn lower_one<'tcx>(tcx: TyCtxt<'tcx>, instance: Instance<'tcx>, linkage: ir::Linkage) -> Lowered {
    let mut m = ModCx::new(tcx);
    func::lower(&mut m, instance, linkage).ok()?;
    let vtables = std::mem::take(&mut m.vtables);
    let facts = m.skippable.facts(tcx, &vtables);
    Some((m.finish(), facts))
}

fn lower_all<'tcx>(tcx: TyCtxt<'tcx>, work: Vec<(MonoItem<'tcx>, ir::Linkage)>) -> Vec<(MonoItem<'tcx>, Lowered)> {
    let _timer = tcx.prof.generic_activity("athrad_lower_to_ir");
    rustc_data_structures::sync::par_map(work, |(item, linkage)| {
        let MonoItem::Fn(instance) = item else { return (item, None) };
        (item, lower_one(tcx, instance, linkage))
    })
}

fn split<'tcx>(
    tcx: TyCtxt<'tcx>,
    parts: &MonoItemPartitions<'tcx>,
    tier: &Tier,
) -> Result<(MonoItemPartitions<'tcx>, Cold), String> {
    // Every item once, and by symbol: an `#[inline]` item is in each unit
    // that uses it, the same item with the same data.
    let mut items: Vec<(MonoItem<'tcx>, MonoItemData)> = Vec::new();
    let mut seen = HashSet::new();
    for cgu in parts.codegen_units {
        for (&item, &data) in cgu.items() {
            if seen.insert(item) {
                items.push((item, data));
            }
        }
    }
    let by_symbol: HashMap<String, (MonoItem<'tcx>, MonoItemData)> =
        items.iter().map(|&(item, data)| (item.symbol_name(tcx).name.to_owned(), (item, data))).collect();
    let main = tcx.entry_fn(()).map(|(def, _)| MonoItem::Fn(Instance::mono(tcx, def)));

    // Cold: a function the profile did not find hot, not `#[inline]`, not
    // `main` (whose unit gets the C `main`), with plain linkage.
    let mut work = Vec::new();
    for &(item, data) in &items {
        let MonoItem::Fn(_) = item else { continue };
        if data.inlined || Some(item) == main || tier.hot.contains(item.symbol_name(tcx).name) {
            continue;
        }
        if !matches!(data.linkage, RLinkage::Internal | RLinkage::External) {
            continue;
        }
        let linkage = match (data.linkage, data.visibility) {
            (RLinkage::External, Visibility::Default | Visibility::Protected) => ir::Linkage::Export,
            _ => ir::Linkage::Hidden,
        };
        work.push((item, linkage));
    }
    let cold_roots: HashSet<MonoItem<'tcx>> = work.iter().map(|&(item, _)| item).collect();

    let mut lowered: HashMap<MonoItem<'tcx>, Lowered> = lower_all(tcx, work).into_iter().collect();
    // Why cold functions went back to LLVM, for `ATHRAD_TIME`.
    let failed_to_lower = lowered.values().filter(|l| l.is_none()).count();

    // Copies of the `#[inline]` functions the cold ones call, as LLVM's
    // units have their own, until no lowered function imports one.
    loop {
        let mut more = Vec::new();
        let mut queued = HashSet::new();
        for (module, _) in lowered.values().flatten() {
            for import in &module.imports {
                if let Some(&(item, data)) = by_symbol.get(&import.name)
                    && data.inlined
                    && !lowered.contains_key(&item)
                    && queued.insert(item)
                {
                    more.push((item, ir::Linkage::Local));
                }
            }
        }
        if more.is_empty() {
            break;
        }
        lowered.extend(lower_all(tcx, more));
    }

    // How many functions call each function, from the collector's graph.
    let mut callers: HashMap<MonoItem<'tcx>, usize> = HashMap::new();
    for &(item, _) in &items {
        let MonoItem::Fn(instance) = item else { continue };
        let Ok((used, _)) = tcx.items_of_instance((instance, rustc_middle::mono::CollectionMode::UsedItems)) else {
            continue;
        };
        let mut seen_callee = HashSet::new();
        for callee in used.iter().map(|s| s.node) {
            if seen_callee.insert(callee) {
                *callers.entry(callee).or_default() += 1;
            }
        }
    }

    // A cold function that calls a hot `#[inline]` function, directly or
    // through other copies, stays with LLVM: LLVM would inline the hot code
    // into it, and its own count says nothing about that code (a function
    // run once can hold the whole hot loop, as `Map::fold` does). Only
    // helpers with at most [`FEW_CALLERS`] callers count: one specialized to
    // its caller's closure runs its loop for that caller, while `Vec::push`
    // is hot because thousands of functions call it a few times each.
    let mut reaches_hot: HashMap<String, bool> = HashMap::new();
    let mut held_hot_inline = 0usize;
    let imports_of: HashMap<String, Vec<String>> = lowered
        .iter()
        .filter(|(item, _)| !cold_roots.contains(item))
        .filter_map(|(item, l)| {
            let (module, _) = l.as_ref()?;
            Some((item.symbol_name(tcx).name.to_owned(), module.imports.iter().map(|i| i.name.clone()).collect()))
        })
        .collect();
    let few_callers = |name: &str| -> bool {
        by_symbol.get(name).is_some_and(|(item, _)| callers.get(item).copied().unwrap_or(0) <= FEW_CALLERS)
    };
    fn hot_through(
        name: &str,
        hot: &HashSet<String>,
        imports_of: &HashMap<String, Vec<String>>,
        few_callers: &dyn Fn(&str) -> bool,
        memo: &mut HashMap<String, bool>,
    ) -> bool {
        if let Some(&known) = memo.get(name) {
            return known;
        }
        memo.insert(name.to_owned(), false); // a cycle adds nothing
        let result = few_callers(name)
            && (hot.contains(name)
                || imports_of
                    .get(name)
                    .is_some_and(|is| is.iter().any(|i| hot_through(i, hot, imports_of, few_callers, memo))));
        memo.insert(name.to_owned(), result);
        result
    }
    for (item, l) in lowered.iter_mut() {
        if !cold_roots.contains(item) {
            continue;
        }
        let Some((module, _)) = l else { continue };
        let calls_hot_inline = module.imports.iter().any(|i| {
            by_symbol.get(&i.name).is_some_and(|(_, d)| d.inlined)
                && hot_through(&i.name, &tier.hot, &imports_of, &few_callers, &mut reaches_hot)
        });
        if calls_hot_inline {
            *l = None;
            held_hot_inline += 1;
        }
    }

    let mut held_sole_caller = 0usize;
    // LLVM inlines a function with one caller into that caller, whatever
    // its size, and that is often where a hot loop meets what it knows about
    // its data (`spec_extend` inlining `extend_trusted`: the vector has room
    // already). So a cold function that is the only caller of a function in
    // LLVM's half that is hot, or that stays in LLVM by these rules, stays
    // in LLVM too, and so on up a chain of single callers.
    loop {
        // In LLVM's half, and worth inlining into a sole caller.
        let llvm_hot = |name: &str, lowered: &HashMap<MonoItem<'tcx>, Lowered>| -> bool {
            let Some(&(item, data)) = by_symbol.get(name) else { return false };
            if data.inlined {
                return false; // copies are handled above
            }
            tier.hot.contains(name) || (cold_roots.contains(&item) && lowered.get(&item).is_some_and(Option::is_none))
        };
        let mut moved = Vec::new();
        for (item, l) in &lowered {
            if !cold_roots.contains(item) {
                continue;
            }
            let Some((module, _)) = l else { continue };
            let sole_caller_of_hot = module.imports.iter().any(|i| {
                llvm_hot(&i.name, &lowered)
                    && by_symbol.get(&i.name).is_some_and(|(callee, _)| callers.get(callee).copied().unwrap_or(0) <= 1)
            });
            if sole_caller_of_hot {
                moved.push(*item);
            }
        }
        if moved.is_empty() {
            break;
        }
        held_sole_caller += moved.len();
        for item in moved {
            lowered.insert(item, None);
        }
    }

    // A function that needs a copy Athrad could not make stays with LLVM,
    // and so on up its callers.
    loop {
        let failed: HashSet<String> = lowered
            .iter()
            .filter(|(_, l)| l.is_none())
            .map(|(item, _)| item.symbol_name(tcx).name.to_owned())
            .collect();
        let mut changed = false;
        for l in lowered.values_mut() {
            let Some((module, _)) = l else { continue };
            let needs_failed_copy = module
                .imports
                .iter()
                .any(|i| failed.contains(&i.name) && by_symbol.get(&i.name).is_some_and(|(_, d)| d.inlined));
            if needs_failed_copy {
                *l = None;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let cold: HashSet<MonoItem<'tcx>> =
        lowered.iter().filter(|(item, l)| l.is_some() && cold_roots.contains(item)).map(|(&item, _)| item).collect();
    if let Some(dir) = std::env::var_os("ATHRAD_TIER_DUMP") {
        // Each cold function, `#[inline]` copies marked `+`, with what it imports.
        let mut lines = Vec::new();
        for (item, l) in &lowered {
            let Some((module, _)) = l else { continue };
            if cold_roots.contains(item) && !cold.contains(item) {
                continue;
            }
            let mark = if cold_roots.contains(item) { "" } else { "+" };
            lines.push(format!("{mark}{}", item.symbol_name(tcx).name));
            for i in &module.imports {
                lines.push(format!("    {}", i.name));
            }
        }
        let crate_name = tcx.crate_name(rustc_hir::def_id::LOCAL_CRATE);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(std::path::Path::new(&dir).join(format!("{crate_name}.graph")), lines.join("\n"));
    }
    let mut modules = Vec::new();
    let mut facts = Vec::new();
    let mut functions = 0;
    for (item, l) in lowered {
        if let Some((module, fact)) = l
            && (cold.contains(&item) || !cold_roots.contains(&item))
        {
            functions += module.functions.len();
            modules.push(module);
            facts.push(fact);
        }
    }
    let module = crate::merge_modules(modules).map_err(|e| format!("{e:#}"))?;
    // For looking inside: which functions went to Cranelift, one file per crate.
    if let Some(dir) = std::env::var_os("ATHRAD_TIER_DUMP") {
        let names: Vec<&str> = module.functions.iter().map(|f| f.name.as_str()).collect();
        let crate_name = tcx.crate_name(rustc_hir::def_id::LOCAL_CRATE);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(std::path::Path::new(&dir).join(format!("{crate_name}.cold")), names.join("\n"));
    }
    ir::verify(&module).map_err(|e| format!("Athrad's cold half: {e}"))?;

    // What the cold half calls in LLVM's units must be visible to it.
    let imported: HashSet<&str> = module.imports.iter().map(|i| i.name.as_str()).collect();

    // `#[inline]` copies LLVM's units still need: what the functions and
    // statics LLVM compiles use, and what that uses in turn. The rest were
    // there only for functions that went to Cranelift, which has its own.
    let mut needed: HashSet<MonoItem<'tcx>> = HashSet::new();
    let mut stack: Vec<MonoItem<'tcx>> = Vec::new();
    for &(item, data) in &items {
        if data.inlined || cold.contains(&item) {
            continue;
        }
        match item {
            MonoItem::Fn(_) => stack.push(item),
            MonoItem::Static(def_id) => {
                if let Ok(alloc) = tcx.eval_static_initializer(def_id) {
                    functions_in_alloc(tcx, alloc.inner(), &mut stack, &mut HashSet::new());
                }
            }
            MonoItem::GlobalAsm(_) => {}
        }
    }
    while let Some(item) = stack.pop() {
        if !needed.insert(item) {
            continue;
        }
        let MonoItem::Fn(instance) = item else { continue };
        if let Ok((used, _)) = tcx.items_of_instance((instance, rustc_middle::mono::CollectionMode::UsedItems)) {
            stack.extend(used.iter().map(|s| s.node));
        }
    }

    let mut units = Vec::with_capacity(parts.codegen_units.len());
    let mut llvm_functions = 0;
    let mut crossing = Vec::new();
    let (mut llvm_inline, mut llvm_hot, mut llvm_held) = (0usize, 0usize, 0usize);
    let mut seen_crossing = HashSet::new();
    for cgu in parts.codegen_units {
        let mut unit = CodegenUnit::new(cgu.name());
        if cgu.is_primary() {
            unit.make_primary();
        }
        for (&item, &data) in cgu.items() {
            if cold.contains(&item) || (data.inlined && !needed.contains(&item)) {
                continue;
            }
            let mut data = data;
            let name = item.symbol_name(tcx).name;
            if !data.inlined && imported.contains(name) {
                if data.linkage == RLinkage::Internal {
                    data.linkage = RLinkage::External;
                    data.visibility = Visibility::Hidden;
                }
                if seen_crossing.insert(name) {
                    crossing.push((name.to_owned(), matches!(item, MonoItem::Static(_))));
                }
            }
            llvm_functions += usize::from(matches!(item, MonoItem::Fn(_)));
            if matches!(item, MonoItem::Fn(_)) {
                if data.inlined {
                    llvm_inline += 1;
                } else if tier.hot.contains(name) {
                    llvm_hot += 1;
                } else {
                    llvm_held += 1;
                }
            }
            unit.items_mut().insert(item, data);
        }
        unit.compute_size_estimate();
        units.push(unit);
    }

    *CROSSING.lock().unwrap_or_else(|e| e.into_inner()) = crossing;
    if std::env::var_os("ATHRAD_TIME").is_some() {
        eprintln!(
            "athrad: {}: LLVM gets {llvm_hot} hot functions, {llvm_held} held ({failed_to_lower} not lowered, \
             {held_hot_inline} reach hot #[inline] code, {held_sole_caller} sole callers), {llvm_inline} #[inline] copies",
            tcx.crate_name(rustc_hir::def_id::LOCAL_CRATE)
        );
    }
    let codegen = tier.codegen.clone();
    let prof = tcx.prof.clone();
    let object = std::thread::spawn(move || {
        let _timer = prof.generic_activity("athrad_cranelift_codegen");
        codegen(&module, "athrad-cold").map_err(|e| format!("{e:#}"))
    });
    let cold = Cold { object, facts: unwind::Facts::merged(&facts), functions, llvm_functions };
    let parts =
        MonoItemPartitions { codegen_units: tcx.arena.alloc_from_iter(units), all_mono_items: parts.all_mono_items };
    Ok((parts, cold))
}

/// The functions a static's initializer points to, through the allocations
/// and vtables it points to.
fn functions_in_alloc<'tcx>(
    tcx: TyCtxt<'tcx>,
    alloc: &rustc_middle::mir::interpret::Allocation,
    out: &mut Vec<MonoItem<'tcx>>,
    seen: &mut HashSet<rustc_middle::mir::interpret::AllocId>,
) {
    use rustc_middle::mir::interpret::GlobalAlloc;
    for &(_, prov) in alloc.provenance().ptrs().iter() {
        let id = prov.alloc_id();
        if !seen.insert(id) {
            continue;
        }
        match tcx.global_alloc(id) {
            GlobalAlloc::Function { instance } => out.push(MonoItem::Fn(instance)),
            GlobalAlloc::Memory(inner) => functions_in_alloc(tcx, inner.inner(), out, seen),
            GlobalAlloc::VTable(ty, dyn_ty) => {
                let principal = dyn_ty.principal().map(|p| tcx.instantiate_bound_regions_with_erased(p));
                let vtable = tcx.vtable_allocation((ty, principal));
                if seen.insert(vtable) {
                    functions_in_alloc(tcx, tcx.global_alloc(vtable).unwrap_memory().inner(), out, seen);
                }
            }
            GlobalAlloc::Static(_) | GlobalAlloc::TypeId { .. } => {}
        }
    }
}
