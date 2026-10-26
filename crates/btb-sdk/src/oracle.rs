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
    pub const REPORT_REQUEST: u64 = 42;
    pub const PRICE_TWAP: u64 = 43;
    /// Status codes of `price` and `price_twap`.
    pub const PRICE_PAUSED: u64 = 1;
    pub const PRICE_BAD_WINDOW: u64 = 2;
    pub const NOT_FOUND: u64 = u64::MAX;
}

/// Largest TWAP window `price_twap` accepts, in blocks.
pub const MAX_TWAP_WINDOW: u64 = 10_000;

/// Largest report `report::request` accepts.
pub const MAX_REPORT_BYTES: usize = 1024;

/// Reports for other chains (`docs/ORACLES.md`, section 4).
pub mod report {
    use super::*;

    /// Ask the beacon committee to threshold-sign `data` for other chains. Returns the report id.
    /// Mutations and crons only. The signature covers the chain id, this contract's address, the
    /// id and `data`; relayers fetch it with the `btb_oracleReport` RPC and a verifier on the
    /// other chain checks it against the committee's group key.
    pub fn request(data: &[u8]) -> u64 {
        if data.len() > MAX_REPORT_BYTES {
            crate::host::abort("report::request: at most 1024 bytes");
        }
        let id = ecall(
            num::REPORT_REQUEST,
            data.as_ptr() as u64,
            data.len() as u64,
            0,
            0,
            0,
        );
        if id == u64::MAX {
            crate::host::abort("report::request: this chain has no oracle committee");
        }
        id
    }
}

/// HTTPS actions (`docs/ORACLES.md`, section 3).
///
/// ```ignore
/// // Phase 1, read-only, in the requesting transaction: what should every validator fetch?
/// #[btb::action]
/// pub fn weather(city: String) -> HttpRequest {
///     HttpRequest::get(format!("https://api.example.com/w/{city}"))
/// }
///
/// // Phase 2, per validator, pure: keep only what is stable. The validators sign the encoding
/// // of the return value; the string names the action.
/// #[btb::transform("weather")]
/// pub fn weather_transform(resp: HttpResponse) -> i32 {
///     /* parse resp.body */
///     0
/// }
///
/// // Phase 3: the chain delivers the agreed result (or an error status) in a later block.
/// #[btb::mutation]
/// pub fn weather_callback(id: Address, args: Vec<u8>, status: u8, data: Vec<u8>) {
///     action::require_callback();
///     if status == action::STATUS_OK { /* i32::decode(&data) ... */ }
/// }
/// ```
pub mod action {
    use btb_codec::Address;

    pub use btb_codec::http::{HttpRequest, HttpResponse, METHOD_GET, METHOD_POST};

    /// The caller address the chain gives a callback. No account or contract has it.
    pub const CALLER: Address = Address(*b"btb.action.callback.caller.v1\0\0\0");

    /// Delivered: `data` is the transformed result that at least two thirds of the stake signed.
    pub const STATUS_OK: u8 = 0;
    /// Agreed failure: at least two thirds of the stake report the same error; `data` is a text.
    pub const STATUS_FAILED: u8 = 1;
    /// The validators answered differently and no result can reach two thirds any more.
    pub const STATUS_DISAGREEMENT: u8 = 2;
    /// Not enough validators answered before the deadline.
    pub const STATUS_TIMEOUT: u8 = 3;

    /// Is the running call the chain delivering an action result?
    pub fn from_oracle() -> bool {
        crate::host::caller() == CALLER
    }

    /// Abort unless the chain is delivering an action result. A callback must call this first:
    /// the chain also refuses to run it from a transaction, this is the second lock.
    pub fn require_callback() {
        if !from_oracle() {
            crate::host::abort("callback: only the chain may deliver action results");
        }
    }
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

/// Why a price could not be given. Contracts that move money on prices must handle `Paused`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceError {
    /// The feed does not exist or has no price yet.
    NotFound,
    /// The feed's circuit breaker is open: the price moved too fast and has not settled.
    Paused,
    /// (`price_twap` only) the window is 0, above `MAX_TWAP_WINDOW`, or longer than the feed's
    /// history.
    BadWindow,
}

fn status(r: u64) -> PriceError {
    match r {
        num::PRICE_PAUSED => PriceError::Paused,
        num::PRICE_BAD_WINDOW => PriceError::BadWindow,
        _ => PriceError::NotFound,
    }
}

/// A time-weighted average price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Twap {
    /// Average over the window, an integer with `decimals` decimal places (floor division).
    pub price: u128,
    /// The window in blocks.
    pub window: u32,
    pub decimals: u8,
}

/// The average price of feed `pair` over the last `window` blocks (`1..=MAX_TWAP_WINDOW`,
/// not longer than the feed's history). A price published at block `h` counts from block
/// `h + 1`. Free in queries; a mutation or cron pays a per-read fee (extra gas).
pub fn price_twap(pair: u32, window: u64) -> Result<Twap, PriceError> {
    let mut out = [0u8; 32];
    let r = ecall(
        num::PRICE_TWAP,
        u64::from(pair),
        window,
        out.as_mut_ptr() as u64,
        0,
        0,
    );
    if r != 0 {
        return Err(status(r));
    }
    let mut p = [0u8; 16];
    p.copy_from_slice(&out[..16]);
    let mut w = [0u8; 4];
    w.copy_from_slice(&out[16..20]);
    Ok(Twap {
        price: u128::from_le_bytes(p),
        window: u32::from_le_bytes(w),
        decimals: out[20],
    })
}

/// The latest agreed price of feed `pair` (an id from the chain's genesis config), or `None`
/// if the feed does not exist, has no price yet, or is paused by its circuit breaker (use
/// [`price_checked`] to tell them apart). Free of fees in queries; a mutation or cron
/// pays a per-read fee (extra gas).
pub fn price(pair: u32) -> Option<Price> {
    price_checked(pair).ok()
}

/// Like [`price`], but says why there is none: `NotFound`, or `Paused` while the feed's circuit
/// breaker is open (`docs/ORACLES.md`). Same fees as `price`.
pub fn price_checked(pair: u32) -> Result<Price, PriceError> {
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
        return Err(status(r));
    }
    let mut p = [0u8; 16];
    p.copy_from_slice(&out[..16]);
    let mut h = [0u8; 8];
    h.copy_from_slice(&out[16..24]);
    Ok(Price {
        price: u128::from_le_bytes(p),
        height: u64::from_le_bytes(h),
        decimals: out[24],
    })
}
