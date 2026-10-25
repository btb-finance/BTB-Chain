//! Minimal ELF64 little-endian RISC-V (ET_EXEC) loader.
//!
//! No dependencies, no panics: every offset is bounds- and overflow-checked. Loading is
//! all-or-nothing: the whole file is validated before the first byte is written to the VM.
//! Exported symbols (global `STT_FUNC`) are the entry-point convention: one export per
//! query/mutation/cron/action.

use crate::vm::Vm;
use std::collections::BTreeMap;

const EM_RISCV: u16 = 243;
const ET_EXEC: u16 = 2;
const PT_LOAD: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const STT_FUNC: u8 = 2;
const STB_GLOBAL: u8 = 1;
const EHSIZE: usize = 64;
const PHENTSIZE: usize = 56;
const SHENTSIZE: usize = 64;
const SYMSIZE: usize = 24;

/// Why an ELF file was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// File ends before a header, table or segment it describes.
    Truncated,
    BadMagic,
    /// Not ELFCLASS64.
    BadClass,
    /// Not little-endian.
    BadEndian,
    /// Not ET_EXEC.
    BadType,
    /// Not EM_RISCV.
    BadMachine,
    /// Unsupported header field (version, entry size).
    BadHeader,
    /// Segment has filesz > memsz.
    BadSegment,
    /// Segment (or the entry point) lies outside VM memory.
    OutOfBounds,
    /// Two PT_LOAD segments overlap in memory.
    Overlap,
    /// An offset/size computation overflowed.
    Overflow,
    /// Symbol table or string table is malformed.
    BadSymbols,
}

/// Result of a successful load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub entry: u64,
    /// End (exclusive) of the highest PT_LOAD segment in memory; 0 if none.
    pub image_end: u64,
    /// Exported functions (global `STT_FUNC`, defined): name -> address.
    pub exports: BTreeMap<String, u64>,
}

fn slice(data: &[u8], off: u64, len: u64) -> Result<&[u8], ElfError> {
    let end = off.checked_add(len).ok_or(ElfError::Overflow)?;
    if end > data.len() as u64 {
        return Err(ElfError::Truncated);
    }
    Ok(&data[off as usize..end as usize])
}

fn u16_at(d: &[u8], o: usize) -> Result<u16, ElfError> {
    let b = d.get(o..o + 2).ok_or(ElfError::Truncated)?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}
fn u32_at(d: &[u8], o: usize) -> Result<u32, ElfError> {
    let b = d.get(o..o + 4).ok_or(ElfError::Truncated)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}
fn u64_at(d: &[u8], o: usize) -> Result<u64, ElfError> {
    let b = d.get(o..o + 8).ok_or(ElfError::Truncated)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(b);
    Ok(u64::from_le_bytes(a))
}

/// Parse global function symbols from the symbol table, if any. A name defined twice keeps the
/// last address; use `export_list` to see duplicates.
pub fn exports(data: &[u8]) -> Result<BTreeMap<String, u64>, ElfError> {
    Ok(export_list(data)?.into_iter().collect())
}

