//! Chain side of the oracles: the randomness beacon and the price feeds (`docs/ORACLES.md`).
//!
//! # State (tag `0x50`, shared with `btb_runtime::layout`)
//!
//! | Key | Value |
//! | --- | --- |
//! | `50 01` | the oracle config (`OracleGenesis`): beacon group key, holders, feeds |
//! | `50 02` | `height:u64 LE \|\| seed[32]`: the seed of the block being executed (host call input) |
//! | `50 03` | `epoch:u64 LE \|\| height:u64 LE \|\| signature[96]`: the last block's full beacon |
//! | `50 10 pair:u32be` | the agreed price: `price:u128 LE \|\| height:u64 LE \|\| decimals:u8 \|\| reporters:u32 LE` |
//! | `50 20 pair:u32be key[32]` | a validator's latest observation: `price:u128 LE \|\| height:u64 LE` |
//!
//! # Block input
//!
//! `Block::oracle` carries what the block needs beyond transactions: the beacon signature for
//! its height (verified here against the group key, so a node cannot be fed a wrong or stale
//! one) and the validators' signed price observations (verified here, one by one; an invalid or
//! stale one is skipped, never an error, so a Byzantine proposer cannot make a block unexecutable
//! with junk).
//!
//! * **Beacon.** `begin_block` stores the seed derived from the signature; contracts reach it
//!   only through `random_bytes`, which the VM allows to marked mutations and crons alone.
//! * **Prices.** `end_block` (after the transactions, so a transaction sees the previous block's
//!   prices) stores each accepted observation and, for every pair that got news, recomputes the
//!   stake-weighted median over the current epoch's validators' fresh observations, if at least
//!   two thirds of the stake reported.

use crate::block::Block;
use crate::staking;
use crate::state::StateError;
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_oracle::price::weighted_median;
use btb_oracle::{Beacon, GroupKey, SignedObservation};
use btb_runtime::kv::KvStore;
use btb_runtime::layout::{oracle_price_key, oracle_seed_key, TAG_ORACLE};
use std::collections::BTreeMap;

pub const KEY_CONFIG: u8 = 0x01;
pub const KEY_BEACON: u8 = 0x03;
const TAG_OBS: u8 = 0x20;

/// Most feeds a chain can configure.
pub const MAX_PAIRS: usize = 64;
pub const MAX_PAIR_NAME: usize = 32;
/// Most observations a block may carry (extra ones are ignored; `payload_ok` rejects them).
pub const MAX_OBS_PER_BLOCK: usize = 1024;
pub const MAX_AGE_LIMIT: u64 = 100_000;

/// One configured price feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairConfig {
    pub id: u32,
    /// For humans and RPC, for example `BTC/USD`.
    pub name: String,
    /// Decimals of the integer price.
    pub decimals: u8,
}

/// The oracle section of the genesis: who holds the beacon key, which feeds exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleGenesis {
    /// `GroupKey::encode()` of the beacon key.
    pub group_key: Vec<u8>,
    /// Consensus key of beacon holder `i` (its share's x-coordinate is `i + 1`).
    pub holders: Vec<[u8; 32]>,
    pub pairs: Vec<PairConfig>,
    /// An observation older than this many blocks is ignored, and so is a price built from
    /// older ones.
    pub max_age_blocks: u64,
}

impl Encode for PairConfig {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.id.encode_to(out);
        self.name.encode_to(out);
        self.decimals.encode_to(out);
    }
}
impl Decode for PairConfig {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(PairConfig {
            id: u32::decode_from(r)?,
            name: String::decode_from(r)?,
            decimals: u8::decode_from(r)?,
        })
    }
}

impl Encode for OracleGenesis {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.group_key.encode_to(out);
        self.holders.encode_to(out);
        self.pairs.encode_to(out);
        self.max_age_blocks.encode_to(out);
    }
}
impl Decode for OracleGenesis {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(OracleGenesis {
            group_key: Vec::decode_from(r)?,
            holders: Vec::decode_from(r)?,
            pairs: Vec::decode_from(r)?,
            max_age_blocks: u64::decode_from(r)?,
        })
    }
}

