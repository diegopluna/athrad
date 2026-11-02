//! Unwind tables, so a panic can unwind through Athrad's frames to the
//! `catch_unwind` in `std::rt::lang_start` and exit with code 101, as a
//! `rustc` binary does.
//!
//! A function with landing pads (`try_call`s) also gets an LSDA, the table
//! `rust_eh_personality` reads to find which landing pad covers a call, and
//! whether it cleans up or catches; its FDE names that personality.
//! Functions without landing pads have neither, and unwinding passes through.
//!
//! * ELF and Mach-O: a DWARF `.eh_frame`, as `rustc_codegen_cranelift`
//!   writes it. `cranelift-object` can write one for ELF but not Mach-O, so
//!   Athrad writes its own for both.
//! * COFF (Windows x64): `.pdata` and `.xdata`, which Windows' unwinder reads.

use anyhow::{Result, anyhow, bail};
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::isa::unwind::UnwindInfo;
use cranelift_module::FuncId;
use cranelift_object::ObjectProduct;
use gimli::write::{Address, EhFrame, EndianVec, FrameTable, Writer};
use gimli::{DwEhPe, RunTimeEndian};
use object::write::{Relocation, StandardSection, SymbolId};
use object::{RelocationEncoding, RelocationFlags, RelocationKind, SectionKind};

/// One defined function's unwind info and code size.
pub struct Entry {
    pub func: FuncId,
    pub info: UnwindInfo,
    pub size: u32,
    pub call_sites: Vec<crate::cache::CallSite>,
}

/// Address symbols at or above this are `extra` symbols, not functions.
const EXTRA: usize = 1 << 31;
/// On Mach-O, the personality, reached through its GOT entry: the last
/// `extra` symbol pushed before the CIE.
const GOT: usize = usize::MAX;

/// Add unwind sections for `entries` to `product`.
pub fn emit(isa: &dyn TargetIsa, product: &mut ObjectProduct, entries: Vec<Entry>) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    match product.object.format() {
        object::BinaryFormat::Elf | object::BinaryFormat::MachO => eh_frame(isa, product, entries),
        object::BinaryFormat::Coff => pdata(product, entries),
        other => bail!("no unwind tables for {other:?}"),
    }
}

