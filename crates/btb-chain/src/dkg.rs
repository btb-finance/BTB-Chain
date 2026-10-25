//! On-chain distributed key generation for the randomness beacon (`docs/ORACLES.md`).
//!
//! There is no trusted dealer. The validators of each epoch generate the threshold key that
//! signs that epoch's beacons, with the joint-Feldman protocol of `btb_oracle::dkg`, using the
//! chain itself as the public broadcast board. Everything is a transaction, so the transcript
//! is deterministic, replayable and auditable by anyone.
//!
//! # Schedule
//!
//! With window `W` (`OracleGenesis::dkg_window`, `6 <= W`, `2W <= epoch_len`):
//!
//! * **Epoch change.** In the tail of epoch `e`, heights `ws..=we` with `we = (e + 1) * E` and
//!   `ws = we - W + 1`, the committee of epoch `e + 1` is generated. At the first block `ws` the
//!   chain *locks* the next validator set (`staking::select_set`) into a [`Session`]; the
//!   staking module then uses exactly that set for epoch `e + 1`, so the beacon committee is
//!   the validator set (stake changes in the last `W` blocks take effect one epoch later). If
//!   the chain holds the key of epoch `e` the session *reshares* it (the group public key does
//!   not change); otherwise it deals a fresh key.
//! * **Genesis.** A chain whose `OracleGenesis` carries no `group_key` runs a fresh session for
//!   epoch 0 in heights `1..=W`. Blocks in that window need no beacon, and `random_bytes`
//!   traps with `RandomnessUnavailable` there. With a `group_key` (the interim trusted dealer,
//!   testnets only) the key is simply epoch 0's.
//! * **Deal** (`ws + 1..=ws + W/2`; the session exists from block `ws` on, nodes see it after
//!   executing that block). Each dealer posts its Feldman commitments and one
//!   ciphertext per recipient (`btb_oracle::dkg_crypto`). Dealers are the new committee
//!   (fresh) or the holders of the old key (reshare).
//! * **Complain** (from the deal until `we`). A recipient whose share is missing, undecryptable
//!   or inconsistent with the commitments posts the shared Diffie-Hellman point with a
//!   proof. The chain decrypts the same ciphertext itself: if the share is bad the dealer is
//!   disqualified at once, if it is good the complaint reverts (the accuser pays the fee). No
//!   justification round is needed because the complaint is publicly decidable.
//! * **Finalize** (end of block `we`). Qualified dealers are those that dealt and were not
//!   upheld against. If enough qualified (`t` for a fresh key, the old threshold for a reshare)
//!   the group key and every public share are derived from the board and stored as the key of
//!   epoch `e + 1`. Otherwise the previous key carries over unchanged (same holders): liveness
//!   then depends on those holders, see `docs/ORACLES.md`.
//!
//! # State (all under tag `0x50`)
//!
//! | Key | Value |
//! | --- | --- |
//! | `50 30 epoch:u64be` | [`EpochKey`]: origin epoch, group key, holders; signs the beacons of `epoch` |
//! | `50 40 target:u64be` | [`Session`] of the key generation for epoch `target` |
//! | `50 41 target:u64be dealer:u32be` | [`Deal`] |
//!
//! Sessions and deals older than the previous key generation are pruned at each finalization;
//! epoch keys are small and kept.

use crate::staking::{self, ActiveValidator};
use crate::state::StateError;
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_oracle::bls::{G1, G1_BYTES};
use btb_oracle::dkg::verify_share;
use btb_oracle::dkg_crypto::{self, Reveal, ShareCtx, CT_BYTES, PROOF_BYTES};
use btb_oracle::{GroupKey, Mode, Outcome, Params, Transcript};
use btb_runtime::kv::KvStore;
use btb_runtime::layout::TAG_ORACLE;

pub const KEY_EPOCH_KEY: u8 = 0x30;
pub const KEY_SESSION: u8 = 0x40;
pub const KEY_DEAL: u8 = 0x41;

/// Smallest DKG window (a session is visible from the block after it opens, so three blocks
/// to deal and three to complain).
pub const MIN_WINDOW: u64 = 6;
/// Most dealers or recipients in one key generation (`btb_oracle::threshold::MAX_HOLDERS`).
pub const MAX_PARTIES: usize = btb_oracle::threshold::MAX_HOLDERS;

pub const DEAL_BASE_GAS: u64 = 60_000;
pub const DEAL_PER_ITEM_GAS: u64 = 1_500;
/// A complaint verifies a Chaum-Pedersen proof, decrypts and checks one share.
pub const COMPLAIN_GAS: u64 = 250_000;