/// Like `exports`, but in symbol-table order and keeping duplicate names.
pub fn export_list(data: &[u8]) -> Result<Vec<(String, u64)>, ElfError> {
    let h = header(data)?;
    let mut out = Vec::new();
    for i in 0..h.shnum {
        let sh = slice(
            data,
            h.shoff
                .checked_add((i as u64) * SHENTSIZE as u64)
                .ok_or(ElfError::Overflow)?,
            SHENTSIZE as u64,
        )?;
        if u32_at(sh, 4)? != SHT_SYMTAB {
            continue;
        }
        let (off, size, link) = (u64_at(sh, 24)?, u64_at(sh, 32)?, u32_at(sh, 40)?);
        if link as usize >= h.shnum {
            return Err(ElfError::BadSymbols);
        }
        let strsh = slice(
            data,
            h.shoff
                .checked_add(link as u64 * SHENTSIZE as u64)
                .ok_or(ElfError::Overflow)?,
            SHENTSIZE as u64,
        )?;
        let strtab = slice(data, u64_at(strsh, 24)?, u64_at(strsh, 32)?)?;
        let syms = slice(data, off, size)?;
        for s in syms.as_chunks::<SYMSIZE>().0 {
            let name = u32_at(s, 0)? as usize;
            let info = s[4];
            let shndx = u16_at(s, 6)?;
            if info & 0xf != STT_FUNC || info >> 4 != STB_GLOBAL || shndx == 0 {
                continue;
            }
            let rest = strtab.get(name..).ok_or(ElfError::BadSymbols)?;
            let len = rest
                .iter()
                .position(|&b| b == 0)
                .ok_or(ElfError::BadSymbols)?;
            let name = std::str::from_utf8(&rest[..len]).map_err(|_| ElfError::BadSymbols)?;
            out.push((name.to_string(), u64_at(s, 8)?));
        }
    }
    Ok(out)
}

struct Header {
    entry: u64,
    phoff: u64,
    shoff: u64,
    phnum: usize,
    shnum: usize,
}

fn header(d: &[u8]) -> Result<Header, ElfError> {
    if d.len() < EHSIZE {
        return Err(ElfError::Truncated);
    }
    if d[..4] != [0x7f, b'E', b'L', b'F'] {
        return Err(ElfError::BadMagic);
    }
    if d[4] != 2 {
        return Err(ElfError::BadClass);
    }
    if d[5] != 1 {
        return Err(ElfError::BadEndian);
    }
    if d[6] != 1 || u32_at(d, 20)? != 1 {
        return Err(ElfError::BadHeader);
    }
    if u16_at(d, 16)? != ET_EXEC {
        return Err(ElfError::BadType);
    }
    if u16_at(d, 18)? != EM_RISCV {
        return Err(ElfError::BadMachine);
    }
    let phnum = u16_at(d, 56)? as usize;
    let shnum = u16_at(d, 60)? as usize;
    if (phnum > 0 && u16_at(d, 54)? as usize != PHENTSIZE)
        || (shnum > 0 && u16_at(d, 58)? as usize != SHENTSIZE)
    {
        return Err(ElfError::BadHeader);
    }
    Ok(Header {
        entry: u64_at(d, 24)?,
        phoff: u64_at(d, 32)?,
        shoff: u64_at(d, 40)?,
        phnum,
        shnum,
    })
}

