//! Validator staking, epochs, rewards and slashing. See `docs/CHAIN.md` and `docs/TOKENOMICS.md`.
//!
//! Nothing here creates BTB. Stake moves from a balance into `0x26` (bonded) and, on
//! `Unstake`, into `0x27` (unbonding) until it matures back into the balance. Slashing moves
//! a fixed share of bonded plus unbonding stake to the reporter and the locked slash vault.
//! Rewards move the validator fee pool to the validators' balances. The supply invariant sums
//! all of it (`state::money`).
//!
//! State (all integers big-endian in keys, little-endian in values):
//!
//! | Tag | Key | Value |
//! | --- | --- | --- |
//! | `0x26` bonded | `26 O` | stake of operator `O`, `u64` LE |
//! | `0x27` unbonding | `27 O mature:u64` | amount, `u64` LE (one entry per operator and maturity height) |
//! | `0x28` validator | `28 O` | `(consensus_key, network_addr, jailed)` |
//! | `0x29` consensus key | `29 K` | operator `O` that registered key `K` |
//! | `0x2A` epoch | `2A epoch:u64` | the epoch's validator set, start height and anchor hash |
//! | `0x2B` slashed | `2B epoch:u64 O` | evidence key; one slash per operator and epoch |
//! | `0x2C` unbonding index | `2C mature:u64 O` | empty (finds what matures at a height) |

use crate::state::{read_u64, ChainState, StateError};
use crate::tx::{Address, Hash};
use crate::{slash_vault, validator_pool};
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_consensus::{Context, Evidence, Validator, ValidatorSet, MAX_VALIDATORS};
use btb_runtime::kv::{KvError, KvStore};
use btb_runtime::layout;
use ed25519_dalek::VerifyingKey;

pub const TAG_STAKE: u8 = 0x26;
pub const TAG_UNBOND: u8 = 0x27;
pub const TAG_VALIDATOR: u8 = 0x28;
pub const TAG_VKEY: u8 = 0x29;
pub const TAG_EPOCH: u8 = 0x2A;
pub const TAG_SLASHED: u8 = 0x2B;
pub const TAG_UNBOND_IDX: u8 = 0x2C;

/// Longest network address a validator can register (opaque bytes, for example a multiaddr).
pub const MAX_NETWORK_ADDR: usize = 256;

/// Staking and epoch parameters, fixed at genesis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StakingConfig {
    /// Blocks per epoch (`E`). Epoch `e` covers heights `e*E + 1 ..= (e+1)*E`. The set of
    /// epoch `e + 1` is fixed by the last block of epoch `e`.
    pub epoch_len: u64,
    /// Most validators in an epoch's set (`N`).
    pub max_validators: u32,
    /// Bonded stake needed to be selected, base units.
    pub min_stake: u64,
    /// Blocks between the end of the epoch in which `Unstake` ran and the release of the funds.
    /// Also the evidence window: evidence about epoch `e` is accepted while the height is
    /// below `(e + 1) * epoch_len + unbonding_blocks`.
    pub unbonding_blocks: u64,
    /// Share of bonded plus unbonding stake slashed for double signing, basis points.
    pub slash_bps: u64,
    /// Share of the slashed amount paid to the evidence reporter, basis points.
    pub reporter_reward_bps: u64,
}

impl Default for StakingConfig {
    fn default() -> Self {
        StakingConfig {
            epoch_len: 600,
            max_validators: 100,
            min_stake: 100_000 * crate::UNITS_PER_BTB,
            unbonding_blocks: 1_209_600,
            slash_bps: 500,
            reporter_reward_bps: 1_000,
        }
    }
}

