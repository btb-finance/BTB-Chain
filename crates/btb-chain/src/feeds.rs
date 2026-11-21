//! Open data feeds (proposal 0005): anyone registers a feed, each validator decides for itself
//! which feeds it reports, and every read of a feed's value in a mutation or cron pays the
//! validators whose observations made that value.
//!
//! * **Register** (`FeedOp::Register`, transaction tag 26) locks [`FEED_DEPOSIT`] from the
//!   registrant (money tag `0x56`, counted by the supply invariant) and creates a feed with an
//!   id from [`FIRST_ID`] up, a unique name, decimals, a read fee (gas, at least
//!   [`MIN_READ_FEE`]) and a quorum. `Update` changes fee and quorum (owner only), `Close` deletes
//!   the feed and its value and returns the deposit.
//! * **Join / Leave**: a validator of the current epoch opts in or out, signing with its
//!   consensus key. Only joined validators' observations count for a registered feed (at most
//!   [`MAX_FEEDS_PER_VALIDATOR`] feeds each). Nobody has to report anything.
//! * **Quorum** of a registered feed: at least `min_reporters` joined validators of the current
//!   set with a fresh observation, holding at least `min_stake_bps` of the stake of all joined
//!   validators of the set. Then the stake-weighted median is the candidate (`oracle::end_block`,
//!   same update rule, breaker and TWAP as the genesis pairs). Genesis pairs keep their rule:
//!   every validator counts, two thirds of the total stake must report.
//! * **Contributors**: when a value is published, the operators and stakes of the reporters in
//!   the median's input are stored with it ([`contributors`]).
//! * **Fees**: a `price` read in a mutation or cron costs `200 + read_fee` gas (the feed's fee;
//!   [`btb_vm::gas::PRICE_READ_FEE`] for genesis pairs). Like all gas it is paid into the
//!   validator pool (minus the relay share of a relayed transaction); the receipt records the
//!   part of it that came from reads, per feed, and after the block's transactions [`pay_reporters`]
//!   moves exactly that part from the pool to the contributors of the value that was read, pro
//!   rata by stake. Nothing is minted: the units only move from the pool.
//!
//! | key | value |
//! | --- | --- |
//! | `50 60 id:u32be` | [`FeedRecord`] |
//! | `50 61 name` | id, `u32` LE |
//! | `50 62` | next id, `u32` LE |
//! | `50 63 id:u32be key[32]` | empty: the validator with consensus key `key` joined `id` |
//! | `50 64 key[32]` | number of feeds `key` joined, `u32` LE |
//! | `50 65 id:u32be` | contributors of the published value: `Vec<(operator, stake)>` |
//! | `56 id:u32be` | deposit, `u64` LE (money) |

use crate::staking;
use crate::state::{read_u64, StateError};
use crate::tx::Address;
use crate::UNITS_PER_BTB;
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_runtime::kv::{KvError, KvStore};
use btb_runtime::layout::{self, TAG_ORACLE};

pub const TAG_FEED_DEPOSIT: u8 = 0x56;
const K_FEED: u8 = 0x60;
const K_NAME: u8 = 0x61;
const K_NEXT: u8 = 0x62;
const K_JOIN: u8 = 0x63;
const K_JOINED: u8 = 0x64;
const K_CONTRIB: u8 = 0x65;

/// Registered feeds get ids from here up; genesis pairs must stay below.
pub const FIRST_ID: u32 = layout::FIRST_FEED_ID;
/// Locked while the feed exists.
pub const FEED_DEPOSIT: u64 = 1_000 * UNITS_PER_BTB;
/// Gas of every feed operation.
pub const FEED_GAS: u64 = 20_000;
/// Smallest and largest read fee, gas.
pub const MIN_READ_FEE: u64 = btb_vm::gas::PRICE_READ_FEE;
pub const MAX_READ_FEE: u64 = 10_000_000;
/// Quorum floor of a registered feed.
pub const MIN_REPORTERS: u32 = 3;
pub const MIN_STAKE_BPS: u32 = 1_000;
pub const MAX_FEEDS_PER_VALIDATOR: u32 = 256;
pub const MAX_NAME: usize = crate::oracle::MAX_PAIR_NAME;

/// A registered feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedRecord {
    pub name: String,
    pub decimals: u8,
    pub owner: Address,
    /// Gas charged per read in a mutation or cron, on top of the 200 gas read.
    pub read_fee: u64,
    pub min_reporters: u32,
    pub min_stake_bps: u32,
    /// Observations older than this many blocks do not count.
    pub max_age_blocks: u64,
}

