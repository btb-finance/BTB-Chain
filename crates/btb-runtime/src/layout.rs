//! The key layout: every piece of state lives in ONE ordered keyspace.
//!
//! The first byte is a tag; integers are big-endian so byte order equals numeric order;
//! variable-length names are length-prefixed (`u32` BE) so no key is a prefix of another
//! name's key. `A` is a 32-byte contract/account address.
//!
//! | Tag | Key | Value |
//! | --- | --- | --- |
//! | `0x01` storage | `01 A key` | raw bytes |
//! | `0x02` row | `02 A len(table) table id:u64be` | row bytes |
//! | `0x03` counter | `03 A len(table) table` | next row id, `u64` LE |
//! | `0x04` index | `04 A len(table) table len(index) index enc(value) id:u64be` | empty |
//! | `0x05` balance | `05 A` | `u64` LE |
//! | `0x08` usage | `08 A` | bytes of state the contract stores, `u64` LE |
//! | `0x09` deposit | `09 A` | BTB locked for that state, `u64` LE |
//! | `0x0A` token | `0A T` | `btb-codec` tuple `(name, symbol, decimals, supply, mint_authority, creator)` |
//! | `0x0B` token balance | `0B T A` | `u128` LE, non-zero only (zero deletes the key) |
//! | `0x0C` token nonce | `0C A` | tokens created by contract `A`, `u64` LE |
//!
//! `T` is the 32-byte token id. Scanning `0B T` yields the holders of a token in ascending
//! address order. Token state is not metered (no storage deposit) and never touches `0x05`.
//!
//! Scanning `02 A len(table) table` yields rows in id order; scanning an index prefix yields
//! row ids ordered by (index value, id). The row id is always the last 8 bytes of a row or
//! index key, so a cursor (the last key seen) is enough to resume a scan.
//!
//! # Order-preserving index value encodings
//!
//! Byte order of the encoding equals the order of the values, and no encoding is a prefix
//! of another of the same type, so `enc(value)` followed by the id never mixes entries.
//!
//! - `u64`: 8 bytes big-endian.
//! - `u128`: 16 bytes big-endian.
//! - `i64`: sign bit flipped (`x ^ 0x8000...`), then 8 bytes big-endian, so negatives
//!   sort before positives.
//! - `bytes` / `string` (UTF-8 bytes, ordered bytewise): each `0x00` is written as
//!   `0x00 0xFF`, and the value ends with the terminator `0x00 0x00`. A shorter value that is
//!   a prefix of a longer one sorts first because the terminator is below any escaped or
//!   ordinary byte that could follow (`0x00 0x00` < `0x00 0xFF` < `0x01...`).
//!
//! An index must always produce the same variant for a given index name; the layout does not
//! store the type.

use btb_vm::host::Address;

pub const TAG_STORAGE: u8 = 1;
pub const TAG_ROW: u8 = 2;
pub const TAG_COUNTER: u8 = 3;
pub const TAG_INDEX: u8 = 4;
pub const TAG_BALANCE: u8 = 5;
pub const TAG_USAGE: u8 = 8;
pub const TAG_DEPOSIT: u8 = 9;
pub const TAG_TOKEN: u8 = 0x0A;
pub const TAG_TOKEN_BALANCE: u8 = 0x0B;
pub const TAG_TOKEN_NONCE: u8 = 0x0C;
/// Oracle state written by `btb-chain` (`oracle.rs`) and read by the host calls: see
/// `oracle_seed_key` and `oracle_price_key`.
pub const TAG_ORACLE: u8 = 0x50;

/// BTB (smallest unit) a contract locks per stored byte. A stored entry counts
/// `key.len() + value.len()` bytes for storage keys (`0x01`), table rows (`0x02`) and index
/// entries (`0x04`). Row id counters, balances and the usage/deposit records are not metered.
/// The deposit is refunded to the contract's balance as the bytes are freed, so the total
/// supply never changes.
pub const DEPOSIT_PER_BYTE: u64 = 10;

/// Whether a key (of any contract) holds metered state.
pub fn is_metered(key: &[u8]) -> bool {
    matches!(key.first(), Some(&(TAG_STORAGE | TAG_ROW | TAG_INDEX)))
}

/// The contract address embedded in a metered key.
pub fn key_contract(key: &[u8]) -> Option<&[u8]> {
    key.get(1..33)
}

/// `50 02`: the beacon of the block being executed, `height:u64 LE || seed[32]`. Written at the
/// start of every block of a chain with an oracle config; `random_bytes` derives from it.
pub fn oracle_seed_key() -> Vec<u8> {
    vec![TAG_ORACLE, 0x02]
}