fn eh_frame(isa: &dyn TargetIsa, product: &mut ObjectProduct, entries: Vec<Entry>) -> Result<()> {
    let Some(mut cie) = isa.create_systemv_cie() else { bail!("{} has no System V CIE", isa.triple()) };
    // Position independent: FDEs point at their functions PC-relative.
    let pcrel = DwEhPe(gimli::DW_EH_PE_pcrel.0 | gimli::DW_EH_PE_sdata4.0);
    cie.fde_address_encoding = pcrel;
    let mut table = FrameTable::default();
    let plain = table.add_cie(cie);
    let macho = product.object.format() == object::BinaryFormat::MachO;

    // Symbols the tables point at besides functions: the personality's
    // indirection cell and each LSDA.
    let mut extra: Vec<SymbolId> = Vec::new();
    let mut with_pads = None;
    let mut lsda_section = None;
    let mut lsda_bytes: Vec<u8> = Vec::new();
    for e in entries {
        let UnwindInfo::SystemV(info) = e.info else { bail!("expected System V unwind info") };
        let mut fde = info.to_fde(Address::Symbol { symbol: e.func.as_u32() as usize, addend: 0 });
        if e.call_sites.is_empty() {
            table.add_fde(plain, fde);
            continue;
        }
        let cie = match with_pads {
            Some(c) => c,
            None => {
                let c = personality_cie(isa, product, &mut table, &mut extra, pcrel)?;
                with_pads = Some(c);
                c
            }
        };
        let section = *lsda_section.get_or_insert_with(|| {
            let (segment, name): (&[u8], &[u8]) =
                if macho { (b"__TEXT", b"__gcc_except_tab") } else { (b"", b".gcc_except_table") };
            product.object.add_section(segment.to_vec(), name.to_vec(), SectionKind::ReadOnlyData)
        });
        while !lsda_bytes.len().is_multiple_of(4) {
            lsda_bytes.push(0);
        }
        let at = lsda_bytes.len();
        lsda_bytes.extend(lsda(&e.call_sites));
        let sym = product.object.add_symbol(object::write::Symbol {
            name: format!("GCC_except_table{}", extra.len()).into_bytes(),
            value: at as u64,
            size: 0,
            kind: object::SymbolKind::Data,
            scope: object::SymbolScope::Compilation,
            weak: false,
            section: object::write::SymbolSection::Section(section),
            flags: object::SymbolFlags::None,
        });
        extra.push(sym);
        fde.lsda = Some(Address::Symbol { symbol: EXTRA + extra.len() - 1, addend: 0 });
        table.add_fde(cie, fde);
    }
    if let Some(section) = lsda_section {
        product.object.section_mut(section).set_data(lsda_bytes, 4);
    }

    let endian = match isa.endianness() {
        cranelift_codegen::ir::Endianness::Little => RunTimeEndian::Little,
        cranelift_codegen::ir::Endianness::Big => RunTimeEndian::Big,
    };
    let mut eh = EhFrame(Relocating { out: EndianVec::new(endian), relocs: Vec::new() });
    table.write_eh_frame(&mut eh).map_err(|e| anyhow!("writing .eh_frame: {e}"))?;
    let Relocating { out, relocs } = eh.0;

    let section = product.object.section_id(StandardSection::EhFrame);
    product.object.section_mut(section).set_data(out.into_vec(), 8);
    // ELF linkers want relocations against the section symbol; Mach-O's
    // against the function's own symbol.
    for r in relocs {
        if r.func == GOT {
            let personality = *extra.first().expect("the personality comes first");
            let (r_type, pcrel) = if isa.triple().architecture == target_lexicon::Architecture::X86_64 {
                (object::macho::X86_64_RELOC_GOT, true)
            } else {
                (object::macho::ARM64_RELOC_POINTER_TO_GOT, true)
            };
            product.object.add_relocation(
                section,
                Relocation {
                    offset: r.offset,
                    symbol: personality,
                    addend: r.addend,
                    flags: RelocationFlags::MachO { r_type, r_pcrel: pcrel, r_length: 2 },
                },
            )?;
            continue;
        }
        let (symbol, offset) = if r.func >= EXTRA {
            (extra[r.func - EXTRA], 0)
        } else {
            let sym = product.function_symbol(FuncId::from_u32(r.func as u32));
            if macho { (sym, 0) } else { product.object.symbol_section_and_offset(sym).unwrap_or((sym, 0)) }
        };
        product.object.add_relocation(
            section,
            Relocation {
                offset: r.offset,
                symbol,
                addend: offset as i64 + r.addend,
                flags: RelocationFlags::Generic {
                    kind: r.kind,
                    encoding: RelocationEncoding::Generic,
                    size: r.size * 8,
                },
            },
        )?;
    }
    Ok(())
}

