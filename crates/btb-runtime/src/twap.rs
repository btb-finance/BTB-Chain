//! Time-weighted average prices from cumulative checkpoints (`docs/ORACLES.md`).
//!
//! Every time a feed publishes a price at height `h`, the chain stores a checkpoint
//! `(h, cum, price)` under `50 12 pair h:u64be`, where `cum` is the running sum of
//! `price * blocks` up to `h` (the price in force at each block). The cumulative value at any
//! later height `x` is `cum + price * (x - h)` for the newest checkpoint at or before `x`, so
//! the average over `(x - w, x]` is `(cum(x) - cum(x - w)) / w`.
//!
//! Integer math only. Sums wrap modulo 2^128 and are subtracted with wrapping, which is exact
//! as long as the true sum `price * w` fits in a `u128` (a price below 2^128 / 10,000 always
//! does). Division floors. Same inputs, same bytes, on every node.

use crate::kv::{KvError, KvStore};
use crate::layout::TAG_ORACLE;

pub const KEY_CHECKPOINT: u8 = 0x12;

/// `50 12 pair:u32be height:u64be`: `cum:u128 LE || price:u128 LE`.
pub fn checkpoint_key(pair: u32, height: u64) -> Vec<u8> {
    let mut k = vec![TAG_ORACLE, KEY_CHECKPOINT];
    k.extend_from_slice(&pair.to_be_bytes());
    k.extend_from_slice(&height.to_be_bytes());
    k
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {
    pub height: u64,
    pub cum: u128,
    pub price: u128,
}

fn decode(k: &[u8], v: &[u8]) -> Option<Checkpoint> {
    if k.len() != 14 || v.len() != 32 {
        return None;
    }
    Some(Checkpoint {
        height: u64::from_be_bytes(k[6..14].try_into().ok()?),
        cum: u128::from_le_bytes(v[..16].try_into().ok()?),
        price: u128::from_le_bytes(v[16..].try_into().ok()?),
    })
}

/// The newest checkpoint at or before `x`.
fn at_or_before(kv: &impl KvStore, pair: u32, x: u64) -> Result<Option<Checkpoint>, KvError> {
    let start = checkpoint_key(pair, 0);
    let end = checkpoint_key(pair, x.saturating_add(1));
    // x == u64::MAX would wrap the end bound; use the pair's whole range then
    let end = if x == u64::MAX {
        let mut e = vec![TAG_ORACLE, KEY_CHECKPOINT];
        e.extend_from_slice(&pair.saturating_add(1).to_be_bytes());
        e
    } else {
        end
    };
    Ok(kv
        .scan(&start, Some(&end), true, 1)?
        .first()
        .and_then(|(k, v)| decode(k, v)))
}

/// Cumulative `price * blocks` at height `x`; `None` if the feed had no checkpoint by then.
pub fn cum_at(kv: &impl KvStore, pair: u32, x: u64) -> Result<Option<u128>, KvError> {
    Ok(at_or_before(kv, pair, x)?.map(|c| extend(c, x)))
}

fn extend(c: Checkpoint, x: u64) -> u128 {
    c.cum
        .wrapping_add(c.price.wrapping_mul(u128::from(x - c.height)))
}

/// Why a TWAP cannot be given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwapError {
    /// No checkpoint at all: the feed never published.
    NoPrice,
    /// `window` is zero, reaches before the feed's first checkpoint, or before height 0.
    BadWindow,
}

/// The average price over the `window` blocks ending at `now`.
pub fn twap(kv: &impl KvStore, pair: u32, now: u64, window: u64) -> Result<u128, TwapError> {
    let failed = |_e: KvError| TwapError::NoPrice;
    let end = cum_at(kv, pair, now).map_err(failed)?;
    let Some(end) = end else {
        return Err(TwapError::NoPrice);
    };
    if window == 0 || window > now {
        return Err(TwapError::BadWindow);
    }
    let Some(start) = cum_at(kv, pair, now - window).map_err(failed)? else {
        return Err(TwapError::BadWindow);
    };
    Ok(end.wrapping_sub(start) / u128::from(window))
}