impl Encode for FeedRecord {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.name.encode_to(out);
        self.decimals.encode_to(out);
        self.owner.encode_to(out);
        self.read_fee.encode_to(out);
        self.min_reporters.encode_to(out);
        self.min_stake_bps.encode_to(out);
        self.max_age_blocks.encode_to(out);
    }
}
impl Decode for FeedRecord {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(FeedRecord {
            name: String::decode_from(r)?,
            decimals: u8::decode_from(r)?,
            owner: Address::decode_from(r)?,
            read_fee: u64::decode_from(r)?,
            min_reporters: u32::decode_from(r)?,
            min_stake_bps: u32::decode_from(r)?,
            max_age_blocks: u64::decode_from(r)?,
        })
    }
}

/// Feed operations (transaction tag 26).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedOp {
    Register {
        name: String,
        decimals: u8,
        read_fee: u64,
        min_reporters: u32,
        min_stake_bps: u32,
        max_age_blocks: u64,
    },
    /// Owner only: new fee and quorum (not below the floor).
    Update {
        feed: u32,
        read_fee: u64,
        min_reporters: u32,
        min_stake_bps: u32,
        max_age_blocks: u64,
    },
    /// Owner only: delete the feed, its value and its joins; the deposit comes back.
    Close { feed: u32 },
    /// A validator of the current epoch opts in (signed with its consensus key).
    Join { feed: u32 },
    /// A validator opts out (any time, also after leaving the set).
    Leave { feed: u32 },
}

/// Why a feed operation was refused (the fee is still charged, nothing else changes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedError {
    BadName,
    Taken,
    BadParams,
    NoSuchFeed,
    NotOwner,
    NotValidator,
    AlreadyJoined,
    NotJoined,
    TooManyFeeds,
    InsufficientBalance,
}

impl FeedOp {
    pub fn gas(&self) -> u64 {
        FEED_GAS
    }

    /// The most BTB the operation can take from the sender besides the fee.
    pub fn debit(&self) -> u64 {
        match self {
            FeedOp::Register { .. } => FEED_DEPOSIT,
            _ => 0,
        }
    }

    pub fn name_len(&self) -> usize {
        match self {
            FeedOp::Register { name, .. } => name.len(),
            _ => 0,
        }
    }

    pub(crate) fn encode_to(&self, out: &mut Vec<u8>) {
        match self {
            FeedOp::Register {
                name,
                decimals,
                read_fee,
                min_reporters,
                min_stake_bps,
                max_age_blocks,
            } => {
                0u8.encode_to(out);
                name.encode_to(out);
                decimals.encode_to(out);
                read_fee.encode_to(out);
                min_reporters.encode_to(out);
                min_stake_bps.encode_to(out);
                max_age_blocks.encode_to(out);
            }
            FeedOp::Update {
                feed,
                read_fee,
                min_reporters,
                min_stake_bps,
                max_age_blocks,
            } => {
                1u8.encode_to(out);
                feed.encode_to(out);
                read_fee.encode_to(out);
                min_reporters.encode_to(out);
                min_stake_bps.encode_to(out);
                max_age_blocks.encode_to(out);
            }
            FeedOp::Close { feed } => {
                2u8.encode_to(out);
                feed.encode_to(out);
            }
            FeedOp::Join { feed } => {
                3u8.encode_to(out);
                feed.encode_to(out);
            }
            FeedOp::Leave { feed } => {
                4u8.encode_to(out);
                feed.encode_to(out);
            }
        }
    }

    /// `Ok(Err(sub))` is an unknown sub-tag.
    pub(crate) fn decode_from(r: &mut Reader<'_>) -> Result<Result<Self, u8>, DecodeError> {
        let sub = u8::decode_from(r)?;
        Ok(Ok(match sub {
            0 => FeedOp::Register {
                name: String::decode_from(r)?,
                decimals: u8::decode_from(r)?,
                read_fee: u64::decode_from(r)?,
                min_reporters: u32::decode_from(r)?,
                min_stake_bps: u32::decode_from(r)?,
                max_age_blocks: u64::decode_from(r)?,
            },
            1 => FeedOp::Update {
                feed: u32::decode_from(r)?,
                read_fee: u64::decode_from(r)?,
                min_reporters: u32::decode_from(r)?,
                min_stake_bps: u32::decode_from(r)?,
                max_age_blocks: u64::decode_from(r)?,
            },
            2 => FeedOp::Close {
                feed: u32::decode_from(r)?,
            },
            3 => FeedOp::Join {
                feed: u32::decode_from(r)?,
            },
            4 => FeedOp::Leave {
                feed: u32::decode_from(r)?,
            },
            t => return Ok(Err(t)),
        }))
    }
}

