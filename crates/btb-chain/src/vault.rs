//! The native vault module: wrapped assets for Bitcoin and Solana backed by the cross-chain
//! threshold keys (`docs/CROSS_CHAIN.md`).
//!
//! Each [`Scheme`] has one vault: the external address derived from the group key
//! ([`output_key`], Taproot or Ed25519), owned by the validators as a threshold group. Users
//! deposit there; the chain credits **wrapped user tokens** and burns them again on withdrawal.
//!
//! * **Wrapped tokens are user tokens, never BTB.** They live in the native token ledger
//!   (`btb_vm::token`, balances under tag `0x0B`), have their own supply and the vault as sole
//!   mint authority. The fixed BTB supply is untouched by any code in this file; it never reads
//!   or writes a BTB balance.
//! * **Minted only against observed deposits.** Validators vote with `CrossOp::Attest` for
//!   deposit claims they observed on the external chain (`txid`, output index, amount and the BTB
//!   account named in the deposit's memo). A claim is credited exactly once, when validators
//!   holding at least two thirds of the current epoch's stake have voted for the identical claim;
//!   claims that differ in any field are different claims. There is no other way to mint: the only
//!   code that adds to a wrapped token's supply is [`credit`], called from the vote that reaches
//!   the quorum. For every asset, `supply == credited - burned` (see [`accounting`]).
//! * **Withdrawals burn before they sign.** `CrossOp::Withdraw` burns the amount from the sender
//!   at once and records a [`Withdrawal`]. Validators then agree (the same two-thirds rule) on
//!   the unsigned external transaction (its digest and its signature hashes) with
//!   `CrossOp::Prepare`; at quorum one signing request per sighash is queued
//!   (`crate::xsign`). A withdrawal is prepared at most once, so a burn can never produce two
//!   different signed external transactions.
//!
//! **Withdrawal plans.** A `Prepare` may carry the unsigned external transaction (`plan`). The
//! chain then re-derives what it can ([`crate::vault_plan`]): for Bitcoin it parses the
//! transaction, checks that the inputs are vault UTXOs (each spendable once, tracked under key
//! `0x56`), that it pays the destination and returns change to the vault, that the outflow equals
//! the burn, and recomputes the BIP-341 signature hashes; for Solana it checks the message is one
//! System transfer of the burned amount. An empty plan (the mock observer) is accepted unchecked.
//! The chain cannot know that the external transaction is later broadcast and confirms; that is
//! the validators' watchers (`docs/CROSS_CHAIN.md`, "Chain watchers").
//!
//! # State (tag `0x70`)
//!
//! | Key | Value |
//! | --- | --- |
//! | `70 50 scheme` | [`AssetState`]: credited and burned totals |
//! | `70 51 claim[32]` | [`ClaimVotes`] of a claim not yet credited |
//! | `70 52 claim[32]` | empty: the claim was credited (replay guard) |
//! | `70 53` | next withdrawal id (`u64` LE) |
//! | `70 54 id:u64be` | [`Withdrawal`] |
//! | `70 55 id:u64be digest[32]` | [`ClaimVotes`] for a prepare proposal |
//! | `70 56 txid[32] vout:u32be` | a spendable Bitcoin vault UTXO: amount `u64` LE |
//! | `70 57 outpoint[32]` | empty: this external output was credited (first quorum wins) |

use crate::staking;
use crate::state::StateError;
use crate::tx::Address;
use crate::vault_plan;
use crate::xdkg::{self, TAG_CROSS};
use crate::xsign::{self, RequestTag, SignRequest};
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_oracle::frost::{addr, Scheme};
use btb_runtime::kv::KvStore;
use btb_runtime::layout;
use btb_vm::token::{derive_id, TokenInfo};
use sha2::{Digest, Sha256};

pub const KEY_ASSET: u8 = 0x50;
pub const KEY_CLAIM: u8 = 0x51;
pub const KEY_CREDITED: u8 = 0x52;
pub const KEY_WNEXT: u8 = 0x53;
pub const KEY_WITHDRAWAL: u8 = 0x54;
pub const KEY_PREPARE: u8 = 0x55;
pub const KEY_UTXO: u8 = 0x56;
pub const KEY_OUTPOINT: u8 = 0x57;

/// Most claims in one attestation, most sighashes in one withdrawal, longest destination.
pub const MAX_CLAIMS: usize = 32;
pub const MAX_SIGHASHES: usize = 16;
pub const MAX_DESTINATION: usize = 64;

pub const ATTEST_BASE_GAS: u64 = 30_000;
pub const ATTEST_PER_CLAIM_GAS: u64 = 20_000;
pub const PREPARE_GAS: u64 = 60_000;
pub const WITHDRAW_GAS: u64 = 50_000;

pub const STATUS_BURNED: u8 = 0;
pub const STATUS_PREPARED: u8 = 1;
pub const STATUS_SIGNED: u8 = 2;

