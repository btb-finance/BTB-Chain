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