impl OracleGenesis {
    /// Check everything that does not need chain state; returns the parsed group key.
    pub fn validate(&self) -> Result<GroupKey, String> {
        let group = GroupKey::decode(&self.group_key).map_err(|e| format!("beacon key: {e:?}"))?;
        if self.holders.len() != group.n() {
            return Err("holders and public shares differ in number".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        if !self.holders.iter().all(|h| seen.insert(*h)) {
            return Err("a beacon holder is listed twice".into());
        }
        if self.pairs.len() > MAX_PAIRS {
            return Err("too many price pairs".into());
        }
        let (mut ids, mut names) = (
            std::collections::BTreeSet::new(),
            std::collections::BTreeSet::new(),
        );
        for p in &self.pairs {
            if p.name.is_empty() || p.name.len() > MAX_PAIR_NAME || p.decimals > 36 {
                return Err(format!("bad price pair {:?}", p.name));
            }
            if !ids.insert(p.id) || !names.insert(p.name.clone()) {
                return Err(format!("price pair {:?} listed twice", p.name));
            }
        }
        if !(1..=MAX_AGE_LIMIT).contains(&self.max_age_blocks) {
            return Err(format!("max_age_blocks must be 1..={MAX_AGE_LIMIT}"));
        }
        Ok(group)
    }

    /// The index of `key` among the beacon holders.
    pub fn holder_index(&self, key: &[u8; 32]) -> Option<usize> {
        self.holders.iter().position(|h| h == key)
    }
}

pub(crate) fn config_key() -> Vec<u8> {
    vec![TAG_ORACLE, KEY_CONFIG]
}

fn beacon_key() -> Vec<u8> {
    vec![TAG_ORACLE, KEY_BEACON]
}

fn obs_key(pair: u32, key: &[u8; 32]) -> Vec<u8> {
    let mut k = vec![TAG_ORACLE, TAG_OBS];
    k.extend_from_slice(&pair.to_be_bytes());
    k.extend_from_slice(key);
    k
}

fn obs_prefix(pair: u32) -> (Vec<u8>, Vec<u8>) {
    let mut a = vec![TAG_ORACLE, TAG_OBS];
    a.extend_from_slice(&pair.to_be_bytes());
    let mut b = a.clone();
    // the next pair id's prefix is the exclusive end
    let n = pair.checked_add(1);
    match n {
        Some(n) => {
            b.truncate(2);
            b.extend_from_slice(&n.to_be_bytes());
        }
        None => b = vec![TAG_ORACLE, TAG_OBS + 1],
    }
    (a, b)
}

/// What a block brings to the oracles besides its transactions.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OracleInput {
    /// The beacon signature for this block's height (required when the chain has an oracle
    /// config, forbidden otherwise).
    pub beacon: Option<Beacon>,
    pub observations: Vec<SignedObservation>,
}

impl OracleInput {
    pub fn is_empty(&self) -> bool {
        self.beacon.is_none() && self.observations.is_empty()
    }

    pub fn encode_to(&self, out: &mut Vec<u8>) {
        match &self.beacon {
            Some(b) => {
                1u8.encode_to(out);
                out.extend_from_slice(&b.encode());
            }
            None => 0u8.encode_to(out),
        }
        (self.observations.len() as u32).encode_to(out);
        for o in &self.observations {
            out.extend_from_slice(&o.encode());
        }
    }

    pub fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let beacon = match u8::decode_from(r)? {
            0 => None,
            1 => {
                let raw = r.take(16 + 96)?;
                Some(Beacon::decode(raw).ok_or(DecodeError::TrailingBytes)?)
            }
            _ => return Err(DecodeError::TrailingBytes),
        };
        let n = u32::decode_from(r)? as usize;
        if n > MAX_OBS_PER_BLOCK {
            return Err(DecodeError::TrailingBytes);
        }
        let mut observations = Vec::with_capacity(n);
        for _ in 0..n {
            let raw = r.take(btb_oracle::price::OBS_BYTES)?;
            observations.push(SignedObservation::decode(raw).ok_or(DecodeError::TrailingBytes)?);
        }
        Ok(OracleInput {
            beacon,
            observations,
        })
    }
}