/// The address that is mint authority and creator of every wrapped token. No key and no contract
/// hashes to it, and the module is the only code that moves its authority.
pub fn vault_address() -> Address {
    crate::sha256(&[b"btb.pool.v1", b"cross-chain-vault"])
}

/// Id of the wrapped token of `scheme` (Bitcoin: `Taproot`, Solana: `Ed25519`).
pub fn token_id(scheme: Scheme) -> [u8; 32] {
    derive_id(&vault_address(), u64::from(scheme.byte()))
}

fn token_meta(scheme: Scheme) -> (&'static str, &'static str, u8) {
    match scheme {
        Scheme::Taproot => ("Wrapped Bitcoin", "wBTC", 8),
        Scheme::Ed25519 => ("Wrapped SOL", "wSOL", 9),
    }
}

// ------------------------------------------------------------------ records

/// A deposit as validators observed it on the external chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepositClaim {
    pub asset: Scheme,
    /// Transaction id (Bitcoin) or signature (Solana, first 32 bytes) on the external chain.
    pub txid: [u8; 32],
    /// Output index (Bitcoin) or instruction index (Solana).
    pub index: u32,
    /// In the external chain's base unit (satoshis, lamports).
    pub amount: u64,
    /// The BTB account the deposit's memo names.
    pub recipient: Address,
    /// Height (Bitcoin) or slot (Solana) of the block that holds the deposit.
    pub height: u64,
    /// Hash of that block. Validators that see the deposit in different blocks (a reorg inside
    /// the confirmation window) vote for different claims and nothing is minted.
    pub block_hash: [u8; 32],
}

impl DepositClaim {
    pub fn id(&self) -> [u8; 32] {
        crate::sha256(&[
            b"btb.vault.claim.v1",
            &[self.asset.byte()],
            &self.txid,
            &self.index.to_le_bytes(),
            &self.amount.to_le_bytes(),
            &self.recipient,
            &self.height.to_le_bytes(),
            &self.block_hash,
        ])
    }

    /// Identifies the external output regardless of the block it sits in: a deposit is credited
    /// at most once even if a reorg moves it to another block.
    pub fn outpoint_id(&self) -> [u8; 32] {
        crate::sha256(&[
            b"btb.vault.outpoint.v1",
            &[self.asset.byte()],
            &self.txid,
            &self.index.to_le_bytes(),
        ])
    }

    pub(crate) fn encode_to(&self, o: &mut Vec<u8>) {
        self.asset.byte().encode_to(o);
        self.txid.encode_to(o);
        self.index.encode_to(o);
        self.amount.encode_to(o);
        self.recipient.encode_to(o);
        self.height.encode_to(o);
        self.block_hash.encode_to(o);
    }

    pub(crate) fn decode_from(r: &mut Reader<'_>) -> Result<DepositClaim, DecodeError> {
        Ok(DepositClaim {
            asset: Scheme::from_byte(u8::decode_from(r)?).ok_or(DecodeError::TrailingBytes)?,
            txid: <[u8; 32]>::decode_from(r)?,
            index: u32::decode_from(r)?,
            amount: u64::decode_from(r)?,
            recipient: <[u8; 32]>::decode_from(r)?,
            height: u64::decode_from(r)?,
            block_hash: <[u8; 32]>::decode_from(r)?,
        })
    }
}

/// Totals for one asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AssetState {
    pub credited: u128,
    pub burned: u128,
}

/// Validators (consensus keys) that voted for something.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClaimVotes {
    pub voters: Vec<[u8; 32]>,
}

/// A withdrawal request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withdrawal {
    pub id: u64,
    pub asset: Scheme,
    pub owner: Address,
    pub amount: u64,
    /// Bitcoin: the scriptPubKey to pay; Solana: the 32-byte destination account.
    pub destination: Vec<u8>,
    pub height: u64,
    pub status: u8,
    /// Digest of the unsigned external transaction (set at `Prepare` quorum).
    pub digest: [u8; 32],
    /// The signing requests of its sighashes.
    pub requests: Vec<u64>,
    /// The agreed unsigned external transaction (Bitcoin: witness-free tx; Solana: message).
    /// Empty for plan-less (mock) proposals.
    pub plan: Vec<u8>,
}

