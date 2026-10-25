//! `RuntimeHost`: runs a contract call against a `KvStore`.
//!
//! All writes go to an in-memory overlay. Nothing reaches the store until `finish` returns the
//! batch and the caller commits it, so a trapped call (or any fault) leaves state untouched.
//!
//! The `Host` trait cannot report errors, so a backend failure or a rejected row (index
//! extractor error, id overflow) is recorded as a fault, the call gets a harmless default
//! result, and `finish` then returns the fault instead of a batch.

use crate::kv::{KvError, KvStore, Overlay, WriteBatch};
use crate::layout;
use crate::readset::ReadSet;
use crate::tables::{TableError, Tables};
use btb_vm::host::{Address, Host};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    Kv(KvError),
    Table(TableError),
}

pub struct RuntimeHost<'a, S: KvStore> {
    kv: Overlay<'a, S>,
    tables: &'a Tables,
    /// The contract being run (its storage, tables and token balance).
    pub contract: Address,
    pub caller: Address,
    pub height: u64,
    pub read_only: bool,
    fault: Option<Fault>,
}

impl<'a, S: KvStore> RuntimeHost<'a, S> {
    pub fn new(
        store: &'a S,
        tables: &'a Tables,
        contract: Address,
        caller: Address,
        height: u64,
    ) -> Self {
        Self {
            kv: Overlay::new(store),
            tables,
            contract,
            caller,
            height,
            read_only: false,
            fault: None,
        }
    }

    /// The writes of the call, to be committed with `KvStore::commit`; or the first fault.
    pub fn finish(self) -> Result<WriteBatch, Fault> {
        match self.fault {
            Some(f) => Err(f),
            None => Ok(self.kv.into_batch()),
        }
    }

    /// Like `finish`, also returning everything the call read (for subscriptions and
    /// optimistic transactions).
    pub fn finish_tracked(self) -> Result<(WriteBatch, ReadSet), Fault> {
        match self.fault {
            Some(f) => Err(f),
            None => Ok(self.kv.into_parts()),
        }
    }

    /// The read set so far.
    pub fn read_set(&self) -> ReadSet {
        self.kv.read_set()
    }

    fn note(&mut self, f: Fault) {
        self.fault.get_or_insert(f);
    }

    fn kv<T: Default>(&mut self, r: Result<T, KvError>) -> T {
        r.unwrap_or_else(|e| {
            self.note(Fault::Kv(e));
            T::default()
        })
    }

    fn tbl<T: Default>(&mut self, r: Result<T, TableError>) -> T {
        r.unwrap_or_else(|e| {
            self.note(Fault::Table(e));
            T::default()
        })
    }

    fn balance(&mut self, addr: &Address) -> u64 {
        let r = self.kv.get(&layout::balance_key(addr));
        self.kv(r)
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map_or(0, u64::from_le_bytes)
    }

    fn set_balance(&mut self, addr: &Address, v: u64) {
        let r = self.kv.put(&layout::balance_key(addr), &v.to_le_bytes());
        self.kv(r);
    }
}

impl<S: KvStore> Host for RuntimeHost<'_, S> {
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn storage_read(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let r = self.kv.get(&layout::storage_key(&self.contract, key));
        self.kv(r)
    }
    fn storage_write(&mut self, key: &[u8], value: &[u8]) {
        let r = self
            .kv
            .put(&layout::storage_key(&self.contract, key), value);
        self.kv(r);
    }
    fn storage_remove(&mut self, key: &[u8]) {
        let r = self.kv.delete(&layout::storage_key(&self.contract, key));
        self.kv(r);
    }
    fn caller(&self) -> Address {
        self.caller
    }
    fn block_height(&self) -> u64 {
        self.height
    }
    fn token_balance(&self, addr: &Address) -> u64 {
        self.kv
            .get(&layout::balance_key(addr))
            .ok()
            .flatten()
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map_or(0, u64::from_le_bytes)
    }
    fn token_transfer(&mut self, to: &Address, amount: u64) -> bool {
        let c = self.contract;
        let from = self.balance(&c);
        if *to == c {
            return from >= amount;
        }
        let Some(rest) = from.checked_sub(amount) else {
            return false;
        };
        let Some(new_to) = self.balance(to).checked_add(amount) else {
            return false;
        };
        self.set_balance(&c, rest);
        self.set_balance(to, new_to);
        true
    }
    fn table_insert(&mut self, table: &[u8], row: &[u8]) -> u64 {
        let (t, c) = (self.tables, self.contract);
        let r = t.insert(&mut self.kv, &c, table, row);
        self.tbl(r)
    }
    fn table_get(&mut self, table: &[u8], id: u64) -> Option<Vec<u8>> {
        let r = self.tables.get(&self.kv, &self.contract, table, id);
        self.tbl(r)
    }
    fn table_update(&mut self, table: &[u8], id: u64, row: &[u8]) -> bool {
        let (t, c) = (self.tables, self.contract);
        let r = t.update(&mut self.kv, &c, table, id, row);
        self.tbl(r)
    }
    fn table_delete(&mut self, table: &[u8], id: u64) -> bool {
        let (t, c) = (self.tables, self.contract);
        let r = t.delete(&mut self.kv, &c, table, id);
        self.tbl(r)
    }
    fn table_scan(&mut self, table: &[u8], start_id: u64, max: u64) -> Vec<u64> {
        let prefix = layout::row_prefix(&self.contract, table);
        let end = layout::prefix_successor(&prefix);
        let mut from = prefix;
        from.extend_from_slice(&start_id.to_be_bytes());
        let limit = usize::try_from(max).unwrap_or(usize::MAX);
        let r = self.kv.scan(&from, end.as_deref(), false, limit);
        self.kv(r)
            .iter()
            .filter_map(|(k, _)| layout::id_from_key(k))
            .collect()
    }
}