pub const STATUS_OPEN: u8 = 0;
pub const STATUS_DONE: u8 = 1;
/// Too few qualified dealers: the previous key (if any) carried over.
pub const STATUS_FAILED: u8 = 2;

// ------------------------------------------------------------------ records

/// The threshold key that signs the beacons of one epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochKey {
    /// The epoch whose key generation produced this key (`< epoch` after a carry-over).
    pub origin: u64,
    pub group: GroupKey,
    /// Consensus key of holder `i` (its share's x-coordinate is `i + 1`).
    pub holders: Vec<[u8; 32]>,
}

impl EpochKey {
    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.origin.encode_to(&mut o);
        self.group.encode().encode_to(&mut o);
        self.holders.encode_to(&mut o);
        o
    }

    fn decode(b: &[u8]) -> Result<EpochKey, StateError> {
        let mut r = Reader::new(b, 1 << 20);
        let origin = u64::decode_from(&mut r)?;
        let raw = Vec::<u8>::decode_from(&mut r)?;
        let holders = Vec::<[u8; 32]>::decode_from(&mut r)?;
        if r.remaining() != 0 {
            return Err(DecodeError::TrailingBytes.into());
        }
        let group = GroupKey::decode_unchecked(&raw)
            .map_err(|e| StateError(format!("stored beacon key invalid: {e:?}")))?;
        Ok(EpochKey {
            origin,
            group,
            holders,
        })
    }
}

/// One key generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The epoch the generated key will sign for.
    pub target: u64,
    /// `Some(e)`: reshare the key of epoch `e`; `None`: a fresh key.
    pub from: Option<u64>,
    pub window_start: u64,
    pub window_end: u64,
    pub deal_end: u64,
    /// The new committee: the validator set of epoch `target`, in consensus order.
    pub validators: Vec<ActiveValidator>,
    /// Who deals: the committee (fresh) or the holders of the old key (reshare).
    pub dealers: Vec<[u8; 32]>,
    pub status: u8,
}

impl Session {
    pub fn committee(&self) -> Vec<[u8; 32]> {
        self.validators.iter().map(|v| v.consensus_key).collect()
    }

    pub fn params(&self) -> Params {
        Params::standard(self.validators.len())
    }

    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.target.encode_to(&mut o);
        self.from.encode_to(&mut o);
        self.window_start.encode_to(&mut o);
        self.window_end.encode_to(&mut o);
        self.deal_end.encode_to(&mut o);
        (self.validators.len() as u32).encode_to(&mut o);
        for v in &self.validators {
            v.operator.encode_to(&mut o);
            v.consensus_key.encode_to(&mut o);
            v.stake.encode_to(&mut o);
        }
        self.dealers.encode_to(&mut o);
        self.status.encode_to(&mut o);
        o
    }

    fn decode(b: &[u8]) -> Result<Session, DecodeError> {
        let mut r = Reader::new(b, 1 << 22);
        let target = u64::decode_from(&mut r)?;
        let from = Option::<u64>::decode_from(&mut r)?;
        let window_start = u64::decode_from(&mut r)?;
        let window_end = u64::decode_from(&mut r)?;
        let deal_end = u64::decode_from(&mut r)?;
        let n = u32::decode_from(&mut r)? as usize;
        if n > MAX_PARTIES {
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
        let dealers = Vec::<[u8; 32]>::decode_from(&mut r)?;
        let status = u8::decode_from(&mut r)?;
        if r.remaining() != 0 {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(Session {
            target,
            from,
            window_start,
            window_end,
            deal_end,
            validators,
            dealers,
            status,
        })
    }
}

/// A dealer's posting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deal {
    pub eph: [u8; 32],
    pub commitments: Vec<[u8; G1_BYTES]>,
    /// One per member of the new committee.
    pub ciphertexts: Vec<[u8; CT_BYTES]>,
    /// A complaint against it was upheld.
    pub disqualified: bool,
}

impl Deal {
    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.eph.encode_to(&mut o);
        self.commitments.encode_to(&mut o);
        self.ciphertexts.encode_to(&mut o);
        self.disqualified.encode_to(&mut o);
        o
    }

    fn decode(b: &[u8]) -> Result<Deal, DecodeError> {
        let mut r = Reader::new(b, 1 << 22);
        let d = Deal {
            eph: <[u8; 32]>::decode_from(&mut r)?,
            commitments: Vec::decode_from(&mut r)?,
            ciphertexts: Vec::decode_from(&mut r)?,
            disqualified: bool::decode_from(&mut r)?,
        };
        if r.remaining() != 0 {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(d)
    }

    /// The commitments as points (they were checked when the deal was accepted).
    pub fn points(&self) -> Option<Vec<G1>> {
        self.commitments.iter().map(G1::from_bytes).collect()
    }
}