impl Withdrawal {
    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.asset.byte().encode_to(&mut o);
        self.owner.encode_to(&mut o);
        self.amount.encode_to(&mut o);
        self.destination.encode_to(&mut o);
        self.height.encode_to(&mut o);
        self.status.encode_to(&mut o);
        self.digest.encode_to(&mut o);
        self.requests.encode_to(&mut o);
        self.plan.encode_to(&mut o);
        o
    }

    fn decode(id: u64, b: &[u8]) -> Result<Withdrawal, DecodeError> {
        let mut r = Reader::new(b, 1 << 14);
        let w = Withdrawal {
            id,
            asset: Scheme::from_byte(u8::decode_from(&mut r)?).ok_or(DecodeError::TrailingBytes)?,
            owner: <[u8; 32]>::decode_from(&mut r)?,
            amount: u64::decode_from(&mut r)?,
            destination: Vec::decode_from(&mut r)?,
            height: u64::decode_from(&mut r)?,
            status: u8::decode_from(&mut r)?,
            digest: <[u8; 32]>::decode_from(&mut r)?,
            requests: Vec::decode_from(&mut r)?,
            plan: Vec::decode_from(&mut r)?,
        };
        if r.remaining() != 0 {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(w)
    }
}

/// Why a vault transaction was refused (the fee is still charged, nothing changed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultError {
    /// The sender is not a validator of the current epoch.
    NotValidator,
    /// No cross-chain key for the asset yet.
    NoKey,
    /// Zero amount, or too many claims or sighashes.
    BadShape,
    BadDestination,
    InsufficientBalance,
    NoSuchWithdrawal,
    /// The withdrawal was already prepared: a burn is signed for exactly one transaction.
    AlreadyPrepared,
    Overflow,
    /// The proposed external transaction is malformed or does not pay what was burned.
    BadPlan,
    /// The plan spends a UTXO the vault does not hold (or already spent).
    UtxoUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VaultFail {
    Tx(VaultError),
    State(StateError),
}
impl From<VaultError> for VaultFail {
    fn from(e: VaultError) -> Self {
        VaultFail::Tx(e)
    }
}
impl From<StateError> for VaultFail {
    fn from(e: StateError) -> Self {
        VaultFail::State(e)
    }
}
impl From<btb_runtime::KvError> for VaultFail {
    fn from(e: btb_runtime::KvError) -> Self {
        VaultFail::State(e.into())
    }
}

// ------------------------------------------------------------------ storage

fn key1(sub: u8, tail: &[u8]) -> Vec<u8> {
    let mut k = vec![TAG_CROSS, sub];
    k.extend_from_slice(tail);
    k
}

pub fn asset_state(kv: &impl KvStore, scheme: Scheme) -> Result<AssetState, StateError> {
    Ok(kv
        .get(&key1(KEY_ASSET, &[scheme.byte()]))?
        .and_then(|b| {
            Some(AssetState {
                credited: u128::from_le_bytes(b.get(..16)?.try_into().ok()?),
                burned: u128::from_le_bytes(b.get(16..32)?.try_into().ok()?),
            })
        })
        .unwrap_or_default())
}

fn put_asset(kv: &mut impl KvStore, scheme: Scheme, a: AssetState) -> Result<(), StateError> {
    let mut v = a.credited.to_le_bytes().to_vec();
    v.extend_from_slice(&a.burned.to_le_bytes());
    Ok(kv.put(&key1(KEY_ASSET, &[scheme.byte()]), &v)?)
}

fn votes(kv: &impl KvStore, key: &[u8]) -> Result<ClaimVotes, StateError> {
    Ok(kv
        .get(key)?
        .map(|b| {
            let mut r = Reader::new(&b, 1 << 16);
            Vec::<[u8; 32]>::decode_from(&mut r).map(|voters| ClaimVotes { voters })
        })
        .transpose()?
        .unwrap_or_default())
}

fn put_votes(kv: &mut impl KvStore, key: &[u8], v: &ClaimVotes) -> Result<(), StateError> {
    Ok(kv.put(key, &v.voters.encode())?)
}

/// Has the deposit claim been credited?
pub fn is_credited(kv: &impl KvStore, claim: &[u8; 32]) -> Result<bool, StateError> {
    Ok(kv.get(&key1(KEY_CREDITED, claim))?.is_some())
}

/// Voters so far for a claim that is not yet credited.
pub fn claim_votes(kv: &impl KvStore, claim: &[u8; 32]) -> Result<Vec<[u8; 32]>, StateError> {
    Ok(votes(kv, &key1(KEY_CLAIM, claim))?.voters)
}

fn wid_key(sub: u8, id: u64) -> Vec<u8> {
    key1(sub, &id.to_be_bytes())
}

pub fn withdrawal(kv: &impl KvStore, id: u64) -> Result<Option<Withdrawal>, StateError> {
    kv.get(&wid_key(KEY_WITHDRAWAL, id))?
        .map(|b| Withdrawal::decode(id, &b).map_err(StateError::from))
        .transpose()
}

fn put_withdrawal(kv: &mut impl KvStore, w: &Withdrawal) -> Result<(), StateError> {
    Ok(kv.put(&wid_key(KEY_WITHDRAWAL, w.id), &w.encode())?)
}

pub fn next_withdrawal_id(kv: &impl KvStore) -> Result<u64, StateError> {
    Ok(kv
        .get(&key1(KEY_WNEXT, &[]))?
        .and_then(|b| <[u8; 8]>::try_from(b).ok())
        .map_or(0, u64::from_le_bytes))
}

