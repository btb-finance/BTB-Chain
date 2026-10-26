//! Example contract: file references (`btb_sdk::files`, see `docs/STORAGE.md`, "Files from
//! contracts").
//!
//! The contract never holds file bytes, only the 32-byte **root** a client got back from the
//! upload (or from `btb store`). It keeps a name -> root map, refuses roots that are not stored
//! and paid, and can pay for storage itself from its balance.
//!
//! Build: `./build.sh` (writes `files.elf`).
#![no_std]
#![no_main]

extern crate alloc;

use btb_sdk::btb;
use btb_sdk::files::{self, PinError};
use btb_sdk::host;
use btb_sdk::prelude::String;
use btb_sdk::{Address, Decode, Encode};

fn key(name: &str) -> alloc::vec::Vec<u8> {
    let mut k = b"f/".to_vec();
    k.extend_from_slice(name.as_bytes());
    k
}

/// A small number per way a pin can fail (0 is success).
fn pin_code(e: PinError) -> u8 {
    match e {
        PinError::Exists => 1,
        PinError::Insufficient => 2,
        PinError::NoNodes => 3,
        PinError::Invalid => 4,
        PinError::Unsupported => 5,
    }
}

/// Attach the file with `root` to `name`. Aborts unless the file is stored and paid for.
#[btb::mutation]
pub fn attach(name: String, root: Address) -> u64 {
    let f = files::require_stored(&root.0);
    host::storage_set(&key(&name), &root.encode());
    f.size
}

/// Attach without looking at the file: only for tests of the unknown-root path.
#[btb::mutation]
pub fn attach_unchecked(name: String, root: Address) {
    host::storage_set(&key(&name), &root.encode());
}

/// Pay for the file from this contract's balance, then attach it. Returns 0 on success, else
/// the pin failure code (1 already stored, 2 balance, 3 no nodes, 4 bad shape, 5 unsupported).
/// A root that is already stored is attached without paying again.
#[btb::mutation]
pub fn pin_and_attach(name: String, root: Address, size: u64, k: u8, m: u8, periods: u32) -> u8 {
    match files::ensure_pinned(&root.0, size, k, m, periods) {
        Ok(()) => {
            host::storage_set(&key(&name), &root.encode());
            0
        }
        Err(e) => pin_code(e),
    }
}

/// The root attached to `name`.
#[btb::query]
pub fn root_of(name: String) -> Option<Address> {
    host::storage_get(&key(&name)).and_then(|b| Address::decode(&b).ok())
}

/// `(size, status, expiry)` of a root: status 0 unknown, 1 stored, 2 expired.
#[btb::query]
pub fn info(root: Address) -> (u64, u8, u64) {
    let f = files::info(&root.0);
    let status = match f.status {
        files::FileStatus::Unknown => 0,
        files::FileStatus::Stored => 1,
        files::FileStatus::Expired => 2,
    };
    (f.size, status, f.expiry)
}

/// Is the attached file of `name` stored and paid right now?
#[btb::query]
pub fn is_available(name: String) -> bool {
    host::storage_get(&key(&name))
        .and_then(|b| Address::decode(&b).ok())
        .is_some_and(|r| files::is_stored(&r.0))
}

/// Query-side misuse: pinning from a query must trap (`WriteInQuery`).
#[btb::query]
pub fn bad_pin_in_query(root: Address) -> u8 {
    match files::pin(&root.0, 1, 1, 1, 1) {
        Ok(()) => 0,
        Err(e) => pin_code(e),
    }
}