/// A feed name: 1 to 32 printable ASCII characters without spaces (`BTC/USD`, `PSX:OGDC`).
pub fn valid_name(name: &str) -> bool {
    (1..=MAX_NAME).contains(&name.len()) && name.bytes().all(|c| c.is_ascii_graphic())
}

fn params_ok(read_fee: u64, min_reporters: u32, min_stake_bps: u32, max_age: u64) -> bool {
    (MIN_READ_FEE..=MAX_READ_FEE).contains(&read_fee)
        && (MIN_REPORTERS..=10_000).contains(&min_reporters)
        && (MIN_STAKE_BPS..=10_000).contains(&min_stake_bps)
        && (1..=crate::oracle::MAX_AGE_LIMIT).contains(&max_age)
}

fn k1(sub: u8, rest: &[u8]) -> Vec<u8> {
    let mut k = vec![TAG_ORACLE, sub];
    k.extend_from_slice(rest);
    k
}

fn feed_key(id: u32) -> Vec<u8> {
    layout::oracle_feed_key(id)
}

fn join_key(id: u32, key: &[u8; 32]) -> Vec<u8> {
    let mut k = k1(K_JOIN, &id.to_be_bytes());
    k.extend_from_slice(key);
    k
}

fn deposit_key(id: u32) -> Vec<u8> {
    let mut k = vec![TAG_FEED_DEPOSIT];
    k.extend_from_slice(&id.to_be_bytes());
    k
}

/// The registered feed `id`, if any.
pub fn get(kv: &impl KvStore, id: u32) -> Result<Option<FeedRecord>, StateError> {
    kv.get(&feed_key(id))?
        .map(|b| FeedRecord::decode(&b).map_err(StateError::from))
        .transpose()
}

/// The id of the feed called `name`, if registered.
pub fn id_of(kv: &impl KvStore, name: &str) -> Result<Option<u32>, StateError> {
    Ok(kv
        .get(&k1(K_NAME, name.as_bytes()))?
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_le_bytes))
}

/// All registered feeds, by id (at most `limit`, from `from`).
pub fn list(
    kv: &impl KvStore,
    from: u32,
    limit: usize,
) -> Result<Vec<(u32, FeedRecord)>, StateError> {
    let start = feed_key(from);
    let end = vec![TAG_ORACLE, K_FEED + 1];
    let mut out = Vec::new();
    for (k, v) in kv.scan(&start, Some(&end), false, limit)? {
        let Some(id) = k.get(2..6).and_then(|b| <[u8; 4]>::try_from(b).ok()) else {
            continue;
        };
        out.push((u32::from_be_bytes(id), FeedRecord::decode(&v)?));
    }
    Ok(out)
}

/// Has the validator with consensus key `key` joined feed `id`?
pub fn joined(kv: &impl KvStore, id: u32, key: &[u8; 32]) -> Result<bool, KvError> {
    Ok(kv.get(&join_key(id, key))?.is_some())
}

/// Consensus keys that joined feed `id`.
pub fn reporters(kv: &impl KvStore, id: u32) -> Result<Vec<[u8; 32]>, KvError> {
    let start = k1(K_JOIN, &id.to_be_bytes());
    let end = match id.checked_add(1) {
        Some(n) => k1(K_JOIN, &n.to_be_bytes()),
        None => vec![TAG_ORACLE, K_JOIN + 1],
    };
    Ok(kv
        .scan(&start, Some(&end), false, 100_000)?
        .into_iter()
        .filter_map(|(k, _)| k.get(6..38).and_then(|b| <[u8; 32]>::try_from(b).ok()))
        .collect())
}

/// How many feeds the validator with consensus key `key` joined.
pub fn joined_count(kv: &impl KvStore, key: &[u8; 32]) -> Result<u32, KvError> {
    Ok(kv
        .get(&k1(K_JOINED, key))?
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map_or(0, u32::from_le_bytes))
}

/// The deposit locked for feed `id`.
pub fn deposit(kv: &impl KvStore, id: u32) -> Result<u64, KvError> {
    read_u64(kv, &deposit_key(id))
}

