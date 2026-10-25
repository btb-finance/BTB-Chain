//! Signature requests for the cross-chain keys, their results, and misbehaviour evidence
//! (`docs/CROSS_CHAIN.md`).
//!
//! A native module (the vault, `crate::vault`) asks for a signature over a 32-byte message under
//! the key of a [`Scheme`] with [`request`]. The chain only keeps the request: validators run the
//! FROST rounds off-chain (`btb-node`, `docs/CROSS_CHAIN.md`) and anyone submits the finished
//! signature with `CrossOp::Signature`. The chain verifies it with the same verifier an external
//! chain uses and stores it; a signature that verifies is self-authenticating, so no vote is
//! needed.
//!
//! * **Signer sets** are a deterministic function of (key, request, attempt, recorded
//!   exclusions): [`select_signers`]. Attempt `a + 1` takes over when attempt `a` does not
//!   finish or a signer in it is proven to have misbehaved.
//! * **Evidence.** A signer's round-1 announcement and round-2 share are signed with its consensus
//!   key. A share that does not verify for the package the signer itself signed, or two different
//!   round-1 announcements for one attempt, are provable to anyone: `CrossOp::Evidence` checks
//!   the proof, records the offence under `70 30` and excludes the signer from the signer sets of
//!   the current and the next epoch. Nothing is slashed yet (the record is the input a slashing
//!   rule needs; `docs/CROSS_CHAIN.md`).
//!
//! # State (tag `0x70`)
//!
//! | Key | Value |
//! | --- | --- |
//! | `70 20` | next request id (`u64` LE) |
//! | `70 21 id:u64be` | [`SignRequest`] |
//! | `70 22 id:u64be` | empty: the request is still pending |
//! | `70 30 scheme holder_key[32]` | [`Offender`] |
//! | `70 31 digest[32]` | empty: this evidence was already recorded |

use crate::state::StateError;
use crate::xdkg::{self, TAG_CROSS};
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_oracle::frost::{self, Scheme, SigningPackage};
use btb_runtime::kv::KvStore;
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const KEY_NEXT: u8 = 0x20;
pub const KEY_REQ: u8 = 0x21;
pub const KEY_PENDING: u8 = 0x22;
pub const KEY_OFFENDER: u8 = 0x30;
pub const KEY_EVIDENCE: u8 = 0x31;

/// Gas of a signature submission (one signature verification on the curve).
pub const SIGNATURE_GAS: u64 = 150_000;
/// Gas of an evidence submission (one signature share verification and two Ed25519 checks).
pub const EVIDENCE_GAS: u64 = 400_000;

/// Blocks a finished request stays in state.
pub const RETENTION_BLOCKS: u64 = 100_000;
const PRUNE_PER_BLOCK: usize = 32;
/// Most attempts a node makes for one request before it waits for a new epoch or an operator.
pub const MAX_ATTEMPT: u32 = 1_000;

/// Who asked for a signature (the module that consumes the result).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestTag {
    /// Test and tooling requests: no consumer.
    Raw,
    /// Sighash number `index` of withdrawal `id` (`crate::vault`).
    Withdrawal { id: u64, index: u32 },
}

impl RequestTag {
    fn encode_to(&self, o: &mut Vec<u8>) {
        match self {
            RequestTag::Raw => {
                0u8.encode_to(o);
                0u64.encode_to(o);
                0u32.encode_to(o);
            }
            RequestTag::Withdrawal { id, index } => {
                1u8.encode_to(o);
                id.encode_to(o);
                index.encode_to(o);
            }
        }
    }

    fn decode_from(r: &mut Reader<'_>) -> Result<RequestTag, DecodeError> {
        let kind = u8::decode_from(r)?;
        let id = u64::decode_from(r)?;
        let index = u32::decode_from(r)?;
        match kind {
            0 => Ok(RequestTag::Raw),
            1 => Ok(RequestTag::Withdrawal { id, index }),
            _ => Err(DecodeError::TrailingBytes),
        }
    }
}

/// A request for a signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignRequest {
    pub id: u64,
    pub scheme: Scheme,
    pub message: [u8; 32],
    /// Height of the block that queued it.
    pub height: u64,
    pub tag: RequestTag,
    /// The finished 64-byte signature.
    pub signature: Option<Vec<u8>>,
    pub done_height: u64,
}