impl StakingConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.epoch_len == 0 {
            return Err("epoch_len must be at least 1".into());
        }
        if self.max_validators == 0 || self.max_validators as usize > MAX_VALIDATORS {
            return Err("max_validators out of range".into());
        }
        if self.min_stake == 0 {
            return Err("min_stake must be at least 1".into());
        }
        if self.unbonding_blocks < self.epoch_len {
            return Err("unbonding_blocks must cover at least one epoch".into());
        }
        if self.slash_bps == 0 || self.slash_bps > 10_000 {
            return Err("slash_bps must be 1..=10000".into());
        }
        if self.reporter_reward_bps > 10_000 {
            return Err("reporter_reward_bps must be at most 10000".into());
        }
        Ok(())
    }

    pub(crate) fn fields(&self) -> [u64; 6] {
        [
            self.epoch_len,
            u64::from(self.max_validators),
            self.min_stake,
            self.unbonding_blocks,
            self.slash_bps,
            self.reporter_reward_bps,
        ]
    }

    pub(crate) fn from_fields(f: [u64; 6]) -> Self {
        StakingConfig {
            epoch_len: f[0],
            max_validators: f[1] as u32,
            min_stake: f[2],
            unbonding_blocks: f[3],
            slash_bps: f[4],
            reporter_reward_bps: f[5],
        }
    }

    /// Epoch that contains block `height` (`height >= 1`).
    pub fn epoch_of_block(&self, height: u64) -> u64 {
        height.saturating_sub(1) / self.epoch_len
    }

    /// Height of the last block of `epoch`.
    pub fn epoch_end(&self, epoch: u64) -> u64 {
        epoch.saturating_add(1).saturating_mul(self.epoch_len)
    }

    /// Height from which evidence about `epoch` is no longer accepted.
    pub fn evidence_deadline(&self, epoch: u64) -> u64 {
        self.epoch_end(epoch).saturating_add(self.unbonding_blocks)
    }

    /// Height at which stake unbonded by a transaction in block `height` is released.
    pub fn maturity(&self, height: u64) -> u64 {
        self.evidence_deadline(self.epoch_of_block(height))
    }
}

/// Why a staking transaction failed (the transaction is reverted: nonce used, fee charged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StakeError {
    /// `Stake` names a validator key whose account is not the sender (delegation is not supported yet).
    NotOperator,
    NotRegistered,
    /// Double signers are jailed for good: no new stake, no re-registration.
    Jailed,
    ZeroAmount,
    InsufficientBalance,
    InsufficientStake,
    /// Not a valid, non-weak Ed25519 public key.
    BadConsensusKey,
    /// Another operator already registered this consensus key.
    KeyTaken,
    AddrTooLong,
    /// No validator set is stored for that epoch (not started yet, or pruned).
    UnknownEpoch,
    /// Evidence about that epoch is past the unbonding window.
    Expired,
    /// The evidence names a validator index outside the epoch's set.
    UnknownValidator,
    /// The evidence does not verify against the epoch's validator set.
    BadEvidence,
    /// This operator was already slashed for that epoch.
    Duplicate,
}

/// A staking operation failed either for a transaction reason or a state fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fail {
    Tx(StakeError),
    State(StateError),
}
impl From<StakeError> for Fail {
    fn from(e: StakeError) -> Self {
        Fail::Tx(e)
    }
}
impl From<StateError> for Fail {
    fn from(e: StateError) -> Self {
        Fail::State(e)
    }
}
impl From<KvError> for Fail {
    fn from(e: KvError) -> Self {
        Fail::State(e.into())
    }
}
impl From<DecodeError> for Fail {
    fn from(e: DecodeError) -> Self {
        Fail::State(e.into())
    }
}

/// A validator of one epoch, in consensus order (index = consensus `ValidatorIndex`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveValidator {
    /// The staking account.
    pub operator: Address,
    /// Ed25519 consensus key.
    pub consensus_key: [u8; 32],
    /// Bonded stake when the set was fixed; it is the consensus weight for the whole epoch.
    pub stake: u64,
}

/// The validator set of one epoch plus where it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochRecord {
    pub epoch: u64,
    /// Height of the last block of the previous epoch (0 for epoch 0): the consensus anchor height.
    pub start_height: u64,
    /// Header hash of that block (the genesis hash for epoch 0): the consensus anchor id.
    pub anchor: Hash,
    pub validators: Vec<ActiveValidator>,
}

