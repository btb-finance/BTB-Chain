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
//! What the chain does **not** check: that the prepared external transaction really pays the
//! burned amount to the destination. That binding is the validators' two-thirds agreement; a
//! chain-side parser per external chain, and watchers that confirm execution, are the next steps
//! (`docs/CROSS_CHAIN.md`, "Implemented and planned").
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

use crate::staking;
use crate::state::StateError;
use crate::tx::Address;
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
        ])
    }

    pub(crate) fn encode_to(&self, o: &mut Vec<u8>) {
        self.asset.byte().encode_to(o);
        self.txid.encode_to(o);
        self.index.encode_to(o);
        self.amount.encode_to(o);
        self.recipient.encode_to(o);
    }

    pub(crate) fn decode_from(r: &mut Reader<'_>) -> Result<DepositClaim, DecodeError> {
        Ok(DepositClaim {
            asset: Scheme::from_byte(u8::decode_from(r)?).ok_or(DecodeError::TrailingBytes)?,
            txid: <[u8; 32]>::decode_from(r)?,
            index: u32::decode_from(r)?,
            amount: u64::decode_from(r)?,
            recipient: <[u8; 32]>::decode_from(r)?,
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
        o
    }

    fn decode(id: u64, b: &[u8]) -> Result<Withdrawal, DecodeError> {
        let mut r = Reader::new(b, 1 << 12);
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
        if is_credited(kv, &id)? {
            continue; // replay of a credited deposit: nothing to do
        }
        let key = key1(KEY_CLAIM, &id);
        let mut v = votes(kv, &key)?;
        if !v.voters.contains(sender_key) {
            v.voters.push(*sender_key);
        }
        if has_quorum(kv, epoch, &v.voters)? {
            credit(kv, c)?;
            kv.put(&key1(KEY_CREDITED, &id), &[])?;
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
        },
    )?;
    kv.put(&key1(KEY_WNEXT, &[]), &(id + 1).to_le_bytes())?;
    Ok(id.to_le_bytes().to_vec())
}

/// The id of a prepare proposal: the same bytes from every validator vote for the same
/// transaction.
pub fn proposal_id(withdrawal: u64, digest: &[u8; 32], sighashes: &[[u8; 32]]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"btb.vault.prepare.v1");
    h.update(withdrawal.to_le_bytes());
    h.update(digest);
    h.update((sighashes.len() as u32).to_le_bytes());
    for s in sighashes {
        h.update(s);
    }
    h.finalize().into()
}

/// `Prepare`: a validator votes for the unsigned external transaction of a withdrawal.
pub(crate) fn apply_prepare(
    kv: &mut impl KvStore,
    epoch: u64,
    height: u64,
    sender_key: &[u8; 32],
    id: u64,
    digest: &[u8; 32],
    sighashes: &[[u8; 32]],
) -> Result<Vec<u8>, VaultFail> {
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
    let proposal = proposal_id(id, digest, sighashes);
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
    for (i, sighash) in sighashes.iter().enumerate() {
        let r = xsign::request(
            kv,
            height,
            w.asset,
            *sighash,
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
}

/// What a validator knows about the external chains. The real implementations (Bitcoin and
/// Solana chain watchers, `docs/CROSS_CHAIN.md`) read their own full nodes or several providers;
/// the chain never trusts one validator's answer, only two thirds of the stake agreeing.
pub trait Observer: Send + Sync + std::fmt::Debug {
    /// Deposits to the vault `vault_key` that are final on the external chain.
    fn deposits(&self, scheme: Scheme, vault_key: &[u8; 32]) -> Vec<DepositClaim>;

    /// The unsigned external transaction paying a withdrawal (UTXO selection, fees, recent
    /// blockhash...), or `None` if it cannot be built yet.
    fn prepare(&self, w: &Withdrawal, vault_key: &[u8; 32]) -> Option<Prepared>;
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
        })
    }
}