impl SignRequest {
    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.scheme.byte().encode_to(&mut o);
        self.message.encode_to(&mut o);
        self.height.encode_to(&mut o);
        self.tag.encode_to(&mut o);
        self.signature.encode_to(&mut o);
        self.done_height.encode_to(&mut o);
        o
    }

    fn decode(id: u64, b: &[u8]) -> Result<SignRequest, DecodeError> {
        let mut r = Reader::new(b, 1 << 12);
        let scheme =
            Scheme::from_byte(u8::decode_from(&mut r)?).ok_or(DecodeError::TrailingBytes)?;
        let q = SignRequest {
            id,
            scheme,
            message: <[u8; 32]>::decode_from(&mut r)?,
            height: u64::decode_from(&mut r)?,
            tag: RequestTag::decode_from(&mut r)?,
            signature: Option::<Vec<u8>>::decode_from(&mut r)?,
            done_height: u64::decode_from(&mut r)?,
        };
        if r.remaining() != 0 {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(q)
    }

    pub fn is_done(&self) -> bool {
        self.signature.is_some()
    }
}

/// A signer with recorded offences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Offender {
    pub count: u32,
    /// Excluded from signer sets of epochs up to and including this one.
    pub until_epoch: u64,
    pub last_request: u64,
}

impl Offender {
    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.count.encode_to(&mut o);
        self.until_epoch.encode_to(&mut o);
        self.last_request.encode_to(&mut o);
        o
    }

    fn decode(b: &[u8]) -> Result<Offender, DecodeError> {
        let mut r = Reader::new(b, 64);
        let o = Offender {
            count: u32::decode_from(&mut r)?,
            until_epoch: u64::decode_from(&mut r)?,
            last_request: u64::decode_from(&mut r)?,
        };
        if r.remaining() != 0 {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(o)
    }
}

/// Why a signature or evidence transaction was refused (the fee is still charged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XsignError {
    NoSuchRequest,
    AlreadySigned,
    /// The chain has no key for the scheme yet.
    NoKey,
    /// The signature does not verify under the group key.
    BadSignature,
    /// The holder index is outside the key, or the proof is malformed.
    BadEvidence,
    /// The accused signer's signature on the evidence does not verify.
    NotSignedByAccused,
    /// The share is valid, or the two announcements are not different: no offence.
    NoOffence,
    AlreadyRecorded,
}

// ------------------------------------------------------------------ digests (shared with nodes)

fn h(parts: &[&[u8]]) -> [u8; 32] {
    let mut s = Sha256::new();
    for p in parts {
        s.update(p);
    }
    s.finalize().into()
}

/// What a signer signs with its consensus key to announce which of its preprocessed nonces it
/// uses for `attempt` of `request`.
pub fn use_digest(
    chain_id: u64,
    scheme: Scheme,
    origin: u64,
    request: u64,
    attempt: u32,
    counter: u64,
) -> [u8; 32] {
    h(&[
        b"btb.xsign.use.v1",
        &chain_id.to_le_bytes(),
        &[scheme.byte()],
        &origin.to_le_bytes(),
        &request.to_le_bytes(),
        &attempt.to_le_bytes(),
        &counter.to_le_bytes(),
    ])
}

/// Hash of an encoded signing package.
pub fn package_hash(package: &[u8]) -> [u8; 32] {
    h(&[b"btb.xsign.pkg.v1", package])
}

/// What a signer signs with its consensus key over its round-2 share: the share is bound to the
/// package it was computed for.
pub fn share_digest(
    chain_id: u64,
    scheme: Scheme,
    origin: u64,
    request: u64,
    attempt: u32,
    package: &[u8; 32],
    share: &[u8],
) -> [u8; 32] {
    h(&[
        b"btb.xsign.share.v1",
        &chain_id.to_le_bytes(),
        &[scheme.byte()],
        &origin.to_le_bytes(),
        &request.to_le_bytes(),
        &attempt.to_le_bytes(),
        package,
        share,
    ])
}