/// A price as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceEntry {
    pub price: u128,
    pub height: u64,
    pub decimals: u8,
    pub reporters: u32,
}

impl PriceEntry {
    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(29);
        o.extend_from_slice(&self.price.to_le_bytes());
        o.extend_from_slice(&self.height.to_le_bytes());
        o.push(self.decimals);
        o.extend_from_slice(&self.reporters.to_le_bytes());
        o
    }

    fn decode(b: &[u8]) -> Option<PriceEntry> {
        if b.len() != 29 {
            return None;
        }
        Some(PriceEntry {
            price: u128::from_le_bytes(b[..16].try_into().ok()?),
            height: u64::from_le_bytes(b[16..24].try_into().ok()?),
            decimals: b[24],
            reporters: u32::from_le_bytes(b[25..29].try_into().ok()?),
        })
    }
}

/// The oracle config and the parsed group key of a chain, if it has one.
pub struct Loaded {
    pub genesis: OracleGenesis,
    pub group: GroupKey,
}

pub fn load(kv: &impl KvStore) -> Result<Option<Loaded>, StateError> {
    let Some(b) = kv.get(&config_key())? else {
        return Ok(None);
    };
    let genesis = OracleGenesis::decode(&b)?;
    // The key was fully validated when it entered the genesis state.
    let group = GroupKey::decode_unchecked(&genesis.group_key)
        .map_err(|e| StateError(format!("stored beacon key invalid: {e:?}")))?;
    Ok(Some(Loaded { genesis, group }))
}

/// Genesis writes: the config under `50 01`.
pub(crate) fn genesis_ops(
    og: &OracleGenesis,
    batch: &mut btb_runtime::WriteBatch,
) -> Result<(), String> {
    og.validate()?;
    batch.put(config_key(), og.encode());
    Ok(())
}

/// Start of a block: verify the beacon and store its seed.
pub(crate) fn begin_block(
    kv: &mut impl KvStore,
    chain_id: u64,
    epoch_len: u64,
    height: u64,
    input: &OracleInput,
) -> Result<(), String> {
    let loaded = load(&*kv).map_err(|e| e.0)?;
    let Some(loaded) = loaded else {
        if input.is_empty() {
            return Ok(());
        }
        return Err("block carries oracle input but the chain has no oracle config".into());
    };
    let Some(beacon) = &input.beacon else {
        return Err("block has no beacon".into());
    };
    let epoch = height.saturating_sub(1) / epoch_len;
    if beacon.height != height || beacon.epoch != epoch {
        return Err("beacon is for another height or epoch".into());
    }
    if !beacon.verify(chain_id, &loaded.group) {
        return Err("beacon signature invalid".into());
    }
    let mut seed_rec = height.to_le_bytes().to_vec();
    seed_rec.extend_from_slice(&beacon.seed());
    kv.put(&oracle_seed_key(), &seed_rec).map_err(|e| e.0)?;
    let mut full = beacon.epoch.to_le_bytes().to_vec();
    full.extend_from_slice(&beacon.height.to_le_bytes());
    full.extend_from_slice(&beacon.signature);
    kv.put(&beacon_key(), &full).map_err(|e| e.0)?;
    Ok(())
}