impl EpochRecord {
    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.start_height.encode_to(&mut o);
        self.anchor.encode_to(&mut o);
        (self.validators.len() as u32).encode_to(&mut o);
        for v in &self.validators {
            v.operator.encode_to(&mut o);
            v.consensus_key.encode_to(&mut o);
            v.stake.encode_to(&mut o);
        }
        o
    }

    fn decode(epoch: u64, b: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(b, 1 << 24);
        let start_height = u64::decode_from(&mut r)?;
        let anchor = <[u8; 32]>::decode_from(&mut r)?;
        let n = u32::decode_from(&mut r)? as usize;
        if n > MAX_VALIDATORS {
            return Err(DecodeError::TrailingBytes);
        }
        let mut validators = Vec::with_capacity(n);
        for _ in 0..n {
            validators.push(ActiveValidator {
                operator: <[u8; 32]>::decode_from(&mut r)?,
                consensus_key: <[u8; 32]>::decode_from(&mut r)?,
                stake: u64::decode_from(&mut r)?,
            });
        }
        if r.remaining() != 0 {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(EpochRecord {
            epoch,
            start_height,
            anchor,
            validators,
        })
    }

    /// The set as a consensus `ValidatorSet` (same order, so the same indices).
    pub fn consensus_set(&self) -> Result<ValidatorSet, StateError> {
        ValidatorSet::new(
            self.validators
                .iter()
                .map(|v| Validator {
                    key: v.consensus_key,
                    stake: v.stake,
                })
                .collect(),
        )
        .map_err(|e| StateError(format!("epoch {} set invalid: {e:?}", self.epoch)))
    }
}

/// A registered validator as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatorInfo {
    pub operator: Address,
    pub consensus_key: [u8; 32],
    pub network_addr: Vec<u8>,
    pub jailed: bool,
    pub bonded: u64,
    pub unbonding: u64,
}

/// A validator in the genesis state; its stake is taken from the operator's allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenesisValidator {
    pub operator: Address,
    pub consensus_key: [u8; 32],
    pub network_addr: Vec<u8>,
    pub stake: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatorRecord {
    pub key: [u8; 32],
    pub addr: Vec<u8>,
    pub jailed: bool,
}

impl ValidatorRecord {
    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.key.encode_to(&mut o);
        self.addr.encode_to(&mut o);
        self.jailed.encode_to(&mut o);
        o
    }
    fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(b, 1 << 16);
        let v = ValidatorRecord {
            key: <[u8; 32]>::decode_from(&mut r)?,
            addr: Vec::decode_from(&mut r)?,
            jailed: bool::decode_from(&mut r)?,
        };
        if r.remaining() != 0 {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(v)
    }
}

// ------------------------------------------------------------------ keys

fn tagged(tag: u8, a: &Address) -> Vec<u8> {
    let mut k = vec![tag];
    k.extend_from_slice(a);
    k
}
fn stake_key(a: &Address) -> Vec<u8> {
    tagged(TAG_STAKE, a)
}
fn validator_key(a: &Address) -> Vec<u8> {
    tagged(TAG_VALIDATOR, a)
}
fn vkey_key(k: &[u8; 32]) -> Vec<u8> {
    tagged(TAG_VKEY, k)
}
fn unbond_key(a: &Address, mature: u64) -> Vec<u8> {
    let mut k = tagged(TAG_UNBOND, a);
    k.extend_from_slice(&mature.to_be_bytes());
    k
}
fn unbond_idx_key(mature: u64, a: &Address) -> Vec<u8> {
    let mut k = vec![TAG_UNBOND_IDX];
    k.extend_from_slice(&mature.to_be_bytes());
    k.extend_from_slice(a);
    k
}
fn epoch_key(epoch: u64) -> Vec<u8> {
    let mut k = vec![TAG_EPOCH];
    k.extend_from_slice(&epoch.to_be_bytes());
    k
}
fn slashed_key(epoch: u64, a: &Address) -> Vec<u8> {
    let mut k = vec![TAG_SLASHED];
    k.extend_from_slice(&epoch.to_be_bytes());
    k.extend_from_slice(a);
    k
}

// ------------------------------------------------------------------ small helpers

fn balance(kv: &impl KvStore, a: &Address) -> Result<u64, KvError> {
    read_u64(kv, &layout::balance_key(a))
}