/// A CIE naming `rust_eh_personality`, through a pointer-sized cell that
/// holds its address (`DW.ref.rust_eh_personality`), as LLVM and
/// `rustc_codegen_cranelift` refer to it, so the personality may live in
/// another module.
fn personality_cie(
    isa: &dyn TargetIsa,
    product: &mut ObjectProduct,
    table: &mut FrameTable,
    extra: &mut Vec<SymbolId>,
    pcrel: DwEhPe,
) -> Result<gimli::write::CieId> {
    let personality = product.object.add_symbol(object::write::Symbol {
        name: b"rust_eh_personality".to_vec(),
        value: 0,
        size: 0,
        kind: object::SymbolKind::Text,
        scope: object::SymbolScope::Dynamic,
        weak: false,
        section: object::write::SymbolSection::Undefined,
        flags: object::SymbolFlags::None,
    });
    if product.object.format() == object::BinaryFormat::MachO {
        // ld64 wants the personality through its GOT entry, as LLVM writes
        // it on Darwin (`_rust_eh_personality@GOT-.`); see `GOT`.
        extra.push(personality);
        let Some(mut cie) = isa.create_systemv_cie() else { bail!("{} has no System V CIE", isa.triple()) };
        cie.fde_address_encoding = pcrel;
        let indirect = DwEhPe(gimli::DW_EH_PE_indirect.0 | gimli::DW_EH_PE_pcrel.0 | gimli::DW_EH_PE_sdata4.0);
        cie.personality = Some((indirect, Address::Symbol { symbol: GOT, addend: 0 }));
        cie.lsda_encoding = Some(pcrel);
        return Ok(table.add_cie(cie));
    }
    let data = product.object.section_id(StandardSection::Data);
    let at = product.object.append_section_data(data, &[0; 8], 8);
    let cell = product.object.add_symbol(object::write::Symbol {
        name: b"DW.ref.rust_eh_personality".to_vec(),
        value: at,
        size: 8,
        kind: object::SymbolKind::Data,
        scope: object::SymbolScope::Compilation,
        weak: false,
        section: object::write::SymbolSection::Section(data),
        flags: object::SymbolFlags::None,
    });
    product.object.add_relocation(
        data,
        Relocation {
            offset: at,
            symbol: personality,
            addend: 0,
            flags: RelocationFlags::Generic {
                kind: RelocationKind::Absolute,
                encoding: RelocationEncoding::Generic,
                size: 64,
            },
        },
    )?;
    extra.push(cell);
    let Some(mut cie) = isa.create_systemv_cie() else { bail!("{} has no System V CIE", isa.triple()) };
    cie.fde_address_encoding = pcrel;
    let size = if isa.triple().architecture == target_lexicon::Architecture::X86_64 {
        gimli::DW_EH_PE_sdata4
    } else {
        gimli::DW_EH_PE_sdata8
    };
    let indirect = DwEhPe(gimli::DW_EH_PE_indirect.0 | gimli::DW_EH_PE_pcrel.0 | size.0);
    cie.personality = Some((indirect, Address::Symbol { symbol: EXTRA + extra.len() - 1, addend: 0 }));
    cie.lsda_encoding = Some(pcrel);
    Ok(table.add_cie(cie))
}

/// The LSDA of one function: every call site, in order, with its landing
/// pad (0 for none: unwinding goes on) and action (0 to clean up, 1 for the
/// catch-all action that `catch_unwind` needs). Without a type table:
/// `rust_eh_personality` treats any non-zero type filter as a catch.
fn lsda(sites: &[crate::cache::CallSite]) -> Vec<u8> {
    let uleb = |out: &mut Vec<u8>, mut v: u64| loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    };
    let mut sites: Vec<&crate::cache::CallSite> = sites.iter().collect();
    sites.sort_by_key(|s| s.ret_addr);
    let mut table = Vec::new();
    for s in &sites {
        uleb(&mut table, u64::from(s.ret_addr) - 1);
        uleb(&mut table, 1);
        let (pad, action) = match s.pad {
            None => (0, 0),
            Some((false, pad)) => (u64::from(pad), 0),
            Some((true, pad)) => (u64::from(pad), 1),
        };
        uleb(&mut table, pad);
        uleb(&mut table, action);
    }
    let mut out = vec![gimli::DW_EH_PE_omit.0, gimli::DW_EH_PE_omit.0, gimli::DW_EH_PE_uleb128.0];
    uleb(&mut out, table.len() as u64);
    out.extend(table);
    // The action table: one catch-all action (type filter 1, no next).
    out.extend([1, 0]);
    out
}

