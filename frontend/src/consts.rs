//! Constants, statics, and the allocations they point into, as IR data.

use std::collections::HashMap;

use rustc_abi::Size;
use rustc_hir::def_id::DefId;
use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags;
use rustc_middle::mir::interpret::{AllocId, GlobalAlloc, read_target_uint};
use rustc_middle::ty::layout::FnAbiOf;
use rustc_middle::ty::{self, Instance, Ty, TyCtxt};

use crate::cx::{Cx, signature};
use crate::{Result, unsupported};

/// Module-wide state while lowering one crate: the IR being built, and every
/// symbol the code refers to, so the undefined ones become imports.
pub(crate) struct ModCx<'tcx> {
    pub(crate) tcx: TyCtxt<'tcx>,
    pub(crate) module: ir::Module,
    /// Functions referred to by call or address, with their signatures.
    funcs: HashMap<String, ir::Signature>,
    /// Data symbols referred to that may live in another crate.
    datas: Vec<String>,
    allocs: HashMap<AllocId, String>,
    /// Which drops a panic may skip; see `unwind.rs`.
    pub(crate) skippable: crate::unwind::Skippable<'tcx>,
    /// The concrete type and principal trait of every vtable the program
    /// builds.
    pub(crate) vtables: Vec<(Ty<'tcx>, Option<DefId>)>,
    /// Thread-local statics referred to.
    thread_locals: std::collections::BTreeSet<String>,
}

/// What an address constant points at.
pub(crate) enum Addr {
    Symbol(String, i64),
    /// No symbol, such as the dangling address of an empty allocation.
    Int(u64),
}

impl<'tcx> ModCx<'tcx> {
    pub(crate) fn new(tcx: TyCtxt<'tcx>) -> Self {
        ModCx {
            tcx,
            module: ir::Module::new(),
            funcs: HashMap::new(),
            datas: Vec::new(),
            allocs: HashMap::new(),
            skippable: Default::default(),
            vtables: Vec::new(),
            thread_locals: Default::default(),
        }
    }

