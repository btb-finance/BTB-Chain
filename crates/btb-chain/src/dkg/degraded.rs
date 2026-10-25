//! Degraded mode: what happens when a failed reshare would leave the beacon key with too few
//! holders in the validator set (`docs/ORACLES.md`, "Losing the beacon committee").
//!
//! Every decision here is a pure function of chain state at the end of the DKG window, so every
//! node takes it at the same block; no timeout, vote or message decides it.
//!
//! * A reshare for epoch `e + 1` fails (too few qualified dealers). The old key `K` has `n`
//!   holders and threshold `t`. Let `r` be the number of holders of `K` that are validators of
//!   epoch `e + 1` (the set locked at the start of the window). If `r >= t` the key carries over
//!   as before: the beacon needs `t` signers and the validators among them are the ones the BFT
//!   assumption already requires to be online.
//! * If `r < t`, a beacon for epoch `e + 1` needs at least `t - r` signatures from validators that
//!   have left the set. They are not bound by anything the chain can check, so the chain does
//!   not wait for them: epoch `e + 1` gets **no key**. Blocks run without a beacon, `random_bytes`
//!   traps with `RandomnessUnavailable` (the same state as the genesis DKG window), and the
//!   [`Degraded`] record `50 34` is written.
//! * At the first block of epoch `e + 1` a **fresh** key generation (not a reshare) for `e + 1`
//!   opens among its validators, with the normal window. When it succeeds the new key is the key
//!   of `e + 1` from then on, the degraded record is deleted and a [`KeyChange`] is stored under
//!   `50 32 epoch`. If it fails, the next epoch tail retries with a fresh generation.
//!
//! No tokens move: nothing here touches balances, so the supply is unchanged.

use super::{
    delete_prefix, epoch_key, key_of, put_epoch_key, session, EpochKey, Session, KEY_DEAL,
    KEY_SESSION, STATUS_FAILED,
};
use crate::state::StateError;
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_oracle::GroupKey;
use btb_runtime::kv::KvStore;
use btb_runtime::layout::TAG_ORACLE;

/// `50 32 epoch:u64be`: [`KeyChange`] for the key of `epoch`.
pub const KEY_CHANGE: u8 = 0x32;
/// `50 34`: the [`Degraded`] record, present while the chain is in degraded mode.
pub const KEY_DEGRADED: u8 = 0x34;

/// The chain is in degraded mode since `height`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Degraded {
    /// First epoch that has no beacon key.
    pub epoch: u64,
    /// Block at which the failed reshare was finalized.
    pub height: u64,
    /// Holders of the abandoned key that are validators of `epoch`.
    pub reachable: u32,
    /// Threshold of the abandoned key.
    pub threshold: u32,
    /// Compressed group public key that was abandoned.
    pub old_group: Vec<u8>,
}

/// A change of the group key (the result of leaving degraded mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyChange {
    /// Epoch from which the new key signs.
    pub epoch: u64,
    /// Block at which the fresh key generation finalized.
    pub height: u64,
    /// When degraded mode began (epoch and block).
    pub degraded_epoch: u64,
    pub degraded_height: u64,
    pub old_group: Vec<u8>,
    pub new_group: Vec<u8>,
}

impl Degraded {
    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.epoch.encode_to(&mut o);
        self.height.encode_to(&mut o);
        self.reachable.encode_to(&mut o);
        self.threshold.encode_to(&mut o);
        self.old_group.encode_to(&mut o);
        o
    }

    fn decode(b: &[u8]) -> Result<Degraded, DecodeError> {
        let mut r = Reader::new(b, 1 << 16);
        let d = Degraded {
            epoch: u64::decode_from(&mut r)?,
            height: u64::decode_from(&mut r)?,
            reachable: u32::decode_from(&mut r)?,
            threshold: u32::decode_from(&mut r)?,
            old_group: Vec::<u8>::decode_from(&mut r)?,
        };
        if r.remaining() != 0 {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(d)
    }
}