/// `50 10 pair:u32be`: the agreed price of a pair, `price:u128 LE || height:u64 LE ||
/// decimals:u8 || reporters:u32 LE` (29 bytes).
pub fn oracle_price_key(pair: u32) -> Vec<u8> {
    let mut k = vec![TAG_ORACLE, 0x10];
    k.extend_from_slice(&pair.to_be_bytes());
    k
}

pub fn usage_key(addr: &Address) -> Vec<u8> {
    let mut k = vec![TAG_USAGE];
    k.extend_from_slice(addr);
    k
}

pub fn deposit_key(addr: &Address) -> Vec<u8> {
    let mut k = vec![TAG_DEPOSIT];
    k.extend_from_slice(addr);
    k
}

/// A typed, order-preserving index value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexValue {
    U64(u64),
    I64(i64),
    U128(u128),
    Bytes(Vec<u8>),
    Str(String),
}

impl IndexValue {
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            IndexValue::U64(v) => out.extend_from_slice(&v.to_be_bytes()),
            IndexValue::I64(v) => out.extend_from_slice(&((*v as u64) ^ (1 << 63)).to_be_bytes()),
            IndexValue::U128(v) => out.extend_from_slice(&v.to_be_bytes()),
            IndexValue::Bytes(b) => encode_bytes(b, out),
            IndexValue::Str(s) => encode_bytes(s.as_bytes(), out),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }
}

fn encode_bytes(b: &[u8], out: &mut Vec<u8>) {
    for &x in b {
        out.push(x);
        if x == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
}

fn name(out: &mut Vec<u8>, n: &[u8]) {
    out.extend_from_slice(&(n.len() as u32).to_be_bytes());
    out.extend_from_slice(n);
}

fn with_table(tag: u8, contract: &Address, table: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + 32 + 4 + table.len() + 8);
    k.push(tag);
    k.extend_from_slice(contract);
    name(&mut k, table);
    k
}

pub fn storage_key(contract: &Address, key: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(33 + key.len());
    k.push(TAG_STORAGE);
    k.extend_from_slice(contract);
    k.extend_from_slice(key);
    k
}

/// Prefix shared by every row key of the table.
pub fn row_prefix(contract: &Address, table: &[u8]) -> Vec<u8> {
    with_table(TAG_ROW, contract, table)
}

pub fn row_key(contract: &Address, table: &[u8], id: u64) -> Vec<u8> {
    let mut k = row_prefix(contract, table);
    k.extend_from_slice(&id.to_be_bytes());
    k
}

pub fn counter_key(contract: &Address, table: &[u8]) -> Vec<u8> {
    with_table(TAG_COUNTER, contract, table)
}

/// Prefix shared by every entry of one index.
pub fn index_prefix(contract: &Address, table: &[u8], index: &str) -> Vec<u8> {
    let mut k = with_table(TAG_INDEX, contract, table);
    name(&mut k, index.as_bytes());
    k
}

pub fn index_key(
    contract: &Address,
    table: &[u8],
    index: &str,
    value: &IndexValue,
    id: u64,
) -> Vec<u8> {
    let mut k = index_prefix(contract, table, index);
    value.encode_into(&mut k);
    k.extend_from_slice(&id.to_be_bytes());
    k
}

pub fn balance_key(addr: &Address) -> Vec<u8> {
    let mut k = Vec::with_capacity(33);
    k.push(TAG_BALANCE);
    k.extend_from_slice(addr);
    k
}

/// Info record of user token `id`.
pub fn token_key(id: &[u8; 32]) -> Vec<u8> {
    let mut k = vec![TAG_TOKEN];
    k.extend_from_slice(id);
    k
}

/// Prefix shared by every holder entry of user token `id`.
pub fn token_balance_prefix(id: &[u8; 32]) -> Vec<u8> {
    let mut k = vec![TAG_TOKEN_BALANCE];
    k.extend_from_slice(id);
    k
}

pub fn token_balance_key(id: &[u8; 32], holder: &Address) -> Vec<u8> {
    let mut k = token_balance_prefix(id);
    k.extend_from_slice(holder);
    k
}

/// Number of user tokens a contract has created (the next token nonce).
pub fn token_nonce_key(creator: &Address) -> Vec<u8> {
    let mut k = vec![TAG_TOKEN_NONCE];
    k.extend_from_slice(creator);
    k
}

/// The row id stored in the last 8 bytes of a row or index key.
pub fn id_from_key(key: &[u8]) -> Option<u64> {
    let tail = key.get(key.len().checked_sub(8)?..)?;
    Some(u64::from_be_bytes(tail.try_into().ok()?))
}

/// Smallest key greater than every key starting with `prefix`; `None` if there is none
/// (the prefix is all `0xFF`).
pub fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut p = prefix.to_vec();
    while let Some(last) = p.pop() {
        if last != 0xFF {
            p.push(last + 1);
            return Some(p);
        }
    }
    None
}
