//! Block execution.
//!
//! `Executor::execute_block` applies the block's transactions strictly in order, then the cron
//! jobs that are due at this height (as system transactions), all against one overlay of the
//! committed state. Only when every step has succeeded, the supply invariant holds and the new
//! Merkle root is computed is anything written, in a single atomic batch. A block-level error
//! leaves the state untouched.
//!
//! Per transaction: invalid transactions (bad signature, wrong chain id, wrong nonce, cannot
//! afford the fee, ...) change nothing and cost nothing. A valid transaction that fails while
//! running (out of gas, abort, ...) reverts its own state changes but still consumes its nonce
//! and pays for the gas it used.

use crate::block::{tx_root, Block};
use crate::state::{
    contract_key, cron_key, meta_head_key, money, nonce_key, read_u64, register_manifest,
    ChainConfig, ChainState, ContractRecord, Head, StateError, TAG_CRON,
};
use crate::tx::{
    contract_address, Address, Hash, Transaction, TxKind, MAX_ARG_BYTES, MAX_CODE_BYTES,
    MAX_NAME_BYTES,
};
use crate::{relay_pool, sha256, validator_pool, RELAY_SHARE_BPS, TOTAL_SUPPLY};
use btb_runtime::kv::{BatchOp, KvStore, Overlay, WriteBatch};
use btb_runtime::{layout, merkle, RuntimeHost};
use btb_vm::decode::Trap;
use btb_vm::{CallError, Contract, FunctionKind};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Why a transaction is not valid (it is skipped: no state change, no fee, nonce not used).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invalid {
    WrongChainId {
        expected: u64,
        got: u64,
    },
    BadSignature,
    /// Queries never go through consensus; actions are not transactions.
    KindNotAllowed(FunctionKind),
    BadNonce {
        expected: u64,
        got: u64,
    },
    InsufficientBalance {
        needed: u64,
        available: u64,
    },
    GasLimitTooLow {
        min: u64,
    },
    GasLimitTooHigh {
        max: u64,
    },
    /// `max_fee` is below `gas_limit * gas_price`.
    FeeCapTooLow {
        needed: u64,
        max_fee: u64,
    },
    TooLarge,
}

