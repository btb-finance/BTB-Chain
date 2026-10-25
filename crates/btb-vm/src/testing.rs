//! In-memory `Host` for tests and tooling.

use crate::host::{Address, Host};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct MemHost {
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
}