fn verify_ed(key: &[u8; 32], digest: &[u8; 32], sig: &[u8; 64]) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(key) else {
        return false;
    };
    vk.verify_strict(digest, &Signature::from_bytes(sig))
        .is_ok()
}

/// The signer set of `attempt` of `request`: the `t` holders with the smallest
/// `H(key, request, attempt, holder)` among those not excluded. If exclusions would leave fewer
/// than `t` eligible holders they are ignored (liveness before punishment).
pub fn select_signers(
    key: &xdkg::XKey,
    scheme: Scheme,
    excluded: &BTreeSet<[u8; 32]>,
    request: u64,
    attempt: u32,
) -> Vec<usize> {
    let t = key.group.threshold;
    let mut eligible: Vec<usize> = (0..key.holders.len())
        .filter(|i| !excluded.contains(&key.holders[*i]))
        .collect();
    if eligible.len() < t {
        eligible = (0..key.holders.len()).collect();
    }
    let mut ranked: Vec<([u8; 32], usize)> = eligible
        .into_iter()
        .map(|i| {
            (
                h(&[
                    b"btb.xsign.rank.v1",
                    &[scheme.byte()],
                    &key.origin.to_le_bytes(),
                    &request.to_le_bytes(),
                    &attempt.to_le_bytes(),
                    &(i as u32).to_le_bytes(),
                ]),
                i,
            )
        })
        .collect();
    ranked.sort();
    let mut set: Vec<usize> = ranked.into_iter().take(t).map(|(_, i)| i).collect();
    set.sort_unstable();
    set
}

// ------------------------------------------------------------------ storage

fn req_key(sub: u8, id: u64) -> Vec<u8> {
    let mut k = vec![TAG_CROSS, sub];
    k.extend_from_slice(&id.to_be_bytes());
    k
}

fn next_key() -> Vec<u8> {
    vec![TAG_CROSS, KEY_NEXT]
}

fn offender_key(scheme: Scheme, holder: &[u8; 32]) -> Vec<u8> {
    let mut k = vec![TAG_CROSS, KEY_OFFENDER, scheme.byte()];
    k.extend_from_slice(holder);
    k
}

/// The id the next request gets (= how many were ever made).
pub fn next_id(kv: &impl KvStore) -> Result<u64, StateError> {
    Ok(kv
        .get(&next_key())?
        .and_then(|b| <[u8; 8]>::try_from(b).ok())
        .map_or(0, u64::from_le_bytes))
}

pub fn get(kv: &impl KvStore, id: u64) -> Result<Option<SignRequest>, StateError> {
    kv.get(&req_key(KEY_REQ, id))?
        .map(|b| SignRequest::decode(id, &b).map_err(StateError::from))
        .transpose()
}

fn put(kv: &mut impl KvStore, r: &SignRequest) -> Result<(), StateError> {
    Ok(kv.put(&req_key(KEY_REQ, r.id), &r.encode())?)
}

/// Up to `limit` pending requests with id >= `from`, ascending.
pub fn pending_from(
    kv: &impl KvStore,
    from: u64,
    limit: usize,
) -> Result<Vec<SignRequest>, StateError> {
    let start = req_key(KEY_PENDING, from);
    let end = vec![TAG_CROSS, KEY_PENDING + 1];
    let mut out = Vec::new();
    for (k, _) in kv.scan(&start, Some(&end), false, limit)? {
        let id = u64::from_be_bytes(
            k.get(2..10)
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| StateError("bad pending key".into()))?,
        );
        if let Some(r) = get(kv, id)? {
            out.push(r);
        }
    }
    Ok(out)
}

/// Queue a signature request (called by native modules only; there is no transaction for it).
pub fn request(
    kv: &mut impl KvStore,
    height: u64,
    scheme: Scheme,
    message: [u8; 32],
    tag: RequestTag,
) -> Result<u64, StateError> {
    let id = next_id(&*kv)?;
    put(
        kv,
        &SignRequest {
            id,
            scheme,
            message,
            height,
            tag,
            signature: None,
            done_height: 0,
        },
    )?;
    kv.put(&req_key(KEY_PENDING, id), &[])?;
    kv.put(&next_key(), &(id + 1).to_le_bytes())?;
    Ok(id)
}