/// Record that the feed now publishes `price` from `height` on, and drop checkpoints that no
/// window up to `keep_window` can reach any more (the newest one at or before the boundary is
/// kept: it anchors the oldest window).
pub fn record(
    kv: &mut impl KvStore,
    pair: u32,
    height: u64,
    price: u128,
    keep_window: u64,
) -> Result<(), KvError> {
    let cum = match at_or_before(&*kv, pair, height)? {
        Some(c) => extend(c, height),
        None => 0,
    };
    let mut v = cum.to_le_bytes().to_vec();
    v.extend_from_slice(&price.to_le_bytes());
    kv.put(&checkpoint_key(pair, height), &v)?;
    let boundary = height.saturating_sub(keep_window);
    let old = kv.scan(
        &checkpoint_key(pair, 0),
        Some(&checkpoint_key(pair, boundary.saturating_add(1))),
        false,
        64,
    )?;
    if old.len() > 1 {
        for (k, _) in &old[..old.len() - 1] {
            kv.delete(k)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv::MemKv;

    #[test]
    fn piecewise_average_is_exact_and_floors() {
        let mut kv = MemKv::default();
        record(&mut kv, 1, 10, 100, 1000).unwrap();
        record(&mut kv, 1, 20, 200, 1000).unwrap();
        record(&mut kv, 1, 40, 50, 1000).unwrap();
        // heights 11..=20 at 100, 21..=40 at 200, 41..=50 at 50
        // window (10, 50]: 10*100 + 20*200 + 10*50 = 5500 over 40 blocks = 137.5 -> 137
        assert_eq!(twap(&kv, 1, 50, 40), Ok(137));
        assert_eq!(twap(&kv, 1, 50, 10), Ok(50));
        assert_eq!(twap(&kv, 1, 50, 30), Ok((20 * 200 + 10 * 50) / 30));
        assert_eq!(twap(&kv, 1, 50, 41), Err(TwapError::BadWindow));
        assert_eq!(twap(&kv, 1, 50, 0), Err(TwapError::BadWindow));
        assert_eq!(twap(&kv, 2, 50, 5), Err(TwapError::NoPrice));
        // a window anchored before the first checkpoint is refused, not guessed
        assert_eq!(twap(&kv, 1, 12, 5), Err(TwapError::BadWindow));
    }

    #[test]
    fn same_height_overwrite_and_pruning() {
        let mut kv = MemKv::default();
        record(&mut kv, 1, 5, 10, 20).unwrap();
        record(&mut kv, 1, 5, 12, 20).unwrap();
        assert_eq!(twap(&kv, 1, 9, 4), Ok(12));
        for h in 6..=100 {
            record(&mut kv, 1, h, u128::from(h), 20).unwrap();
        }
        let n = kv
            .scan(
                &checkpoint_key(1, 0),
                Some(&checkpoint_key(1, u64::MAX)),
                false,
                1000,
            )
            .unwrap()
            .len();
        assert!(n <= 22, "{n}");
        // window of 20 still answerable; a price published at h is in force from h+1, so blocks 81..=100 carry 80..=99
        assert_eq!(twap(&kv, 1, 100, 20), Ok((80..=99u128).sum::<u128>() / 20));
        assert_eq!(twap(&kv, 1, 100, 30), Err(TwapError::BadWindow));
    }

    #[test]
    fn wrapping_sums_stay_exact_for_large_prices() {
        let mut kv = MemKv::default();
        let big = u128::MAX / 100_000;
        record(&mut kv, 1, 1, big, 10_000).unwrap();
        record(&mut kv, 1, 5000, big, 10_000).unwrap();
        assert_eq!(twap(&kv, 1, 9000, 8000), Ok(big));
    }
}