/// Contributors (operator, stake) of the value of feed or pair `id` as last published.
pub fn contributors(kv: &impl KvStore, id: u32) -> Result<Vec<(Address, u64)>, StateError> {
    match kv.get(&k1(K_CONTRIB, &id.to_be_bytes()))? {
        None => Ok(Vec::new()),
        Some(b) => Ok(Vec::<(Address, u64)>::decode(&b)?),
    }
}

pub(crate) fn set_contributors(
    kv: &mut impl KvStore,
    id: u32,
    who: &[(Address, u64)],
) -> Result<(), KvError> {
    kv.put(&k1(K_CONTRIB, &id.to_be_bytes()), &who.to_vec().encode())
}

pub(crate) enum Fail {
    Tx(FeedError),
    State(StateError),
}

impl From<KvError> for Fail {
    fn from(e: KvError) -> Self {
        Fail::State(e.into())
    }
}

impl From<StateError> for Fail {
    fn from(e: StateError) -> Self {
        Fail::State(e)
    }
}

fn balance(kv: &impl KvStore, a: &Address) -> Result<u64, KvError> {
    read_u64(kv, &layout::balance_key(a))
}

fn set_balance(kv: &mut impl KvStore, a: &Address, v: u64) -> Result<(), KvError> {
    kv.put(&layout::balance_key(a), &v.to_le_bytes())
}

fn credit(kv: &mut impl KvStore, a: &Address, amount: u64) -> Result<(), StateError> {
    let b = balance(kv, a)?
        .checked_add(amount)
        .ok_or_else(|| StateError("balance overflow".into()))?;
    Ok(set_balance(kv, a, b)?)
}

fn set_joined_count(kv: &mut impl KvStore, key: &[u8; 32], n: u32) -> Result<(), KvError> {
    if n == 0 {
        kv.delete(&k1(K_JOINED, key))
    } else {
        kv.put(&k1(K_JOINED, key), &n.to_le_bytes())
    }
}