pub fn offender(
    kv: &impl KvStore,
    scheme: Scheme,
    holder: &[u8; 32],
) -> Result<Option<Offender>, StateError> {
    kv.get(&offender_key(scheme, holder))?
        .map(|b| Offender::decode(&b).map_err(StateError::from))
        .transpose()
}

/// Consensus keys excluded from the signer sets of `epoch` for `scheme`.
pub fn excluded(
    kv: &impl KvStore,
    scheme: Scheme,
    epoch: u64,
) -> Result<BTreeSet<[u8; 32]>, StateError> {
    let start = vec![TAG_CROSS, KEY_OFFENDER, scheme.byte()];
    let end = vec![TAG_CROSS, KEY_OFFENDER, scheme.byte() + 1];
    let mut out = BTreeSet::new();
    for (k, v) in kv.scan(&start, Some(&end), false, 1024)? {
        let o = Offender::decode(&v)?;
        if o.until_epoch >= epoch {
            if let Some(key) = k.get(3..35).and_then(|b| <[u8; 32]>::try_from(b).ok()) {
                out.insert(key);
            }
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ transactions

/// What the evidence proves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proof {
    /// The accused signed a share that does not verify for the package it signed.
    BadShare {
        package: Vec<u8>,
        share: Vec<u8>,
        sig: [u8; 64],
    },
    /// The accused signed two different nonce announcements for one attempt.
    Equivocation {
        counter_a: u64,
        sig_a: [u8; 64],
        counter_b: u64,
        sig_b: [u8; 64],
    },
}

impl Proof {
    pub(crate) fn encode_to(&self, o: &mut Vec<u8>) {
        match self {
            Proof::BadShare {
                package,
                share,
                sig,
            } => {
                0u8.encode_to(o);
                package.encode_to(o);
                share.encode_to(o);
                sig.encode_to(o);
            }
            Proof::Equivocation {
                counter_a,
                sig_a,
                counter_b,
                sig_b,
            } => {
                1u8.encode_to(o);
                counter_a.encode_to(o);
                sig_a.encode_to(o);
                counter_b.encode_to(o);
                sig_b.encode_to(o);
            }
        }
    }

    pub(crate) fn decode_from(r: &mut Reader<'_>) -> Result<Proof, DecodeError> {
        match u8::decode_from(r)? {
            0 => Ok(Proof::BadShare {
                package: Vec::decode_from(r)?,
                share: Vec::decode_from(r)?,
                sig: <[u8; 64]>::decode_from(r)?,
            }),
            1 => Ok(Proof::Equivocation {
                counter_a: u64::decode_from(r)?,
                sig_a: <[u8; 64]>::decode_from(r)?,
                counter_b: u64::decode_from(r)?,
                sig_b: <[u8; 64]>::decode_from(r)?,
            }),
            _ => Err(DecodeError::TrailingBytes),
        }
    }
}

/// Everything an evidence transaction names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceOp {
    pub request: u64,
    pub attempt: u32,
    /// Origin epoch of the key (`XKey::origin`) the accused held a share of.
    pub origin: u64,
    pub holder: u32,
    pub proof: Proof,
}

/// A submitted signature for a pending request.
pub(crate) fn apply_signature(
    kv: &mut impl KvStore,
    epoch: u64,
    height: u64,
    id: u64,
    sig: &[u8],
) -> Result<Vec<u8>, XsignFail> {
    let mut r = get(kv, id)?.ok_or(XsignError::NoSuchRequest)?;
    if r.is_done() {
        return Err(XsignError::AlreadySigned.into());
    }
    let (_, key) = xdkg::key_at(kv, r.scheme, epoch)?.ok_or(XsignError::NoKey)?;
    if sig.len() != frost::SIGNATURE_BYTES || !frost::verify(&key.group, &r.message, sig) {
        return Err(XsignError::BadSignature.into());
    }
    r.signature = Some(sig.to_vec());
    r.done_height = height;
    put(kv, &r)?;
    kv.delete(&req_key(KEY_PENDING, id))?;
    crate::vault::on_signed(kv, &r)?;
    Ok(Vec::new())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum XsignFail {
    Tx(XsignError),
    State(StateError),
}
impl From<XsignError> for XsignFail {
    fn from(e: XsignError) -> Self {
        XsignFail::Tx(e)
    }
}
impl From<StateError> for XsignFail {
    fn from(e: StateError) -> Self {
        XsignFail::State(e)
    }
}
impl From<btb_runtime::KvError> for XsignFail {
    fn from(e: btb_runtime::KvError) -> Self {
        XsignFail::State(e.into())
    }
}

/// Check and record evidence of misbehaviour.
pub(crate) fn apply_evidence(
    kv: &mut impl KvStore,
    chain_id: u64,
    epoch: u64,
    ev: &EvidenceOp,
) -> Result<Vec<u8>, XsignFail> {
    let r = get(kv, ev.request)?.ok_or(XsignError::NoSuchRequest)?;
    let key = xdkg::xkey(kv, r.scheme, ev.origin)?.ok_or(XsignError::NoKey)?;
    let accused = *key
        .holders
        .get(ev.holder as usize)
        .ok_or(XsignError::BadEvidence)?;
    let digest = match &ev.proof {
        Proof::BadShare {
            package,
            share,
            sig,
        } => {
            let pkg = SigningPackage::decode(package).ok_or(XsignError::BadEvidence)?;
            if pkg.message != r.message
                || pkg.commitments.len() < key.group.threshold
                || !pkg.commitments.contains_key(&(ev.holder as usize))
            {
                return Err(XsignError::BadEvidence.into());
            }
            let d = share_digest(
                chain_id,
                r.scheme,
                ev.origin,
                ev.request,
                ev.attempt,
                &package_hash(package),
                share,
            );
            if !verify_ed(&accused, &d, sig) {
                return Err(XsignError::NotSignedByAccused.into());
            }
            if frost::verify_signature_share(&key.group, &pkg, ev.holder as usize, share) {
                return Err(XsignError::NoOffence.into());
            }
            d
        }
        Proof::Equivocation {
            counter_a,
            sig_a,
            counter_b,
            sig_b,
        } => {
            if counter_a == counter_b {
                return Err(XsignError::NoOffence.into());
            }
            let da = use_digest(
                chain_id, r.scheme, ev.origin, ev.request, ev.attempt, *counter_a,
            );
            let db = use_digest(
                chain_id, r.scheme, ev.origin, ev.request, ev.attempt, *counter_b,
            );
            if !verify_ed(&accused, &da, sig_a) || !verify_ed(&accused, &db, sig_b) {
                return Err(XsignError::NotSignedByAccused.into());
            }
            h(&[&da, &db])
        }
    };
    let marker = {
        let mut k = vec![TAG_CROSS, KEY_EVIDENCE];
        k.extend_from_slice(&h(&[b"btb.xsign.evid.v1", &digest]));
        k
    };
    if kv.get(&marker)?.is_some() {
        return Err(XsignError::AlreadyRecorded.into());
    }
    kv.put(&marker, &[])?;
    let mut o = offender(kv, r.scheme, &accused)?.unwrap_or_default();
    o.count = o.count.saturating_add(1);
    o.until_epoch = o.until_epoch.max(epoch.saturating_add(1));
    o.last_request = ev.request;
    kv.put(&offender_key(r.scheme, &accused), &o.encode())?;
    Ok(Vec::new())
}

// ------------------------------------------------------------------ block hook

/// End of a block: drop finished requests older than the retention window (oldest first,
/// bounded work). Pending requests are never dropped.
pub(crate) fn end_block(kv: &mut impl KvStore, height: u64) -> Result<(), StateError> {
    if height <= RETENTION_BLOCKS {
        return Ok(());
    }
    let cutoff = height - RETENTION_BLOCKS;
    let start = req_key(KEY_REQ, 0);
    let end = vec![TAG_CROSS, KEY_REQ + 1];
    for (k, v) in kv.scan(&start, Some(&end), false, PRUNE_PER_BLOCK)? {
        let id = u64::from_be_bytes(
            k.get(2..10)
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| StateError("bad request key".into()))?,
        );
        let r = SignRequest::decode(id, &v)?;
        if r.is_done() && r.done_height < cutoff {
            kv.delete(&k)?;
        }
    }
    Ok(())
}