/// End of a block (after transactions and crons): take in the observations and update prices.
pub(crate) fn end_block(
    kv: &mut impl KvStore,
    chain_id: u64,
    epoch_len: u64,
    height: u64,
    obs: &[SignedObservation],
) -> Result<(), StateError> {
    let Some(loaded) = load(&*kv)? else {
        return Ok(());
    };
    let og = &loaded.genesis;
    let epoch = height.saturating_sub(1) / epoch_len;
    let Some(rec) = staking::epoch_record(&*kv, epoch)? else {
        return Ok(());
    };
    let stakes: BTreeMap<[u8; 32], u64> = rec
        .validators
        .iter()
        .map(|v| (v.consensus_key, v.stake))
        .collect();
    let total: u128 = stakes.values().map(|s| u128::from(*s)).sum();
    let mut dirty = std::collections::BTreeSet::new();
    for o in obs.iter().take(MAX_OBS_PER_BLOCK) {
        let ob = &o.obs;
        let Some(pair) = og.pairs.iter().find(|p| p.id == ob.pair) else {
            continue;
        };
        if !stakes.contains_key(&o.key)
            || ob.height > height
            || height - ob.height > og.max_age_blocks
            || !o.verify(chain_id)
        {
            continue;
        }
        let k = obs_key(pair.id, &o.key);
        if let Some(prev) = kv.get(&k)? {
            let prev_h = prev
                .get(16..24)
                .and_then(|b| <[u8; 8]>::try_from(b).ok())
                .map_or(0, u64::from_le_bytes);
            if ob.height <= prev_h {
                continue; // a replay or an older statement
            }
        }
        let mut v = ob.price.to_le_bytes().to_vec();
        v.extend_from_slice(&ob.height.to_le_bytes());
        kv.put(&k, &v)?;
        dirty.insert(pair.id);
    }
    for id in dirty {
        let Some(pair) = og.pairs.iter().find(|p| p.id == id) else {
            continue;
        };
        let (start, end) = obs_prefix(id);
        let mut items: Vec<(u128, u64)> = Vec::new();
        let mut reporting: u128 = 0;
        for (k, v) in kv.scan(&start, Some(&end), false, 4096)? {
            let key: Option<[u8; 32]> = k.get(6..38).and_then(|b| b.try_into().ok());
            let (Some(key), true) = (key, v.len() == 24) else {
                continue;
            };
            let h = u64::from_le_bytes(v[16..24].try_into().expect("8 bytes"));
            match stakes.get(&key) {
                Some(stake) if height.saturating_sub(h) <= og.max_age_blocks => {
                    items.push((
                        u128::from_le_bytes(v[..16].try_into().expect("16 bytes")),
                        *stake,
                    ));
                    reporting += u128::from(*stake);
                }
                // stale or no longer a validator: it can never count again
                _ => kv.delete(&k)?,
            }
        }
        // At least two thirds of the stake must have reported fresh observations. Then the
        // honest reporters hold more than half of the reporting weight whenever less than a
        // third of the stake is faulty, so the median is an honest value or between two.
        if total == 0 || reporting * 3 < total * 2 {
            continue;
        }
        if let Some(price) = weighted_median(&items) {
            let e = PriceEntry {
                price,
                height,
                decimals: pair.decimals,
                reporters: items.len() as u32,
            };
            kv.put(&oracle_price_key(id), &e.encode())?;
        }
    }
    Ok(())
}

/// The agreed price of `pair`, if any.
pub fn price(kv: &impl KvStore, pair: u32) -> Result<Option<PriceEntry>, StateError> {
    Ok(kv
        .get(&oracle_price_key(pair))?
        .and_then(|b| PriceEntry::decode(&b)))
}

/// The last block's beacon as stored in state.
pub fn last_beacon(kv: &impl KvStore) -> Result<Option<Beacon>, StateError> {
    Ok(kv.get(&beacon_key())?.and_then(|b| Beacon::decode(&b)))
}

/// Cheap checks a replica can run on a block's oracle input before voting: sizes only (the
/// executor verifies signatures when it executes, and skips bad observations).
pub fn input_ok(input: &OracleInput) -> bool {
    input.observations.len() <= MAX_OBS_PER_BLOCK
}

/// Attach `input` to a block.
pub fn with_input(mut b: Block, input: OracleInput) -> Block {
    b.oracle = input;
    b
}