fn pdata(product: &mut ObjectProduct, entries: Vec<Entry>) -> Result<()> {
    let xdata = product.object.add_section(Vec::new(), b".xdata".to_vec(), SectionKind::ReadOnlyData);
    let pdata = product.object.add_section(Vec::new(), b".pdata".to_vec(), SectionKind::ReadOnlyData);
    let xdata_sym = product.object.section_symbol(xdata);
    let mut xbytes = Vec::new();
    let mut pbytes = Vec::new();
    let mut relocs = Vec::new();
    for e in entries {
        let UnwindInfo::WindowsX64(info) = e.info else { bail!("expected Windows x64 unwind info") };
        // UNWIND_INFO is 4-byte aligned.
        while xbytes.len() % 4 != 0 {
            xbytes.push(0);
        }
        let at = xbytes.len();
        xbytes.resize(at + info.emit_size(), 0);
        info.emit(&mut xbytes[at..]);

        // RUNTIME_FUNCTION: begin, end, and unwind info, as image-relative
        // addresses the linker fills in.
        let sym = product.function_symbol(e.func);
        let base = pbytes.len() as u64;
        pbytes.extend_from_slice(&[0; 12]);
        relocs.push((base, sym, 0));
        relocs.push((base + 4, sym, i64::from(e.size)));
        relocs.push((base + 8, xdata_sym, at as i64));
    }
    product.object.section_mut(xdata).set_data(xbytes, 4);
    product.object.section_mut(pdata).set_data(pbytes, 4);
    for (offset, symbol, addend) in relocs {
        product.object.add_relocation(
            pdata,
            Relocation {
                offset,
                symbol,
                addend,
                flags: RelocationFlags::Generic {
                    kind: RelocationKind::ImageOffset,
                    encoding: RelocationEncoding::Generic,
                    size: 32,
                },
            },
        )?;
    }
    Ok(())
}

struct Reloc {
    offset: u64,
    size: u8,
    func: usize,
    addend: i64,
    kind: RelocationKind,
}

/// A gimli writer that records symbol references as relocations.
struct Relocating {
    out: EndianVec<RunTimeEndian>,
    relocs: Vec<Reloc>,
}

impl Writer for Relocating {
    type Endian = RunTimeEndian;

    fn endian(&self) -> RunTimeEndian {
        self.out.endian()
    }

    fn len(&self) -> usize {
        self.out.len()
    }

    fn write(&mut self, bytes: &[u8]) -> gimli::write::Result<()> {
        self.out.write(bytes)
    }

    fn write_at(&mut self, offset: usize, bytes: &[u8]) -> gimli::write::Result<()> {
        self.out.write_at(offset, bytes)
    }

    fn write_address(&mut self, address: Address, size: u8) -> gimli::write::Result<()> {
        match address {
            Address::Constant(v) => self.write_udata(v, size),
            Address::Symbol { symbol, addend } => {
                let offset = self.len() as u64;
                self.relocs.push(Reloc { offset, size, func: symbol, addend, kind: RelocationKind::Absolute });
                self.write_udata(0, size)
            }
        }
    }

    fn write_eh_pointer(&mut self, address: Address, eh_pe: DwEhPe, size: u8) -> gimli::write::Result<()> {
        let Address::Symbol { symbol, addend } = address else {
            return self.write_address(address, size);
        };
        let (kind, size) = match (eh_pe.application(), eh_pe.format()) {
            (gimli::DW_EH_PE_pcrel, gimli::DW_EH_PE_sdata4) => (RelocationKind::Relative, 4),
            (gimli::DW_EH_PE_pcrel, gimli::DW_EH_PE_sdata8) => (RelocationKind::Relative, 8),
            (gimli::DW_EH_PE_absptr, _) => (RelocationKind::Absolute, size),
            _ => return Err(gimli::write::Error::UnsupportedPointerEncoding(eh_pe)),
        };
        let offset = self.len() as u64;
        self.relocs.push(Reloc { offset, size, func: symbol, addend, kind });
        self.write_udata(0, size)
    }
}
