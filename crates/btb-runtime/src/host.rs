//! `RuntimeHost`: runs a contract call against a `KvStore`.
//!
//! All writes go to an in-memory overlay. Nothing reaches the store until `finish` returns the
//! batch and the caller commits it, so a trapped call (or any fault) leaves state untouched.
//!
//! The `Host` trait cannot report errors, so a backend failure or a rejected row (index
//! extractor error, id overflow) is recorded as a fault, the call gets a harmless default
//! result, and `finish` then returns the fault instead of a batch.

use crate::kv::{KvError, KvStore, Overlay, WriteBatch};
use crate::layout::{self, IndexValue};
use crate::query::{self, Cursor, Order, QueryOptions};
use crate::readset::ReadSet;
use crate::tables::{TableError, Tables};
use btb_vm::decode::Trap;
use btb_vm::host::{
    abi, Address, CallEnd, CallReply, CallRequest, Host, IndexBound, IndexKey, IndexScan,
    IndexScanPage,
};
use btb_vm::token::{TokenId, TokenInfo, TokenStore};
use btb_vm::{CallError, Contract, FunctionKind};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;

/// A contract loaded for a nested call.
pub struct Callee {
    pub contract: Arc<Contract>,
    /// The callee's table schemas, when the host's registry does not already hold them (the
    /// parallel executor registers lazily); merged into the running host's registry.
    pub tables: Option<Tables>,
}

/// Finds the code of a contract for `call_contract`. It reads through `kv` (the running call's
/// overlay) so that the read is tracked and sees this block's earlier writes. `Ok(None)`: no
/// contract at `addr`.
pub trait Resolver {
    fn resolve(&self, kv: &dyn KvStore, addr: &Address) -> Result<Option<Callee>, KvError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    Kv(KvError),
    Table(TableError),
    /// The contract's balance cannot cover the storage deposit of the call's growth.
    InsufficientDeposit {
        needed: u64,
        available: u64,
    },
    /// Usage, deposit or balance arithmetic overflowed.
    DepositOverflow,
    /// A stored user-token record did not decode.
    CorruptToken,
}

fn index_value(k: &IndexKey) -> IndexValue {
    match k {
        IndexKey::U64(v) => IndexValue::U64(*v),
        IndexKey::I64(v) => IndexValue::I64(*v),
        IndexKey::U128(v) => IndexValue::U128(*v),
        IndexKey::Bytes(v) => IndexValue::Bytes(v.clone()),
        IndexKey::Str(v) => IndexValue::Str(v.clone()),
    }
}

