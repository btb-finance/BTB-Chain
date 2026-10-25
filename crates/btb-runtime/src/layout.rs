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