// ------------------------------------------------------------------ transactions

/// A DKG transaction (transaction tag 14). The sender's key must be the dealer's or the
/// recipient's consensus key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DkgOp {
    /// Post commitments and encrypted shares for the key generation of epoch `target`.
    Deal {
        target: u64,
        eph: [u8; 32],
        commitments: Vec<[u8; G1_BYTES]>,
        ciphertexts: Vec<[u8; CT_BYTES]>,
    },
    /// "The share dealer `dealer` encrypted for me is wrong": the shared point and its proof.
    Complain {
        target: u64,
        dealer: u32,
        shared: [u8; 32],
        proof: [u8; PROOF_BYTES],
    },
}

/// Why a DKG transaction was refused (the fee is still charged, nothing changed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DkgError {
    /// No key generation for that epoch (or it is already finished).
    NoSession,
    /// Outside the deal or complaint window.
    WrongPhase,
    /// The sender is not a dealer of this session.
    NotDealer,
    /// The sender is not a member of the new committee.
    NotMember,
    AlreadyDealt,
    /// Wrong number of commitments or ciphertexts.
    BadShape,
    /// A commitment is not a valid point, or (reshare) the constant term is not the dealer's
    /// public share.
    BadCommitments,
    BadEphemeral,
    NoSuchDeal,
    AlreadyDisqualified,
    /// The decryption proof does not verify.
    BadProof,
    /// The share was fine: the complaint is false.
    ComplaintRejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fail {
    Tx(DkgError),
    State(StateError),
}
impl From<DkgError> for Fail {
    fn from(e: DkgError) -> Self {
        Fail::Tx(e)
    }
}
impl From<StateError> for Fail {
    fn from(e: StateError) -> Self {
        Fail::State(e)
    }
}
impl From<btb_runtime::KvError> for Fail {
    fn from(e: btb_runtime::KvError) -> Self {
        Fail::State(e.into())
    }
}

impl DkgOp {
    pub fn gas(&self) -> u64 {
        match self {
            DkgOp::Deal {
                commitments,
                ciphertexts,
                ..
            } => {
                DEAL_BASE_GAS
                    + DEAL_PER_ITEM_GAS
                        * (commitments.len() + ciphertexts.len()).min(2 * MAX_PARTIES) as u64
            }
            DkgOp::Complain { .. } => COMPLAIN_GAS,
        }
    }

    pub(crate) fn encode_to(&self, out: &mut Vec<u8>) {
        match self {
            DkgOp::Deal {
                target,
                eph,
                commitments,
                ciphertexts,
            } => {
                0u8.encode_to(out);
                target.encode_to(out);
                eph.encode_to(out);
                commitments.encode_to(out);
                ciphertexts.encode_to(out);
            }
            DkgOp::Complain {
                target,
                dealer,
                shared,
                proof,
            } => {
                1u8.encode_to(out);
                target.encode_to(out);
                dealer.encode_to(out);
                shared.encode_to(out);
                proof.encode_to(out);
            }
        }
    }

    /// `Ok(Err(sub))` is an unknown sub-tag.
    pub(crate) fn decode_from(r: &mut Reader<'_>) -> Result<Result<Self, u8>, DecodeError> {
        Ok(Ok(match u8::decode_from(r)? {
            0 => DkgOp::Deal {
                target: u64::decode_from(r)?,
                eph: <[u8; 32]>::decode_from(r)?,
                commitments: Vec::decode_from(r)?,
                ciphertexts: Vec::decode_from(r)?,
            },
            1 => DkgOp::Complain {
                target: u64::decode_from(r)?,
                dealer: u32::decode_from(r)?,
                shared: <[u8; 32]>::decode_from(r)?,
                proof: <[u8; PROOF_BYTES]>::decode_from(r)?,
            },
            t => return Ok(Err(t)),
        }))
    }