fn put_u64(kv: &mut impl KvStore, key: &[u8], v: u64) -> Result<(), KvError> {
    kv.put(key, &v.to_le_bytes())
}

fn credit(kv: &mut impl KvStore, a: &Address, amount: u64) -> Result<(), StateError> {
    if amount == 0 {
        return Ok(());
    }
    let b = balance(kv, a)?
        .checked_add(amount)
        .ok_or_else(|| StateError("balance overflow".into()))?;
    Ok(put_u64(kv, &layout::balance_key(a), b)?)
}

fn debit(kv: &mut impl KvStore, a: &Address, amount: u64) -> Result<(), StateError> {
    let b = balance(kv, a)?
        .checked_sub(amount)
        .ok_or_else(|| StateError("debit exceeds balance".into()))?;
    Ok(put_u64(kv, &layout::balance_key(a), b)?)
}

fn set_or_delete(kv: &mut impl KvStore, key: &[u8], v: u64) -> Result<(), KvError> {
    if v == 0 {
        kv.delete(key)
    } else {
        put_u64(kv, key, v)
    }
}

fn fail_state(f: Fail) -> StateError {
    match f {
        Fail::State(s) => s,
        Fail::Tx(t) => StateError(format!("{t:?}")),
    }
}

fn record(kv: &impl KvStore, a: &Address) -> Result<Option<ValidatorRecord>, Fail> {
    match kv.get(&validator_key(a))? {
        None => Ok(None),
        Some(b) => Ok(Some(ValidatorRecord::decode(&b)?)),
    }
}

/// Bonded stake of `a`.
pub(crate) fn bonded(kv: &impl KvStore, a: &Address) -> Result<u64, KvError> {
    read_u64(kv, &stake_key(a))
}

/// All unbonding entries of `a`: `(maturity height, amount)`.
fn unbonding_entries(kv: &impl KvStore, a: &Address) -> Result<Vec<(u64, u64)>, KvError> {
    let start = tagged(TAG_UNBOND, a);
    let mut end = start.clone();
    end.extend_from_slice(&[0xFF; 9]);
    let mut out = Vec::new();
    for (k, v) in kv.scan(&start, Some(&end), false, 1 << 20)? {
        let mature = <[u8; 8]>::try_from(&k[33..]).map_or(0, u64::from_be_bytes);
        let amount = <[u8; 8]>::try_from(v.as_slice()).map_or(0, u64::from_le_bytes);
        out.push((mature, amount));
    }
    Ok(out)
}

pub(crate) fn epoch_record(
    kv: &impl KvStore,
    epoch: u64,
) -> Result<Option<EpochRecord>, StateError> {
    match kv.get(&epoch_key(epoch))? {
        None => Ok(None),
        Some(b) => Ok(Some(EpochRecord::decode(epoch, &b)?)),
    }
}

fn put_epoch(kv: &mut impl KvStore, rec: &EpochRecord) -> Result<(), KvError> {
    kv.put(&epoch_key(rec.epoch), &rec.encode())
}

fn key_ok(key: &[u8; 32]) -> bool {
    VerifyingKey::from_bytes(key).is_ok_and(|vk| !vk.is_weak())
}

// ------------------------------------------------------------------ transactions

/// `RegisterValidator`: register (or update) the consensus key and network address of `operator`.
pub(crate) fn register_validator(
    kv: &mut impl KvStore,
    operator: &Address,
    key: &[u8; 32],
    addr: &[u8],
) -> Result<(), Fail> {
    if addr.len() > MAX_NETWORK_ADDR {
        return Err(StakeError::AddrTooLong.into());
    }
    if !key_ok(key) {
        return Err(StakeError::BadConsensusKey.into());
    }
    let existing = record(kv, operator)?;
    if existing.as_ref().is_some_and(|r| r.jailed) {
        return Err(StakeError::Jailed.into());
    }
    if let Some(owner) = kv.get(&vkey_key(key))? {
        if owner.as_slice() != operator {
            return Err(StakeError::KeyTaken.into());
        }
    }
    if let Some(old) = &existing {
        if old.key != *key {
            kv.delete(&vkey_key(&old.key))?;
        }
    }
    kv.put(&vkey_key(key), operator)?;
    let rec = ValidatorRecord {
        key: *key,
        addr: addr.to_vec(),
        jailed: false,
    };
    kv.put(&validator_key(operator), &rec.encode())?;
    Ok(())
}

