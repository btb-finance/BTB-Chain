//! In-memory `Host` for tests and tooling.

use crate::host::{Address, Host};
use std::collections::BTreeMap;

/// One table: rows by id, plus the next id to hand out (never decreases, so ids are not reused).
#[derive(Debug, Clone, Default)]
pub struct MemTable {
    pub next_id: u64,
    pub rows: BTreeMap<u64, Vec<u8>>,
}

#[derive(Debug, Clone, Default)]
pub struct MemHost {
    pub tables: BTreeMap<Vec<u8>, MemTable>,
    pub storage: BTreeMap<Vec<u8>, Vec<u8>>,
    pub balances: BTreeMap<Address, u64>,
    pub caller: Address,
    pub height: u64,
    pub read_only: bool,
    /// Address that `token_transfer` debits.
    pub contract: Address,
}

impl Host for MemHost {
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn storage_read(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        self.storage.get(key).cloned()
    }
    fn storage_write(&mut self, key: &[u8], value: &[u8]) {
        self.storage.insert(key.to_vec(), value.to_vec());
    }
    fn storage_remove(&mut self, key: &[u8]) {
        self.storage.remove(key);
    }
    fn caller(&self) -> Address {
        self.caller
    }
    fn block_height(&self) -> u64 {
        self.height
    }
    fn token_balance(&self, addr: &Address) -> u64 {
        self.balances.get(addr).copied().unwrap_or(0)
    }
    fn token_transfer(&mut self, to: &Address, amount: u64) -> bool {
        if *to == self.contract {
            return self.token_balance(to) >= amount;
        }
        let from = self.token_balance(&self.contract);
        let Some(rest) = from.checked_sub(amount) else {
            return false;
        };
        let Some(new_to) = self.token_balance(to).checked_add(amount) else {
            return false;
        };
        self.balances.insert(self.contract, rest);
        self.balances.insert(*to, new_to);
        true
    }
    fn table_insert(&mut self, table: &[u8], row: &[u8]) -> u64 {
        let t = self.tables.entry(table.to_vec()).or_default();
        let id = t.next_id;
        t.next_id = id.saturating_add(1);
        t.rows.insert(id, row.to_vec());
        id
    }
    fn table_get(&mut self, table: &[u8], id: u64) -> Option<Vec<u8>> {
        self.tables.get(table)?.rows.get(&id).cloned()
    }
    fn table_update(&mut self, table: &[u8], id: u64, row: &[u8]) -> bool {
        match self.tables.get_mut(table).and_then(|t| t.rows.get_mut(&id)) {
            Some(r) => {
                *r = row.to_vec();
                true
            }
            None => false,
        }
    }
    fn table_delete(&mut self, table: &[u8], id: u64) -> bool {
        self.tables
            .get_mut(table)
            .is_some_and(|t| t.rows.remove(&id).is_some())
    }
    fn table_scan(&mut self, table: &[u8], start_id: u64, max: u64) -> Vec<u64> {
        let Some(t) = self.tables.get(table) else {
            return Vec::new();
        };
        t.rows
            .range(start_id..)
            .take(usize::try_from(max).unwrap_or(usize::MAX))
            .map(|(id, _)| *id)
            .collect()
    }
}

/// Build a minimal ELF64 RISC-V ET_EXEC for tests: one PT_LOAD segment holding `code` at
/// `0x1000` (entry = `0x1000`) and a global `STT_FUNC` symbol per `(name, offset into code)`.
pub fn build_elf(code: &[u8], symbols: &[(&str, u64)]) -> Vec<u8> {
    const VADDR: u64 = 0x1000;
    let mut f = vec![0u8; 64];
    f[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    f[4] = 2;
    f[5] = 1;
    f[6] = 1;
    f[16..18].copy_from_slice(&2u16.to_le_bytes());
    f[18..20].copy_from_slice(&243u16.to_le_bytes());
    f[20..24].copy_from_slice(&1u32.to_le_bytes());
    f[24..32].copy_from_slice(&VADDR.to_le_bytes());
    f[32..40].copy_from_slice(&64u64.to_le_bytes());
    f[54..56].copy_from_slice(&56u16.to_le_bytes());
    f[56..58].copy_from_slice(&1u16.to_le_bytes());
    f[58..60].copy_from_slice(&64u16.to_le_bytes());
    f[60..62].copy_from_slice(&3u16.to_le_bytes());
    let mut ph = vec![0u8; 56];
    ph[0..4].copy_from_slice(&1u32.to_le_bytes());
    ph[8..16].copy_from_slice(&176u64.to_le_bytes());
    ph[16..24].copy_from_slice(&VADDR.to_le_bytes());
    ph[32..40].copy_from_slice(&(code.len() as u64).to_le_bytes());
    ph[40..48].copy_from_slice(&(code.len() as u64).to_le_bytes());
    f.extend(ph);
    f.extend([0u8; 56]);
    f.extend(code);
    let mut strtab = vec![0u8];
    let mut sym = vec![0u8; 24];
    for (name, off) in symbols {
        let mut s = vec![0u8; 24];
        s[0..4].copy_from_slice(&(strtab.len() as u32).to_le_bytes());
        s[4] = 0x12; // global func
        s[6..8].copy_from_slice(&1u16.to_le_bytes());
        s[8..16].copy_from_slice(&(VADDR + off).to_le_bytes());
        sym.extend(s);
        strtab.extend(name.as_bytes());
        strtab.push(0);
    }
    let sym_off = f.len() as u64;
    f.extend(&sym);
    let str_off = f.len() as u64;
    f.extend(&strtab);
    let shoff = f.len() as u64;
    f[40..48].copy_from_slice(&shoff.to_le_bytes());
    f.extend([0u8; 64]);
    for (ty, link, off, len) in [
        (2u32, 2u32, sym_off, sym.len()),
        (3, 0, str_off, strtab.len()),
    ] {
        let mut sh = vec![0u8; 64];
        sh[4..8].copy_from_slice(&ty.to_le_bytes());
        sh[24..32].copy_from_slice(&off.to_le_bytes());
        sh[32..40].copy_from_slice(&(len as u64).to_le_bytes());
        sh[40..44].copy_from_slice(&link.to_le_bytes());
        f.extend(sh);
    }
    f
}
