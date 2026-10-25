//! Example contract: a persistent counter.
//!
//! Build: `./build.sh` (writes `counter.elf`).
#![no_std]
#![no_main]

use btb_sdk::btb;
use btb_sdk::host;
use btb_sdk::table::Table;

/// One row per increment; exercises `#[btb::table]`.
#[btb::table]
pub struct Change {
    pub by: u64,
    pub total: u64,
    pub height: u64,
}

const KEY: &[u8] = b"count";

fn load() -> u64 {
    match host::storage_get(KEY) {
        Some(b) => <u64 as btb_sdk::Decode>::decode(&b).unwrap_or(0),
        None => 0,
    }
}

fn store(v: u64) {
    host::storage_set(KEY, &btb_sdk::Encode::encode(&v));
}

/// Current value.
#[btb::query]
pub fn get() -> u64 {
    load()
}

fn bump(by: u64) -> u64 {
    let Some(v) = load().checked_add(by) else {
        host::abort("counter overflow");
    };
    store(v);
    Change {
        by,
        total: v,
        height: host::block_height(),
    }
    .insert();
    v
}

/// Add `by` and return the new value; aborts on overflow.
#[btb::mutation]
pub fn increment(by: u64) -> u64 {
    bump(by)
}

/// Deliberately wrong: a query that writes. It exists so tests can show the VM trapping with
/// `WriteInQuery`; real contracts must not do this.
#[btb::query]
pub fn increment_in_query(by: u64) -> u64 {
    bump(by)
}

/// Scheduled bump by one.
#[btb::cron(every = "10 blocks")]
pub fn tick() {
    store(load().wrapping_add(1));
}

/// `(by, total, height)` of the change with row id `id`.
#[btb::query]
pub fn change(id: u64) -> Option<(u64, u64, u64)> {
    Change::get(id).map(|c| (c.by, c.total, c.height))
}