/// `Stake`: bond `amount` of the operator's own balance. Counts from the next epoch on.
pub(crate) fn stake(
    kv: &mut impl KvStore,
    sender: &Address,
    validator: &[u8; 32],
    amount: u64,
) -> Result<(), Fail> {
    if crate::tx::account_address(validator) != *sender {
        return Err(StakeError::NotOperator.into());
    }
    if amount == 0 {
        return Err(StakeError::ZeroAmount.into());
    }
    let Some(rec) = record(kv, sender)? else {
        return Err(StakeError::NotRegistered.into());
    };
    if rec.jailed {
        return Err(StakeError::Jailed.into());
    }
    let bal = balance(kv, sender)?;
    if bal < amount {
        return Err(StakeError::InsufficientBalance.into());
    }
    let new = bonded(kv, sender)?
        .checked_add(amount)
        .ok_or_else(|| StateError("stake overflow".into()))?;
    put_u64(kv, &layout::balance_key(sender), bal - amount)?;
    put_u64(kv, &stake_key(sender), new)?;
    Ok(())
}

/// `Unstake`: move `amount` of bonded stake into unbonding. It stays slashable until released.
pub(crate) fn unstake(
    kv: &mut impl KvStore,
    cfg: &StakingConfig,
    sender: &Address,
    amount: u64,
    height: u64,
) -> Result<(), Fail> {
    if amount == 0 {
        return Err(StakeError::ZeroAmount.into());
    }
    let have = bonded(kv, sender)?;
    if amount > have {
        return Err(StakeError::InsufficientStake.into());
    }
    let mature = cfg.maturity(height);
    let key = unbond_key(sender, mature);
    let pending = read_u64(kv, &key)?
        .checked_add(amount)
        .ok_or_else(|| StateError("unbonding overflow".into()))?;
    set_or_delete(kv, &stake_key(sender), have - amount)?;
    put_u64(kv, &key, pending)?;
    kv.put(&unbond_idx_key(mature, sender), &[])?;
    Ok(())
}

/// What a verified `SubmitEvidence` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashOutcome {
    pub operator: Address,
    /// Total taken from bonded and unbonding stake.
    pub slashed: u64,
    /// Part of it paid to the reporter (or to the vault when the reporter is the offender).
    pub reward: u64,
}

/// `SubmitEvidence`: verify consensus misbehaviour of `epoch` and slash the offender once.
pub(crate) fn submit_evidence(
    kv: &mut impl KvStore,
    cfg: &StakingConfig,
    chain_id: u64,
    reporter: &Address,
    height: u64,
    epoch: u64,
    ev: &Evidence,
) -> Result<SlashOutcome, Fail> {
    let Some(rec) = epoch_record(kv, epoch)? else {
        return Err(StakeError::UnknownEpoch.into());
    };
    if height >= cfg.evidence_deadline(epoch) {
        return Err(StakeError::Expired.into());
    }
    let Some(offender) = rec.validators.get(ev.offender() as usize).copied() else {
        return Err(StakeError::UnknownValidator.into());
    };
    let ctx = Context::for_epoch(
        &consensus_chain_id(chain_id),
        epoch,
        rec.consensus_set()?,
        None,
    );
    if !ev.verify(&ctx) {
        return Err(StakeError::BadEvidence.into());
    }
    let op = offender.operator;
    if kv.get(&slashed_key(epoch, &op))?.is_some() {
        return Err(StakeError::Duplicate.into());
    }
    let mut vrec =
        record(kv, &op)?.ok_or_else(|| StateError("slashed validator missing".into()))?;

    // Everything below is a write; nothing can fail with a transaction error any more.
    let bps = u128::from(cfg.slash_bps);
    let cut = |x: u64| (u128::from(x) * bps / 10_000) as u64;
    let mut total = 0u64;
    let b = bonded(kv, &op)?;
    let s = cut(b);
    set_or_delete(kv, &stake_key(&op), b - s)?;
    total += s;
    for (mature, amount) in unbonding_entries(kv, &op)? {
        let s = cut(amount);
        set_or_delete(kv, &unbond_key(&op, mature), amount - s)?;
        if amount - s == 0 {
            kv.delete(&unbond_idx_key(mature, &op))?;
        }
        total += s;
    }
    let reward = (u128::from(total) * u128::from(cfg.reporter_reward_bps) / 10_000) as u64;
    // A validator cannot collect the reward for reporting itself.
    let payee = if *reporter == op {
        slash_vault()
    } else {
        *reporter
    };
    credit(kv, &payee, reward)?;
    credit(kv, &slash_vault(), total - reward)?;
    vrec.jailed = true;
    kv.put(&validator_key(&op), &vrec.encode())?;
    let mut marker = vec![ev.key().0];
    marker.extend_from_slice(&ev.round().to_le_bytes());
    kv.put(&slashed_key(epoch, &op), &marker)?;
    Ok(SlashOutcome {
        operator: op,
        slashed: total,
        reward,
    })
}