/// Apply a feed operation. `signer` is the transaction's public key (Join checks it against the
/// current validator set). Every check runs before the first write. Output: the feed id for
/// `Register`, empty otherwise.
pub(crate) fn apply(
    kv: &mut impl KvStore,
    epoch_len: u64,
    height: u64,
    sender: &Address,
    signer: &[u8; 32],
    op: &FeedOp,
) -> Result<Vec<u8>, Fail> {
    let tx = |e| Err(Fail::Tx(e));
    match op {
        FeedOp::Register {
            name,
            decimals,
            read_fee,
            min_reporters,
            min_stake_bps,
            max_age_blocks,
        } => {
            if !valid_name(name) {
                return tx(FeedError::BadName);
            }
            if *decimals > 36
                || !params_ok(*read_fee, *min_reporters, *min_stake_bps, *max_age_blocks)
            {
                return tx(FeedError::BadParams);
            }
            let genesis_name = crate::oracle::load(&*kv)?
                .is_some_and(|og| og.pairs.iter().any(|p| &p.name == name));
            if genesis_name || id_of(&*kv, name)?.is_some() {
                return tx(FeedError::Taken);
            }
            let bal = balance(&*kv, sender)?;
            if bal < FEED_DEPOSIT {
                return tx(FeedError::InsufficientBalance);
            }
            let next_key = vec![TAG_ORACLE, K_NEXT];
            let id = kv
                .get(&next_key)?
                .and_then(|b| <[u8; 4]>::try_from(b).ok())
                .map_or(FIRST_ID, u32::from_le_bytes);
            let Some(after) = id.checked_add(1) else {
                return tx(FeedError::BadParams);
            };
            set_balance(kv, sender, bal - FEED_DEPOSIT)?;
            kv.put(&deposit_key(id), &FEED_DEPOSIT.to_le_bytes())?;
            kv.put(&next_key, &after.to_le_bytes())?;
            kv.put(&k1(K_NAME, name.as_bytes()), &id.to_le_bytes())?;
            let rec = FeedRecord {
                name: name.clone(),
                decimals: *decimals,
                owner: *sender,
                read_fee: *read_fee,
                min_reporters: *min_reporters,
                min_stake_bps: *min_stake_bps,
                max_age_blocks: *max_age_blocks,
            };
            kv.put(&feed_key(id), &rec.encode())?;
            Ok(id.encode())
        }
        FeedOp::Update {
            feed,
            read_fee,
            min_reporters,
            min_stake_bps,
            max_age_blocks,
        } => {
            let Some(mut rec) = get(&*kv, *feed)? else {
                return tx(FeedError::NoSuchFeed);
            };
            if &rec.owner != sender {
                return tx(FeedError::NotOwner);
            }
            if !params_ok(*read_fee, *min_reporters, *min_stake_bps, *max_age_blocks) {
                return tx(FeedError::BadParams);
            }
            rec.read_fee = *read_fee;
            rec.min_reporters = *min_reporters;
            rec.min_stake_bps = *min_stake_bps;
            rec.max_age_blocks = *max_age_blocks;
            kv.put(&feed_key(*feed), &rec.encode())?;
            Ok(Vec::new())
        }
        FeedOp::Close { feed } => {
            let Some(rec) = get(&*kv, *feed)? else {
                return tx(FeedError::NoSuchFeed);
            };
            if &rec.owner != sender {
                return tx(FeedError::NotOwner);
            }
            for key in reporters(&*kv, *feed)? {
                let n = joined_count(&*kv, &key)?;
                set_joined_count(kv, &key, n.saturating_sub(1))?;
                kv.delete(&join_key(*feed, &key))?;
            }
            crate::oracle::forget_feed(kv, *feed)?;
            let dep = deposit(&*kv, *feed)?;
            kv.delete(&deposit_key(*feed))?;
            credit(kv, sender, dep)?;
            kv.delete(&k1(K_NAME, rec.name.as_bytes()))?;
            kv.delete(&feed_key(*feed))?;
            kv.delete(&k1(K_CONTRIB, &feed.to_be_bytes()))?;
            Ok(Vec::new())
        }
        FeedOp::Join { feed } => {
            if get(&*kv, *feed)?.is_none() {
                return tx(FeedError::NoSuchFeed);
            }
            let epoch = height.saturating_sub(1) / epoch_len.max(1);
            let in_set = staking::epoch_record(&*kv, epoch)?
                .is_some_and(|r| r.validators.iter().any(|v| &v.consensus_key == signer));
            if !in_set {
                return tx(FeedError::NotValidator);
            }
            if joined(&*kv, *feed, signer)? {
                return tx(FeedError::AlreadyJoined);
            }
            let n = joined_count(&*kv, signer)?;
            if n >= MAX_FEEDS_PER_VALIDATOR {
                return tx(FeedError::TooManyFeeds);
            }
            kv.put(&join_key(*feed, signer), &[])?;
            set_joined_count(kv, signer, n + 1)?;
            Ok(Vec::new())
        }
        FeedOp::Leave { feed } => {
            if !joined(&*kv, *feed, signer)? {
                return tx(FeedError::NotJoined);
            }
            kv.delete(&join_key(*feed, signer))?;
            let n = joined_count(&*kv, signer)?;
            set_joined_count(kv, signer, n.saturating_sub(1))?;
            Ok(Vec::new())
        }
    }
}

/// Move the reporters' share of this block's reads out of the validator pool: `fees` is, per
/// feed, the base units that reads of it paid into the pool (summed from the receipts). Each
/// feed's units go to the contributors of its current value pro rata by stake (remainder one
/// unit each to the first). A feed without contributors leaves its units in the pool, and so
/// does any amount above the pool's balance (it cannot happen, but money is never created).
pub(crate) fn pay_reporters(
    kv: &mut impl KvStore,
    fees: &std::collections::BTreeMap<u32, u64>,
) -> Result<(), StateError> {
    let pool = crate::validator_pool();
    for (id, amount) in fees {
        let who = contributors(&*kv, *id)?;
        let total: u128 = who.iter().map(|(_, s)| u128::from(*s)).sum();
        if *amount == 0 || who.is_empty() || total == 0 {
            continue;
        }
        let amount = (*amount).min(balance(&*kv, &pool)?);
        let mut shares: Vec<u64> = who
            .iter()
            .map(|(_, s)| (u128::from(amount) * u128::from(*s) / total) as u64)
            .collect();
        let rem = (amount - shares.iter().sum::<u64>()) as usize;
        for s in shares.iter_mut().take(rem) {
            *s += 1;
        }
        let b = balance(&*kv, &pool)?;
        set_balance(kv, &pool, b - amount)?;
        for ((op, _), s) in who.iter().zip(shares) {
            if s > 0 {
                credit(kv, op, s)?;
            }
        }
    }
    Ok(())
}