fn utxo_key(txid: &[u8; 32], vout: u32) -> Vec<u8> {
    let mut t = txid.to_vec();
    t.extend_from_slice(&vout.to_be_bytes());
    key1(KEY_UTXO, &t)
}

fn put_utxo(
    kv: &mut impl KvStore,
    txid: &[u8; 32],
    vout: u32,
    amount: u64,
) -> Result<(), StateError> {
    Ok(kv.put(&utxo_key(txid, vout), &amount.to_le_bytes())?)
}

/// Value of the spendable vault UTXO `(txid, vout)`, if the vault holds it.
pub fn utxo(kv: &impl KvStore, txid: &[u8; 32], vout: u32) -> Result<Option<u64>, StateError> {
    Ok(kv
        .get(&utxo_key(txid, vout))?
        .and_then(|b| <[u8; 8]>::try_from(b).ok())
        .map(u64::from_le_bytes))
}

/// A spendable Bitcoin vault output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VaultUtxo {
    pub txid: [u8; 32],
    pub vout: u32,
    pub amount: u64,
}

/// The vault's spendable UTXOs, ascending by `(txid, vout)`, at most `limit`.
pub fn utxos(kv: &impl KvStore, limit: usize) -> Result<Vec<VaultUtxo>, StateError> {
    let start = key1(KEY_UTXO, &[]);
    let end = key1(KEY_UTXO + 1, &[]);
    let mut out = Vec::new();
    for (k, v) in kv.scan(&start, Some(&end), false, limit)? {
        let bad = || StateError("bad utxo".into());
        out.push(VaultUtxo {
            txid: k
                .get(2..34)
                .and_then(|b| b.try_into().ok())
                .ok_or_else(bad)?,
            vout: u32::from_be_bytes(
                k.get(34..38)
                    .and_then(|b| b.try_into().ok())
                    .ok_or_else(bad)?,
            ),
            amount: u64::from_le_bytes(v.as_slice().try_into().map_err(|_| bad())?),
        });
    }
    Ok(out)
}

/// The deterministic coin selection every honest validator uses: largest first (ties by
/// outpoint), stopping as soon as the inputs cover `amount`. `None` if the vault cannot cover it
/// or more than [`vault_plan::MAX_INPUTS`] inputs would be needed.
pub fn select_coins(utxos: &[VaultUtxo], amount: u64) -> Option<Vec<VaultUtxo>> {
    let mut all = utxos.to_vec();
    all.sort_by(|a, b| {
        b.amount
            .cmp(&a.amount)
            .then(a.txid.cmp(&b.txid))
            .then(a.vout.cmp(&b.vout))
    });
    let mut sum = 0u64;
    let mut picked = Vec::new();
    for u in all {
        sum = sum.checked_add(u.amount)?;
        picked.push(u);
        if sum >= amount {
            return (picked.len() <= vault_plan::MAX_INPUTS).then_some(picked);
        }
    }
    None
}

/// Up to `limit` withdrawals with id >= `from`, ascending.
pub fn withdrawals_from(
    kv: &impl KvStore,
    from: u64,
    limit: usize,
) -> Result<Vec<Withdrawal>, StateError> {
    let start = wid_key(KEY_WITHDRAWAL, from);
    let end = key1(KEY_WITHDRAWAL + 1, &[]);
    let mut out = Vec::new();
    for (k, v) in kv.scan(&start, Some(&end), false, limit)? {
        let id = u64::from_be_bytes(
            k.get(2..10)
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| StateError("bad withdrawal key".into()))?,
        );
        out.push(Withdrawal::decode(id, &v)?);
    }
    Ok(out)
}

/// Votes for a prepare proposal.
pub fn prepare_votes(
    kv: &impl KvStore,
    id: u64,
    proposal: &[u8; 32],
) -> Result<Vec<[u8; 32]>, StateError> {
    let mut tail = id.to_be_bytes().to_vec();
    tail.extend_from_slice(proposal);
    Ok(votes(kv, &key1(KEY_PREPARE, &tail))?.voters)
}

// ------------------------------------------------------------------ the wrapped ledger

fn tok_balance(kv: &impl KvStore, id: &[u8; 32], who: &Address) -> Result<u128, StateError> {
    Ok(kv
        .get(&layout::token_balance_key(id, who))?
        .and_then(|b| <[u8; 16]>::try_from(b).ok())
        .map_or(0, u128::from_le_bytes))
}

fn set_tok_balance(
    kv: &mut impl KvStore,
    id: &[u8; 32],
    who: &Address,
    v: u128,
) -> Result<(), StateError> {
    let key = layout::token_balance_key(id, who);
    if v == 0 {
        kv.delete(&key)?;
    } else {
        kv.put(&key, &v.to_le_bytes())?;
    }
    Ok(())
}