/// The consensus `Config::chain_id` bytes for a chain id.
pub fn consensus_chain_id(chain_id: u64) -> Vec<u8> {
    let mut v = b"btb-chain-".to_vec();
    v.extend_from_slice(&chain_id.to_le_bytes());
    v
}

// ------------------------------------------------------------------ block hooks

/// Start of every block: release unbonded stake that has matured.
pub(crate) fn begin_block(kv: &mut impl KvStore, height: u64) -> Result<(), StateError> {
    let mut end = vec![TAG_UNBOND_IDX];
    end.extend_from_slice(&height.saturating_add(1).to_be_bytes());
    loop {
        let due = kv.scan(&[TAG_UNBOND_IDX], Some(&end), false, 256)?;
        if due.is_empty() {
            return Ok(());
        }
        for (k, _) in due {
            let mature = <[u8; 8]>::try_from(&k[1..9]).map_or(0, u64::from_be_bytes);
            let op: Address = k[9..]
                .try_into()
                .map_err(|_| StateError("bad unbonding index key".into()))?;
            let ukey = unbond_key(&op, mature);
            let amount = read_u64(kv, &ukey)?;
            credit(kv, &op, amount)?;
            kv.delete(&ukey)?;
            kv.delete(&k)?;
        }
    }
}

/// End of a block: at the last block of an epoch, pay the validator pool out to the epoch's
/// validators, fix the next epoch's set and drop what the evidence window no longer needs.
pub(crate) fn end_block(
    kv: &mut impl KvStore,
    cfg: &StakingConfig,
    height: u64,
    block_hash: Hash,
) -> Result<(), StateError> {
    if height == 0 || !height.is_multiple_of(cfg.epoch_len) {
        return Ok(());
    }
    let epoch = cfg.epoch_of_block(height);
    let Some(current) = epoch_record(kv, epoch)? else {
        return Ok(()); // a chain without validators has no epochs to rotate
    };
    pay_rewards(kv, &current)?;
    let mut next = select_set(kv, cfg)?;
    if next.is_empty() {
        next = current.validators.clone(); // never leave consensus without a set
    }
    put_epoch(
        kv,
        &EpochRecord {
            epoch: epoch + 1,
            start_height: height,
            anchor: block_hash,
            validators: next,
        },
    )?;
    prune(kv, cfg, height)
}