    pub(crate) fn cx(&self) -> Cx<'tcx> {
        Cx(self.tcx)
    }

    /// The symbol of `instance`, noting it is used with this signature.
    pub(crate) fn func_symbol(&mut self, instance: Instance<'tcx>) -> Result<(String, ir::Signature)> {
        let name = self.tcx.symbol_name(instance).name.to_owned();
        if let Some(sig) = self.funcs.get(&name) {
            return Ok((name, sig.clone()));
        }
        let abi = self.cx().fn_abi_of_instance(instance, ty::List::empty());
        let sig = signature(self.cx(), abi)?;
        self.funcs.insert(name.clone(), sig.clone());
        Ok((name, sig))
    }

    /// Note a function used by name with a signature built by hand.
    pub(crate) fn use_func(&mut self, name: &str, sig: ir::Signature) {
        self.funcs.entry(name.to_owned()).or_insert(sig);
    }

    /// The symbol of a static, defined by this crate or another.
    pub(crate) fn static_symbol(&mut self, def_id: DefId) -> Result<String> {
        let attrs = self.tcx.codegen_fn_attrs(def_id);
        if attrs.import_linkage.is_some() {
            return Err(unsupported("statics with `#[linkage]`"));
        }
        let name = self.tcx.symbol_name(Instance::mono(self.tcx, def_id)).name.to_owned();
        if attrs.flags.contains(CodegenFnAttrFlags::THREAD_LOCAL) {
            self.thread_locals.insert(name.clone());
        } else if !self.datas.contains(&name) {
            self.datas.push(name.clone());
        }
        Ok(name)
    }

    /// Where a pointer into `alloc_id` points, before its offset.
    pub(crate) fn alloc_addr(&mut self, alloc_id: AllocId) -> Result<Addr> {
        Ok(match self.tcx.global_alloc(alloc_id) {
            GlobalAlloc::Memory(alloc) if alloc.inner().len() == 0 => Addr::Int(alloc.inner().align.bytes()),
            GlobalAlloc::Memory(_) => Addr::Symbol(self.alloc_symbol(alloc_id)?, 0),
            GlobalAlloc::Function { instance } => Addr::Symbol(self.func_symbol(instance)?.0, 0),
            GlobalAlloc::VTable(ty, dyn_ty) => {
                self.vtables.push((ty, dyn_ty.principal_def_id()));
                let principal = dyn_ty.principal().map(|p| self.tcx.instantiate_bound_regions_with_erased(p));
                let vtable = self.tcx.vtable_allocation((ty, principal));
                Addr::Symbol(self.alloc_symbol(vtable)?, 0)
            }
            GlobalAlloc::Static(def_id) => Addr::Symbol(self.static_symbol(def_id)?, 0),
            GlobalAlloc::TypeId { .. } => Addr::Int(0),
        })
    }

    /// The data symbol holding a memory allocation, defined on first use.
    /// The symbol of a constant allocation. Its name comes from its contents,
    /// so equal immutable allocations share one symbol.
    fn alloc_symbol(&mut self, alloc_id: AllocId) -> Result<String> {
        if let Some(name) = self.allocs.get(&alloc_id) {
            if name.is_empty() {
                return Err(unsupported("constants that point at themselves"));
            }
            return Ok(name.clone());
        }
        let GlobalAlloc::Memory(alloc) = self.tcx.global_alloc(alloc_id) else {
            unreachable!("only memory allocations get data symbols")
        };
        // Marks the allocation as in progress, to catch a cycle.
        self.allocs.insert(alloc_id, String::new());
        let mut data = self.data_for(alloc.inner(), String::new(), ir::Linkage::Local)?;
        let mut h = <sha2::Sha256 as sha2::Digest>::new();
        sha2::Digest::update(&mut h, data.align.to_le_bytes());
        sha2::Digest::update(&mut h, &data.bytes);
        for r in &data.relocs {
            sha2::Digest::update(&mut h, r.offset.to_le_bytes());
            sha2::Digest::update(&mut h, r.symbol.as_bytes());
            sha2::Digest::update(&mut h, [0]);
            sha2::Digest::update(&mut h, r.addend.to_le_bytes());
        }
        let digest = sha2::Digest::finalize(h);
        let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
        let name = if data.writable {
            // A mutable allocation is its own, however alike its bytes are.
            // The id, not a counter, so two threads lowering the same one
            // agree on its symbol.
            format!("__athrad_alloc_{hex}_{}", alloc_id.0)
        } else {
            format!("__athrad_alloc_{hex}")
        };
        self.allocs.insert(alloc_id, name.clone());
        if self.module.data.iter().all(|d| d.name != name) {
            data.name = name.clone();
            self.module.push_data(data);
        }
        Ok(name)
    }

    /// Define the static `def_id` of this crate.
    pub(crate) fn define_static(&mut self, def_id: DefId, linkage: ir::Linkage) -> Result<()> {
        let name = self.static_symbol(def_id)?;
        let alloc =
            self.tcx.eval_static_initializer(def_id).map_err(|_| unsupported("a static whose initializer failed"))?;
        let mut data = self.data_for(alloc.inner(), name, linkage)?;
        data.tls = self.tcx.codegen_fn_attrs(def_id).flags.contains(CodegenFnAttrFlags::THREAD_LOCAL);
        self.module.push_data(data);
        Ok(())
    }

    fn data_for(
        &mut self,
        alloc: &rustc_middle::mir::interpret::Allocation,
        name: String,
        linkage: ir::Linkage,
    ) -> Result<ir::Data> {
        let bytes = alloc.inspect_with_uninit_and_ptr_outside_interpreter(0..alloc.len()).to_vec();
        let mut relocs = Vec::new();
        let ptr_size = self.tcx.data_layout.pointer_size().bytes() as usize;
        let mut out = bytes.clone();
        for &(offset, prov) in alloc.provenance().ptrs().iter() {
            let at = offset.bytes() as usize;
            let addend = read_target_uint(self.tcx.data_layout.endian, &bytes[at..at + ptr_size])
                .expect("pointer-sized slice") as i64;
            match self.alloc_addr(prov.alloc_id())? {
                Addr::Symbol(symbol, base) => {
                    out[at..at + ptr_size].fill(0);
                    relocs.push(ir::Reloc { offset: at as u32, symbol, addend: base + addend });
                }
                Addr::Int(base) => {
                    out[at..at + ptr_size].copy_from_slice(&(base as i64 + addend).to_le_bytes()[..ptr_size]);
                }
            }
        }
        Ok(ir::Data {
            name,
            linkage,
            writable: alloc.mutability.is_mut(),
            tls: false,
            align: alloc.align.bytes(),
            bytes: out,
            relocs,
        })
    }

    /// Declare every symbol used but not defined as an import.
    pub(crate) fn finish(mut self) -> ir::Module {
        let mut funcs: Vec<(String, ir::Signature)> = self.funcs.drain().collect();
        funcs.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, sig) in funcs {
            if self.module.function(&name).is_none() {
                self.module.import_func(&name, sig);
            }
        }
        for name in std::mem::take(&mut self.thread_locals) {
            if !self.module.data.iter().any(|d| d.name == name) {
                self.module.import_thread_local(&name);
            }
        }
        for name in std::mem::take(&mut self.datas) {
            if !self.module.data.iter().any(|d| d.name == name) {
                self.module.import_data(&name);
            }
        }
        self.module
    }
}

/// `size` as an `i64` offset.
pub(crate) fn off(size: Size) -> i64 {
    i64::try_from(size.bytes()).expect("offsets fit in i64")
}
