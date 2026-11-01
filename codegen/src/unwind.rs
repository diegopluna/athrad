//! Unwind tables, so a panic can unwind through Athrad's frames to the
//! `catch_unwind` in `std::rt::lang_start` and exit with code 101, as a
//! `rustc` binary does.
//!
//! Athrad's frames have no landing pads yet (the frontend rejects programs
//! whose unwinding would run destructors), so the tables only describe how
//! to step out of each frame: no personality routine, no LSDA.
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
use object::write::{Relocation, StandardSection};
use object::{RelocationEncoding, RelocationFlags, RelocationKind, SectionKind};

/// One defined function's unwind info and code size.
pub struct Entry {
    pub func: FuncId,
    pub info: UnwindInfo,
    pub size: u32,
}

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
    cie.fde_address_encoding = DwEhPe(gimli::DW_EH_PE_pcrel.0 | gimli::DW_EH_PE_sdata4.0);
    let mut table = FrameTable::default();
    let cie = table.add_cie(cie);
    for e in entries {
        let UnwindInfo::SystemV(info) = e.info else { bail!("expected System V unwind info") };
        table.add_fde(cie, info.to_fde(Address::Symbol { symbol: e.func.as_u32() as usize, addend: 0 }));
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
    let section_relative = product.object.format() != object::BinaryFormat::MachO;
    for r in relocs {
        let sym = product.function_symbol(FuncId::from_u32(r.func as u32));
        let (symbol, offset) =
            if section_relative { product.object.symbol_section_and_offset(sym).unwrap_or((sym, 0)) } else { (sym, 0) };
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
