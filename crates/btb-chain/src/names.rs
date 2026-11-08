//! Names: `alice.btb` (proposal 0002, phase B).
//!
//! A native registry from a name to the account that owns it. Names are 3 to 32 characters of
//! `a-z`, `0-9` and `-` (not first or last), written without the `.btb` suffix on chain. One
//! name per account.
//!
//! Registering locks a **deposit** from the owner's balance (more for shorter names, see
//! [`deposit_for`]); it is counted by the supply invariant (`0x75`), nothing is minted or
//! burned, and releasing the name returns it. The deposit travels with the name: a buyer pays
//! the seller the price **and** the deposit, so the seller gets their deposit back and the
//! buyer's money is now the one locked; a gift hands the deposit over with the name.
//!
//! Selling is an atomic swap with no middleman: the owner lists the name (`List`: price,
//! optionally one allowed buyer, an expiry height); `Buy` moves the price and the deposit to the
//! seller and the name to the buyer in one state change, or nothing at all. A listing ends when
//! the name changes owner. `Pay` sends BTB to whoever owns a name *at execution*, and its output
//! is the address that was paid.
//!
//! When the sender acts as an account with key slots (`account.rs`), every name operation
//! except `Pay` needs an **admin** slot (checked at authorization).
//!
//! | tag | key | value |
//! | --- | --- | --- |
//! | `0x74` | `74 name` | owner[32] \|\| listing: Option<price u64, buyer Option<[32]>, expires u64> |
//! | `0x75` | `75 name` | deposit, `u64` LE (money) |
//! | `0x76` | `76 owner` | name bytes |

use crate::state::{read_u64, StateError};
use crate::tx::Address;
use crate::UNITS_PER_BTB;
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_runtime::kv::{KvError, KvStore};
use btb_runtime::layout;

pub const TAG_NAME: u8 = 0x74;
pub const TAG_NAME_DEPOSIT: u8 = 0x75;
pub const TAG_NAME_OF: u8 = 0x76;
pub const NAME_GAS: u64 = 10_000;
pub const MIN_NAME: usize = 3;
pub const MAX_NAME: usize = 32;

/// Deposit locked for a name of `len` characters: 3 → 10,000 BTB, 4 → 1,000 BTB, longer → 100.
pub fn deposit_for(len: usize) -> u64 {
    match len {
        0..=3 => 10_000 * UNITS_PER_BTB,
        4 => 1_000 * UNITS_PER_BTB,
        _ => 100 * UNITS_PER_BTB,
    }
}

/// Whether `name` is a valid name (without the `.btb` suffix).
pub fn valid(name: &str) -> bool {
    let b = name.as_bytes();
    (MIN_NAME..=MAX_NAME).contains(&b.len())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}

/// `alice.btb` or `alice` → `alice`.
pub fn strip_suffix(name: &str) -> &str {
    name.strip_suffix(".btb").unwrap_or(name)
}

fn key(tag: u8, rest: &[u8]) -> Vec<u8> {
    let mut k = vec![tag];
    k.extend_from_slice(rest);
    k
}

/// An offer to sell a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listing {
    pub price: u64,
    /// `Some`: only this address may buy.
    pub buyer: Option<Address>,
    /// The first height at which the offer no longer works.
    pub expires_height: u64,
}

impl Encode for Listing {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.price.encode_to(out);
        self.buyer.encode_to(out);
        self.expires_height.encode_to(out);
    }
}

impl Decode for Listing {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Listing {
            price: u64::decode_from(r)?,
            buyer: Option::decode_from(r)?,
            expires_height: u64::decode_from(r)?,
        })
    }
}

/// A registered name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameRecord {
    pub owner: Address,
    pub listing: Option<Listing>,
}

impl Encode for NameRecord {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.owner.encode_to(out);
        self.listing.encode_to(out);
    }
}

impl Decode for NameRecord {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(NameRecord {
            owner: Address::decode_from(r)?,
            listing: Option::decode_from(r)?,
        })
    }
}

/// Name operations (transaction tag 25). `name` is without `.btb`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameOp {
    Register {
        name: String,
    },
    Release {
        name: String,
    },
    List {
        name: String,
        price: u64,
        buyer: Option<Address>,
        expires_height: u64,
    },
    Unlist {
        name: String,
    },
    /// Buy a listed name, paying at most `max_price` (plus the deposit, which goes to the seller).
    Buy {
        name: String,
        max_price: u64,
    },
    Give {
        name: String,
        to: Address,
    },
    /// Send `amount` to whoever owns `name` now.
    Pay {
        name: String,
        amount: u64,
    },
}

/// Why a name operation was refused (the fee is still charged, nothing else changes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameError {
    BadName,
    Taken,
    NoSuchName,
    NotOwner,
    /// The account already owns a name (one per account).
    AlreadyHasName,
    NotListed,
    ListingExpired,
    /// The listing is reserved for another buyer.
    NotForYou,
    /// The price is above the buyer's `max_price`.
    PriceAboveMax,
    /// Selling or giving to yourself, or a listing that expires in the past.
    BadListing,
    InsufficientBalance,
}