/// Pay the whole validator pool to the validators of `rec` that are not jailed, pro rata by
/// their stake in the set. Integer division leaves less than one unit per validator; those
/// remainder units go one each to the first validators in set order, so the payouts add up to
/// exactly the pool.
fn pay_rewards(kv: &mut impl KvStore, rec: &EpochRecord) -> Result<(), StateError> {
    let pool = balance(kv, &validator_pool())?;
    if pool == 0 {
        return Ok(());
    }
    let mut eligible = Vec::new();
    for v in &rec.validators {
        let jailed = record(kv, &v.operator)
            .map_err(fail_state)?
            .is_some_and(|r| r.jailed);
        if !jailed {
            eligible.push(*v);
        }
    }
    let total: u128 = eligible.iter().map(|v| u128::from(v.stake)).sum();
    if total == 0 {
        return Ok(()); // everybody was jailed: the pool carries over to the next epoch
    }
    let mut shares: Vec<u64> = eligible
        .iter()
        .map(|v| (u128::from(pool) * u128::from(v.stake) / total) as u64)
        .collect();
    let paid: u64 = shares.iter().sum();
    let rem = (pool - paid) as usize;
    for s in shares.iter_mut().take(rem) {
        *s += 1;
    }
    debit(kv, &validator_pool(), pool)?;
    for (v, s) in eligible.iter().zip(shares) {
        credit(kv, &v.operator, s)?;
    }
    Ok(())
}

/// The top `max_validators` registered, non-jailed operators with at least `min_stake` bonded,
/// by stake descending, ties broken by operator address ascending.
pub(crate) fn select_set(
    kv: &impl KvStore,
    cfg: &StakingConfig,
) -> Result<Vec<ActiveValidator>, StateError> {
    let end = [TAG_VALIDATOR + 1];
    let mut from = vec![TAG_VALIDATOR];
    let mut all = Vec::new();
    loop {
        let page = kv.scan(&from, Some(&end), false, 1024)?;
        for (k, v) in &page {
            let op: Address = k[1..]
                .try_into()
                .map_err(|_| StateError("bad validator key".into()))?;
            let rec = ValidatorRecord::decode(v)?;
            if rec.jailed {
                continue;
            }
            let stake = bonded(kv, &op)?;
            if stake >= cfg.min_stake {
                all.push(ActiveValidator {
                    operator: op,
                    consensus_key: rec.key,
                    stake,
                });
            }
        }
        match page.last() {
            Some((k, _)) if page.len() == 1024 => {
                from = k.clone();
                from.push(0);
            }
            _ => break,
        }
    }
    all.sort_by(|a, b| b.stake.cmp(&a.stake).then(a.operator.cmp(&b.operator)));
    all.truncate(cfg.max_validators as usize);
    Ok(all)
}