/// Load `data` into `vm`: PT_LOAD segments are copied (tail zero-filled), `pc` is set to the
/// entry point and `sp` (x2) to the top of memory aligned down to 16. On error the VM is
/// left untouched.
pub fn load(vm: &mut Vm, data: &[u8]) -> Result<Loaded, ElfError> {
    let h = header(data)?;
    let mem_len = vm.memory().len() as u64;
    // (vaddr, memsz, file bytes)
    let mut segs: Vec<(u64, u64, &[u8])> = Vec::new();
    for i in 0..h.phnum {
        let ph = slice(
            data,
            h.phoff
                .checked_add(i as u64 * PHENTSIZE as u64)
                .ok_or(ElfError::Overflow)?,
            PHENTSIZE as u64,
        )?;
        if u32_at(ph, 0)? != PT_LOAD {
            continue;
        }
        let (off, vaddr) = (u64_at(ph, 8)?, u64_at(ph, 16)?);
        let (filesz, memsz) = (u64_at(ph, 32)?, u64_at(ph, 40)?);
        if filesz > memsz {
            return Err(ElfError::BadSegment);
        }
        let bytes = slice(data, off, filesz)?;
        let end = vaddr.checked_add(memsz).ok_or(ElfError::Overflow)?;
        if end > mem_len {
            return Err(ElfError::OutOfBounds);
        }
        if memsz > 0 {
            segs.push((vaddr, memsz, bytes));
        }
    }
    segs.sort_by_key(|s| s.0);
    for w in segs.windows(2) {
        // Sums were checked above.
        if w[0].0 + w[0].1 > w[1].0 {
            return Err(ElfError::Overlap);
        }
    }
    if h.entry >= mem_len {
        return Err(ElfError::OutOfBounds);
    }
    let exports = exports(data)?;
    for (vaddr, memsz, bytes) in &segs {
        let mut buf = bytes.to_vec();
        buf.resize(*memsz as usize, 0);
        vm.write_mem(*vaddr, &buf)
            .map_err(|_| ElfError::OutOfBounds)?;
    }
    vm.set_pc(h.entry);
    vm.set_reg(2, mem_len & !15);
    Ok(Loaded {
        entry: h.entry,
        image_end: segs.last().map_or(0, |s| s.0 + s.1),
        exports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EBREAK: u32 = 0x0010_0073;

    /// One PT_LOAD segment (file bytes `code`, `memsz`, at `vaddr`) plus a symtab with `main`.
    fn build(vaddr: u64, memsz: u64, code: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8; 64];
        f[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        f[4] = 2;
        f[5] = 1;
        f[6] = 1;
        f[16..18].copy_from_slice(&2u16.to_le_bytes());
        f[18..20].copy_from_slice(&243u16.to_le_bytes());
        f[20..24].copy_from_slice(&1u32.to_le_bytes());
        f[24..32].copy_from_slice(&vaddr.to_le_bytes());
        f[32..40].copy_from_slice(&64u64.to_le_bytes()); // phoff
        f[54..56].copy_from_slice(&56u16.to_le_bytes());
        f[56..58].copy_from_slice(&1u16.to_le_bytes());
        f[58..60].copy_from_slice(&64u16.to_le_bytes());
        f[60..62].copy_from_slice(&3u16.to_le_bytes()); // null, symtab, strtab
        let mut ph = vec![0u8; 56];
        ph[0..4].copy_from_slice(&1u32.to_le_bytes());
        ph[8..16].copy_from_slice(&176u64.to_le_bytes()); // offset
        ph[16..24].copy_from_slice(&vaddr.to_le_bytes());
        ph[32..40].copy_from_slice(&(code.len() as u64).to_le_bytes());
        ph[40..48].copy_from_slice(&memsz.to_le_bytes());
        f.extend(ph); // 120
        f.extend([0u8; 56]); // pad to 176
        f.extend(code);
        let strtab = b"\0main\0local\0".to_vec();
        let mut sym = vec![0u8; 24]; // null symbol
        for (name, info) in [(1u32, 0x12u8), (6, 0x02)] {
            let mut s = vec![0u8; 24];
            s[0..4].copy_from_slice(&name.to_le_bytes());
            s[4] = info; // global func / local func
            s[6..8].copy_from_slice(&1u16.to_le_bytes());
            s[8..16].copy_from_slice(&vaddr.to_le_bytes());
            sym.extend(s);
        }
        let sym_off = f.len() as u64;
        f.extend(&sym);
        let str_off = f.len() as u64;
        f.extend(&strtab);
        let shoff = f.len() as u64;
        f[40..48].copy_from_slice(&shoff.to_le_bytes());
        f.extend([0u8; 64]);
        let mut sh = vec![0u8; 64];
        sh[4..8].copy_from_slice(&2u32.to_le_bytes());
        sh[24..32].copy_from_slice(&sym_off.to_le_bytes());
        sh[32..40].copy_from_slice(&(sym.len() as u64).to_le_bytes());
        sh[40..44].copy_from_slice(&2u32.to_le_bytes());
        f.extend(sh);
        let mut sh = vec![0u8; 64];
        sh[4..8].copy_from_slice(&3u32.to_le_bytes());
        sh[24..32].copy_from_slice(&str_off.to_le_bytes());
        sh[32..40].copy_from_slice(&(strtab.len() as u64).to_le_bytes());
        f.extend(sh);
        f
    }

    fn code() -> Vec<u8> {
        EBREAK.to_le_bytes().to_vec()
    }

    #[test]
    fn valid_load_runs() {
        let mut vm = Vm::new(4096 + 7, 100);
        let elf = build(0x100, 32, &code());
        let l = load(&mut vm, &elf).unwrap();
        assert_eq!(l.entry, 0x100);
        assert_eq!(vm.pc(), 0x100);
        assert_eq!(vm.reg(2), 4096);
        assert_eq!(vm.read_mem(0x100, 4).unwrap(), &code()[..]);
        assert_eq!(vm.read_mem(0x104, 28).unwrap(), &[0u8; 28][..]);
        assert_eq!(vm.run(), Ok(crate::vm::Exit::Halt));
    }

    #[test]
    fn bad_magic() {
        let mut elf = build(0, 4, &code());
        elf[0] = 0;
        assert_eq!(load(&mut Vm::new(1024, 1), &elf), Err(ElfError::BadMagic));
    }

    #[test]
    fn wrong_machine_and_class() {
        let mut elf = build(0, 4, &code());
        elf[18] = 62;
        assert_eq!(load(&mut Vm::new(1024, 1), &elf), Err(ElfError::BadMachine));
        let mut elf = build(0, 4, &code());
        elf[4] = 1;
        assert_eq!(load(&mut Vm::new(1024, 1), &elf), Err(ElfError::BadClass));
    }

    #[test]
    fn truncated() {
        let elf = build(0, 4, &code());
        for n in [0, 10, 63, 100, 180] {
            assert_eq!(
                load(&mut Vm::new(1024, 1), &elf[..n]),
                Err(ElfError::Truncated)
            );
        }
    }

    #[test]
    fn segment_out_of_bounds_leaves_vm_untouched() {
        let elf = build(1020, 8, &code());
        let mut vm = Vm::new(1024, 1);
        assert_eq!(load(&mut vm, &elf), Err(ElfError::OutOfBounds));
        assert_eq!(vm.pc(), 0);
        assert_eq!(vm.reg(2), 0);
    }

    #[test]
    fn offset_overflow() {
        let mut elf = build(0, 4, &code());
        elf[120 - 56 + 8..120 - 56 + 16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(load(&mut Vm::new(1024, 1), &elf), Err(ElfError::Overflow));
        let mut elf = build(0, 4, &code());
        elf[64 + 16..64 + 24].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(load(&mut Vm::new(1024, 1), &elf), Err(ElfError::Overflow));
        let mut elf = build(0, 4, &code());
        elf[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(load(&mut Vm::new(1024, 1), &elf), Err(ElfError::Overflow));
    }

    #[test]
    fn overlapping_and_bad_filesz() {
        let mut elf = build(0, 8, &code());
        elf[64 + 32..64 + 40].copy_from_slice(&16u64.to_le_bytes());
        assert_eq!(load(&mut Vm::new(1024, 1), &elf), Err(ElfError::BadSegment));
        // Two identical segments overlap.
        let mut elf = build(0, 8, &code());
        elf[56..58].copy_from_slice(&2u16.to_le_bytes());
        let ph: Vec<u8> = elf[64..120].to_vec();
        elf[120..176].copy_from_slice(&ph);
        assert_eq!(load(&mut Vm::new(1024, 1), &elf), Err(ElfError::Overlap));
    }

    #[test]
    fn symbol_lookup() {
        let mut vm = Vm::new(1024, 1);
        let l = load(&mut vm, &build(0x40, 8, &code())).unwrap();
        assert_eq!(l.exports.len(), 1);
        assert_eq!(l.exports.get("main"), Some(&0x40));
        assert_eq!(exports(&build(0x40, 8, &code())).unwrap(), l.exports);
    }
}