/// The wrapped token's record, created on first use with the vault as creator and mint authority.
fn token_info(kv: &impl KvStore, scheme: Scheme) -> Result<TokenInfo, StateError> {
    if let Some(info) = kv
        .get(&layout::token_key(&token_id(scheme)))?
        .and_then(|b| TokenInfo::decode(&b))
    {
        return Ok(info);
    }
    let (name, symbol, decimals) = token_meta(scheme);
    Ok(TokenInfo {
        name: name.into(),
        symbol: symbol.into(),
        decimals,
        supply: 0,
        mint_authority: Some(vault_address()),
        creator: vault_address(),
    })
}

fn put_token_info(
    kv: &mut impl KvStore,
    scheme: Scheme,
    info: &TokenInfo,
) -> Result<(), StateError> {
    Ok(kv.put(&layout::token_key(&token_id(scheme)), &info.encode())?)
}

/// Balance of the wrapped token of `scheme` held by `who`.
pub fn balance_of(kv: &impl KvStore, scheme: Scheme, who: &Address) -> Result<u128, StateError> {
    tok_balance(kv, &token_id(scheme), who)
}

/// `(credited, burned, supply)`: the invariant is `supply == credited - burned`.
pub fn accounting(kv: &impl KvStore, scheme: Scheme) -> Result<(u128, u128, u128), StateError> {
    let a = asset_state(kv, scheme)?;
    Ok((a.credited, a.burned, token_info(kv, scheme)?.supply))
}

/// Credit a quorum-confirmed deposit. The only place that mints a wrapped token.
fn credit(kv: &mut impl KvStore, claim: &DepositClaim) -> Result<(), VaultError2> {
    let id = token_id(claim.asset);
    let mut info = token_info(kv, claim.asset)?;
    let amount = u128::from(claim.amount);
    info.supply = info
        .supply
        .checked_add(amount)
        .ok_or(VaultError::Overflow)?;
    let bal = tok_balance(kv, &id, &claim.recipient)?
        .checked_add(amount)
        .ok_or(VaultError::Overflow)?;
    let mut a = asset_state(kv, claim.asset)?;
    a.credited = a.credited.checked_add(amount).ok_or(VaultError::Overflow)?;
    put_token_info(kv, claim.asset, &info)?;
    set_tok_balance(kv, &id, &claim.recipient, bal)?;
    put_asset(kv, claim.asset, a)?;
    Ok(())
}

/// Internal result type for helpers that can fail either way.
type VaultError2 = VaultFail;

// ------------------------------------------------------------------ quorum

/// Does `voters` hold at least two thirds of the stake of the current epoch's validator set?
pub fn has_quorum(kv: &impl KvStore, epoch: u64, voters: &[[u8; 32]]) -> Result<bool, StateError> {
    let Some(rec) = staking::epoch_record(kv, epoch)? else {
        return Ok(false);
    };
    let total: u128 = rec.validators.iter().map(|v| u128::from(v.stake)).sum();
    let voted: u128 = rec
        .validators
        .iter()
        .filter(|v| voters.contains(&v.consensus_key))
        .map(|v| u128::from(v.stake))
        .sum();
    Ok(total > 0 && voted * 3 >= total * 2)
}

fn is_validator(kv: &impl KvStore, epoch: u64, key: &[u8; 32]) -> Result<bool, StateError> {
    Ok(staking::epoch_record(kv, epoch)?
        .is_some_and(|r| r.validators.iter().any(|v| &v.consensus_key == key)))
}

// ------------------------------------------------------------------ transactions

/// `Attest`: a validator votes for deposit claims it observed. Returns the number of claims
/// credited by this vote (as one byte).
pub(crate) fn apply_attest(
    kv: &mut impl KvStore,
    epoch: u64,
    sender_key: &[u8; 32],
    claims: &[DepositClaim],
) -> Result<Vec<u8>, VaultFail> {
    if claims.is_empty() || claims.len() > MAX_CLAIMS || claims.iter().any(|c| c.amount == 0) {
        return Err(VaultError::BadShape.into());
    }
    if !is_validator(kv, epoch, sender_key)? {
        return Err(VaultError::NotValidator.into());
    }
    for c in claims {
        if xdkg::key_at(kv, c.asset, epoch)?.is_none() {
            return Err(VaultError::NoKey.into());
        }
    }
    let mut credited = 0u8;
    for c in claims {
        let id = c.id();
        if is_credited(kv, &id)? || kv.get(&key1(KEY_OUTPOINT, &c.outpoint_id()))?.is_some() {
            continue; // replay of a credited deposit (in any block): nothing to do
        }
        let key = key1(KEY_CLAIM, &id);
        let mut v = votes(kv, &key)?;
        if !v.voters.contains(sender_key) {
            v.voters.push(*sender_key);
        }
        if has_quorum(kv, epoch, &v.voters)? {
            credit(kv, c)?;
            kv.put(&key1(KEY_CREDITED, &id), &[])?;
            kv.put(&key1(KEY_OUTPOINT, &c.outpoint_id()), &[])?;
            if c.asset == Scheme::Taproot {
                put_utxo(kv, &c.txid, c.index, c.amount)?;
            }
            kv.delete(&key)?;
            credited = credited.saturating_add(1);
        } else {
            put_votes(kv, &key, &v)?;
        }
    }
    Ok(vec![credited])
}