    /// Checks that need no state.
    pub fn check_static(&self) -> Result<(), DkgError> {
        if let DkgOp::Deal {
            commitments,
            ciphertexts,
            ..
        } = self
        {
            if commitments.is_empty()
                || commitments.len() > MAX_PARTIES
                || ciphertexts.is_empty()
                || ciphertexts.len() > MAX_PARTIES
            {
                return Err(DkgError::BadShape);
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ storage

fn key_of(sub: u8, target: u64) -> Vec<u8> {
    let mut k = vec![TAG_ORACLE, sub];
    k.extend_from_slice(&target.to_be_bytes());
    k
}

fn deal_key(target: u64, dealer: u32) -> Vec<u8> {
    let mut k = key_of(KEY_DEAL, target);
    k.extend_from_slice(&dealer.to_be_bytes());
    k
}

/// The key that signs the beacons of `epoch`, if there is one.
pub fn epoch_key(kv: &impl KvStore, epoch: u64) -> Result<Option<EpochKey>, StateError> {
    kv.get(&key_of(KEY_EPOCH_KEY, epoch))?
        .map(|b| EpochKey::decode(&b))
        .transpose()
}

fn put_epoch_key(kv: &mut impl KvStore, epoch: u64, k: &EpochKey) -> Result<(), StateError> {
    Ok(kv.put(&key_of(KEY_EPOCH_KEY, epoch), &k.encode())?)
}

pub fn session(kv: &impl KvStore, target: u64) -> Result<Option<Session>, StateError> {
    kv.get(&key_of(KEY_SESSION, target))?
        .map(|b| Session::decode(&b).map_err(StateError::from))
        .transpose()
}

fn put_session(kv: &mut impl KvStore, s: &Session) -> Result<(), StateError> {
    Ok(kv.put(&key_of(KEY_SESSION, s.target), &s.encode())?)
}

pub fn deal(kv: &impl KvStore, target: u64, dealer: u32) -> Result<Option<Deal>, StateError> {
    kv.get(&deal_key(target, dealer))?
        .map(|b| Deal::decode(&b).map_err(StateError::from))
        .transpose()
}

/// Every deal of a key generation, by dealer index.
pub fn deals(kv: &impl KvStore, target: u64) -> Result<Vec<(u32, Deal)>, StateError> {
    let start = key_of(KEY_DEAL, target);
    let end = key_of(KEY_DEAL, target.saturating_add(1));
    let mut out = Vec::new();
    for (k, v) in kv.scan(&start, Some(&end), false, MAX_PARTIES + 1)? {
        let idx = k
            .get(10..14)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(u32::from_be_bytes)
            .ok_or_else(|| StateError("bad deal key".into()))?;
        out.push((idx, Deal::decode(&v)?));
    }
    Ok(out)
}

/// Every session still on the board (the last two key generations), oldest first.
pub fn sessions(kv: &impl KvStore) -> Result<Vec<Session>, StateError> {
    let start = vec![TAG_ORACLE, KEY_SESSION];
    let end = vec![TAG_ORACLE, KEY_SESSION + 1];
    kv.scan(&start, Some(&end), false, 64)?
        .iter()
        .map(|(_, v)| Ok(Session::decode(v)?))
        .collect()
}

/// The next validator set as locked by the session for `target`, if there is one. Used by the
/// staking module so that the beacon committee is exactly the epoch's validator set.
pub fn locked_set(
    kv: &impl KvStore,
    target: u64,
) -> Result<Option<Vec<ActiveValidator>>, StateError> {
    Ok(session(kv, target)?.map(|s| s.validators))
}

fn delete_prefix(kv: &mut impl KvStore, start: Vec<u8>, end: Vec<u8>) -> Result<(), StateError> {
    loop {
        let page = kv.scan(&start, Some(&end), false, 256)?;
        if page.is_empty() {
            return Ok(());
        }
        for (k, _) in page {
            kv.delete(&k)?;
        }
    }
}

// ------------------------------------------------------------------ transcript

/// Rebuild the public transcript of `s` from the board: the deals not disqualified.
pub fn transcript(kv: &impl KvStore, s: &Session) -> Result<Transcript, StateError> {
    let mode = match s.from {
        None => Mode::Fresh,
        Some(from) => {
            let old = epoch_key(kv, from)?
                .ok_or_else(|| StateError(format!("reshare source epoch {from} has no key")))?;
            Mode::Reshare { old: old.group }
        }
    };
    let mut t = Transcript::new(mode, s.params())
        .map_err(|e| StateError(format!("dkg session {}: {e:?}", s.target)))?;
    for (dealer, d) in deals(kv, s.target)? {
        if d.disqualified {
            continue;
        }
        if let Some(points) = d.points() {
            // Entries were validated when posted; a failure here would only drop the dealer.
            let _ = t.add_commitments(dealer as usize, points);
        }
    }
    Ok(t)
}

/// The transcript and its outcome (`None`: not enough qualified dealers).
pub fn outcome(
    kv: &impl KvStore,
    s: &Session,
) -> Result<Option<(Transcript, Outcome)>, StateError> {
    let t = transcript(kv, s)?;
    Ok(t.finalize().ok().map(|o| (t, o)))
}

// ------------------------------------------------------------------ transactions

fn in_window(s: &Session, height: u64, deal_phase: bool) -> bool {
    s.status == STATUS_OPEN
        && height >= s.window_start
        && height <= if deal_phase { s.deal_end } else { s.window_end }
}

/// Run a DKG transaction sent by the holder of consensus key `sender_key`.
pub(crate) fn apply(
    kv: &mut impl KvStore,
    chain_id: u64,
    height: u64,
    sender_key: &[u8; 32],
    op: &DkgOp,
) -> Result<Vec<u8>, Fail> {
    op.check_static()?;
    match op {
        DkgOp::Deal {
            target,
            eph,
            commitments,
            ciphertexts,
        } => {
            let s = session(kv, *target)?.ok_or(DkgError::NoSession)?;
            if s.status != STATUS_OPEN {
                return Err(DkgError::NoSession.into());
            }
            if !in_window(&s, height, true) {
                return Err(DkgError::WrongPhase.into());
            }
            let idx = s
                .dealers
                .iter()
                .position(|k| k == sender_key)
                .ok_or(DkgError::NotDealer)? as u32;
            if deal(kv, *target, idx)?.is_some() {
                return Err(DkgError::AlreadyDealt.into());
            }
            let params = s.params();
            if commitments.len() != params.t || ciphertexts.len() != params.n {
                return Err(DkgError::BadShape.into());
            }
            if !dkg_crypto::valid_point(eph) {
                return Err(DkgError::BadEphemeral.into());
            }
            let points: Option<Vec<G1>> = commitments.iter().map(G1::from_bytes).collect();
            let points = points.ok_or(DkgError::BadCommitments)?;
            if let Some(from) = s.from {
                let old = epoch_key(kv, from)?
                    .ok_or_else(|| StateError(format!("epoch {from} has no key")))?;
                if old.group.public_shares.get(idx as usize) != points.first() {
                    return Err(DkgError::BadCommitments.into());
                }
            }
            let d = Deal {
                eph: *eph,
                commitments: commitments.clone(),
                ciphertexts: ciphertexts.clone(),
                disqualified: false,
            };
            kv.put(&deal_key(*target, idx), &d.encode())?;
            Ok(Vec::new())
        }
        DkgOp::Complain {
            target,
            dealer,
            shared,
            proof,
        } => {
            let s = session(kv, *target)?.ok_or(DkgError::NoSession)?;
            if s.status != STATUS_OPEN {
                return Err(DkgError::NoSession.into());
            }
            if !in_window(&s, height, false) {
                return Err(DkgError::WrongPhase.into());
            }
            let committee = s.committee();
            let j = committee
                .iter()
                .position(|k| k == sender_key)
                .ok_or(DkgError::NotMember)?;
            let mut d = deal(kv, *target, *dealer)?.ok_or(DkgError::NoSuchDeal)?;
            if d.disqualified {
                return Err(DkgError::AlreadyDisqualified.into());
            }
            let reveal = Reveal {
                shared: *shared,
                proof: *proof,
            };
            let opened =
                dkg_crypto::verify_reveal(sender_key, &d.eph, &reveal).ok_or(DkgError::BadProof)?;
            let ctx = ShareCtx {
                chain_id,
                target: *target,
                dealer: *dealer,
                recipient: j as u32,
            };
            let ct = d.ciphertexts.get(j).ok_or(DkgError::BadShape)?;
            if let Some(share) = opened.open(ct, &ctx) {
                let points = d
                    .points()
                    .ok_or_else(|| StateError("stored commitment invalid".into()))?;
                if verify_share(&points, j, &share) {
                    return Err(DkgError::ComplaintRejected.into());
                }
            }
            // undecryptable, not a scalar, or not on the committed polynomial: upheld
            d.disqualified = true;
            kv.put(&deal_key(*target, *dealer), &d.encode())?;
            Ok(vec![1])
        }
    }
}

// ------------------------------------------------------------------ block hooks

/// The window `ws..=we` of the key generation at the end of `epoch`.
fn tail(epoch_len: u64, window: u64, epoch: u64) -> (u64, u64) {
    let we = epoch.saturating_add(1).saturating_mul(epoch_len);
    (we.saturating_sub(window).saturating_add(1), we)
}

fn open_session(
    kv: &mut impl KvStore,
    target: u64,
    from: Option<u64>,
    window: (u64, u64),
    validators: Vec<ActiveValidator>,
) -> Result<(), StateError> {
    if validators.is_empty() || validators.len() > MAX_PARTIES {
        return Ok(()); // no committee can be formed; the previous key carries over
    }
    let dealers = match from {
        None => validators.iter().map(|v| v.consensus_key).collect(),
        Some(e) => {
            epoch_key(kv, e)?
                .ok_or_else(|| StateError(format!("epoch {e} has no key to reshare")))?
                .holders
        }
    };
    let span = window.1 - window.0 + 1;
    put_session(
        kv,
        &Session {
            target,
            from,
            window_start: window.0,
            window_end: window.1,
            deal_end: window.0 + span / 2,
            validators,
            dealers,
            status: STATUS_OPEN,
        },
    )
}

/// Start of a block: open the key generation whose window begins here.
pub(crate) fn begin_block(
    kv: &mut impl KvStore,
    cfg: &staking::StakingConfig,
    window: u64,
    height: u64,
) -> Result<(), StateError> {
    if window < MIN_WINDOW {
        return Ok(());
    }
    let epoch_len = cfg.epoch_len.max(1);
    let epoch = height.saturating_sub(1) / epoch_len;
    if height == 1 && epoch_key(&*kv, 0)?.is_none() {
        if let Some(rec) = staking::epoch_record(&*kv, 0)? {
            open_session(kv, 0, None, (1, window), rec.validators)?;
        }
    }
    let (ws, we) = tail(epoch_len, window, epoch);
    if height == ws && session(&*kv, epoch + 1)?.is_none() {
        let mut next = staking::select_set(&*kv, cfg)?;
        if next.is_empty() {
            next = staking::epoch_record(&*kv, epoch)?
                .map(|r| r.validators)
                .unwrap_or_default();
        }
        let from = epoch_key(&*kv, epoch)?.map(|_| epoch);
        open_session(kv, epoch + 1, from, (ws, we), next)?;
    }
    Ok(())
}

/// End of a block: finish the key generation whose window closes here.
pub(crate) fn end_block(
    kv: &mut impl KvStore,
    epoch_len: u64,
    height: u64,
) -> Result<(), StateError> {
    let epoch_len = epoch_len.max(1);
    let mut targets = Vec::new();
    if height.is_multiple_of(epoch_len) {
        targets.push(height / epoch_len); // the session for the next epoch
    }
    targets.push(0); // the genesis session
    for target in targets {
        let Some(mut s) = session(&*kv, target)? else {
            continue;
        };
        if s.status != STATUS_OPEN || s.window_end != height {
            continue;
        }
        finalize(kv, &mut s)?;
    }
    Ok(())
}

fn finalize(kv: &mut impl KvStore, s: &mut Session) -> Result<(), StateError> {
    match outcome(&*kv, s)? {
        Some((_, out)) => {
            put_epoch_key(
                kv,
                s.target,
                &EpochKey {
                    origin: s.target,
                    group: out.group,
                    holders: s.committee(),
                },
            )?;
            s.status = STATUS_DONE;
        }
        None => {
            // Too few qualified dealers: keep the key we had, with its holders.
            if let Some(prev) = s.target.checked_sub(1) {
                if let Some(k) = epoch_key(&*kv, prev)? {
                    put_epoch_key(kv, s.target, &k)?;
                }
            }
            s.status = STATUS_FAILED;
        }
    }
    put_session(kv, s)?;
    // Keep this and the previous generation; drop older boards.
    if let Some(old) = s.target.checked_sub(1) {
        delete_prefix(kv, key_of(KEY_SESSION, 0), key_of(KEY_SESSION, old))?;
        delete_prefix(kv, key_of(KEY_DEAL, 0), key_of(KEY_DEAL, old))?;
    }
    Ok(())
}

/// Genesis: the interim trusted-dealer key becomes the key of epoch 0.
pub(crate) fn genesis_key(
    group: &GroupKey,
    holders: &[[u8; 32]],
    batch: &mut btb_runtime::WriteBatch,
) {
    batch.put(
        key_of(KEY_EPOCH_KEY, 0),
        EpochKey {
            origin: 0,
            group: group.clone(),
            holders: holders.to_vec(),
        }
        .encode(),
    );
}