impl NameOp {
    pub fn gas(&self) -> u64 {
        NAME_GAS
    }

    pub fn name(&self) -> &str {
        match self {
            NameOp::Register { name }
            | NameOp::Release { name }
            | NameOp::List { name, .. }
            | NameOp::Unlist { name }
            | NameOp::Buy { name, .. }
            | NameOp::Give { name, .. }
            | NameOp::Pay { name, .. } => name,
        }
    }

    /// The most BTB the operation can take from the sender besides the fee.
    pub fn debit(&self) -> u64 {
        match self {
            NameOp::Register { name } => deposit_for(name.len()),
            NameOp::Buy { name, max_price } => max_price.saturating_add(deposit_for(name.len())),
            NameOp::Pay { amount, .. } => *amount,
            _ => 0,
        }
    }

    pub(crate) fn encode_to(&self, out: &mut Vec<u8>) {
        match self {
            NameOp::Register { name } => {
                0u8.encode_to(out);
                name.encode_to(out);
            }
            NameOp::Release { name } => {
                1u8.encode_to(out);
                name.encode_to(out);
            }
            NameOp::List {
                name,
                price,
                buyer,
                expires_height,
            } => {
                2u8.encode_to(out);
                name.encode_to(out);
                price.encode_to(out);
                buyer.encode_to(out);
                expires_height.encode_to(out);
            }
            NameOp::Unlist { name } => {
                3u8.encode_to(out);
                name.encode_to(out);
            }
            NameOp::Buy { name, max_price } => {
                4u8.encode_to(out);
                name.encode_to(out);
                max_price.encode_to(out);
            }
            NameOp::Give { name, to } => {
                5u8.encode_to(out);
                name.encode_to(out);
                to.encode_to(out);
            }
            NameOp::Pay { name, amount } => {
                6u8.encode_to(out);
                name.encode_to(out);
                amount.encode_to(out);
            }
        }
    }

    /// `Ok(Err(sub))` is an unknown sub-tag.
    pub(crate) fn decode_from(r: &mut Reader<'_>) -> Result<Result<Self, u8>, DecodeError> {
        let sub = u8::decode_from(r)?;
        let name = String::decode_from(r)?;
        Ok(Ok(match sub {
            0 => NameOp::Register { name },
            1 => NameOp::Release { name },
            2 => NameOp::List {
                name,
                price: u64::decode_from(r)?,
                buyer: Option::decode_from(r)?,
                expires_height: u64::decode_from(r)?,
            },
            3 => NameOp::Unlist { name },
            4 => NameOp::Buy {
                name,
                max_price: u64::decode_from(r)?,
            },
            5 => NameOp::Give {
                name,
                to: Address::decode_from(r)?,
            },
            6 => NameOp::Pay {
                name,
                amount: u64::decode_from(r)?,
            },
            t => return Ok(Err(t)),
        }))
    }

    /// Checks that need no state: the name is well formed.
    pub fn check_static(&self) -> Result<(), NameError> {
        if valid(self.name()) {
            Ok(())
        } else {
            Err(NameError::BadName)
        }
    }
}

/// The record of `name`, if registered.
pub fn get(kv: &impl KvStore, name: &str) -> Result<Option<NameRecord>, StateError> {
    match kv.get(&key(TAG_NAME, name.as_bytes()))? {
        None => Ok(None),
        Some(b) => {
            let mut r = Reader::new(&b, b.len().max(1));
            NameRecord::decode_from(&mut r)
                .map(Some)
                .map_err(|_| StateError("bad stored name".into()))
        }
    }
}

/// The name `owner` holds, if any.
pub fn name_of(kv: &impl KvStore, owner: &Address) -> Result<Option<String>, StateError> {
    match kv.get(&key(TAG_NAME_OF, owner))? {
        None => Ok(None),
        Some(b) => String::from_utf8(b)
            .map(Some)
            .map_err(|_| StateError("bad stored name".into())),
    }
}

/// The deposit locked under `name` (0 if none).
pub fn deposit(kv: &impl KvStore, name: &str) -> Result<u64, KvError> {
    read_u64(kv, &key(TAG_NAME_DEPOSIT, name.as_bytes()))
}