/// `Withdraw`: burn `amount` of the sender's wrapped balance and record the request.
pub(crate) fn apply_withdraw(
    kv: &mut impl KvStore,
    epoch: u64,
    height: u64,
    sender: &Address,
    asset: Scheme,
    amount: u64,
    destination: &[u8],
) -> Result<Vec<u8>, VaultFail> {
    if amount == 0 {
        return Err(VaultError::BadShape.into());
    }
    let ok = match asset {
        // any standard scriptPubKey, at most 64 bytes
        Scheme::Taproot => (2..=MAX_DESTINATION).contains(&destination.len()),
        Scheme::Ed25519 => destination.len() == 32,
    };
    if !ok {
        return Err(VaultError::BadDestination.into());
    }
    if xdkg::key_at(kv, asset, epoch)?.is_none() {
        return Err(VaultError::NoKey.into());
    }
    let tid = token_id(asset);
    let have = tok_balance(kv, &tid, sender)?;
    let amt = u128::from(amount);
    let rest = have
        .checked_sub(amt)
        .ok_or(VaultError::InsufficientBalance)?;
    let mut info = token_info(kv, asset)?;
    info.supply = info.supply.checked_sub(amt).ok_or(VaultError::Overflow)?;
    let mut a = asset_state(kv, asset)?;
    a.burned = a.burned.checked_add(amt).ok_or(VaultError::Overflow)?;
    let id = next_withdrawal_id(kv)?;
    // validated; now write: burn first, then the record
    set_tok_balance(kv, &tid, sender, rest)?;
    put_token_info(kv, asset, &info)?;
    put_asset(kv, asset, a)?;
    put_withdrawal(
        kv,
        &Withdrawal {
            id,
            asset,
            owner: *sender,
            amount,
            destination: destination.to_vec(),
            height,
            status: STATUS_BURNED,
            digest: [0; 32],
            requests: Vec::new(),
            plan: Vec::new(),
        },
    )?;
    kv.put(&key1(KEY_WNEXT, &[]), &(id + 1).to_le_bytes())?;
    Ok(id.to_le_bytes().to_vec())
}

/// The id of a prepare proposal: the same bytes from every validator vote for the same
/// transaction.
pub fn proposal_id(
    withdrawal: u64,
    digest: &[u8; 32],
    sighashes: &[[u8; 32]],
    plan: &[u8],
) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"btb.vault.prepare.v1");
    h.update(withdrawal.to_le_bytes());
    h.update(digest);
    h.update((sighashes.len() as u32).to_le_bytes());
    for s in sighashes {
        h.update(s);
    }
    h.update(Sha256::digest(plan));
    h.finalize().into()
}

/// `Prepare`: a validator votes for the unsigned external transaction of a withdrawal.
/// A validator's proposal for the unsigned external transaction of a withdrawal.
pub(crate) struct Proposal<'a> {
    pub id: u64,
    pub digest: &'a [u8; 32],
    pub sighashes: &'a [[u8; 32]],
    pub plan: &'a [u8],
}

pub(crate) fn apply_prepare(
    kv: &mut impl KvStore,
    epoch: u64,
    height: u64,
    sender_key: &[u8; 32],
    p: Proposal<'_>,
) -> Result<Vec<u8>, VaultFail> {
    let Proposal {
        id,
        digest,
        sighashes,
        plan,
    } = p;
    if plan.len() > vault_plan::MAX_PLAN {
        return Err(VaultError::BadShape.into());
    }
    if sighashes.is_empty() || sighashes.len() > MAX_SIGHASHES {
        return Err(VaultError::BadShape.into());
    }
    if !is_validator(kv, epoch, sender_key)? {
        return Err(VaultError::NotValidator.into());
    }
    let mut w = withdrawal(kv, id)?.ok_or(VaultError::NoSuchWithdrawal)?;
    if w.status != STATUS_BURNED {
        return Err(VaultError::AlreadyPrepared.into());
    }
    let effect = check_plan(kv, epoch, &w, digest, sighashes, plan)?;
    let proposal = proposal_id(id, digest, sighashes, plan);
    let mut tail = id.to_be_bytes().to_vec();
    tail.extend_from_slice(&proposal);
    let key = key1(KEY_PREPARE, &tail);
    let mut v = votes(kv, &key)?;
    if !v.voters.contains(sender_key) {
        v.voters.push(*sender_key);
    }
    if !has_quorum(kv, epoch, &v.voters)? {
        put_votes(kv, &key, &v)?;
        return Ok(vec![0]);
    }
    // quorum: fix the transaction and queue its signatures (exactly once: the status changes)
    w.status = STATUS_PREPARED;
    w.digest = *digest;
    w.plan = plan.to_vec();
    if let Some(e) = &effect {
        // the inputs are spent exactly once: removed now, whatever happens to the signing
        for (t, v) in &e.spend {
            kv.delete(&utxo_key(t, *v))?;
        }
        if let Some((t, v, a)) = e.change {
            put_utxo(kv, &t, v, a)?;
        }
    }
    for (i, sighash) in sighashes.iter().enumerate() {
        let message = match &effect {
            Some(e) if w.asset == Scheme::Ed25519 => e.sol_message.clone(),
            _ => sighash.to_vec(),
        };
        let r = xsign::request(
            kv,
            height,
            w.asset,
            message,
            RequestTag::Withdrawal {
                id,
                index: i as u32,
            },
        )?;
        w.requests.push(r);
    }
    put_withdrawal(kv, &w)?;
    kv.delete(&key)?;
    Ok(vec![1])
}

