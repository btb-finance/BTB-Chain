//! In-memory `Host` for tests and tooling.

use crate::contract::FunctionKind;
use crate::decode::Trap;
use crate::host::{
    abi, Address, CallEnd, CallReply, CallRequest, FilePin, FileRecord, Host, PriceRecord,
    TwapOutcome,
};
use crate::rand::RandomSource;
use crate::token::{TokenId, TokenInfo, TokenStore};
use crate::{CallError, Contract};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

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
    /// Contracts `call_contract` can run (see [`MemHost::add_contract`]). With none registered,
    /// `call_contract` answers from `call_replies`.
    pub contracts: Code,
    /// State of the contracts called so far (storage, tables), by address. A callee's native
    /// and token balances live in the caller's `balances` / `token_*` maps (shared).
    pub callees: BTreeMap<Address, MemHost>,
    /// Contracts below the running frame, outermost first.
    pub stack: Vec<Address>,
    /// Files by root. `file_info` answers from here (unknown roots are `FILE_UNKNOWN`).
    pub files: BTreeMap<[u8; 32], FileRecord>,
    /// Every `file_pin` received, in order.
    pub pins_seen: Vec<FilePin>,
    /// What `file_pin` answers next (`FILE_PIN_OK`, after recording the file as stored, when
    /// `None`).
    pub pin_result: Option<u64>,
}

/// Code of the contracts a [`MemHost`] can call.
#[derive(Clone, Default)]
pub struct Code(pub BTreeMap<Address, Arc<Contract>>);

impl std::fmt::Debug for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.0.keys()).finish()
    }
}

impl MemHost {
    /// Make `contract` callable at `addr` through `call_contract`: a resolver for tests. The
    /// callee runs in a fresh VM against its own storage and tables (kept between calls), the
    /// shared balances and token ledger, with the same rules as the chain (depth, re-entrancy,
    /// read-only frames, no randomness-marked or callback targets), and its writes are undone
    /// unless it succeeds.
    pub fn add_contract(&mut self, addr: Address, contract: Arc<Contract>) {
        self.contracts.0.insert(addr, contract);
    }

    fn run_callee(&mut self, req: &CallRequest<'_>) -> CallReply {
        let refused = CallReply::refused;
        let read_only = self.read_only || req.read_only;
        if read_only && req.kind == FunctionKind::Mutation {
            return refused(CallEnd::Forbidden(Trap::WriteInQuery));
        }
        if req.target == self.contract || self.stack.contains(&req.target) {
            return refused(CallEnd::Reentrant);
        }
        if self.stack.len() + 2 > abi::MAX_CALL_DEPTH {
            return refused(CallEnd::Depth);
        }
        let Some(code) = self.contracts.0.get(&req.target).cloned() else {
            return refused(CallEnd::NoContract);
        };
        if !code.has_entry(req.kind, req.function) {
            return refused(CallEnd::NoFunction);
        }
        if req.kind == FunctionKind::Mutation && code.is_action_callback(req.function) {
            return refused(CallEnd::Forbidden(Trap::CallForbidden(
                "an HTTPS action callback cannot be called by a contract",
            )));
        }
        if code.is_randomness(req.kind, req.function) {
            return refused(CallEnd::Forbidden(Trap::RandomnessForbidden));
        }
        // The callee's own state in, the shared state alongside; everything comes back only if
        // the callee succeeds.
        let mut callees = std::mem::take(&mut self.callees);
        let before = callees.get(&req.target).cloned();
        let mut sub = callees.remove(&req.target).unwrap_or_default();
        sub.contract = req.target;
        sub.caller = self.contract;
        sub.height = self.height;
        sub.read_only = read_only || req.kind == FunctionKind::Query;
        sub.contracts = self.contracts.clone();
        sub.stack = self.stack.clone();
        sub.stack.push(self.contract);
        sub.callees = callees;
        sub.balances = self.balances.clone();
        sub.tokens = self.tokens.clone();
        sub.token_balances = self.token_balances.clone();
        sub.token_nonces = self.token_nonces.clone();
        sub.files = self.files.clone();
        let ran = code.call(req.kind, req.function, req.args, &mut sub, req.gas, 1 << 20);
        let (end, used) = match ran {
            Ok(o) if o.output.len() > req.max_output => (CallEnd::OutputTooLarge, o.gas_used),
            Ok(o) => (CallEnd::Ok(o.output), o.gas_used),
            Err(CallError::Trap { trap, gas_used }) => match trap {
                Trap::OutOfGas => (CallEnd::OutOfGas, req.gas),
                Trap::Abort(_) => (CallEnd::Aborted, gas_used),
                _ => (CallEnd::Trapped, gas_used),
            },
            Err(CallError::NoSuchEntry { .. }) => (CallEnd::NoFunction, 0),
            Err(_) => (CallEnd::Trapped, 0),
        };
        let mut callees = std::mem::take(&mut sub.callees);
        if matches!(end, CallEnd::Ok(_)) {
            self.balances = std::mem::take(&mut sub.balances);
            self.tokens = std::mem::take(&mut sub.tokens);
            self.token_balances = std::mem::take(&mut sub.token_balances);
            self.token_nonces = std::mem::take(&mut sub.token_nonces);
            self.files = std::mem::take(&mut sub.files);
            self.pins_seen.append(&mut sub.pins_seen);
            callees.insert(req.target, sub);
        } else if let Some(b) = before {
            callees.insert(req.target, b);
        }
        self.callees = callees;
        CallReply {
            end,
            gas_used: used.min(req.gas),
        }
    }
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
    fn file_info(&mut self, root: &[u8; 32]) -> Option<FileRecord> {
        Some(self.files.get(root).copied().unwrap_or_default())
    }
    fn file_pin(&mut self, pin: &FilePin) -> u64 {
        self.pins_seen.push(*pin);
        let r = self.pin_result.unwrap_or(abi::FILE_PIN_OK);
        if r == abi::FILE_PIN_OK {
            self.files.insert(
                pin.root,
                FileRecord {
                    size: pin.size,
                    status: abi::FILE_STORED,
                    expiry: self.height + u64::from(pin.periods),
                },
            );
        }
        r
    }
    fn call_contract(&mut self, req: &CallRequest<'_>) -> CallReply {
        if !self.contracts.0.is_empty() {
            return self.run_callee(req);
        }
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
