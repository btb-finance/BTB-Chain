//! In-memory `Host` for tests and tooling.

use crate::contract::FunctionKind;
use crate::host::{Address, CallEnd, CallReply, CallRequest, Host, PriceRecord, TwapOutcome};
use crate::rand::RandomSource;
use crate::token::{TokenId, TokenInfo, TokenStore};
use std::collections::{BTreeMap, VecDeque};

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
    /// User tokens (see `token.rs`); separate from `balances` (native BTB).
    pub tokens: BTreeMap<TokenId, TokenInfo>,
    pub token_balances: BTreeMap<(TokenId, Address), u128>,
    pub token_nonces: BTreeMap<Address, u64>,
    /// Randomness for `random_bytes`; `None` = the host has no beacon.
    pub random: Option<RandomSource>,
    /// Agreed prices by pair id.
    pub prices: BTreeMap<u32, PriceRecord>,
    /// TWAP answers by `(pair, window)`; anything else is `NotFound`.
    pub twaps: BTreeMap<(u32, u64), TwapOutcome>,
    /// Reports queued by `report_request`, in order (the id is the index).
    pub reports: Vec<Vec<u8>>,
    /// Scripted answers to `call_contract`, first in first out; with none left the call is
    /// `Unsupported`.
    pub call_replies: VecDeque<CallReply>,
    /// Every `call_contract` the host received: `(target, kind, function, args, gas, read_only,
    /// max_output)`.
    pub calls_seen: Vec<SeenCall>,
}

/// A `call_contract` request as `MemHost` recorded it.
pub type SeenCall = (Address, FunctionKind, String, Vec<u8>, u64, bool, usize);

impl TokenStore for MemHost {
    fn contract(&self) -> Address {
        self.contract
    }
    fn info(&mut self, id: &TokenId) -> Option<TokenInfo> {
        self.tokens.get(id).cloned()
    }
    fn put_info(&mut self, id: &TokenId, info: &TokenInfo) {
        self.tokens.insert(*id, info.clone());
    }
    fn balance(&mut self, id: &TokenId, holder: &Address) -> u128 {
        self.token_balances
            .get(&(*id, *holder))
            .copied()
            .unwrap_or(0)
    }
    fn put_balance(&mut self, id: &TokenId, holder: &Address, v: u128) {
        if v == 0 {
            self.token_balances.remove(&(*id, *holder));
        } else {
            self.token_balances.insert((*id, *holder), v);
        }
    }
    fn nonce(&mut self, creator: &Address) -> u64 {
        self.token_nonces.get(creator).copied().unwrap_or(0)
    }
    fn put_nonce(&mut self, creator: &Address, n: u64) {
        self.token_nonces.insert(*creator, n);
    }
    fn holders(
        &mut self,
        id: &TokenId,
        after: Option<&Address>,
        max: usize,
    ) -> Vec<(Address, u128)> {
        self.token_balances
            .range((*id, [0u8; 32])..=(*id, [0xFF; 32]))
            .filter(|((_, a), _)| after.is_none_or(|x| a > x))
            .take(max)
            .map(|((_, a), b)| (*a, *b))
            .collect()
    }
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
    fn contract_address(&self) -> Address {
        self.contract
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
    fn tokens(&mut self) -> Option<&mut dyn TokenStore> {
        Some(self)
    }
    fn random_bytes(&mut self, len: usize) -> Option<Vec<u8>> {
        self.random.as_mut().map(|r| r.next(len))
    }
    fn price(&mut self, pair: u32) -> Option<PriceRecord> {
        self.prices.get(&pair).copied()
    }
    fn price_twap(&mut self, pair: u32, window: u64) -> TwapOutcome {
        self.twaps
            .get(&(pair, window))
            .copied()
            .unwrap_or(TwapOutcome::NotFound)
    }
    fn report_request(&mut self, data: &[u8]) -> Option<u64> {
        self.reports.push(data.to_vec());
        Some(self.reports.len() as u64 - 1)
    }
    fn call_contract(&mut self, req: &CallRequest<'_>) -> CallReply {
        self.calls_seen.push((
            req.target,
            req.kind,
            req.function.to_string(),
            req.args.to_vec(),
            req.gas,
            req.read_only,
            req.max_output,
        ));
        self.call_replies
            .pop_front()
            .unwrap_or_else(|| CallReply::refused(CallEnd::Unsupported))
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