/// What an accepted plan does to the vault's UTXO set.
struct PlanEffect {
    spend: Vec<([u8; 32], u32)>,
    change: Option<([u8; 32], u32, u64)>,
    sol_message: Vec<u8>,
}

/// Check a proposal's plan against the burn and the vault's state. An empty plan is accepted
/// without checks (mock observers); real watchers always send one.
fn check_plan(
    kv: &impl KvStore,
    epoch: u64,
    w: &Withdrawal,
    digest: &[u8; 32],
    sighashes: &[[u8; 32]],
    plan: &[u8],
) -> Result<Option<PlanEffect>, VaultFail> {
    if plan.is_empty() {
        return Ok(None);
    }
    let vkey = output_key(kv, w.asset, epoch)?.ok_or(VaultError::NoKey)?;
    match w.asset {
        Scheme::Taproot => {
            let script = addr::taproot_script_pubkey(&vkey);
            let missing = std::cell::Cell::new(false);
            let lookup = |t: &[u8; 32], v: u32| match utxo(kv, t, v) {
                Ok(Some(a)) => Some(a),
                _ => {
                    missing.set(true);
                    None
                }
            };
            let checked = vault_plan::check_btc(plan, &w.destination, w.amount, &script, &lookup);
            let (p, amounts) = match checked {
                Ok(x) => x,
                Err(_) if missing.get() => return Err(VaultError::UtxoUnavailable.into()),
                Err(_) => return Err(VaultError::BadPlan.into()),
            };
            let prevouts: Vec<(u64, Vec<u8>)> =
                amounts.iter().map(|a| (*a, script.clone())).collect();
            let txid = p.tx.txid();
            let expect: Option<Vec<[u8; 32]>> = (0..p.tx.inputs.len())
                .map(|i| p.tx.taproot_sighash(i, &prevouts))
                .collect();
            if digest != &txid || expect.as_deref() != Some(sighashes) {
                return Err(VaultError::BadPlan.into());
            }
            Ok(Some(PlanEffect {
                spend: p.tx.inputs.iter().map(|(t, v, _)| (*t, *v)).collect(),
                change: p.change.map(|(i, a)| (txid, i, a)),
                sol_message: Vec::new(),
            }))
        }
        Scheme::Ed25519 => {
            vault_plan::check_sol(plan, &vkey, &w.destination, w.amount)
                .map_err(|_| VaultError::BadPlan)?;
            let h: [u8; 32] = Sha256::digest(plan).into();
            if digest != &h || sighashes != [h] {
                return Err(VaultError::BadPlan.into());
            }
            Ok(Some(PlanEffect {
                spend: Vec::new(),
                change: None,
                sol_message: plan.to_vec(),
            }))
        }
    }
}

/// A signing request finished: a withdrawal whose requests are all done becomes `Signed`.
pub(crate) fn on_signed(kv: &mut impl KvStore, r: &SignRequest) -> Result<(), StateError> {
    let RequestTag::Withdrawal { id, .. } = r.tag else {
        return Ok(());
    };
    let Some(mut w) = withdrawal(&*kv, id)? else {
        return Ok(());
    };
    if w.status != STATUS_PREPARED {
        return Ok(());
    }
    for rid in &w.requests {
        if *rid == r.id {
            continue;
        }
        if !xsign::get(&*kv, *rid)?.is_some_and(|q| q.is_done()) {
            return Ok(());
        }
    }
    w.status = STATUS_SIGNED;
    put_withdrawal(kv, &w)
}

// ------------------------------------------------------------------ addresses