impl KeyChange {
    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.epoch.encode_to(&mut o);
        self.height.encode_to(&mut o);
        self.degraded_epoch.encode_to(&mut o);
        self.degraded_height.encode_to(&mut o);
        self.old_group.encode_to(&mut o);
        self.new_group.encode_to(&mut o);
        o
    }

    fn decode(b: &[u8]) -> Result<KeyChange, DecodeError> {
        let mut r = Reader::new(b, 1 << 16);
        let k = KeyChange {
            epoch: u64::decode_from(&mut r)?,
            height: u64::decode_from(&mut r)?,
            degraded_epoch: u64::decode_from(&mut r)?,
            degraded_height: u64::decode_from(&mut r)?,
            old_group: Vec::<u8>::decode_from(&mut r)?,
            new_group: Vec::<u8>::decode_from(&mut r)?,
        };
        if r.remaining() != 0 {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(k)
    }
}

fn degraded_key() -> Vec<u8> {
    vec![TAG_ORACLE, KEY_DEGRADED]
}

/// The degraded record, if the chain is in degraded mode.
pub fn degraded(kv: &impl KvStore) -> Result<Option<Degraded>, StateError> {
    kv.get(&degraded_key())?
        .map(|b| Degraded::decode(&b).map_err(StateError::from))
        .transpose()
}

pub fn key_change(kv: &impl KvStore, epoch: u64) -> Result<Option<KeyChange>, StateError> {
    kv.get(&key_of(KEY_CHANGE, epoch))?
        .map(|b| KeyChange::decode(&b).map_err(StateError::from))
        .transpose()
}

/// Every recorded key change, oldest first.
pub fn key_changes(kv: &impl KvStore) -> Result<Vec<KeyChange>, StateError> {
    let (start, end) = (key_of(KEY_CHANGE, 0), key_of(KEY_CHANGE, u64::MAX));
    kv.scan(&start, Some(&end), false, 4096)?
        .into_iter()
        .map(|(_, v)| KeyChange::decode(&v).map_err(StateError::from))
        .collect()
}

/// A reshare for `s` failed and `prev` is the key of the epoch before. Carry it over, or enter
/// degraded mode (returns `true`) when too few of its holders are validators of the new epoch.
pub(super) fn after_failed_reshare(
    kv: &mut impl KvStore,
    s: &Session,
    prev: &EpochKey,
    height: u64,
) -> Result<bool, StateError> {
    let reachable = prev
        .holders
        .iter()
        .filter(|h| s.validators.iter().any(|v| v.consensus_key == **h))
        .count();
    if reachable >= prev.group.threshold {
        put_epoch_key(kv, s.target, prev)?;
        return Ok(false);
    }
    if degraded(&*kv)?.is_none() {
        let d = Degraded {
            epoch: s.target,
            height,
            reachable: reachable as u32,
            threshold: prev.group.threshold as u32,
            old_group: prev.group.public_key.to_bytes().to_vec(),
        };
        kv.put(&degraded_key(), &d.encode())?;
    }
    Ok(true)
}

/// A fresh key for `target` was generated. If the chain was degraded this ends it and records
/// the change of group key.
pub(super) fn after_fresh_key(
    kv: &mut impl KvStore,
    target: u64,
    height: u64,
    group: &GroupKey,
) -> Result<(), StateError> {
    let Some(d) = degraded(&*kv)? else {
        return Ok(());
    };
    let c = KeyChange {
        epoch: target,
        height,
        degraded_epoch: d.epoch,
        degraded_height: d.height,
        old_group: d.old_group,
        new_group: group.public_key.to_bytes().to_vec(),
    };
    kv.put(&key_of(KEY_CHANGE, target), &c.encode())?;
    kv.delete(&degraded_key())?;
    Ok(())
}

/// First block of `epoch`: the epoch in which degraded mode began has no key, so a fresh
/// generation is due. The failed reshare board for the same target (kept until the epoch
/// switch, which reads its locked set) is replaced.
pub(super) fn recovery_due(kv: &mut impl KvStore, epoch: u64) -> Result<bool, StateError> {
    let due = degraded(&*kv)?.is_some_and(|d| d.epoch == epoch)
        && epoch_key(&*kv, epoch)?.is_none()
        && session(&*kv, epoch)?.is_some_and(|s| s.status == STATUS_FAILED);
    if due {
        kv.delete(&key_of(KEY_SESSION, epoch))?;
        delete_prefix(kv, key_of(KEY_DEAL, epoch), key_of(KEY_DEAL, epoch + 1))?;
    }
    Ok(due)
}