fn bound(b: &IndexBound) -> Bound<IndexValue> {
    match b {
        IndexBound::Unbounded => Bound::Unbounded,
        IndexBound::Included(k) => Bound::Included(index_value(k)),
        IndexBound::Excluded(k) => Bound::Excluded(index_value(k)),
    }
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
    /// `Some(tx id)` once `enable_randomness` was called; the source is built lazily on the first
    /// `random_bytes` so transactions that never ask do not read the beacon key.
    rand_tx: Option<[u8; 32]>,
    random: Option<btb_vm::rand::RandomSource>,
    /// Reports queued by this call (capped at `layout::MAX_REPORTS_PER_CALL`).
    reports: u32,
    /// Cross-contract calls: where to find code, the memory of a callee's VM, the contracts
    /// below the running frame (outermost first) and the tables of callees merged in.
    resolver: Option<&'a dyn Resolver>,
    vm_memory: usize,
    ancestors: Vec<Address>,
    extra_tables: Option<Tables>,
    /// Net metered bytes of each contract already settled into its deposit (see
    /// `settle_contract`).
    settled: BTreeMap<Address, i128>,
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
            rand_tx: None,
            random: None,
            reports: 0,
            resolver: None,
            vm_memory: 1 << 20,
            ancestors: Vec::new(),
            extra_tables: None,
            settled: BTreeMap::new(),
        }
    }

    /// Let the running contract call others (`call_contract`): `resolver` finds their code and
    /// each callee gets a fresh VM with `vm_memory` bytes. Without this the call returns
    /// `abi::CALL_UNSUPPORTED`.
    pub fn enable_calls(&mut self, resolver: &'a dyn Resolver, vm_memory: usize) {
        self.resolver = Some(resolver);
        self.vm_memory = vm_memory;
    }

    /// Development hosts (`btb dev`) have no beacon: derive from a fixed, public `seed`
    /// instead. NOT random in any security sense; never used by the chain.
    pub fn enable_dev_randomness(&mut self, seed: [u8; 32], tx_id: [u8; 32]) {
        self.random = Some(btb_vm::rand::RandomSource::new(&seed, &tx_id));
        self.rand_tx = Some(tx_id);
    }

    /// Let `random_bytes` calls derive from the block's beacon (`layout::oracle_seed_key`) and
    /// this transaction id (a cron run passes its receipt id). Without this call, or on a chain
    /// whose state has no beacon, `random_bytes` traps with `RandomnessUnavailable`. The VM
    /// separately decides whether the entry point may ask at all.
    pub fn enable_randomness(&mut self, tx_id: [u8; 32]) {
        self.rand_tx = Some(tx_id);
    }

    /// The writes of the call, to be committed with `KvStore::commit`; or the first fault.
    pub fn finish(self) -> Result<WriteBatch, Fault> {
        self.finish_tracked().map(|(b, _)| b)
    }

    /// Like `finish`, also returning everything the call read (for subscriptions and
    /// optimistic transactions).
    pub fn finish_tracked(mut self) -> Result<(WriteBatch, ReadSet), Fault> {
        if let Some(f) = self.fault.take() {
            return Err(f);
        }
        if !self.read_only {
            self.settle_contract(self.contract)?;
        }
        Ok(self.kv.into_parts())
    }

    /// Storage deposit of `c`: compare its metered bytes in the pending writes with the base.
    /// Growth since the last settlement of `c` locks `delta * DEPOSIT_PER_BYTE` from its balance
    /// into its deposit record; shrinkage returns the proportional share to the balance. Fails
    /// (the caller discards the writes) if the balance is too low. Incremental, so a contract
    /// that is called several times in one transaction (or settles at the end of a nested call
    /// and again at the end of the transaction) is charged each byte once.
    fn settle_contract(&mut self, c: Address) -> Result<(), Fault> {
        let mut changes = Vec::new();
        for (k, v) in self.kv.pending() {
            if layout::is_metered(k) && layout::key_contract(k) == Some(&c[..]) {
                changes.push((k.clone(), v.as_ref().map_or(0, |v| k.len() + v.len())));
            }
        }
        let mut net: i128 = 0;
        for (k, new) in changes {
            let old = match self.kv.base_get(&k) {
                Ok(v) => v.map_or(0, |v| k.len() + v.len()),
                Err(e) => return Err(Fault::Kv(e)),
            };
            net += new as i128 - old as i128;
        }
        let delta = net - self.settled.get(&c).copied().unwrap_or(0);
        if delta == 0 {
            return Ok(());
        }
        let magnitude = u64::try_from(delta.unsigned_abs()).map_err(|_| Fault::DepositOverflow)?;
        let (grown, freed) = if delta > 0 {
            (magnitude, 0)
        } else {
            (0, magnitude)
        };
        let read = |h: &mut Self, key: Vec<u8>| -> Result<u64, Fault> {
            let r = h.kv.get(&key).map_err(Fault::Kv)?;
            Ok(r.and_then(|b| <[u8; 8]>::try_from(b).ok())
                .map_or(0, u64::from_le_bytes))
        };
        let usage = read(self, layout::usage_key(&c))?;
        let deposit = read(self, layout::deposit_key(&c))?;
        let balance = self.balance(&c);
        let new_usage = usage
            .checked_add(grown)
            .and_then(|u| u.checked_sub(freed))
            .ok_or(Fault::DepositOverflow)?;
        let (new_deposit, new_balance) = if grown > freed {
            let need = (grown - freed)
                .checked_mul(layout::DEPOSIT_PER_BYTE)
                .ok_or(Fault::DepositOverflow)?;
            let nb = balance
                .checked_sub(need)
                .ok_or(Fault::InsufficientDeposit {
                    needed: need,
                    available: balance,
                })?;
            (deposit + need, nb)
        } else {
            // Proportional share of the deposit, capped at what is locked.
            let back = if usage == 0 {
                0
            } else {
                (u128::from(deposit) * u128::from(freed - grown) / u128::from(usage)) as u64
            };
            (
                deposit - back,
                balance.checked_add(back).ok_or(Fault::DepositOverflow)?,
            )
        };
        for (k, v) in [
            (layout::usage_key(&c), new_usage),
            (layout::deposit_key(&c), new_deposit),
            (layout::balance_key(&c), new_balance),
        ] {
            self.kv.put(&k, &v.to_le_bytes()).map_err(Fault::Kv)?;
        }
        self.settled.insert(c, net);
        Ok(())
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

    /// Is the circuit breaker of `pair`'s feed open?
    fn price_paused(&mut self, pair: u32) -> bool {
        let r = self.kv.get(&layout::oracle_breaker_key(pair));
        self.kv(r).is_some_and(|b| b.first() == Some(&1))
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

impl<S: KvStore> TokenStore for RuntimeHost<'_, S> {
    fn contract(&self) -> Address {
        self.contract
    }
    fn info(&mut self, id: &TokenId) -> Option<TokenInfo> {
        let r = self.kv.get(&layout::token_key(id));
        let raw = self.kv(r)?;
        let info = TokenInfo::decode(&raw);
        if info.is_none() {
            self.note(Fault::CorruptToken);
        }
        info
    }
    fn put_info(&mut self, id: &TokenId, info: &TokenInfo) {
        let r = self.kv.put(&layout::token_key(id), &info.encode());
        self.kv(r);
    }
    fn balance(&mut self, id: &TokenId, holder: &Address) -> u128 {
        let r = self.kv.get(&layout::token_balance_key(id, holder));
        self.kv(r)
            .and_then(|b| <[u8; 16]>::try_from(b).ok())
            .map_or(0, u128::from_le_bytes)
    }
    fn put_balance(&mut self, id: &TokenId, holder: &Address, v: u128) {
        let key = layout::token_balance_key(id, holder);
        let r = if v == 0 {
            self.kv.delete(&key)
        } else {
            self.kv.put(&key, &v.to_le_bytes())
        };
        self.kv(r);
    }
    fn nonce(&mut self, creator: &Address) -> u64 {
        let r = self.kv.get(&layout::token_nonce_key(creator));
        self.kv(r)
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map_or(0, u64::from_le_bytes)
    }
    fn put_nonce(&mut self, creator: &Address, n: u64) {
        let r = self
            .kv
            .put(&layout::token_nonce_key(creator), &n.to_le_bytes());
        self.kv(r);
    }
    fn holders(
        &mut self,
        id: &TokenId,
        after: Option<&Address>,
        max: usize,
    ) -> Vec<(Address, u128)> {
        let prefix = layout::token_balance_prefix(id);
        let end = layout::prefix_successor(&prefix);
        let mut from = prefix.clone();
        if let Some(a) = after {
            from.extend_from_slice(a);
            // Smallest key strictly greater than the cursor's own entry.
            from.push(0);
        }
        let r = self.kv.scan(&from, end.as_deref(), false, max);
        self.kv(r)
            .into_iter()
            .filter_map(|(k, v)| {
                let a: Address = k.get(prefix.len()..)?.try_into().ok()?;
                let b = u128::from_le_bytes(v.try_into().ok()?);
                Some((a, b))
            })
            .collect()
    }
}

impl<S: KvStore> Host for RuntimeHost<'_, S> {
    fn random_bytes(&mut self, len: usize) -> Option<Vec<u8>> {
        if self.random.is_none() {
            let tx = self.rand_tx?;
            let r = self.kv.get(&layout::oracle_seed_key());
            let rec = self.kv(r)?;
            let seed: [u8; 32] = rec.get(8..40)?.try_into().ok()?;
            self.random = Some(btb_vm::rand::RandomSource::new(&seed, &tx));
        }
        self.random.as_mut().map(|r| r.next(len))
    }

    fn report_request(&mut self, data: &[u8]) -> Option<u64> {
        // Only a chain with an oracle committee can sign reports, and only a state-changing
        // call may queue one (the VM already traps a read-only host).
        if self.read_only || self.reports >= layout::MAX_REPORTS_PER_CALL {
            return None;
        }
        let r = self.kv.get(&layout::oracle_config_key());
        self.kv(r)?;
        let r = self.kv.get(&layout::report_counter_key());
        let id = self
            .kv(r)
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map_or(0, u64::from_le_bytes);
        let next = id.checked_add(1)?;
        let mut rec = self.height.to_le_bytes().to_vec();
        rec.extend_from_slice(&self.contract);
        rec.extend_from_slice(data);
        let r = self.kv.put(&layout::report_key(id), &rec);
        self.kv(r);
        let r = self
            .kv
            .put(&layout::report_counter_key(), &next.to_le_bytes());
        self.kv(r);
        self.reports += 1;
        Some(id)
    }

    fn price(&mut self, pair: u32) -> Option<btb_vm::host::PriceRecord> {
        let r = self.kv.get(&layout::oracle_price_key(pair));
        let b = self.kv(r)?;
        if b.len() != 29 {
            return None;
        }
        Some(btb_vm::host::PriceRecord {
            price: u128::from_le_bytes(b[..16].try_into().ok()?),
            height: u64::from_le_bytes(b[16..24].try_into().ok()?),
            decimals: b[24],
            paused: self.price_paused(pair),
        })
    }

    fn price_twap(&mut self, pair: u32, window: u64) -> btb_vm::host::TwapOutcome {
        use btb_vm::host::TwapOutcome;
        let r = self.kv.get(&layout::oracle_price_key(pair));
        let Some(b) = self.kv(r) else {
            return TwapOutcome::NotFound;
        };
        if b.len() != 29 {
            return TwapOutcome::NotFound;
        }
        if self.price_paused(pair) {
            return TwapOutcome::Paused;
        }
        let now = self.height;
        match crate::twap::twap(&self.kv, pair, now, window) {
            Ok(twap) => TwapOutcome::Ok {
                twap,
                decimals: b[24],
            },
            Err(crate::twap::TwapError::NoPrice) => TwapOutcome::NotFound,
            Err(crate::twap::TwapError::BadWindow) => TwapOutcome::BadWindow,
        }
    }

    fn call_contract(&mut self, req: &CallRequest<'_>) -> CallReply {
        let refused = CallReply::refused;
        let Some(resolver) = self.resolver else {
            return refused(CallEnd::Unsupported);
        };
        let read_only = self.read_only || req.read_only;
        if read_only && req.kind == FunctionKind::Mutation {
            return refused(CallEnd::Forbidden(Trap::WriteInQuery));
        }
        // Re-entering a contract that is already on the stack (the caller included) is
        // refused; so is nesting past the depth limit (the entry point is frame 1).
        if req.target == self.contract || self.ancestors.contains(&req.target) {
            return refused(CallEnd::Reentrant);
        }
        if self.ancestors.len() + 2 > abi::MAX_CALL_DEPTH {
            return refused(CallEnd::Depth);
        }
        let callee = match resolver.resolve(&self.kv, &req.target) {
            Ok(Some(c)) => c,
            Ok(None) => return refused(CallEnd::NoContract),
            Err(e) => {
                self.note(Fault::Kv(e));
                return refused(CallEnd::Trapped);
            }
        };
        let code = callee.contract;
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
        if let Some(t) = &callee.tables {
            let base = self.tables;
            self.extra_tables
                .get_or_insert_with(|| base.clone())
                .absorb(t);
        }

        // The frame: its own writes can be undone, and its caller's state comes back after.
        let sp = self.kv.savepoint();
        let settled = self.settled.clone();
        let outer_fault = self.fault.take();
        let (outer_contract, outer_caller, outer_read_only) =
            (self.contract, self.caller, self.read_only);
        self.ancestors.push(outer_contract);
        self.caller = outer_contract;
        self.contract = req.target;
        self.read_only = read_only || req.kind == FunctionKind::Query;
        let ran = code.call(
            req.kind,
            req.function,
            req.args,
            self,
            req.gas,
            self.vm_memory,
        );
        self.ancestors.pop();
        self.contract = outer_contract;
        self.caller = outer_caller;
        self.read_only = outer_read_only;
        let frame_fault = self.fault.take();
        self.fault = outer_fault;

        let (mut end, used) = match ran {
            Ok(o) => (CallEnd::Ok(o.output), o.gas_used),
            Err(CallError::Trap { trap, gas_used }) => match trap {
                Trap::OutOfGas => (CallEnd::OutOfGas, req.gas),
                Trap::Abort(_) => (CallEnd::Aborted, gas_used),
                _ => (CallEnd::Trapped, gas_used),
            },
            Err(CallError::NoSuchEntry { .. }) => (CallEnd::NoFunction, 0),
            Err(_) => (CallEnd::Trapped, 0),
        };
        if let CallEnd::Ok(out) = &end {
            if out.len() > req.max_output {
                end = CallEnd::OutputTooLarge;
            }
        }
        if let Some(f) = frame_fault {
            if matches!(end, CallEnd::Ok(_)) {
                end = CallEnd::Trapped;
            }
            // A storage backend failure cannot be reverted around: it fails the transaction.
            if matches!(f, Fault::Kv(_)) {
                self.note(f);
            }
        }
        if matches!(end, CallEnd::Ok(_)) && !(read_only || req.kind == FunctionKind::Query) {
            // The callee pays for the bytes it grew; a balance that cannot cover them fails
            // the call, not the caller.
            match self.settle_contract(req.target) {
                Ok(()) => {}
                Err(Fault::InsufficientDeposit { .. }) => end = CallEnd::Deposit,
                Err(f) => {
                    if matches!(f, Fault::Kv(_)) {
                        self.note(f);
                    }
                    end = CallEnd::Trapped;
                }
            }
        }
        if matches!(end, CallEnd::Ok(_)) {
            self.kv.release(sp);
        } else {
            self.kv.rollback(sp);
            self.settled = settled;
        }
        CallReply {
            end,
            gas_used: used.min(req.gas),
        }
    }

    fn tokens(&mut self) -> Option<&mut dyn TokenStore> {
        Some(self)
    }
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
    fn contract_address(&self) -> Address {
        self.contract
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
        let t = self.extra_tables.as_ref().unwrap_or(self.tables);
        let c = self.contract;
        let r = t.insert(&mut self.kv, &c, table, row);
        self.tbl(r)
    }
    fn table_get(&mut self, table: &[u8], id: u64) -> Option<Vec<u8>> {
        let r = self.tables.get(&self.kv, &self.contract, table, id);
        self.tbl(r)
    }
    fn table_update(&mut self, table: &[u8], id: u64, row: &[u8]) -> bool {
        let t = self.extra_tables.as_ref().unwrap_or(self.tables);
        let c = self.contract;
        let r = t.update(&mut self.kv, &c, table, id, row);
        self.tbl(r)
    }
    fn table_delete(&mut self, table: &[u8], id: u64) -> bool {
        let t = self.extra_tables.as_ref().unwrap_or(self.tables);
        let c = self.contract;
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
    fn table_index_scan(&mut self, scan: &IndexScan) -> IndexScanPage {
        let (lo, hi) = (bound(&scan.lo), bound(&scan.hi));
        let opts = QueryOptions {
            order: if scan.descending {
                Order::Desc
            } else {
                Order::Asc
            },
            limit: scan.limit as usize,
            cursor: (!scan.cursor.is_empty()).then(|| Cursor(scan.cursor.clone())),
            filter: None,
        };
        let r = query::by_index_range(
            &self.kv,
            &self.contract,
            &scan.table,
            &scan.index,
            lo.as_ref(),
            hi.as_ref(),
            &opts,
        );
        match r {
            Ok(page) => IndexScanPage {
                ids: page.rows.iter().map(|r| r.id).collect(),
                next_cursor: page.next_cursor.map(|c| c.0).unwrap_or_default(),
            },
            Err(e) => {
                self.note(Fault::Kv(e));
                IndexScanPage::default()
            }
        }
    }
}