/// The 32-byte external key of the vault of `scheme` for `epoch`: the Taproot output key, or the
/// Ed25519 public key. It does not change when the key is reshared.
pub fn output_key(
    kv: &impl KvStore,
    scheme: Scheme,
    epoch: u64,
) -> Result<Option<[u8; 32]>, StateError> {
    let Some((_, k)) = xdkg::key_at(kv, scheme, epoch)? else {
        return Ok(None);
    };
    Ok(k.group.output_key().ok())
}

/// A deposit address: Bitcoin Taproot with the given bech32 prefix (`bc`, `tb`, `bcrt`) or the
/// Solana account.
pub fn deposit_address(scheme: Scheme, output_key: &[u8; 32], hrp: &str) -> String {
    match scheme {
        Scheme::Taproot => addr::taproot_address(hrp, output_key),
        Scheme::Ed25519 => addr::solana_address(output_key),
    }
}

// ------------------------------------------------------------------ observers

/// The unsigned external transaction a validator proposes for a withdrawal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub digest: [u8; 32],
    pub sighashes: Vec<[u8; 32]>,
    /// The unsigned external transaction (see [`Withdrawal::plan`]); empty for the mock.
    pub plan: Vec<u8>,
}

/// How far a broadcast withdrawal got on the external chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Execution {
    /// Not broadcast yet, or the endpoint does not know it.
    Unknown,
    /// Accepted by the network, not yet final.
    Pending,
    /// Final (Bitcoin: at least the configured confirmations; Solana: finalized).
    Confirmed,
    /// The external chain refused or dropped it (error status, expired blockhash). The burn
    /// stays; refunds are planned (`docs/CROSS_CHAIN.md`).
    Failed,
}

/// What a validator knows about the external chains. The real implementation
/// (`btb_node::watch::Watchers`, `docs/CROSS_CHAIN.md`) reads the validator's own endpoints;
/// the chain never trusts one validator's answer, only two thirds of the stake agreeing.
pub trait Observer: Send + Sync + std::fmt::Debug {
    /// Deposits to the vault `vault_key` that are final on the external chain.
    fn deposits(&self, scheme: Scheme, vault_key: &[u8; 32]) -> Vec<DepositClaim>;

    /// The unsigned external transaction paying a withdrawal (UTXO selection, fees, recent
    /// blockhash...), or `None` if it cannot be built yet.
    fn prepare(&self, w: &Withdrawal, vault_key: &[u8; 32]) -> Option<Prepared>;

    /// Like [`Observer::prepare`] for observers that build real transactions and need the
    /// vault's spendable UTXOs (chain state, so identical on every validator).
    fn prepare_with(
        &self,
        w: &Withdrawal,
        vault_key: &[u8; 32],
        _utxos: &[VaultUtxo],
    ) -> Option<Prepared> {
        self.prepare(w, vault_key)
    }

    /// A withdrawal whose signatures are on chain: assemble the external transaction and
    /// broadcast it. Must not block (queue the work); called again until [`Observer::execution`]
    /// says `Confirmed`, so it must be idempotent.
    fn broadcast(&self, _w: &Withdrawal, _vault_key: &[u8; 32], _signatures: &[Vec<u8>]) {}

    /// Has the withdrawal's transaction been confirmed on the external chain?
    fn execution(&self, _w: &Withdrawal) -> Execution {
        Execution::Confirmed
    }
}

/// A scriptable observer for tests and local testnets: deposits are injected by hand and every
/// withdrawal gets a deterministic one-input transaction.
#[derive(Debug, Default)]
pub struct MockObserver {
    deposits: std::sync::Mutex<Vec<DepositClaim>>,
}

impl MockObserver {
    pub fn new() -> MockObserver {
        MockObserver::default()
    }

    /// Make a deposit visible to every validator that shares this observer.
    pub fn deposit(&self, claim: DepositClaim) {
        self.deposits.lock().expect("mock observer").push(claim);
    }

    /// The sighash the mock builds for `withdrawal`.
    pub fn sighash_of(w: &Withdrawal) -> [u8; 32] {
        crate::sha256(&[
            b"btb.mock.sighash.v1",
            &w.id.to_le_bytes(),
            &[w.asset.byte()],
            &w.amount.to_le_bytes(),
            &w.destination,
        ])
    }
}

impl Observer for MockObserver {
    fn deposits(&self, scheme: Scheme, _vault_key: &[u8; 32]) -> Vec<DepositClaim> {
        self.deposits
            .lock()
            .expect("mock observer")
            .iter()
            .filter(|c| c.asset == scheme)
            .cloned()
            .collect()
    }

    fn prepare(&self, w: &Withdrawal, _vault_key: &[u8; 32]) -> Option<Prepared> {
        let sighash = Self::sighash_of(w);
        Some(Prepared {
            digest: crate::sha256(&[b"btb.mock.unsigned.v1", &sighash]),
            sighashes: vec![sighash],
            plan: Vec::new(),
        })
    }
}
