//! Example contract: randomness and price feeds (`docs/ORACLES.md`).
//!
//! Build: `./build.sh` (writes `oracle.elf`).
//!
//! * `roll` and `lottery` are marked `randomness`, so they may call `random::*`.
//! * `cheat_query` and `unmarked` call `random::*` without the marker: the VM traps them. They
//!   exist so tests can show it; real contracts must not do this.
#![no_std]
#![no_main]

use btb_sdk::btb;
use btb_sdk::host;
use btb_sdk::oracle::{price, random};
use btb_sdk::prelude::Vec;

const ROLLS: &[u8] = b"rolls";
const PRICE_SEEN: &[u8] = b"price_seen";

fn push_roll(v: u64) {
    let mut all = host::storage_get(ROLLS).unwrap_or_default();
    all.extend_from_slice(&v.to_le_bytes());
    host::storage_set(ROLLS, &all);
}

/// Roll a die (1 to 6) and remember it.
#[btb::mutation(randomness)]
pub fn roll() -> u64 {
    let n = random::range(1, 7);
    push_roll(n);
    n
}

/// 32 random bytes, remembered as one record (so tests can compare replicas byte for byte).
#[btb::mutation(randomness)]
pub fn draw() -> btb_sdk::Address {
    let b = random::bytes(32);
    let mut a = [0u8; 32];
    a.copy_from_slice(&b);
    host::storage_set(b"draw", &a);
    btb_sdk::Address(a)
}

/// Two draws in one transaction (they must differ).
#[btb::mutation(randomness)]
pub fn two_draws() -> (u64, u64) {
    (random::u64(), random::u64())
}

/// A marked mutation that aborts after drawing: the draw must not make the call free.
#[btb::mutation(randomness)]
pub fn roll_and_abort() {
    let _ = random::u64();
    host::abort("losing roll");
}

/// A cron with randomness (runs every 5 blocks once scheduled in the manifest).
#[btb::cron(randomness, every = "5 blocks")]
pub fn lottery() {
    push_roll(random::range(1, 7) + 100);
}

/// Recorded rolls, as concatenated u64 LE.
#[btb::query]
pub fn rolls() -> Vec<u8> {
    host::storage_get(ROLLS).unwrap_or_default()
}

/// Wrong: a query asking for randomness. The VM traps it.
#[btb::query]
pub fn cheat_query() -> u64 {
    random::u64()
}

/// Wrong: a mutation without the marker asking for randomness. The VM traps it.
#[btb::mutation]
pub fn unmarked() -> u64 {
    random::u64()
}

/// Latest price of feed `pair` as `(price, height, decimals)`; free to read in a query.
#[btb::query]
pub fn quote(pair: u32) -> Option<(u128, u64, u8)> {
    price(pair).map(|p| (p.price, p.height, p.decimals))
}

/// Read a price inside a mutation (pays the per-read fee) and require it to be fresh.
#[btb::mutation]
pub fn use_price(pair: u32, max_age: u64) -> u128 {
    let Some(p) = price(pair) else {
        host::abort("no price");
    };
    if p.age_blocks(host::block_height()) > max_age {
        host::abort("price too old");
    }
    host::storage_set(PRICE_SEEN, &p.price.to_le_bytes());
    p.price
}
