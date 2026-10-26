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

use crate::actions::{self, ActionError, ActionOp, ActionVote};
use crate::block::{tx_root, Block};
use crate::parallel::{self, ParResult, ParStats};
use crate::staking::{self, Fail, StakeError};
use crate::state::{
    contract_key, cron_key, meta_head_key, money, nonce_key, read_u64, register_manifest,
    ChainConfig, ChainState, ContractRecord, Head, StateError, TAG_CRON,
};
use crate::tx::{
    contract_address, Address, Hash, Transaction, TxKind, MAX_ARG_BYTES, MAX_CODE_BYTES,
    MAX_NAME_BYTES,
};
use crate::{sha256, validator_pool, RELAY_SHARE_BPS, TOTAL_SUPPLY};

/// The relay host's part of `fee` when a registered relayer is paid, else 0.
pub(crate) fn relay_share(fee: u64, paid: bool) -> u64 {
    if paid {
        (u128::from(fee) * u128::from(RELAY_SHARE_BPS) / 10_000) as u64
    } else {
        0
    }
}
use btb_codec::{Decode, Encode};
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
    /// A staking transaction was refused (nothing changed).
    Stake(StakeError),
    /// A payment-channel transaction was refused (nothing changed).
    Channel(crate::channel::ChannelError),
    /// A site transaction was refused (nothing changed).
    Site(crate::site::SiteError),
    /// A DEX transaction was refused (nothing changed).
    Dex(crate::dex::DexError),
    /// A storage transaction was refused (nothing changed).
    Storage(crate::storage::StorageError),
    /// A beacon key-generation transaction was refused (nothing changed).
    Dkg(crate::dkg::DkgError),
    /// A cross-chain transaction was refused (nothing changed).
    Cross(crate::cross::CrossError),
    /// An action transaction was refused (nothing changed).
    Action(ActionError),
    /// A hosting-node registry transaction was refused (nothing changed).
    Host(crate::host::HostError),
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
    Tx {
        sender: Address,
    },
    Cron {
        contract: Address,
        function: String,
    },
    /// The callback delivering the result of the HTTPS action request `request`.
    Action {
        contract: Address,
        function: String,
        request: Hash,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// Transaction id, `SHA256("btb.cron.v1" || height || contract || function)`, or for an
    /// action callback `SHA256("btb.action.cb.v1" || request id)`.
    pub id: Hash,
    pub origin: Origin,
    pub status: Status,
    pub gas_used: u64,
    /// Base units charged (to the sender, or to the contract for a cron).
    pub fee: u64,
    /// The registered relay host that earned `RELAY_SHARE_BPS` of `fee` (None: no relayer, or
    /// not a registered host, or the sender itself; the validator pool got the whole fee).
    pub relayer: Option<Address>,
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
    /// The block's oracle input is missing, malformed or does not verify (wrong beacon, ...).
    Oracle(String),
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
        TxKind::Transfer { amount, .. } | TxKind::Stake { amount, .. } => fee.checked_add(*amount),
        TxKind::Channel(op) => fee.checked_add(op.debit()),
        TxKind::Dex(op) => fee.checked_add(op.debit()),
        TxKind::Storage(op) => fee.checked_add(op.debit(&cfg.storage)),
        TxKind::Action(op) => fee.checked_add(op.debit()),
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
        TxKind::Transfer { .. } | TxKind::Stake { .. } | TxKind::Unstake { .. } => {
            if !tx.verify_signature() {
                return Err(Invalid::BadSignature);
            }
        }
        TxKind::RegisterValidator { network_addr, .. } => {
            if network_addr.len() > staking::MAX_NETWORK_ADDR {
                return Err(Invalid::TooLarge);
            }
            if !tx.verify_signature() {
                return Err(Invalid::BadSignature);
            }
        }
        TxKind::SubmitEvidence { .. } | TxKind::Channel(_) | TxKind::Dex(_) => {
            if !tx.verify_signature() {
                return Err(Invalid::BadSignature);
            }
        }
        TxKind::Storage(op) => {
            if op.check_static().is_err() {
                return Err(Invalid::TooLarge);
            }
            if !tx.verify_signature() {
                return Err(Invalid::BadSignature);
            }
        }
        TxKind::Dkg(op) => {
            if op.check_static().is_err() {
                return Err(Invalid::TooLarge);
            }
            if !tx.verify_signature() {
                return Err(Invalid::BadSignature);
            }
        }
        TxKind::Cross(op) => {
            if op.check_static().is_err() {
                return Err(Invalid::TooLarge);
            }
            if !tx.verify_signature() {
                return Err(Invalid::BadSignature);
            }
        }
        TxKind::Action(op) => {
            if op.check_static().is_err() {
                return Err(Invalid::TooLarge);
            }
            if !tx.verify_signature() {
                return Err(Invalid::BadSignature);
            }
            let min = tx.intrinsic_gas();
            let limit = op.gas_limit();
            if limit < min {
                return Err(Invalid::GasLimitTooLow { min });
            }
            if limit > cfg.max_tx_gas {
                return Err(Invalid::GasLimitTooHigh {
                    max: cfg.max_tx_gas,
                });
            }
        }
        TxKind::Host(op) => {
            if op.check_static().is_err() {
                return Err(Invalid::TooLarge);
            }
            if !tx.verify_signature() {
                return Err(Invalid::BadSignature);
            }
        }
        TxKind::Site(op) => {
            // Too big or malformed files never enter a block.
            if op.check_static().is_err() {
                return Err(Invalid::TooLarge);
            }
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
pub(crate) struct Ctx<'a, S: KvStore> {
    pub(crate) overlay: Overlay<'a, S>,
    pub(crate) tables: btb_runtime::Tables,
    pub(crate) cache: &'a mut BTreeMap<Hash, Arc<Contract>>,
    pub(crate) cfg: ChainConfig,
    pub(crate) height: u64,
    /// Parallel mode: `charge` only debits the payer; the pool credits are summed from the
    /// receipts and applied once after the transactions (they commute, so the result is the
    /// same, and no two transactions conflict on the pool balances).
    pub(crate) defer_fees: bool,
    /// Parallel mode: register a called contract's tables from its manifest before the call
    /// (the shared registry cannot be mutated by concurrent deploys).
    pub(crate) lazy_tables: bool,
    pub(crate) registered: std::collections::BTreeSet<Address>,
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

    /// Move `fee` from `payer` into the fee pools. A user transaction (`relayer` is `Some`
    /// for it, `None` for a cron) with a registered relayer other than the payer pays that
    /// relayer `RELAY_SHARE_BPS` of the fee; everything else goes to the validator pool.
    /// Returns the relayer that earns a share. In parallel mode (`defer_fees`) the credits are
    /// applied after the block from the receipts.
    fn charge(
        &mut self,
        payer: &Address,
        fee: u64,
        relayer: Option<Address>,
    ) -> Result<Option<Address>, StateError> {
        if fee == 0 {
            return Ok(None);
        }
        self.debit(payer, fee)?;
        let paid = match relayer {
            Some(r) if &r != payer && crate::host::is_host(&self.overlay, &r)? => Some(r),
            _ => None,
        };
        if self.defer_fees {
            return Ok(paid);
        }
        let relay_part = relay_share(fee, paid.is_some());
        if let Some(r) = &paid {
            self.credit(r, relay_part)?;
        }
        self.credit(&validator_pool(), fee - relay_part)?;
        Ok(paid)
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
        entropy_id: &Hash,
    ) -> Result<RunResult, StateError> {
        let c = load_contract(&mut *self.cache, &record.code)?;
        // The callback of an HTTPS action takes the agreed result of validators; only the chain
        // delivering it (caller `actions::CALLER`) may run it, never a transaction.
        if kind == FunctionKind::Mutation
            && caller != actions::CALLER
            && c.is_action_callback(function)
        {
            return Ok((0, Err(Revert::Action(ActionError::CallbackIsSystemOnly))));
        }
        let mut host = RuntimeHost::new(&self.overlay, &self.tables, contract, caller, self.height);
        // Randomness: only a marked mutation or cron may ask (the VM enforces that); its
        // bytes derive from the block beacon, this id (tx id, or the cron receipt id) and the
        // call index. Such a function is always charged its full gas limit, so a caller
        // cannot make an unwelcome outcome fail by running out of gas (see ORACLES.md).
        let uses_randomness = kind != FunctionKind::Query && c.is_randomness(kind, function);
        if uses_randomness {
            host.enable_randomness(*entropy_id);
        }
        let r = c.call(
            kind,
            function,
            args,
            &mut host,
            vm_gas,
            self.cfg.vm_memory as usize,
        );
        let full = |used: u64| if uses_randomness { vm_gas } else { used };
        Ok(match r {
            Ok(o) => {
                let used = full(o.gas_used.min(vm_gas));
                match host.finish() {
                    Ok(b) => (used, Ok((o.output, b))),
                    Err(f) => (used, Err(Revert::Fault(format!("{f:?}")))),
                }
            }
            Err(CallError::Trap { trap, gas_used }) => match trap {
                Trap::OutOfGas => (vm_gas, Err(Revert::OutOfGas)),
                Trap::Abort(m) => (full(gas_used.min(vm_gas)), Err(Revert::Abort(m))),
                t => (
                    full(gas_used.min(vm_gas)),
                    Err(Revert::Trap(format!("{t:?}"))),
                ),
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

    /// Run a staking operation. A refused one changes nothing (each operation checks
    /// everything before it writes) and is a revert: nonce used, fee charged.
    fn staking_done(
        &mut self,
        gas: u64,
        run: impl FnOnce(&mut Self) -> Result<Vec<u8>, Fail>,
    ) -> Result<Done, StateError> {
        match run(self) {
            Ok(output) => Ok(Done {
                gas_used: gas,
                output,
                result: Ok(()),
            }),
            Err(Fail::Tx(e)) => Ok(Done {
                gas_used: gas,
                output: Vec::new(),
                result: Err(Revert::Stake(e)),
            }),
            Err(Fail::State(e)) => Err(e),
        }
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
        tx_id: &Hash,
    ) -> Result<Done, StateError> {
        let Some(rec) = self.contract_record(contract)? else {
            return Ok(Done {
                gas_used: intrinsic,
                output: Vec::new(),
                result: Err(Revert::NoSuchContract),
            });
        };
        if self.lazy_tables && self.registered.insert(*contract) {
            if let Err(m) = register_manifest(&mut self.tables, *contract, &rec.manifest) {
                return Err(StateError(format!("stored manifest invalid: {m}")));
            }
        }
        let (used, res) = self.run_contract(
            &rec,
            *contract,
            *sender,
            kind,
            function,
            args,
            gas_limit - intrinsic,
            tx_id,
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

    /// An HTTPS action transaction (`crate::actions`).
    fn exec_action(
        &mut self,
        sender: &Address,
        op: &ActionOp,
        id: &Hash,
        intrinsic: u64,
    ) -> Result<Done, StateError> {
        let refuse = |gas_used: u64, e: ActionError| Done {
            gas_used,
            output: Vec::new(),
            result: Err(Revert::Action(e)),
        };
        match op {
            ActionOp::SetHosts { contract, hosts } => {
                let Some(rec) = self.contract_record(contract)? else {
                    return Ok(refuse(intrinsic, ActionError::NoSuchContract));
                };
                if &rec.deployer != sender {
                    return Ok(refuse(intrinsic, ActionError::NotDeployer));
                }
                actions::set_hosts(&mut self.overlay, contract, hosts)?;
                Ok(Done {
                    gas_used: intrinsic,
                    output: Vec::new(),
                    result: Ok(()),
                })
            }
            ActionOp::Request {
                contract,
                function,
                args,
                gas_limit,
                max_fee,
            } => {
                let Some(rec) = self.contract_record(contract)? else {
                    return Ok(refuse(intrinsic, ActionError::NoSuchContract));
                };
                if self.lazy_tables && self.registered.insert(*contract) {
                    if let Err(m) = register_manifest(&mut self.tables, *contract, &rec.manifest) {
                        return Err(StateError(format!("stored manifest invalid: {m}")));
                    }
                }
                let c = load_contract(&mut *self.cache, &rec.code)?;
                if !c.has_entry(FunctionKind::Action, function) {
                    return Ok(refuse(intrinsic, ActionError::NoSuchAction));
                }
                let callback = format!("{function}{}", btb_vm::contract::CALLBACK_SUFFIX);
                if !c.has_entry(FunctionKind::Mutation, &callback) {
                    return Ok(refuse(intrinsic, ActionError::NoCallback));
                }
                if actions::pending_count(&self.overlay)? >= actions::MAX_PENDING
                    || actions::pending_list(&self.overlay)?
                        .iter()
                        .filter(|(_, p)| p.requester == *sender)
                        .count()
                        >= actions::MAX_PENDING_PER_REQUESTER
                {
                    return Ok(refuse(intrinsic, ActionError::TooManyPending));
                }
                let epoch = self.height.saturating_sub(1) / self.cfg.staking.epoch_len;
                let Some(epoch_rec) = staking::epoch_record(&self.overlay, epoch)? else {
                    return Ok(refuse(intrinsic, ActionError::NoValidators));
                };
                // The escrow pays every validator's answer and the callback's full gas limit.
                let needed = (self.cfg.cron_gas_limit)
                    .saturating_add(actions::RESPONSE_GAS * epoch_rec.validators.len() as u64)
                    .saturating_mul(self.cfg.gas_price);
                if *max_fee < needed {
                    return Ok(refuse(
                        intrinsic,
                        ActionError::EscrowTooLow {
                            needed,
                            max_fee: *max_fee,
                        },
                    ));
                }
                // First phase: read-only (the VM forces it), returns the request to make.
                let (used, res) = self.run_contract(
                    &rec,
                    *contract,
                    *sender,
                    FunctionKind::Action,
                    function,
                    args,
                    gas_limit - intrinsic,
                    id,
                )?;
                let gas_used = intrinsic + used;
                let output = match res {
                    Ok((output, _no_writes)) => output,
                    Err(r) => {
                        return Ok(Done {
                            gas_used,
                            output: Vec::new(),
                            result: Err(r),
                        })
                    }
                };
                let Ok(spec) = btb_codec::http::HttpRequest::decode(&output) else {
                    return Ok(refuse(gas_used, ActionError::BadRequestSpec));
                };
                let hosts = actions::hosts_of(&self.overlay, contract)?;
                let spec = match actions::validate_request(spec, &hosts) {
                    Ok(s) => s,
                    Err(e) => return Ok(refuse(gas_used, e)),
                };
                let pending = actions::Pending {
                    requester: *sender,
                    contract: *contract,
                    function: function.clone(),
                    args: args.clone(),
                    request: spec.encode(),
                    hosts,
                    created: self.height,
                    deadline: self.height.saturating_add(actions::DEADLINE_BLOCKS),
                    escrow: *max_fee,
                };
                self.debit(sender, *max_fee)?;
                self.credit(&actions::escrow_pool(), *max_fee)?;
                actions::pending_put(&mut self.overlay, id, &pending)?;
                Ok(Done {
                    gas_used,
                    output: id.to_vec(),
                    result: Ok(()),
                })
            }
        }
    }

    /// End of block, after the crons: take in the validators' votes on pending HTTPS actions and
    /// deliver every request that reached a verdict (agreement, disagreement or deadline) to its
    /// callback. Bad votes are skipped, never an error. Deliveries share the cron gas caps; the
    /// ones that do not fit wait for the next block.
    fn run_actions(
        &mut self,
        votes: &[ActionVote],
        receipts: &mut Vec<Receipt>,
    ) -> Result<(), StateError> {
        if actions::pending_count(&self.overlay)? == 0 {
            return Ok(());
        }
        let epoch = self.height.saturating_sub(1) / self.cfg.staking.epoch_len;
        let rec = staking::epoch_record(&self.overlay, epoch)?;
        let validators: BTreeMap<[u8; 32], (u64, Address)> = rec
            .iter()
            .flat_map(|r| r.validators.iter())
            .map(|v| (v.consensus_key, (v.stake, v.operator)))
            .collect();
        let total: u128 = validators.values().map(|(s, _)| u128::from(*s)).sum();
        for v in votes.iter().take(actions::MAX_VOTES_PER_BLOCK) {
            if !validators.contains_key(&v.key)
                || !v.verify(self.cfg.chain_id)
                || actions::pending_get(&self.overlay, &v.request)?.is_none()
            {
                continue;
            }
            actions::record_vote(&mut self.overlay, v)?;
        }
        let mut delivered = 0usize;
        let mut gas_total = 0u64;
        for (id, p) in actions::pending_list(&self.overlay)? {
            let mut tallies: BTreeMap<Hash, u128> = BTreeMap::new();
            let mut voters = Vec::new();
            for (key, outcome) in actions::votes_of(&self.overlay, &id)? {
                if let Some((stake, operator)) = validators.get(&key) {
                    *tallies.entry(outcome).or_default() += u128::from(*stake);
                    voters.push(*operator);
                }
            }
            let tallies: Vec<(Hash, u128)> = tallies.into_iter().collect();
            let (status, data) = match actions::decide(self.height, p.deadline, total, &tallies) {
                actions::Verdict::Wait => continue,
                actions::Verdict::Agreed(h) => actions::outcome_get(&self.overlay, &id, &h)?
                    .unwrap_or((actions::STATUS_FAILED, b"outcome missing".to_vec())),
                actions::Verdict::Disagreed => (actions::STATUS_DISAGREEMENT, Vec::new()),
                actions::Verdict::TimedOut => (actions::STATUS_TIMEOUT, Vec::new()),
            };
            if delivered >= self.cfg.max_crons_per_block as usize
                || gas_total.saturating_add(self.cfg.cron_gas_limit) > self.cfg.cron_gas_block_cap
            {
                break;
            }
            delivered += 1;
            gas_total += self.settle_action(&id, &p, status, &data, &voters, receipts)?;
        }
        Ok(())
    }

    /// Deliver `(status, data)` to the callback of request `id`, then settle its escrow: the
    /// callback's gas to the validator pool, `RESPONSE_GAS` to each validator that answered
    /// (capped by what is left), the rest back to the requester. Returns the callback's gas.
    fn settle_action(
        &mut self,
        id: &Hash,
        p: &actions::Pending,
        status: u8,
        data: &[u8],
        voters: &[Address],
        receipts: &mut Vec<Receipt>,
    ) -> Result<u64, StateError> {
        actions::remove(&mut self.overlay, id)?;
        let callback = format!("{}{}", p.function, btb_vm::contract::CALLBACK_SUFFIX);
        let cb_id = sha256(&[b"btb.action.cb.v1", id]);
        let mut receipt = Receipt {
            id: cb_id,
            origin: Origin::Action {
                contract: p.contract,
                function: callback.clone(),
                request: *id,
            },
            status: Status::Success,
            gas_used: 0,
            fee: 0,
            relayer: None,
            output: Vec::new(),
        };
        let price = self.cfg.gas_price;
        let mut used = 0;
        match self.contract_record(&p.contract)? {
            None => receipt.status = Status::Reverted(Revert::NoSuchContract),
            Some(rec) => {
                let input = (*id, p.args.clone(), status, data.to_vec()).encode();
                let (u, res) = self.run_contract(
                    &rec,
                    p.contract,
                    actions::CALLER,
                    FunctionKind::Mutation,
                    &callback,
                    &input,
                    self.cfg.cron_gas_limit,
                    &cb_id,
                )?;
                used = u;
                match res {
                    Ok((out, batch)) => {
                        self.overlay.commit(batch)?;
                        receipt.output = out;
                    }
                    Err(r) => receipt.status = Status::Reverted(r),
                }
            }
        }
        let fee = used.saturating_mul(price).min(p.escrow);
        receipt.gas_used = used;
        receipt.fee = fee;
        self.debit(&actions::escrow_pool(), p.escrow)?;
        self.credit(&validator_pool(), fee)?;
        let mut left = p.escrow - fee;
        if !voters.is_empty() {
            let per = (actions::RESPONSE_GAS.saturating_mul(price)).min(left / voters.len() as u64);
            for v in voters {
                self.credit(v, per)?;
            }
            left -= per * voters.len() as u64;
        }
        self.credit(&p.requester, left)?;
        receipts.push(receipt);
        Ok(used)
    }

    pub(crate) fn apply_tx(&mut self, tx: &Transaction) -> Result<Receipt, StateError> {
        let id = tx.id();
        let sender = tx.sender();
        let origin = Origin::Tx { sender };
        let invalid = |i| Receipt {
            id,
            origin: origin.clone(),
            status: Status::Invalid(i),
            gas_used: 0,
            fee: 0,
            relayer: None,
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
            TxKind::Stake { validator, amount } => self.staking_done(tx.intrinsic_gas(), |c| {
                staking::stake(&mut c.overlay, &sender, validator, *amount)?;
                Ok(Vec::new())
            })?,
            TxKind::Unstake { amount } => self.staking_done(tx.intrinsic_gas(), |c| {
                let (cfg, height) = (c.cfg.staking, c.height);
                staking::unstake(&mut c.overlay, &cfg, &sender, *amount, height)?;
                Ok(Vec::new())
            })?,
            TxKind::RegisterValidator {
                consensus_key,
                network_addr,
            } => self.staking_done(tx.intrinsic_gas(), |c| {
                staking::register_validator(&mut c.overlay, &sender, consensus_key, network_addr)?;
                Ok(Vec::new())
            })?,
            TxKind::SubmitEvidence { epoch, evidence } => {
                self.staking_done(tx.intrinsic_gas(), |c| {
                    let (cfg, chain, height) = (c.cfg.staking, c.cfg.chain_id, c.height);
                    let o = staking::submit_evidence(
                        &mut c.overlay,
                        &cfg,
                        chain,
                        &sender,
                        height,
                        *epoch,
                        evidence,
                    )?;
                    Ok(o.slashed.to_le_bytes().to_vec())
                })?
            }
            TxKind::Channel(op) => {
                let (chain, height) = (self.cfg.chain_id, self.height);
                let r = crate::channel::apply(
                    &mut self.overlay,
                    chain,
                    &sender,
                    &tx.public_key,
                    tx.body.nonce,
                    height,
                    op,
                );
                match r {
                    Ok(output) => Done {
                        gas_used: tx.intrinsic_gas(),
                        output,
                        result: Ok(()),
                    },
                    Err(crate::channel::Fail::Tx(e)) => Done {
                        gas_used: tx.intrinsic_gas(),
                        output: Vec::new(),
                        result: Err(Revert::Channel(e)),
                    },
                    Err(crate::channel::Fail::State(e)) => return Err(e),
                }
            }
            TxKind::Dex(op) => {
                let (nonce, height) = (tx.body.nonce, self.height);
                let gas_used = tx.intrinsic_gas();
                match crate::dex::apply(&mut self.overlay, &sender, nonce, height, op) {
                    Ok(output) => Done {
                        gas_used,
                        output,
                        result: Ok(()),
                    },
                    Err(crate::dex::Fail::Tx(e)) => Done {
                        gas_used,
                        output: Vec::new(),
                        result: Err(Revert::Dex(e)),
                    },
                    Err(crate::dex::Fail::State(e)) => return Err(e),
                }
            }
            TxKind::Storage(op) => {
                let (scfg, height) = (self.cfg.storage, self.height);
                let gas_used = tx.intrinsic_gas();
                match crate::storage::apply(&mut self.overlay, &scfg, &sender, height, op) {
                    Ok(output) => Done {
                        gas_used,
                        output,
                        result: Ok(()),
                    },
                    Err(crate::storage::Fail::Tx(e)) => Done {
                        gas_used,
                        output: Vec::new(),
                        result: Err(Revert::Storage(e)),
                    },
                    Err(crate::storage::Fail::State(e)) => return Err(e),
                }
            }
            TxKind::Dkg(op) => {
                let (chain, height) = (self.cfg.chain_id, self.height);
                let gas_used = tx.intrinsic_gas();
                match crate::dkg::apply(&mut self.overlay, chain, height, &tx.public_key, op) {
                    Ok(output) => Done {
                        gas_used,
                        output,
                        result: Ok(()),
                    },
                    Err(crate::dkg::Fail::Tx(e)) => Done {
                        gas_used,
                        output: Vec::new(),
                        result: Err(Revert::Dkg(e)),
                    },
                    Err(crate::dkg::Fail::State(e)) => return Err(e),
                }
            }
            TxKind::Cross(op) => {
                let (scfg, chain, height) = (self.cfg.staking, self.cfg.chain_id, self.height);
                let gas_used = tx.intrinsic_gas();
                match crate::cross::apply(
                    &mut self.overlay,
                    &scfg,
                    chain,
                    height,
                    &sender,
                    &tx.public_key,
                    op,
                ) {
                    Ok(output) => Done {
                        gas_used,
                        output,
                        result: Ok(()),
                    },
                    Err(crate::cross::Fail::Tx(e)) => Done {
                        gas_used,
                        output: Vec::new(),
                        result: Err(Revert::Cross(e)),
                    },
                    Err(crate::cross::Fail::State(e)) => return Err(e),
                }
            }
            TxKind::Action(op) => self.exec_action(&sender, op, &id, tx.intrinsic_gas())?,
            TxKind::Host(op) => {
                let gas_used = tx.intrinsic_gas();
                match crate::host::apply(&mut self.overlay, &self.cfg, self.height, &sender, op) {
                    Ok(()) => Done {
                        gas_used,
                        output: Vec::new(),
                        result: Ok(()),
                    },
                    Err(crate::host::Fail::Tx(e)) => Done {
                        gas_used,
                        output: Vec::new(),
                        result: Err(Revert::Host(e)),
                    },
                    Err(crate::host::Fail::State(e)) => return Err(e),
                }
            }
            TxKind::Site(op) => {
                let gas_used = tx.intrinsic_gas();
                match crate::site::apply(&mut self.overlay, &sender, op) {
                    Ok(output) => Done {
                        gas_used,
                        output,
                        result: Ok(()),
                    },
                    Err(crate::site::Fail::Tx(e)) => Done {
                        gas_used,
                        output: Vec::new(),
                        result: Err(Revert::Site(e)),
                    },
                    Err(crate::site::Fail::State(e)) => return Err(e),
                }
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
                &id,
            )?,
        };
        let fee = done
            .gas_used
            .checked_mul(self.cfg.gas_price)
            .ok_or_else(|| StateError("fee overflow".into()))?;
        self.set_u64(&nonce_key(&sender), next.expect("checked above"))?;
        let relayer = self.charge(&sender, fee, tx.body.relayer)?;
        Ok(Receipt {
            id,
            origin,
            status: match done.result {
                Ok(()) => Status::Success,
                Err(r) => Status::Reverted(r),
            },
            gas_used: done.gas_used,
            fee,
            relayer,
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
                relayer: None,
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
                &id,
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
            self.charge(&contract, fee, None)?;
            gas_total += used;
            receipts.push(receipt);
        }
        Ok(())
    }
}

/// How the transactions of a block are executed. Both modes give byte-identical receipts,
/// state roots, fees and state; `Sequential` is the reference (the oracle the parallel mode is
/// tested against). Crons and the epoch / staking end-of-block logic always run sequentially,
/// after the transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecMode {
    Sequential,
    /// Block-STM style speculative execution on `threads` workers (`0` = one per CPU).
    /// Blocks smaller than `Executor::parallel_min_txs`, and stores that cannot be shared between
    /// threads (`KvStore::as_sync` is `None`), run sequentially.
    Parallel {
        threads: usize,
    },
}

/// Applies blocks to a `ChainState`.
#[derive(Debug, Clone, Copy)]
pub struct Executor {
    /// Re-sum every balance and deposit after each block (O(accounts)). On by default; it is
    /// the supply invariant check, so only turn it off for benchmarks.
    pub check_supply: bool,
    pub mode: ExecMode,
    /// In parallel mode, run statically confined transactions (see `docs/CHAIN.md`) without
    /// conflict tracking.
    pub fast_path: bool,
    /// Blocks with fewer transactions run sequentially even in parallel mode (starting threads
    /// costs more than it saves). Default 16; the result is the same either way.
    pub parallel_min_txs: usize,
}

impl Default for Executor {
    fn default() -> Self {
        Executor {
            check_supply: true,
            mode: ExecMode::Parallel { threads: 0 },
            fast_path: true,
            parallel_min_txs: 16,
        }
    }
}

impl Executor {
    pub fn new() -> Self {
        Self::default()
    }

    /// The reference executor.
    pub fn sequential() -> Self {
        Executor {
            mode: ExecMode::Sequential,
            ..Self::default()
        }
    }

    /// Parallel executor on `threads` workers (`0` = one per CPU).
    pub fn parallel(threads: usize) -> Self {
        Executor {
            mode: ExecMode::Parallel { threads },
            ..Self::default()
        }
    }

    /// Execute `block` on top of the state's head. Returns one receipt per transaction in
    /// order, then one per cron run, and the new state root. On `Err` the state is unchanged.
    pub fn execute_block<S: KvStore>(
        &self,
        state: &mut ChainState<S>,
        block: &Block,
    ) -> Result<(Vec<Receipt>, Hash), BlockError> {
        self.execute_block_stats(state, block)
            .map(|(r, root, _)| (r, root))
    }

    /// `execute_block`, also reporting what the parallel phase did (`None`: it did not run,
    /// the block was executed sequentially from the start).
    pub fn execute_block_stats<S: KvStore>(
        &self,
        state: &mut ChainState<S>,
        block: &Block,
    ) -> Result<(Vec<Receipt>, Hash, Option<ParStats>), BlockError> {
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
            defer_fees: false,
            lazy_tables: false,
            registered: Default::default(),
        };
        let mut receipts = Vec::with_capacity(block.txs.len());
        staking::begin_block(&mut ctx.overlay, h.height)?;
        crate::host::begin_block(&mut ctx.overlay, h.height)?;
        crate::oracle::begin_block(
            &mut ctx.overlay,
            cfg.chain_id,
            &cfg.staking,
            h.height,
            &block.oracle,
        )
        .map_err(BlockError::Oracle)?;
        let mut stats = None;
        let mut ran_parallel = false;
        let threads = match self.mode {
            ExecMode::Parallel { threads } if block.txs.len() >= self.parallel_min_txs.max(2) => {
                Some(if threads == 0 {
                    std::thread::available_parallelism().map_or(1, |n| n.get())
                } else {
                    threads
                })
            }
            _ => None,
        };
        if let (Some(threads), Some(sync_kv)) = (threads, kv.as_sync()) {
            let pre: Vec<(Vec<u8>, Option<Vec<u8>>)> = ctx
                .overlay
                .pending()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            match parallel::run(
                sync_kv,
                &pre,
                &block.txs,
                cfg,
                h.height,
                ctx.cache,
                threads,
                self.fast_path,
            ) {
                ParResult::Done(out) => {
                    let out = *out;
                    ctx.overlay.commit(out.writes)?;
                    if out.any_fee {
                        for (relayer, amount) in &out.relayers {
                            ctx.credit(relayer, *amount)?;
                        }
                        ctx.credit(&validator_pool(), out.validator)?;
                    }
                    for (addr, manifest) in &out.deploys {
                        register_manifest(&mut ctx.tables, *addr, manifest)
                            .map_err(|m| StateError(format!("redeploy registration: {m}")))?;
                    }
                    ctx.cache.extend(out.cache);
                    receipts = out.receipts;
                    stats = Some(out.stats);
                    ran_parallel = true;
                }
                ParResult::Fallback(s) => stats = Some(s),
                ParResult::Err(e) => return Err(e),
            }
        }
        if !ran_parallel {
            for tx in &block.txs {
                receipts.push(ctx.apply_tx(tx)?);
            }
        }
        ctx.run_crons(&mut receipts)?;
        ctx.run_actions(&block.actions, &mut receipts)?;
        crate::dex::end_block(&mut ctx.overlay, &cfg, h.height)?;
        crate::reports::end_block(&mut ctx.overlay, h.height)?;
        crate::oracle::end_block(
            &mut ctx.overlay,
            cfg.chain_id,
            cfg.staking.epoch_len,
            h.height,
            &block.oracle.observations,
        )?;
        crate::storage::end_block(&mut ctx.overlay, &cfg.storage, h.height, h.hash())?;
        staking::end_block(&mut ctx.overlay, &cfg.staking, h.height, h.hash())?;
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
        Ok((receipts, new_root, stats))
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
        self.query_tracked(state, contract, function, args, caller)
            .map(|(out, gas, _)| (out, gas))
    }

    /// Like [`Executor::query`], also returning everything the query read: the keys and key
    /// ranges whose change can change the answer (hosting nodes use it to re-run
    /// subscriptions and to prove key reads).
    pub fn query_tracked<S: KvStore>(
        &self,
        state: &mut ChainState<S>,
        contract: &Address,
        function: &str,
        args: &[u8],
        caller: &Address,
    ) -> Result<(Vec<u8>, u64, btb_runtime::ReadSet), Revert> {
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
            Ok(o) => {
                let (_, reads) = host
                    .finish_tracked()
                    .map_err(|f| Revert::Fault(format!("{f:?}")))?;
                Ok((o.output, o.gas_used, reads))
            }
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