/// Why a valid transaction failed while running (state reverted, fee charged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revert {
    OutOfGas,
    Abort(String),
    Trap(String),
    NoSuchContract,
    NoSuchFunction,
    BadContract(String),
    BadManifest(String),
    AddressTaken,
    /// Storage, table or deposit fault (for example the contract cannot pay its deposit).
    Fault(String),
    Vm(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Success,
    Reverted(Revert),
    Invalid(Invalid),
    /// Cron only: the contract's balance could not reserve the cron gas; nothing ran.
    Paused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Tx { sender: Address },
    Cron { contract: Address, function: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// Transaction id, or `SHA256("btb.cron.v1" || height || contract || function)`.
    pub id: Hash,
    pub origin: Origin,
    pub status: Status,
    pub gas_used: u64,
    /// Base units charged (to the sender, or to the contract for a cron).
    pub fee: u64,
    pub output: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockError {
    BadHeight {
        expected: u64,
        got: u64,
    },
    BadParent,
    BadStateRoot,
    BadTxRoot,
    TimestampBeforeParent,
    TooManyTxs,
    BlockGasLimit,
    /// Balances + deposits no longer add up to the fixed supply (a bug; nothing was written).
    SupplyInvariant {
        expected: u128,
        found: u128,
    },
    State(StateError),
}

impl From<StateError> for BlockError {
    fn from(e: StateError) -> Self {
        BlockError::State(e)
    }
}
impl From<btb_runtime::KvError> for BlockError {
    fn from(e: btb_runtime::KvError) -> Self {
        BlockError::State(e.into())
    }
}
impl From<btb_runtime::merkle::MerkleError> for BlockError {
    fn from(e: btb_runtime::merkle::MerkleError) -> Self {
        BlockError::State(e.into())
    }
}

/// The most a transaction can take from the sender's balance.
pub fn max_cost(tx: &Transaction, cfg: &ChainConfig) -> Option<u64> {
    let fee = tx.gas_limit().checked_mul(cfg.gas_price)?;
    match &tx.body.kind {
        TxKind::Transfer { amount, .. } => fee.checked_add(*amount),
        _ => Some(fee),
    }
}

/// Checks that need no state: chain id, kind, sizes, signature and gas/fee limits.
pub fn check_static(tx: &Transaction, cfg: &ChainConfig) -> Result<(), Invalid> {
    if tx.body.chain_id != cfg.chain_id {
        return Err(Invalid::WrongChainId {
            expected: cfg.chain_id,
            got: tx.body.chain_id,
        });
    }
    match &tx.body.kind {
        TxKind::Call {
            kind,
            function,
            args,
            gas_limit,
            max_fee,
            ..
        } => {
            if *kind != FunctionKind::Mutation {
                return Err(Invalid::KindNotAllowed(*kind));
            }
            if function.len() > MAX_NAME_BYTES || args.len() > MAX_ARG_BYTES {
                return Err(Invalid::TooLarge);
            }
            if !tx.verify_signature() {
                return Err(Invalid::BadSignature);
            }
            let min = tx.intrinsic_gas();
            if *gas_limit < min {
                return Err(Invalid::GasLimitTooLow { min });
            }
            if *gas_limit > cfg.max_tx_gas {
                return Err(Invalid::GasLimitTooHigh {
                    max: cfg.max_tx_gas,
                });
            }
            let needed = gas_limit
                .checked_mul(cfg.gas_price)
                .ok_or(Invalid::TooLarge)?;
            if *max_fee < needed {
                return Err(Invalid::FeeCapTooLow {
                    needed,
                    max_fee: *max_fee,
                });
            }
        }
        TxKind::Deploy { code, manifest } => {
            if code.len() > MAX_CODE_BYTES || manifest.name.len() > MAX_NAME_BYTES {
                return Err(Invalid::TooLarge);
            }
            if !tx.verify_signature() {
                return Err(Invalid::BadSignature);
            }
        }
        TxKind::Transfer { .. } => {
            if !tx.verify_signature() {
                return Err(Invalid::BadSignature);
            }
        }
    }
    if max_cost(tx, cfg).is_none() {
        return Err(Invalid::TooLarge);
    }
    Ok(())
}

fn code_hash(code: &[u8]) -> Hash {
    sha256(&[b"btb.code.v1", code])
}

fn load_contract(
    cache: &mut BTreeMap<Hash, Arc<Contract>>,
    code: &[u8],
) -> Result<Arc<Contract>, StateError> {
    let h = code_hash(code);
    if let Some(c) = cache.get(&h) {
        return Ok(c.clone());
    }
    let c = Arc::new(
        Contract::from_elf(code).map_err(|e| StateError(format!("stored code invalid: {e:?}")))?,
    );
    cache.insert(h, c.clone());
    Ok(c)
}

/// The VM outcome of one call: gas used by the VM and either (output, writes) or a revert.
type RunResult = (u64, Result<(Vec<u8>, WriteBatch), Revert>);

/// Everything one block execution reads and writes before the atomic commit.
struct Ctx<'a, S: KvStore> {
    overlay: Overlay<'a, S>,
    tables: btb_runtime::Tables,
    cache: &'a mut BTreeMap<Hash, Arc<Contract>>,
    cfg: ChainConfig,
    height: u64,
}

/// What running a transaction body did.
struct Done {
    gas_used: u64,
    output: Vec<u8>,
    result: Result<(), Revert>,
}

impl<S: KvStore> Ctx<'_, S> {
    fn balance(&self, a: &Address) -> Result<u64, StateError> {
        Ok(read_u64(&self.overlay, &layout::balance_key(a))?)
    }

    fn set_u64(&mut self, key: &[u8], v: u64) -> Result<(), StateError> {
        Ok(self.overlay.put(key, &v.to_le_bytes())?)
    }

    fn set_balance(&mut self, a: &Address, v: u64) -> Result<(), StateError> {
        self.set_u64(&layout::balance_key(a), v)
    }

    fn credit(&mut self, a: &Address, amount: u64) -> Result<(), StateError> {
        let b = self
            .balance(a)?
            .checked_add(amount)
            .ok_or_else(|| StateError("balance overflow".into()))?;
        self.set_balance(a, b)
    }

    fn debit(&mut self, a: &Address, amount: u64) -> Result<(), StateError> {
        let b = self
            .balance(a)?
            .checked_sub(amount)
            .ok_or_else(|| StateError("debit exceeds balance".into()))?;
        self.set_balance(a, b)
    }

    /// Move `fee` from `payer` into the fee pools. User transactions split it
    /// `RELAY_SHARE_BPS` to the relay pool and the rest to the validator pool; crons pay the
    /// validator pool in full.
    fn charge(&mut self, payer: &Address, fee: u64, relay: bool) -> Result<(), StateError> {
        if fee == 0 {
            return Ok(());
        }
        self.debit(payer, fee)?;
        let relay_part = if relay {
            (u128::from(fee) * u128::from(RELAY_SHARE_BPS) / 10_000) as u64
        } else {
            0
        };
        self.credit(&relay_pool(), relay_part)?;
        self.credit(&validator_pool(), fee - relay_part)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_contract(
        &mut self,
        record: &ContractRecord,
        contract: Address,
        caller: Address,
        kind: FunctionKind,
        function: &str,
        args: &[u8],
        vm_gas: u64,
    ) -> Result<RunResult, StateError> {
        let c = load_contract(&mut *self.cache, &record.code)?;
        let mut host = RuntimeHost::new(&self.overlay, &self.tables, contract, caller, self.height);
        let r = c.call(
            kind,
            function,
            args,
            &mut host,
            vm_gas,
            self.cfg.vm_memory as usize,
        );
        Ok(match r {
            Ok(o) => {
                let used = o.gas_used.min(vm_gas);
                match host.finish() {
                    Ok(b) => (used, Ok((o.output, b))),
                    Err(f) => (used, Err(Revert::Fault(format!("{f:?}")))),
                }
            }
            Err(CallError::Trap { trap, gas_used }) => match trap {
                Trap::OutOfGas => (vm_gas, Err(Revert::OutOfGas)),
                Trap::Abort(m) => (gas_used.min(vm_gas), Err(Revert::Abort(m))),
                t => (gas_used.min(vm_gas), Err(Revert::Trap(format!("{t:?}")))),
            },
            Err(CallError::NoSuchEntry { .. }) => (0, Err(Revert::NoSuchFunction)),
            Err(e) => (0, Err(Revert::Vm(format!("{e:?}")))),
        })
    }

    fn contract_record(&self, a: &Address) -> Result<Option<ContractRecord>, StateError> {
        match self.overlay.get(&contract_key(a))? {
            None => Ok(None),
            Some(b) => Ok(Some(ContractRecord::decode(&b)?)),
        }
    }

    fn exec_transfer(
        &mut self,
        sender: &Address,
        to: &Address,
        amount: u64,
    ) -> Result<Done, StateError> {
        self.debit(sender, amount)?;
        self.credit(to, amount)?;
        Ok(Done {
            gas_used: crate::TRANSFER_GAS,
            output: Vec::new(),
            result: Ok(()),
        })
    }

    fn exec_deploy(
        &mut self,
        sender: &Address,
        nonce: u64,
        code: &[u8],
        manifest: &crate::tx::Manifest,
        gas: u64,
    ) -> Result<Done, StateError> {
        let fail = |r| -> Result<Done, StateError> {
            Ok(Done {
                gas_used: gas,
                output: Vec::new(),
                result: Err(r),
            })
        };
        let c = match Contract::from_elf(code) {
            Ok(c) => c,
            Err(e) => return fail(Revert::BadContract(format!("{e:?}"))),
        };
        if let Err(m) = manifest.validate() {
            return fail(Revert::BadManifest(m));
        }
        for (f, _) in &manifest.crons {
            let ok = c
                .entry_points()
                .iter()
                .any(|e| e.kind == FunctionKind::Cron && &e.name == f);
            if !ok {
                return fail(Revert::BadManifest(format!("no cron entry point {f:?}")));
            }
        }
        let addr = contract_address(sender, nonce);
        if self.overlay.get(&contract_key(&addr))?.is_some() {
            return fail(Revert::AddressTaken);
        }
        let mut tables = self.tables.clone();
        if let Err(m) = register_manifest(&mut tables, addr, manifest) {
            return fail(Revert::BadManifest(m));
        }
        // Nothing below can fail the transaction.
        self.tables = tables;
        let rec = ContractRecord {
            code: code.to_vec(),
            manifest: manifest.clone(),
            deployer: *sender,
            height: self.height,
        };
        self.overlay.put(&contract_key(&addr), &rec.encode())?;
        for (f, every) in &manifest.crons {
            if let Some(due) = self.height.checked_add(*every) {
                self.overlay
                    .put(&cron_key(due, &addr, f), &every.to_le_bytes())?;
            }
        }
        Ok(Done {
            gas_used: gas,
            output: addr.to_vec(),
            result: Ok(()),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn exec_call(
        &mut self,
        sender: &Address,
        contract: &Address,
        kind: FunctionKind,
        function: &str,
        args: &[u8],
        gas_limit: u64,
        intrinsic: u64,
    ) -> Result<Done, StateError> {
        let Some(rec) = self.contract_record(contract)? else {
            return Ok(Done {
                gas_used: intrinsic,
                output: Vec::new(),
                result: Err(Revert::NoSuchContract),
            });
        };
        let (used, res) = self.run_contract(
            &rec,
            *contract,
            *sender,
            kind,
            function,
            args,
            gas_limit - intrinsic,
        )?;
        let gas_used = intrinsic + used;
        Ok(match res {
            Ok((output, batch)) => {
                self.overlay.commit(batch)?;
                Done {
                    gas_used,
                    output,
                    result: Ok(()),
                }
            }
            Err(r) => Done {
                gas_used,
                output: Vec::new(),
                result: Err(r),
            },
        })
    }

    fn apply_tx(&mut self, tx: &Transaction) -> Result<Receipt, StateError> {
        let id = tx.id();
        let sender = tx.sender();
        let origin = Origin::Tx { sender };
        let invalid = |i| Receipt {
            id,
            origin: origin.clone(),
            status: Status::Invalid(i),
            gas_used: 0,
            fee: 0,
            output: Vec::new(),
        };
        if let Err(i) = check_static(tx, &self.cfg) {
            return Ok(invalid(i));
        }
        let expected = read_u64(&self.overlay, &nonce_key(&sender))?;
        let next = expected.checked_add(1);
        if tx.body.nonce != expected || next.is_none() {
            return Ok(invalid(Invalid::BadNonce {
                expected,
                got: tx.body.nonce,
            }));
        }
        let needed = max_cost(tx, &self.cfg).expect("checked by check_static");
        let available = self.balance(&sender)?;
        if available < needed {
            return Ok(invalid(Invalid::InsufficientBalance { needed, available }));
        }

        let done = match &tx.body.kind {
            TxKind::Transfer { to, amount } => self.exec_transfer(&sender, to, *amount)?,
            TxKind::Deploy { code, manifest } => {
                self.exec_deploy(&sender, tx.body.nonce, code, manifest, tx.intrinsic_gas())?
            }
            TxKind::Call {
                contract,
                kind,
                function,
                args,
                gas_limit,
                ..
            } => self.exec_call(
                &sender,
                contract,
                *kind,
                function,
                args,
                *gas_limit,
                tx.intrinsic_gas(),
            )?,
        };
        let fee = done
            .gas_used
            .checked_mul(self.cfg.gas_price)
            .ok_or_else(|| StateError("fee overflow".into()))?;
        self.set_u64(&nonce_key(&sender), next.expect("checked above"))?;
        self.charge(&sender, fee, true)?;
        Ok(Receipt {
            id,
            origin,
            status: match done.result {
                Ok(()) => Status::Success,
                Err(r) => Status::Reverted(r),
            },
            gas_used: done.gas_used,
            fee,
            output: done.output,
        })
    }

    /// Run the crons due at this height, in key order (due height, contract, function), until
    /// the per-block cron gas cap is reached; the rest wait for the next block.
    fn run_crons(&mut self, receipts: &mut Vec<Receipt>) -> Result<(), StateError> {
        let Some(next_h) = self.height.checked_add(1) else {
            return Ok(());
        };
        let mut end = vec![TAG_CRON];
        end.extend_from_slice(&next_h.to_be_bytes());
        let due = self.overlay.scan(
            &[TAG_CRON],
            Some(&end),
            false,
            self.cfg.max_crons_per_block as usize,
        )?;
        let reserve = self.cfg.cron_gas_limit * self.cfg.gas_price;
        let mut gas_total = 0u64;
        for (key, val) in due {
            if gas_total.saturating_add(self.cfg.cron_gas_limit) > self.cfg.cron_gas_block_cap {
                break;
            }
            let bad = || StateError("bad cron key".into());
            let contract: Address = key
                .get(9..41)
                .ok_or_else(bad)?
                .try_into()
                .map_err(|_| bad())?;
            let function =
                String::from_utf8(key.get(41..).ok_or_else(bad)?.to_vec()).map_err(|_| bad())?;
            let every = <[u8; 8]>::try_from(val.as_slice())
                .map(u64::from_le_bytes)
                .map_err(|_| bad())?;
            self.overlay.delete(&key)?;
            if let Some(next) = self.height.checked_add(every) {
                self.overlay
                    .put(&cron_key(next, &contract, &function), &every.to_le_bytes())?;
            }
            let id = sha256(&[
                b"btb.cron.v1",
                &self.height.to_be_bytes(),
                &contract,
                function.as_bytes(),
            ]);
            let origin = Origin::Cron {
                contract,
                function: function.clone(),
            };
            let mut receipt = Receipt {
                id,
                origin,
                status: Status::Paused,
                gas_used: 0,
                fee: 0,
                output: Vec::new(),
            };
            let balance = self.balance(&contract)?;
            if balance < reserve {
                receipts.push(receipt);
                continue;
            }
            let rec = self
                .contract_record(&contract)?
                .ok_or_else(|| StateError("cron of a missing contract".into()))?;
            let (used, res) = self.run_contract(
                &rec,
                contract,
                contract,
                FunctionKind::Cron,
                &function,
                &[],
                self.cfg.cron_gas_limit,
            )?;
            let fee = used * self.cfg.gas_price;
            let res = res.and_then(|(out, batch)| {
                // The deposit for new state comes out of the same balance as the fee.
                let key = layout::balance_key(&contract);
                let mut after = balance;
                for op in batch.ops() {
                    if let BatchOp::Put(k, v) = op {
                        if *k == key {
                            after = <[u8; 8]>::try_from(v.as_slice())
                                .map(u64::from_le_bytes)
                                .unwrap_or(0);
                        }
                    }
                }
                if after < fee {
                    Err(Revert::Fault(
                        "balance cannot pay the fee and deposit".into(),
                    ))
                } else {
                    Ok((out, batch))
                }
            });
            receipt.gas_used = used;
            receipt.fee = fee;
            match res {
                Ok((out, batch)) => {
                    self.overlay.commit(batch)?;
                    receipt.status = Status::Success;
                    receipt.output = out;
                }
                Err(r) => receipt.status = Status::Reverted(r),
            }
            self.charge(&contract, fee, false)?;
            gas_total += used;
            receipts.push(receipt);
        }
        Ok(())
    }
}

/// Applies blocks to a `ChainState`.
#[derive(Debug, Clone, Copy)]
pub struct Executor {
    /// Re-sum every balance and deposit after each block (O(accounts)). On by default; it is
    /// the supply invariant check, so only turn it off for benchmarks.
    pub check_supply: bool,
}

impl Default for Executor {
    fn default() -> Self {
        Executor { check_supply: true }
    }
}

impl Executor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Execute `block` on top of the state's head. Returns one receipt per transaction in
    /// order, then one per cron run, and the new state root. On `Err` the state is unchanged.
    pub fn execute_block<S: KvStore>(
        &self,
        state: &mut ChainState<S>,
        block: &Block,
    ) -> Result<(Vec<Receipt>, Hash), BlockError> {
        let ChainState {
            kv,
            tables,
            code_cache,
            config,
        } = state;
        let cfg = *config;
        let h = &block.header;

        let head_bytes = kv
            .get(&meta_head_key())?
            .ok_or_else(|| StateError("no head".into()))?;
        let head = Head::decode(&head_bytes).map_err(StateError::from)?;
        if h.height != head.height + 1 {
            return Err(BlockError::BadHeight {
                expected: head.height + 1,
                got: h.height,
            });
        }
        if h.parent_hash != head.hash {
            return Err(BlockError::BadParent);
        }
        let root_before =
            merkle::root(&*kv, head.height)?.ok_or_else(|| StateError("missing root".into()))?;
        if h.state_root != root_before {
            return Err(BlockError::BadStateRoot);
        }
        if h.timestamp < head.timestamp {
            return Err(BlockError::TimestampBeforeParent);
        }
        if h.tx_root != tx_root(&block.txs) {
            return Err(BlockError::BadTxRoot);
        }
        if block.txs.len() > cfg.max_block_txs as usize {
            return Err(BlockError::TooManyTxs);
        }
        let gas = block
            .txs
            .iter()
            .fold(0u64, |a, t| a.saturating_add(t.gas_limit()));
        if gas > cfg.block_gas_limit {
            return Err(BlockError::BlockGasLimit);
        }

        let mut ctx = Ctx {
            overlay: Overlay::new(&*kv),
            tables: tables.clone(),
            cache: code_cache,
            cfg,
            height: h.height,
        };
        let mut receipts = Vec::with_capacity(block.txs.len());
        for tx in &block.txs {
            receipts.push(ctx.apply_tx(tx)?);
        }
        ctx.run_crons(&mut receipts)?;
        let new_head = Head {
            height: h.height,
            hash: h.hash(),
            timestamp: h.timestamp,
        };
        ctx.overlay.put(&meta_head_key(), &new_head.encode())?;

        if self.check_supply {
            let found = money(&ctx.overlay)?;
            if found != u128::from(TOTAL_SUPPLY) {
                return Err(BlockError::SupplyInvariant {
                    expected: u128::from(TOTAL_SUPPLY),
                    found,
                });
            }
        }

        let Ctx {
            overlay,
            tables: new_tables,
            ..
        } = ctx;
        let mut batch = overlay.into_batch();
        let (new_root, tree) = merkle::prepare(&*kv, h.height, &batch)?;
        for op in tree.ops() {
            match op {
                BatchOp::Put(k, v) => batch.put(k.clone(), v.clone()),
                BatchOp::Delete(k) => batch.delete(k.clone()),
            }
        }
        kv.commit(batch)?;
        *tables = new_tables;
        Ok((receipts, new_root))
    }

    /// Run a query locally against the head state (no consensus, nothing is written). Returns
    /// the output and the gas used.
    pub fn query<S: KvStore>(
        &self,
        state: &mut ChainState<S>,
        contract: &Address,
        function: &str,
        args: &[u8],
        caller: &Address,
    ) -> Result<(Vec<u8>, u64), Revert> {
        let fault = |e: StateError| Revert::Fault(e.0);
        let height = state.head().map_err(fault)?.height;
        let rec = state
            .contract(contract)
            .map_err(fault)?
            .ok_or(Revert::NoSuchContract)?;
        let c = load_contract(&mut state.code_cache, &rec.code).map_err(fault)?;
        let mut host = RuntimeHost::new(&state.kv, &state.tables, *contract, *caller, height);
        host.read_only = true;
        match c.call(
            FunctionKind::Query,
            function,
            args,
            &mut host,
            state.config.query_gas,
            state.config.vm_memory as usize,
        ) {
            Ok(o) => Ok((o.output, o.gas_used)),
            Err(CallError::Trap { trap, .. }) => Err(match trap {
                Trap::OutOfGas => Revert::OutOfGas,
                Trap::Abort(m) => Revert::Abort(m),
                t => Revert::Trap(format!("{t:?}")),
            }),
            Err(CallError::NoSuchEntry { .. }) => Err(Revert::NoSuchFunction),
            Err(e) => Err(Revert::Vm(format!("{e:?}"))),
        }
    }
}
