//! Oracles: on-chain randomness and price feeds (`docs/ORACLES.md`).
//!
//! ```ignore
//! #[btb::mutation(randomness)]
//! pub fn roll_dice() -> u8 {
//!     random::range(1, 7) as u8 // 1..=6
//! }
//!
//! #[btb::query]
//! pub fn btc() -> Option<u128> {
//!     price(0).map(|p| p.price)
//! }
//! ```
//!
//! `random::*` traps unless the calling function is a mutation or cron marked `randomness`
//! (`#[btb::mutation(randomness)]`, `#[btb::cron(randomness, every = "N blocks")]`): never in a
//! query or an action, never in an unmarked function. A function that uses randomness is always
//! charged its full gas limit, so a caller cannot make a bad outcome fail by running out of gas.
//!
//! The marker is checked at compile time: a query or an action cannot be marked.
//!
//! ```compile_fail
//! use btb_sdk::btb;
//!
//! #[btb::query(randomness)]
//! pub fn nope() -> u8 {
//!     0
//! }
//! ```
//!
//! ```compile_fail
//! use btb_sdk::btb;
//!
//! #[btb::action(randomness)]
//! pub fn nope() {}
//! ```

use crate::host::ecall;
use alloc::vec;
use alloc::vec::Vec;

/// Host-call numbers; checked against `btb_vm::host::abi` by a test in `btb-tests`.
pub mod num {
    pub const RANDOM_BYTES: u64 = 40;
    pub const PRICE: u64 = 41;
}

/// Most bytes one `random_bytes` call returns.
pub const MAX_RANDOM_BYTES: usize = 1024;

/// Randomness from the chain's beacon.
pub mod random {
    use super::*;

    /// `n` (at most 1024) random bytes. Unique per call within a transaction.
    pub fn bytes(n: usize) -> Vec<u8> {
        if n > MAX_RANDOM_BYTES {
            crate::host::abort("random::bytes: at most 1024 bytes per call");
        }
        let mut out = vec![0u8; n];
        ecall(
            num::RANDOM_BYTES,
            out.as_mut_ptr() as u64,
            n as u64,
            0,
            0,
            0,
        );
        out
    }

    /// A uniformly random `u64`.
    pub fn u64() -> u64 {
        let b = bytes(8);
        let mut a = [0u8; 8];
        a.copy_from_slice(&b);
        u64::from_le_bytes(a)
    }

    /// A uniformly random number in `lo..hi` (`hi` exclusive; `lo < hi`), without modulo bias:
    /// values from the biased tail of the `u64` range are redrawn. Each draw is one
    /// `random_bytes` call, and the redraw probability is below one half, so the loop ends
    /// after a handful of draws; after 64 failed draws it aborts instead of looping forever.
    pub fn range(lo: u64, hi: u64) -> u64 {
        if lo >= hi {
            crate::host::abort("random::range: empty range");
        }
        let span = hi - lo;
        // Largest multiple of `span` that fits: values at or above it are redrawn.
        let zone = u64::MAX - (u64::MAX % span + 1) % span;
        for _ in 0..64 {
            let v = u64();
            if v <= zone {
                return lo + v % span;
            }
        }
        crate::host::abort("random::range: too many redraws")
    }
}

/// The agreed price of a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Price {
    /// Price as an integer with `decimals` decimal places.
    pub price: u128,
    /// Block height of the update.
    pub height: u64,
    pub decimals: u8,
}

impl Price {
    /// Blocks since the update, as seen from `now` (use `block_height()`).
    pub fn age_blocks(&self, now: u64) -> u64 {
        now.saturating_sub(self.height)
    }
}

/// The latest agreed price of feed `pair` (an id from the chain's genesis config), or `None`
/// if the feed does not exist or has no price yet. Free of fees in queries; a mutation or cron
/// pays a per-read fee (extra gas).
pub fn price(pair: u32) -> Option<Price> {
    let mut out = [0u8; 32];
    let r = ecall(
        num::PRICE,
        u64::from(pair),
        out.as_mut_ptr() as u64,
        0,
        0,
        0,
    );
    if r != 0 {
        return None;
    }
    let mut p = [0u8; 16];
    p.copy_from_slice(&out[..16]);
    let mut h = [0u8; 8];
    h.copy_from_slice(&out[16..24]);
    Some(Price {
        price: u128::from_le_bytes(p),
        height: u64::from_le_bytes(h),
        decimals: out[24],
    })
}