pub(crate) enum Fail {
    Tx(NameError),
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

fn credit(kv: &mut impl KvStore, a: &Address, amount: u64) -> Result<(), Fail> {
    let b = balance(kv, a)?
        .checked_add(amount)
        .ok_or_else(|| StateError("balance overflow".into()))?;
    Ok(set_balance(kv, a, b)?)
}

fn put_record(kv: &mut impl KvStore, name: &str, rec: &NameRecord) -> Result<(), KvError> {
    kv.put(&key(TAG_NAME, name.as_bytes()), &rec.encode())
}

/// Apply a name operation of `sender` at `height`. Every check runs before the first write, so
/// a refused operation changes nothing. Returns the output (`Pay`: the address paid).
pub(crate) fn apply(
    kv: &mut impl KvStore,
    height: u64,
    sender: &Address,
    op: &NameOp,
) -> Result<Vec<u8>, Fail> {
    op.check_static().map_err(Fail::Tx)?;
    let name = op.name();
    let rec = get(kv, name)?;
    let owned = |r: &Option<NameRecord>| match r {
        None => Err(Fail::Tx(NameError::NoSuchName)),
        Some(r) if r.owner != *sender => Err(Fail::Tx(NameError::NotOwner)),
        Some(r) => Ok(*r),
    };
    match op {
        NameOp::Register { .. } => {
            if rec.is_some() {
                return Err(Fail::Tx(NameError::Taken));
            }
            if name_of(kv, sender)?.is_some() {
                return Err(Fail::Tx(NameError::AlreadyHasName));
            }
            let dep = deposit_for(name.len());
            let rest = balance(kv, sender)?
                .checked_sub(dep)
                .ok_or(Fail::Tx(NameError::InsufficientBalance))?;
            set_balance(kv, sender, rest)?;
            kv.put(&key(TAG_NAME_DEPOSIT, name.as_bytes()), &dep.to_le_bytes())?;
            kv.put(&key(TAG_NAME_OF, sender), name.as_bytes())?;
            put_record(
                kv,
                name,
                &NameRecord {
                    owner: *sender,
                    listing: None,
                },
            )?;
            Ok(Vec::new())
        }
        NameOp::Release { .. } => {
            owned(&rec)?;
            let dep = deposit(kv, name)?;
            kv.delete(&key(TAG_NAME, name.as_bytes()))?;
            kv.delete(&key(TAG_NAME_DEPOSIT, name.as_bytes()))?;
            kv.delete(&key(TAG_NAME_OF, sender))?;
            credit(kv, sender, dep)?;
            Ok(Vec::new())
        }
        NameOp::List {
            price,
            buyer,
            expires_height,
            ..
        } => {
            let mut r = owned(&rec)?;
            if *expires_height <= height || *buyer == Some(*sender) {
                return Err(Fail::Tx(NameError::BadListing));
            }
            r.listing = Some(Listing {
                price: *price,
                buyer: *buyer,
                expires_height: *expires_height,
            });
            put_record(kv, name, &r)?;
            Ok(Vec::new())
        }
        NameOp::Unlist { .. } => {
            let mut r = owned(&rec)?;
            if r.listing.take().is_none() {
                return Err(Fail::Tx(NameError::NotListed));
            }
            put_record(kv, name, &r)?;
            Ok(Vec::new())
        }
        NameOp::Buy { max_price, .. } => {
            let r = rec.ok_or(Fail::Tx(NameError::NoSuchName))?;
            let l = r.listing.ok_or(Fail::Tx(NameError::NotListed))?;
            if l.expires_height <= height {
                return Err(Fail::Tx(NameError::ListingExpired));
            }
            if r.owner == *sender {
                return Err(Fail::Tx(NameError::BadListing));
            }
            if l.buyer.is_some_and(|b| b != *sender) {
                return Err(Fail::Tx(NameError::NotForYou));
            }
            if l.price > *max_price {
                return Err(Fail::Tx(NameError::PriceAboveMax));
            }
            if name_of(kv, sender)?.is_some() {
                return Err(Fail::Tx(NameError::AlreadyHasName));
            }
            let dep = deposit(kv, name)?;
            let total = l
                .price
                .checked_add(dep)
                .ok_or(Fail::Tx(NameError::InsufficientBalance))?;
            let rest = balance(kv, sender)?
                .checked_sub(total)
                .ok_or(Fail::Tx(NameError::InsufficientBalance))?;
            set_balance(kv, sender, rest)?;
            credit(kv, &r.owner, total)?;
            kv.delete(&key(TAG_NAME_OF, &r.owner))?;
            kv.put(&key(TAG_NAME_OF, sender), name.as_bytes())?;
            put_record(
                kv,
                name,
                &NameRecord {
                    owner: *sender,
                    listing: None,
                },
            )?;
            Ok(Vec::new())
        }
        NameOp::Give { to, .. } => {
            owned(&rec)?;
            if to == sender {
                return Err(Fail::Tx(NameError::BadListing));
            }
            if name_of(kv, to)?.is_some() {
                return Err(Fail::Tx(NameError::AlreadyHasName));
            }
            kv.delete(&key(TAG_NAME_OF, sender))?;
            kv.put(&key(TAG_NAME_OF, to), name.as_bytes())?;
            put_record(
                kv,
                name,
                &NameRecord {
                    owner: *to,
                    listing: None,
                },
            )?;
            Ok(Vec::new())
        }
        NameOp::Pay { amount, .. } => {
            let r = rec.ok_or(Fail::Tx(NameError::NoSuchName))?;
            let rest = balance(kv, sender)?
                .checked_sub(*amount)
                .ok_or(Fail::Tx(NameError::InsufficientBalance))?;
            set_balance(kv, sender, rest)?;
            credit(kv, &r.owner, *amount)?;
            Ok(r.owner.to_vec())
        }
    }
}