/// Drop epoch records and slash markers whose evidence window has closed.
fn prune(kv: &mut impl KvStore, cfg: &StakingConfig, height: u64) -> Result<(), StateError> {
    // Epoch e is needed while height < (e + 1) * E + U. Records for all earlier epochs go.
    let keep_from = height
        .saturating_sub(cfg.unbonding_blocks)
        .checked_div(cfg.epoch_len)
        .unwrap_or(0);
    for tag in [TAG_EPOCH, TAG_SLASHED] {
        let mut end = vec![tag];
        end.extend_from_slice(&keep_from.to_be_bytes());
        loop {
            let old = kv.scan(&[tag], Some(&end), false, 256)?;
            if old.is_empty() {
                break;
            }
            for (k, _) in old {
                kv.delete(&k)?;
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ genesis

/// Write the genesis validators: stake is taken out of the operators' allocations, and epoch
/// 0's set is stored. `allocations` has already had the stake subtracted by the caller.
pub(crate) fn genesis_ops(
    cfg: &StakingConfig,
    validators: &[GenesisValidator],
    genesis_hash: Hash,
    batch: &mut btb_runtime::WriteBatch,
) -> Result<(), String> {
    if validators.len() > cfg.max_validators as usize {
        return Err("more genesis validators than max_validators".into());
    }
    let mut ops = std::collections::BTreeSet::new();
    let mut keys = std::collections::BTreeSet::new();
    let mut active = Vec::new();
    for v in validators {
        if v.stake < cfg.min_stake {
            return Err("genesis validator below min_stake".into());
        }
        if v.network_addr.len() > MAX_NETWORK_ADDR || !key_ok(&v.consensus_key) {
            return Err("genesis validator has a bad key or address".into());
        }
        if !ops.insert(v.operator) || !keys.insert(v.consensus_key) {
            return Err("duplicate genesis validator or consensus key".into());
        }
        batch.put(stake_key(&v.operator), v.stake.to_le_bytes().to_vec());
        batch.put(vkey_key(&v.consensus_key), v.operator.to_vec());
        let rec = ValidatorRecord {
            key: v.consensus_key,
            addr: v.network_addr.clone(),
            jailed: false,
        };
        batch.put(validator_key(&v.operator), rec.encode());
        active.push(ActiveValidator {
            operator: v.operator,
            consensus_key: v.consensus_key,
            stake: v.stake,
        });
    }
    if !active.is_empty() {
        active.sort_by(|a, b| b.stake.cmp(&a.stake).then(a.operator.cmp(&b.operator)));
        let rec = EpochRecord {
            epoch: 0,
            start_height: 0,
            anchor: genesis_hash,
            validators: active,
        };
        batch.put(epoch_key(0), rec.encode());
    }
    Ok(())
}

// ------------------------------------------------------------------ queries

impl<S: KvStore> ChainState<S> {
    /// Epoch of the next block to be executed (`head height / epoch_len`). The set of this epoch
    /// is available as soon as the previous epoch's last block has been executed.
    pub fn current_epoch(&self) -> Result<u64, StateError> {
        Ok(self.head()?.height / self.config.staking.epoch_len)
    }

    /// The stored validator set of `epoch` (kept while evidence about it is still accepted).
    pub fn epoch_record(&self, epoch: u64) -> Result<Option<EpochRecord>, StateError> {
        epoch_record(&self.kv, epoch)
    }

    /// The consensus validator set of `epoch`, in validator-index order.
    pub fn consensus_set(&self, epoch: u64) -> Result<Option<ValidatorSet>, StateError> {
        self.epoch_record(epoch)?
            .map(|r| r.consensus_set())
            .transpose()
    }

    /// The consensus context of `epoch`, as validators use it for `Replica::new_epoch` and as
    /// `SubmitEvidence` verifies against.
    pub fn consensus_context(&self, epoch: u64) -> Result<Option<Context>, StateError> {
        let Some(rec) = self.epoch_record(epoch)? else {
            return Ok(None);
        };
        Ok(Some(Context::for_epoch(
            &self.consensus_chain_id(),
            epoch,
            rec.consensus_set()?,
            Some(rec.anchor),
        )))
    }

    /// Bytes to use as the consensus `Config::chain_id`.
    pub fn consensus_chain_id(&self) -> Vec<u8> {
        consensus_chain_id(self.config.chain_id)
    }

    /// Bonded stake of an operator.
    pub fn staked(&self, a: &Address) -> Result<u64, KvError> {
        bonded(&self.kv, a)
    }

    /// Stake of an operator that is unbonding.
    pub fn unbonding(&self, a: &Address) -> Result<u64, KvError> {
        Ok(unbonding_entries(&self.kv, a)?.iter().map(|(_, x)| x).sum())
    }

    /// Unbonding entries `(maturity height, amount)` of an operator.
    pub fn unbonding_entries(&self, a: &Address) -> Result<Vec<(u64, u64)>, KvError> {
        unbonding_entries(&self.kv, a)
    }

    pub fn validator(&self, a: &Address) -> Result<Option<ValidatorInfo>, StateError> {
        let rec = record(&self.kv, a).map_err(fail_state)?;
        Ok(match rec {
            None => None,
            Some(r) => Some(ValidatorInfo {
                operator: *a,
                consensus_key: r.key,
                network_addr: r.addr,
                jailed: r.jailed,
                bonded: bonded(&self.kv, a)?,
                unbonding: self.unbonding(a)?,
            }),
        })
    }

    /// The set that would be selected from the current stakes (what epoch `e + 1` gets if the
    /// epoch ended now).
    pub fn selected_set(&self) -> Result<Vec<ActiveValidator>, StateError> {
        select_set(&self.kv, &self.config.staking)
    }

    /// BTB locked forever by slashing.
    pub fn slash_vault_balance(&self) -> Result<u64, KvError> {
        self.balance(&slash_vault())
    }

    /// Whether `operator` was already slashed for `epoch`.
    pub fn slashed_in_epoch(&self, epoch: u64, operator: &Address) -> Result<bool, KvError> {
        Ok(self.kv.get(&slashed_key(epoch, operator))?.is_some())
    }
}
