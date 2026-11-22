//! Example contract: DeFi yield data on chain (proposal 0006, built on the 0004 provider pattern).
//!
//! A **provider** publishes raw facts: liquidity pools, their latest stats (TVL, volume, fees,
//! APRs, the measured range APR and how often a range stayed in range), token safety grades and
//! other yields (staking, lending, tokenized T-bills, savings). Everything derived is computed
//! **on chain** with integer math, so anyone can recompute it:
//!
//! * the headline APR of a pool (its measured range APR when there is one, else base + reward),
//! * a **risk grade A..E** from a published formula over published facts ([`pool_risk`]),
//! * rankings (`top_pools`, `best_for`, `top_yields`) and chain indices (`index`).
//!
//! Publishing a stat also records it in an hourly history (`Hour`), so the chain keeps the trend
//! that an off-chain snapshot overwrites.
//!
//! Units (no floats anywhere):
//! * money: `u64` whole US dollars;
//! * rates and shares: basis points (`42.31 %` = `4_231`), `i64` for rates (they can be negative);
//! * pool fee: millionths (`fee_pips`, `0.3 %` = `3_000`), `0` when unknown;
//! * times: `as_of` is the provider's UTC unix seconds; `height` the block it was published in.
//!
//! Keys: a pool is `CHAIN_ID:POOL` (`8453:0xec3c...`), a token `CHAIN_ID:TOKEN`, a yield
//! `KIND:ASSET:VENUE` (`tbill:usd-3m:us-treasury`). ASCII letters, digits and `.-_:`; EVM
//! addresses in lower case.
//!
//! Build: `./build.sh` (writes `defi-yields.elf`).
#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use btb_sdk::btb;
use btb_sdk::codec::{Decode, DecodeError, Encode, Reader};
use btb_sdk::ensure;
use btb_sdk::host::{storage_get, storage_set, table_index_scan, Bound, Key};
use btb_sdk::prelude::*;
use btb_sdk::table::Order;

// Contracts run with the SDK's 512 KiB bump heap, never freed within a call (`btb-sdk`
// runtime), so every decoded row costs memory until the call ends. These bounds keep each call
// inside it with a margin; `crates/btb-tests/tests/defi_yields.rs` checks them with worst-case rows.

/// Most rows one publish call may carry (worst-case stats rows fit up to 80).
pub const MAX_BATCH: usize = 64;
/// Most history hours `pool` returns (14 days; 720 does not fit the heap, 480 does).
pub const MAX_HOURS: u32 = 336;
/// Most rows a ranking query returns.
pub const MAX_LIMIT: u32 = 100;
/// Most stats rows a ranking query decodes. Rankings read one stats row per pool (pair label
/// and risk are stored in it), in index order, so the bound is on rows looked at, not on pools.
pub const MAX_SCAN: usize = 600;
/// Most history rows one `prune_hours` call deletes.
pub const MAX_PRUNE: usize = 200;
/// A position may be at most this share of a pool's TVL (bps) to count as usable by `best_for`.
pub const MAX_POOL_SHARE_BPS: u64 = 200;
/// APRs outside this range (bps) are refused as publishing errors (-100 % .. 100,000 %).
pub const APR_MIN: i64 = -10_000;
pub const APR_MAX: i64 = 10_000_000;

// Yield kinds (`1 << kind` in a kind mask). Pools are always kind LP.
pub const KIND_LP: u8 = 1;
pub const KIND_STAKING: u8 = 2;
pub const KIND_LENDING: u8 = 3;
pub const KIND_TBILL: u8 = 4;
pub const KIND_SAVINGS: u8 = 5;
pub const KIND_DIVIDEND: u8 = 6;
pub const KIND_LIQUID_STAKING: u8 = 7;
pub const KIND_OTHER: u8 = 8;

// How fast money can leave a yield.
pub const LIQ_INSTANT: u8 = 0;
pub const LIQ_DAYS: u8 = 1;
pub const LIQ_LOCKED: u8 = 2;

// Token danger flags, as published by the provider's token scan.
pub const FLAG_MINTABLE: u32 = 1;
pub const FLAG_BLACKLIST: u32 = 2;
pub const FLAG_FEE_CHANGE: u32 = 4;
pub const FLAG_PROXY: u32 = 8;
pub const FLAG_HONEYPOT: u32 = 16;
pub const FLAG_LIQ_UNLOCKED: u32 = 32;

// Why a risk score is below 100 (bit mask in `Risk::reasons`).
pub const R_LOW_TVL: u32 = 1;
pub const R_IMPERMANENT_LOSS: u32 = 2;
pub const R_OUT_OF_RANGE: u32 = 4;
pub const R_HIGH_APR: u32 = 8;
pub const R_TOKEN_RISK: u32 = 16;
pub const R_UNGRADED_TOKEN: u32 = 32;
pub const R_LOCKED: u32 = 64;

// Grades: 1 = A (safest) .. 5 = E.
pub const GRADE_A: u8 = 1;
pub const GRADE_E: u8 = 5;

const PROVIDER: &[u8] = b"provider";

// ---------------------------------------------------------------------------------------------
// Tables (raw facts, written only by the provider)
// ---------------------------------------------------------------------------------------------

/// One liquidity pool: what does not change between hours.
#[btb::table]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    /// `CHAIN_ID:POOL`, e.g. `8453:0xec3c3755b9ded4a2960ef50c9b48c1694ce274c2`.
    #[primary]
    pub key: String,
    #[index]
    pub chain_id: u64,
    /// e.g. `Aerodrome`, `Uniswap`.
    pub dex: String,
    /// e.g. `V3`, `V4`; empty if none.
    pub version: String,
    /// Millionths, `0` when unknown.
    pub fee_pips: u32,
    /// Token addresses (or chain-native ids), lower case.
    pub token0: String,
    pub token1: String,
    pub symbol0: String,
    pub symbol1: String,
    /// Both sides are stablecoins.
    pub stable: bool,
    pub source: String,
    pub as_of: u64,
    pub height: u64,
}

/// The latest stats of a pool, replaced on every publish (an older one never replaces a newer).
/// The headline APR, pair label and risk are derived **on chain** when the row is written (the
/// risk from the token grades current at that moment; `pool` always recomputes it), so rankings
/// read one row per pool.
#[btb::table]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    #[primary]
    pub pool: String,
    /// Headline APR: `range_apr_bps` when measured, else `base + reward`.
    #[index]
    pub apr_bps: i64,
    /// `chain_id << 64 | (apr_bps - APR_MIN)`: a chain's pools in APR order.
    #[index]
    pub chain_apr: u128,
    /// The same, for stable pairs only (`None` keeps other pools out of this index).
    #[index]
    pub stable_apr: Option<u128>,
    /// `risk_grade << 64 | apr`: pools of one grade in APR order, all chains, so a grade filter
    /// reads only pools that pass it.
    #[index]
    pub grade_apr: u128,
    /// `chain_id << 67 | risk_grade << 64 | apr`: the same within a chain.
    #[index]
    pub chain_grade_apr: u128,
    /// `tvl_tier << 64 | apr` (tiers: under $100k, $1M, $10M, and above), so a TVL floor reads
    /// only the tiers that can pass it.
    #[index]
    pub tier_apr: u128,
    /// `chain_id << 66 | tvl_tier << 64 | apr`: the same within a chain.
    #[index]
    pub chain_tier_apr: u128,
    pub chain_id: u64,
    pub stable: bool,
    /// `SYMBOL0/SYMBOL1`, copied from the pool.
    pub pair: String,
    pub risk_score: u8,
    pub risk_grade: u8,
    pub risk_reasons: u32,
    /// What the two tokens' grades cost the score, when the row was first written or last
    /// `rescore`d (`token_penalty` of each, summed).
    pub token_points: u8,
    pub token_reasons: u32,
    pub tvl_usd: u64,
    pub volume_24h_usd: Option<u64>,
    /// `None` when the source has no fee data (estimated from the base APR in queries).
    pub fees_24h_usd: Option<u64>,
    pub base_apr_bps: i64,
    pub reward_apr_bps: i64,
    /// What a ±5 % range, reset once a day, really earned, from the pool's fee counters.
    pub range_apr_bps: Option<i64>,
    /// Days the range APR was measured over (0 when none).
    pub range_days: u32,
    /// Share of the time the range stayed in range, bps.
    pub in_range_bps: Option<u32>,
    /// Range result against just holding over `range_days`, bps.
    pub net_vs_hold_bps: Option<i64>,
    /// 24 h price change (or APY change) as the source reports it, bps.
    pub change_24h_bps: Option<i64>,
    pub as_of: u64,
    pub height: u64,
}

/// Running totals of a chain's pool stats, kept up to date on every stats write, so the chain
/// index needs no scan. `id` is `chain_id * 2 + 1` for stable pairs only, `chain_id * 2` for all.
#[btb::table]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainAgg {
    #[primary]
    pub id: u64,
    pub pools: u64,
    pub tvl_usd: u128,
    /// Sum of `apr_bps * tvl_usd`.
    pub weighted: i128,
}

/// One hour of a pool's history. `key` is `POOL|HOUR` (hour = `as_of / 3600`, ten digits), so a
/// primary range scan returns a pool's hours in time order.
#[btb::table]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hour {
    #[primary]
    pub key: String,
    pub hour: u64,
    pub tvl_usd: u64,
    pub apr_bps: i64,
    pub volume_24h_usd: Option<u64>,
    pub fees_24h_usd: Option<u64>,
}

/// A token's safety grade from the provider's scan.
#[btb::table]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grade {
    /// `CHAIN_ID:TOKEN`.
    #[primary]
    pub token: String,
    /// 0 (worst) ..= 100.
    pub score: u8,
    /// `FLAG_*` bits.
    pub flags: u32,
    pub holders: Option<u64>,
    /// Share of supply held by the 10 largest holders, bps.
    pub top10_bps: Option<u32>,
    pub as_of: u64,
    pub height: u64,
}

/// A yield that is not a pool: staking, lending, a T-bill, a savings rate, ...
#[btb::table]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YieldRate {
    /// `KIND:ASSET:VENUE`, e.g. `staking:eth:ethereum`.
    #[primary]
    pub key: String,
    #[index]
    pub kind: u8,
    /// `kind << 64 | (rate_bps - APR_MIN)`: a kind's yields in rate order.
    #[index]
    pub kind_rate: u128,
    /// e.g. `ETH`, `USDC`, `USD 3M`.
    pub asset: String,
    /// e.g. `Ethereum`, `Aave V3`, `US Treasury`.
    pub venue: String,
    /// `None` for yields that are not on a chain.
    pub chain_id: Option<u64>,
    pub rate_bps: i64,
    /// `LIQ_*`.
    pub liquidity: u8,
    pub tvl_usd: Option<u64>,
    pub source: String,
    pub as_of: u64,
    pub height: u64,
}

// ---------------------------------------------------------------------------------------------
// Inputs (tuples, so they need no extra encoding code)
// ---------------------------------------------------------------------------------------------

/// `(key, chain_id, (dex, version, fee_pips), (token0, token1), (symbol0, symbol1), stable,
/// (source, as_of))`
pub type PoolIn = (
    String,
    u64,
    (String, String, u32),
    (String, String),
    (String, String),
    bool,
    (String, u64),
);
/// `(pool, (tvl_usd, volume_24h_usd, fees_24h_usd), (base_apr_bps, reward_apr_bps),
/// (range_apr_bps, range_days, in_range_bps, net_vs_hold_bps), change_24h_bps, as_of)`
pub type StatIn = (
    String,
    (u64, Option<u64>, Option<u64>),
    (i64, i64),
    (Option<i64>, u32, Option<u32>, Option<i64>),
    Option<i64>,
    u64,
);
/// `(token, score, flags, (holders, top10_bps), as_of)`
pub type GradeIn = (String, u8, u32, (Option<u64>, Option<u32>), u64);
/// `(key, kind, (asset, venue), chain_id, rate_bps, liquidity, tvl_usd, (source, as_of))`
pub type YieldIn = (
    String,
    u8,
    (String, String),
    Option<u64>,
    i64,
    u8,
    Option<u64>,
    (String, u64),
);

/// One ranked pool: `(key, pair, chain_id, apr_bps, tvl_usd, grade, score)`.
pub type TopRow = (String, String, u64, i64, u64, u8, u8);
/// One ranked yield of any kind: `(key, kind, label, chain_id, rate_bps, tvl_usd, grade)`.
pub type YieldOut = (String, u8, String, Option<u64>, i64, Option<u64>, u8);

// ---------------------------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------------------------

/// A risk score (0..=100, higher is safer), its grade (`GRADE_A`..=`GRADE_E`) and the `R_*`
/// reasons it lost points for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Risk {
    pub score: u8,
    pub grade: u8,
    pub reasons: u32,
}

/// Everything about one pool in one answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolPayload {
    pub pool: Pool,
    pub stat: Option<Stat>,
    /// Published fees, or `tvl * base APR / 365` when the source has none.
    pub fees_24h_usd: Option<u64>,
    pub fees_estimated: bool,
    pub grade0: Option<Grade>,
    pub grade1: Option<Grade>,
    pub risk: Option<Risk>,
    /// Newest first.
    pub history: Vec<Hour>,
}

/// Yield of a chain's pools: count, total TVL, median APR and TVL-weighted APR.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexView {
    pub chain_id: u64,
    pub stable_only: bool,
    pub pools: u64,
    pub tvl_usd: u128,
    pub median_apr_bps: Option<i64>,
    pub tvl_weighted_apr_bps: Option<i64>,
}

/// Canonical encoding for a plain struct: its fields in order (like a tuple).
macro_rules! record {
    ($t:ident { $($f:ident),* $(,)? }) => {
        impl Encode for $t {
            fn encode_to(&self, out: &mut Vec<u8>) {
                $(self.$f.encode_to(out);)*
            }
        }
        impl Decode for $t {
            fn decode_from(r: &mut Reader<'_>) -> core::result::Result<Self, DecodeError> {
                Ok($t { $($f: Decode::decode_from(r)?,)* })
            }
        }
    };
}
record!(Risk {
    score,
    grade,
    reasons
});
record!(PoolPayload {
    pool,
    stat,
    fees_24h_usd,
    fees_estimated,
    grade0,
    grade1,
    risk,
    history
});
record!(IndexView {
    chain_id,
    stable_only,
    pools,
    tvl_usd,
    median_apr_bps,
    tvl_weighted_apr_bps
});

// ---------------------------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------------------------

fn provider() -> Option<Address> {
    storage_get(PROVIDER).and_then(|b| Address::decode(&b).ok())
}

fn only_provider() -> btb_sdk::Result<()> {
    ensure!(provider() == Some(caller()), "not the provider");
    Ok(())
}

/// Make the caller the provider. Once only.
#[btb::mutation]
pub fn init() -> btb_sdk::Result<()> {
    ensure!(provider().is_none(), "already initialised");
    storage_set(PROVIDER, &caller().encode());
    Ok(())
}

/// Hand the provider role to another account (key rotation).
#[btb::mutation]
pub fn set_provider(next: Address) -> btb_sdk::Result<()> {
    only_provider()?;
    storage_set(PROVIDER, &next.encode());
    Ok(())
}

#[btb::query]
pub fn get_provider() -> Option<Address> {
    provider()
}

fn check_batch(n: usize) -> btb_sdk::Result<()> {
    only_provider()?;
    ensure!(n > 0 && n <= MAX_BATCH, "batch must hold 1..=64 rows");
    Ok(())
}

/// ASCII letters, digits and `.-_:` only, 1..=max bytes. Case is kept: base58 ids (Solana, Sui)
/// are case-sensitive; EVM addresses are published in lower case by convention.
fn valid_id(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
}

/// Printable text without `|`, at most `max` bytes (may be empty).
fn valid_text(s: &str, max: usize) -> bool {
    s.len() <= max && s.bytes().all(|b| (0x20..0x7f).contains(&b) && b != b'|')
}

/// The chain id of a `CHAIN_ID:ID` key whose id part is valid (no allocation).
fn key_chain(key: &str) -> Option<u64> {
    let (c, id) = key.split_once(':')?;
    let chain = c.parse::<u64>().ok().filter(|c| *c > 0)?;
    (valid_id(id, 80) && !c.starts_with('0')).then_some(chain)
}

/// `CHAIN_ID:ID` with `CHAIN_ID` equal to `chain_id`.
fn chain_key_ok(key: &str, chain_id: u64) -> bool {
    key_chain(key) == Some(chain_id)
}

/// `hi << 64 | (v - APR_MIN)`, ordered like `(hi, v)`.
fn composite(hi: u64, v: i64) -> u128 {
    ((hi as u128) << 64) | (v - APR_MIN) as u64 as u128
}

/// `hi << (64 + bits) | mid << 64 | (v - APR_MIN)`, ordered like `(hi, mid, v)`; `mid < 2^bits`.
fn composite3(hi: u64, bits: u32, mid: u64, v: i64) -> u128 {
    ((hi as u128) << (64 + bits)) | composite(mid, v)
}

/// TVL tier: 0 under $100k, 1 under $1M, 2 under $10M, 3 from $10M.
fn tvl_tier(tvl: u64) -> u64 {
    match tvl {
        0..100_000 => 0,
        100_000..1_000_000 => 1,
        1_000_000..10_000_000 => 2,
        _ => 3,
    }
}

/// Write the ranking keys of a stats row from its chain, stability, grade, TVL and APR.
fn set_rank_keys(s: &mut Stat) {
    let tier = tvl_tier(s.tvl_usd);
    s.chain_apr = composite(s.chain_id, s.apr_bps);
    s.stable_apr = s.stable.then_some(s.chain_apr);
    s.grade_apr = composite(s.risk_grade as u64, s.apr_bps);
    s.chain_grade_apr = composite3(s.chain_id, 3, s.risk_grade as u64, s.apr_bps);
    s.tier_apr = composite(tier, s.apr_bps);
    s.chain_tier_apr = composite3(s.chain_id, 2, tier, s.apr_bps);
}

fn apr_ok(v: i64) -> bool {
    (APR_MIN..=APR_MAX).contains(&v)
}

/// Add or replace pools. A row older than the stored one (by `as_of`) is skipped.
/// Returns how many were written.
#[btb::mutation]
pub fn publish_pools(rows: Vec<PoolIn>) -> btb_sdk::Result<u32> {
    check_batch(rows.len())?;
    let mut written = 0;
    for (key, chain_id, (dex, version, fee_pips), (t0, t1), (s0, s1), stable, (src, as_of)) in rows
    {
        ensure!(chain_key_ok(&key, chain_id), "bad pool key");
        ensure!(fee_pips <= 1_000_000, "bad fee");
        ensure!(valid_id(&t0, 80) && valid_id(&t1, 80), "bad token");
        ensure!(
            valid_text(&dex, 32)
                && !dex.is_empty()
                && valid_text(&version, 16)
                && valid_text(&s0, 24)
                && valid_text(&s1, 24)
                && valid_text(&src, 32),
            "bad text"
        );
        let row = Pool {
            key: key.clone(),
            chain_id,
            dex,
            version,
            fee_pips,
            token0: t0,
            token1: t1,
            symbol0: s0,
            symbol1: s1,
            stable,
            source: src,
            as_of,
            height: block_height(),
        };
        match Pool::get_by_key(&key) {
            Some((_, old)) if old.as_of > as_of => continue,
            Some((id, _)) => {
                row.update(id);
            }
            None => {
                row.insert();
            }
        }
        written += 1;
    }
    Ok(written)
}

/// Replace pools' latest stats and record each in the hourly history. A stat older than the
/// stored one (by `as_of`) is skipped, so a replayed or late batch cannot roll a pool back.
/// Returns how many were written.
#[btb::mutation]
pub fn publish_stats(rows: Vec<StatIn>) -> btb_sdk::Result<u32> {
    check_batch(rows.len())?;
    let mut written = 0;
    // Token grades read in this batch (common tokens such as USDC repeat across pools).
    let mut grades: Vec<(String, Option<Grade>)> = Vec::new();
    // Chain totals change once per batch, not once per row: (id, pools, tvl, weighted) deltas.
    let mut deltas: Vec<(u64, i64, i128, i128)> = Vec::new();
    for (pool, (tvl, volume, fees), (base, reward), (range, days, in_range, net), change, as_of) in
        rows
    {
        ensure!(
            apr_ok(base) && apr_ok(reward) && range.is_none_or(apr_ok),
            "bad apr"
        );
        ensure!(in_range.is_none_or(|r| r <= 10_000), "bad in-range share");
        ensure!(range.is_none() || days > 0, "range apr needs its days");
        let old = Stat::get_by_pool(&pool);
        if matches!(&old, Some((_, o)) if o.as_of >= as_of) {
            continue;
        }
        let apr = headline_apr(range, base, reward);
        ensure!(apr_ok(apr), "bad apr");
        // An update reuses what the previous row derived from the pool and its token grades; only
        // a pool's first stats read the pool and the grades (see `rescore` after grades change).
        let (chain_id, stable, pair, token_points, token_reasons) = match &old {
            Some((_, o)) => (
                o.chain_id,
                o.stable,
                o.pair.clone(),
                o.token_points,
                o.token_reasons,
            ),
            None => {
                let Some((_, p)) = Pool::get_by_key(&pool) else {
                    btb_sdk::bail!("unknown pool");
                };
                let (points, reasons) = tokens_penalty(&mut grades, &p);
                (p.chain_id, p.stable, pair_of(&p), points, reasons)
            }
        };
        let risk = pool_risk_with(stable, tvl, in_range, apr, token_points, token_reasons);
        let mut row = Stat {
            pool: pool.clone(),
            apr_bps: apr,
            chain_apr: 0,
            stable_apr: None,
            grade_apr: 0,
            chain_grade_apr: 0,
            tier_apr: 0,
            chain_tier_apr: 0,
            chain_id,
            stable,
            pair,
            risk_score: risk.score,
            risk_grade: risk.grade,
            risk_reasons: risk.reasons,
            token_points,
            token_reasons,
            tvl_usd: tvl,
            volume_24h_usd: volume,
            fees_24h_usd: fees,
            base_apr_bps: base,
            reward_apr_bps: reward,
            range_apr_bps: range,
            range_days: days,
            in_range_bps: in_range,
            net_vs_hold_bps: net,
            change_24h_bps: change,
            as_of,
            height: block_height(),
        };
        set_rank_keys(&mut row);
        let hour = as_of / 3_600;
        // Stats only move forward, so an hour after the previous row's has no history row yet.
        let same_hour = matches!(&old, Some((_, o)) if o.as_of / 3_600 == hour);
        match old {
            Some((id, o)) => {
                add_delta(&mut deltas, o.chain_id, o.stable, -1, o.tvl_usd, o.apr_bps);
                row.update(id);
            }
            None => {
                row.insert();
            }
        }
        add_delta(&mut deltas, chain_id, stable, 1, tvl, apr);
        let hkey = hour_key(&pool, hour);
        let h = Hour {
            key: hkey.clone(),
            hour,
            tvl_usd: tvl,
            apr_bps: apr,
            volume_24h_usd: volume,
            fees_24h_usd: fees,
        };
        match same_hour.then(|| Hour::get_by_key(&hkey)).flatten() {
            Some((id, _)) => {
                h.update(id);
            }
            None => {
                h.insert();
            }
        }
        written += 1;
    }
    for (id, pools, tvl, weighted) in deltas {
        let (row_id, mut agg) = match ChainAgg::get_by_id(id) {
            Some((rid, a)) => (Some(rid), a),
            None => (
                None,
                ChainAgg {
                    id,
                    pools: 0,
                    tvl_usd: 0,
                    weighted: 0,
                },
            ),
        };
        agg.pools = (agg.pools as i64 + pools) as u64;
        agg.tvl_usd = (agg.tvl_usd as i128 + tvl) as u128;
        agg.weighted += weighted;
        match row_id {
            Some(rid) => {
                agg.update(rid);
            }
            None => {
                agg.insert();
            }
        }
    }
    Ok(written)
}

/// Add one stats row (`sign` +1) or remove it (-1) from the chain totals being built, for all
/// pools of the chain and, if stable, for its stable pairs.
fn add_delta(
    deltas: &mut Vec<(u64, i64, i128, i128)>,
    chain: u64,
    stable: bool,
    sign: i64,
    tvl: u64,
    apr: i64,
) {
    let ids = [Some(chain * 2), stable.then_some(chain * 2 + 1)];
    for id in ids.into_iter().flatten() {
        let i = match deltas.iter().position(|d| d.0 == id) {
            Some(i) => i,
            None => {
                deltas.push((id, 0, 0, 0));
                deltas.len() - 1
            }
        };
        let d = &mut deltas[i];
        d.1 += sign;
        d.2 += sign as i128 * tvl as i128;
        d.3 += sign as i128 * tvl as i128 * apr as i128;
    }
}

/// Add or replace token grades (an older grade never replaces a newer one).
#[btb::mutation]
pub fn publish_grades(rows: Vec<GradeIn>) -> btb_sdk::Result<u32> {
    check_batch(rows.len())?;
    let mut written = 0;
    for (token, score, flags, (holders, top10), as_of) in rows {
        let chain_ok = token
            .split_once(':')
            .is_some_and(|(c, t)| c.parse::<u64>().is_ok_and(|c| c > 0) && valid_id(t, 80));
        ensure!(chain_ok, "bad token key");
        ensure!(score <= 100, "bad score");
        ensure!(top10.is_none_or(|t| t <= 10_000), "bad top-10 share");
        let row = Grade {
            token: token.clone(),
            score,
            flags,
            holders,
            top10_bps: top10,
            as_of,
            height: block_height(),
        };
        match Grade::get_by_token(&token) {
            Some((_, old)) if old.as_of >= as_of => continue,
            Some((id, _)) => {
                row.update(id);
            }
            None => {
                row.insert();
            }
        }
        written += 1;
    }
    Ok(written)
}

/// Add or replace yields that are not pools (an older row never replaces a newer one).
#[btb::mutation]
pub fn publish_yields(rows: Vec<YieldIn>) -> btb_sdk::Result<u32> {
    check_batch(rows.len())?;
    let mut written = 0;
    for (key, kind, (asset, venue), chain_id, rate, liquidity, tvl, (src, as_of)) in rows {
        ensure!(
            (KIND_STAKING..=KIND_OTHER).contains(&kind),
            "bad kind (pools are published as pools)"
        );
        ensure!(
            valid_id(&key, 100) && key.matches(':').count() == 2,
            "bad yield key"
        );
        ensure!(apr_ok(rate), "bad apr");
        ensure!(liquidity <= LIQ_LOCKED, "bad liquidity");
        ensure!(chain_id != Some(0), "bad chain");
        ensure!(
            valid_text(&asset, 32)
                && !asset.is_empty()
                && valid_text(&venue, 48)
                && valid_text(&src, 32),
            "bad text"
        );
        let row = YieldRate {
            key: key.clone(),
            kind,
            kind_rate: composite(kind as u64, rate),
            asset,
            venue,
            chain_id,
            rate_bps: rate,
            liquidity,
            tvl_usd: tvl,
            source: src,
            as_of,
            height: block_height(),
        };
        match YieldRate::get_by_key(&key) {
            Some((_, old)) if old.as_of >= as_of => continue,
            Some((id, _)) => {
                row.update(id);
            }
            None => {
                row.insert();
            }
        }
        written += 1;
    }
    Ok(written)
}

/// Recompute the stored token part of the risk of `pools` from the current token grades (after
/// `publish_grades`). Rankings use the stored risk; `pool` always computes it live.
/// Returns how many were rescored.
#[btb::mutation]
pub fn rescore(pools: Vec<String>) -> btb_sdk::Result<u32> {
    check_batch(pools.len())?;
    let mut grades: Vec<(String, Option<Grade>)> = Vec::new();
    let mut n = 0;
    for key in pools {
        let (Some((_, p)), Some((id, mut s))) = (Pool::get_by_key(&key), Stat::get_by_pool(&key))
        else {
            continue;
        };
        let (points, reasons) = tokens_penalty(&mut grades, &p);
        let r = pool_risk_with(
            s.stable,
            s.tvl_usd,
            s.in_range_bps,
            s.apr_bps,
            points,
            reasons,
        );
        s.token_points = points;
        s.token_reasons = reasons;
        s.risk_score = r.score;
        s.risk_grade = r.grade;
        s.risk_reasons = r.reasons;
        set_rank_keys(&mut s);
        s.update(id);
        n += 1;
    }
    Ok(n)
}

/// Delete a pool's history beyond its newest `keep` hours, at most `MAX_PRUNE` rows per call
/// (call again while it returns `MAX_PRUNE`). Returns how many were deleted.
#[btb::mutation]
pub fn prune_hours(pool: String, keep: u32) -> btb_sdk::Result<u32> {
    only_provider()?;
    let lo = format!("{pool}|");
    let hi = format!("{pool}}}"); // '}' sorts right after '|'
                                  // Row ids only, newest first: nothing is decoded, so the heap stays small.
    let mut skip = keep as usize;
    let mut doomed: Vec<u64> = Vec::new();
    let mut cursor: Vec<u8> = Vec::new();
    loop {
        let page = table_index_scan(
            b"Hour",
            "key",
            Bound::Included(Key::Str(&lo)),
            Bound::Excluded(Key::Str(&hi)),
            true,
            256,
            &cursor,
        );
        for id in page.ids {
            if skip > 0 {
                skip -= 1;
            } else if doomed.len() < MAX_PRUNE {
                doomed.push(id);
            }
        }
        match page.next_cursor {
            Some(c) if doomed.len() < MAX_PRUNE => cursor = c,
            _ => break,
        }
    }
    for id in &doomed {
        Hour::delete(*id);
    }
    Ok(doomed.len() as u32)
}

// ---------------------------------------------------------------------------------------------
// Math and the risk formula (integer only; every division guarded)
// ---------------------------------------------------------------------------------------------

/// The measured range APR when there is one, else base + reward.
pub fn headline_apr(range: Option<i64>, base: i64, reward: i64) -> i64 {
    range.unwrap_or(base.saturating_add(reward))
}

fn hour_key(pool: &str, hour: u64) -> String {
    format!("{pool}|{hour:010}")
}

/// Hours of `pool`, oldest first (`.order(Order::Desc)` for newest first).
fn pool_hours(pool: &str) -> btb_sdk::table::Lookup<Hour> {
    let lo = format!("{pool}|");
    let hi = format!("{pool}}}"); // '}' sorts right after '|'
    Hour::by_key_range(lo..hi)
}

/// `tvl * apr / 365` (bps), the fee estimate for sources without fee data.
fn est_fees(tvl: u64, base_apr_bps: i64) -> Option<u64> {
    if base_apr_bps <= 0 {
        return Some(0);
    }
    u64::try_from(tvl as u128 * base_apr_bps as u128 / (10_000 * 365)).ok()
}

/// 0..=100 score to a grade: >= 85 A, >= 70 B, >= 55 C, >= 40 D, else E.
pub fn grade_of(score: u8) -> u8 {
    match score {
        85.. => 1,
        70..=84 => 2,
        55..=69 => 3,
        40..=54 => 4,
        _ => 5,
    }
}

fn score_risk(points: i64, reasons: u32) -> Risk {
    let score = points.clamp(0, 100) as u8;
    Risk {
        score,
        grade: grade_of(score),
        reasons,
    }
}

fn tvl_penalty(tvl: u64) -> i64 {
    match tvl {
        0..100_000 => 30,
        100_000..1_000_000 => 15,
        1_000_000..10_000_000 => 5,
        _ => 0,
    }
}

fn apr_penalty(apr: i64) -> i64 {
    if apr > 100_000 {
        20
    } else if apr > 20_000 {
        10
    } else {
        0
    }
}

/// What a token's grade costs the pool: `(points, reasons)`.
fn token_penalty(g: Option<&Grade>) -> (i64, u32) {
    let Some(g) = g else {
        return (5, R_UNGRADED_TOKEN);
    };
    let mut p = match g.score {
        0..50 => 25,
        50..70 => 10,
        _ => 0,
    };
    for (flag, cost) in [
        (FLAG_HONEYPOT, 40),
        (FLAG_MINTABLE, 10),
        (FLAG_BLACKLIST, 10),
        (FLAG_FEE_CHANGE, 10),
        (FLAG_PROXY, 5),
        (FLAG_LIQ_UNLOCKED, 5),
    ] {
        if g.flags & flag != 0 {
            p += cost;
        }
    }
    (p, if p > 0 { R_TOKEN_RISK } else { 0 })
}

/// The risk formula for a pool, from published facts only. Starts at 100 and subtracts:
///
/// | Fact | Points |
/// | --- | --- |
/// | TVL under $100k / $1M / $10M | 30 / 15 / 5 |
/// | Not a stable pair (impermanent loss) | 10 |
/// | Range stayed in range under 50 % / 80 % of the time | 15 / 5 |
/// | Headline APR over 1,000 % / 200 % (unlikely to last) | 20 / 10 |
/// | Each token: no grade | 5 |
/// | Each token: score under 50 / 70 | 25 / 10 |
/// | Each token flag: honeypot, mintable, blacklist, fee change, proxy, unlocked liquidity | 40, 10, 10, 10, 5, 5 |
///
/// Grade: >= 85 A, >= 70 B, >= 55 C, >= 40 D, else E.
pub fn pool_risk(
    stable: bool,
    tvl_usd: u64,
    in_range_bps: Option<u32>,
    apr_bps: i64,
    g0: Option<&Grade>,
    g1: Option<&Grade>,
) -> Risk {
    let (p0, r0) = token_penalty(g0);
    let (p1, r1) = token_penalty(g1);
    let points = u8::try_from(p0 + p1).unwrap_or(u8::MAX);
    pool_risk_with(stable, tvl_usd, in_range_bps, apr_bps, points, r0 | r1)
}

/// [`pool_risk`] with the token part already summed (`token_points`, `token_reasons`).
fn pool_risk_with(
    stable: bool,
    tvl_usd: u64,
    in_range_bps: Option<u32>,
    apr_bps: i64,
    token_points: u8,
    token_reasons: u32,
) -> Risk {
    let mut points = 100i64;
    let mut reasons = 0u32;
    let t = tvl_penalty(tvl_usd);
    if t > 0 {
        points -= t;
        reasons |= R_LOW_TVL;
    }
    if !stable {
        points -= 10;
        reasons |= R_IMPERMANENT_LOSS;
    }
    match in_range_bps {
        Some(r) if r < 5_000 => {
            points -= 15;
            reasons |= R_OUT_OF_RANGE;
        }
        Some(r) if r < 8_000 => {
            points -= 5;
            reasons |= R_OUT_OF_RANGE;
        }
        _ => {}
    }
    let a = apr_penalty(apr_bps);
    if a > 0 {
        points -= a;
        reasons |= R_HIGH_APR;
    }
    points -= token_points as i64;
    reasons |= token_reasons;
    score_risk(points, reasons)
}

/// The token part of a pool's risk, reading grades through a per-call cache.
fn tokens_penalty(cache: &mut Vec<(String, Option<Grade>)>, p: &Pool) -> (u8, u32) {
    let mut points = 0i64;
    let mut reasons = 0;
    for token in [&p.token0, &p.token1] {
        let at = match cache.iter().position(|(t, _)| t == token) {
            Some(i) => i,
            None => {
                cache.push((token.clone(), grade_for(p.chain_id, token)));
                cache.len() - 1
            }
        };
        let (pts, r) = token_penalty(cache[at].1.as_ref());
        points += pts;
        reasons |= r;
    }
    (u8::try_from(points).unwrap_or(u8::MAX), reasons)
}

/// The risk formula for a non-pool yield: a base by kind (T-bill 95, savings 90, lending and
/// staking 75, liquid staking 70, dividend 65, other 55), minus 5 if locked, the TVL penalty
/// (when the TVL is known) and the APR penalty.
pub fn yield_risk(y: &YieldRate) -> Risk {
    let mut points: i64 = match y.kind {
        KIND_TBILL => 95,
        KIND_SAVINGS => 90,
        KIND_LENDING | KIND_STAKING => 75,
        KIND_LIQUID_STAKING => 70,
        KIND_DIVIDEND => 65,
        _ => 55,
    };
    let mut reasons = 0;
    if y.liquidity == LIQ_LOCKED {
        points -= 5;
        reasons |= R_LOCKED;
    }
    if let Some(tvl) = y.tvl_usd {
        let t = tvl_penalty(tvl);
        if t > 0 {
            points -= t;
            reasons |= R_LOW_TVL;
        }
    }
    let a = apr_penalty(y.rate_bps);
    if a > 0 {
        points -= a;
        reasons |= R_HIGH_APR;
    }
    score_risk(points, reasons)
}

fn grade_for(chain_id: u64, token: &str) -> Option<Grade> {
    Grade::get_by_token(&format!("{chain_id}:{token}")).map(|(_, g)| g)
}

fn pair_of(p: &Pool) -> String {
    format!("{}/{}", p.symbol0, p.symbol1)
}

/// `grade` passes a `max_grade` filter (`0` = any).
fn grade_ok(grade: u8, max_grade: u8) -> bool {
    max_grade == 0 || grade <= max_grade
}

// ---------------------------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------------------------

/// Everything about one pool in one call: facts, latest stats, fee estimate, token grades,
/// risk (recomputed now, from the current grades) and up to `hours` (at most 336) of history,
/// newest first.
#[btb::query]
pub fn pool(key: String, hours: u32) -> Option<PoolPayload> {
    let (_, p) = Pool::get_by_key(&key)?;
    let stat = Stat::get_by_pool(&key).map(|(_, s)| s);
    let g0 = grade_for(p.chain_id, &p.token0);
    let g1 = grade_for(p.chain_id, &p.token1);
    let risk = stat.as_ref().map(|s| {
        pool_risk(
            p.stable,
            s.tvl_usd,
            s.in_range_bps,
            s.apr_bps,
            g0.as_ref(),
            g1.as_ref(),
        )
    });
    let (fees, estimated) = match &stat {
        Some(s) => match s.fees_24h_usd {
            Some(f) => (Some(f), false),
            None => (est_fees(s.tvl_usd, s.base_apr_bps), true),
        },
        None => (None, false),
    };
    let history = if hours == 0 {
        Vec::new()
    } else {
        pool_hours(&key)
            .order(Order::Desc)
            .limit(hours.min(MAX_HOURS) as usize)
            .map(|(_, h)| h)
            .collect()
    };
    Some(PoolPayload {
        pool: p,
        stat,
        fees_24h_usd: fees,
        fees_estimated: estimated,
        grade0: g0,
        grade1: g1,
        risk,
        history,
    })
}

/// Pools ranked by headline APR, highest first. Filters: `chain_id` (0 = all), `max_grade`
/// (`GRADE_A`..=`GRADE_E`, 0 = any), `min_tvl_usd`, `stable_only`. At most `limit` (100) rows,
/// from the `MAX_SCAN` highest-APR pools of the chain (or of all chains).
#[btb::query]
pub fn top_pools(
    chain_id: u64,
    max_grade: u8,
    min_tvl_usd: u64,
    stable_only: bool,
    limit: u32,
) -> Vec<TopRow> {
    ranked(chain_id, max_grade, min_tvl_usd, 0, stable_only, limit)
}

/// The best pools a position of `amount_usd` can use: like `top_pools`, but only pools where
/// the position would be at most 2 % of the TVL (so it neither dilutes the pool's fees much nor
/// moves its price on entry), and of grade `max_grade` or better.
#[btb::query]
pub fn best_for(amount_usd: u64, max_grade: u8, chain_id: u64, limit: u32) -> Vec<TopRow> {
    ranked(chain_id, max_grade, 0, amount_usd, false, limit)
}

/// All `u128` composite values for `hi` (`hi << 64 ..= hi << 64 | u64::MAX`).
fn composite_range(hi: u64) -> core::ops::RangeInclusive<u128> {
    ((hi as u128) << 64)..=(((hi as u128) << 64) | u64::MAX as u128)
}

fn ranked(
    chain_id: u64,
    max_grade: u8,
    min_tvl: u64,
    amount: u64,
    stable_only: bool,
    limit: u32,
) -> Vec<TopRow> {
    let limit = limit.clamp(1, MAX_LIMIT) as usize;
    // A position may be at most 2 % of a pool: the same as a TVL floor of 50x the position.
    let floor = min_tvl.max(
        u64::try_from(amount as u128 * 10_000 / MAX_POOL_SHARE_BPS as u128).unwrap_or(u64::MAX),
    );
    // Read only index ranges whose pools can pass the filters, each in APR order, and merge
    // them: one range per grade up to `max_grade`, else one per TVL tier from the floor's, else
    // the chain's (stable) pools. Without this, thousands of high-APR pools that fail the
    // filters filled the scan and good pools further down were never reached.
    let mut sources: Vec<btb_sdk::table::Lookup<Stat>> = Vec::new();
    if max_grade > 0 {
        for g in GRADE_A..=max_grade.min(GRADE_E) {
            sources.push(if chain_id == 0 {
                Stat::by_grade_apr_range(composite_range(g as u64))
            } else {
                Stat::by_chain_grade_apr_range(composite3_range(chain_id, 3, g as u64))
            });
        }
    } else if floor > 0 {
        for tier in tvl_tier(floor)..=3 {
            sources.push(if chain_id == 0 {
                Stat::by_tier_apr_range(composite_range(tier))
            } else {
                Stat::by_chain_tier_apr_range(composite3_range(chain_id, 2, tier))
            });
        }
    } else if chain_id == 0 {
        sources.push(Stat::by_apr_bps_range(..));
    } else if stable_only {
        sources.push(Stat::by_stable_apr_range(composite_range(chain_id)));
    } else {
        sources.push(Stat::by_chain_apr_range(composite_range(chain_id)));
    }
    let per_source = MAX_SCAN / sources.len();
    let mut out: Vec<TopRow> = Vec::new();
    for rows in sources {
        let mut taken = 0;
        for (_, s) in rows.order(Order::Desc).limit(per_source) {
            if taken >= limit {
                break;
            }
            if (chain_id != 0 && s.chain_id != chain_id)
                || s.tvl_usd < floor
                || (stable_only && !s.stable)
                || !grade_ok(s.risk_grade, max_grade)
            {
                continue;
            }
            // Each range is in APR order, so its first `limit` matches are all it can add.
            let row = (
                s.pool,
                s.pair,
                s.chain_id,
                s.apr_bps,
                s.tvl_usd,
                s.risk_grade,
                s.risk_score,
            );
            let at = out.partition_point(|x| x.3 >= row.3);
            out.insert(at, row);
            out.truncate(limit);
            taken += 1;
        }
    }
    out
}

/// All values of a `composite3(hi, bits, mid, _)` key for fixed `hi` and `mid`.
fn composite3_range(hi: u64, bits: u32, mid: u64) -> core::ops::RangeInclusive<u128> {
    let lo = composite3(hi, bits, mid, APR_MIN);
    lo..=(lo | u64::MAX as u128)
}

/// Yields of every kind in `kind_mask` (`1 << KIND_*`, 0 = all), pools included as kind LP,
/// ranked by rate, highest first, of grade `max_grade` or better (0 = any). At most `limit`.
#[btb::query]
pub fn top_yields(kind_mask: u32, max_grade: u8, limit: u32) -> Vec<YieldOut> {
    let limit = limit.clamp(1, MAX_LIMIT) as usize;
    let want = |k: u8| kind_mask == 0 || kind_mask & (1u32 << k) != 0;
    let mut all: Vec<YieldOut> = Vec::new();
    for kind in KIND_STAKING..=KIND_OTHER {
        if !want(kind) {
            continue;
        }
        let mut taken = 0;
        for (_, y) in YieldRate::by_kind_rate_range(composite_range(kind as u64))
            .order(Order::Desc)
            .limit(MAX_SCAN)
        {
            if taken >= limit {
                break;
            }
            let r = yield_risk(&y);
            if grade_ok(r.grade, max_grade) {
                let label = format!("{} on {}", y.asset, y.venue);
                insert_ranked(
                    &mut all,
                    (
                        y.key, kind, label, y.chain_id, y.rate_bps, y.tvl_usd, r.grade,
                    ),
                    limit,
                );
                taken += 1;
            }
        }
    }
    if want(KIND_LP) {
        for (key, pair, chain, apr, tvl, grade, _) in
            ranked(0, max_grade, 0, 0, false, limit as u32)
        {
            insert_ranked(
                &mut all,
                (key, KIND_LP, pair, Some(chain), apr, Some(tvl), grade),
                limit,
            );
        }
    }
    all
}

/// Insert keeping `v` sorted by rate (highest first) and at most `limit` long. Insertion rather
/// than a sort keeps the contract small (no core sort routines).
fn insert_ranked(v: &mut Vec<YieldOut>, row: YieldOut, limit: usize) {
    let at = v.partition_point(|x| x.4 >= row.4);
    if at < limit {
        v.insert(at, row);
        v.truncate(limit);
    }
}

/// The id of the `n`-th row (0-based, ascending) of `index` within `range`, reading row ids
/// only (nothing is decoded).
fn nth_id(index: &str, range: core::ops::RangeInclusive<u128>, n: u64) -> Option<u64> {
    let (lo, hi) = (*range.start(), *range.end());
    let mut left = n;
    let mut cursor: Vec<u8> = Vec::new();
    loop {
        let page = table_index_scan(
            b"Stat",
            index,
            Bound::Included(Key::U128(lo)),
            Bound::Included(Key::U128(hi)),
            false,
            256,
            &cursor,
        );
        if (page.ids.len() as u64) > left {
            return Some(page.ids[left as usize]);
        }
        left -= page.ids.len() as u64;
        cursor = page.next_cursor?;
    }
}

/// Yield index of a chain's pools (`stable_only` for stable pairs): pool count, total TVL,
/// median APR and TVL-weighted APR. Exact for any number of pools: the totals are kept on every
/// stats write and the median is found by counting row ids in APR order.
#[btb::query]
pub fn index(chain_id: u64, stable_only: bool) -> IndexView {
    let mut v = IndexView {
        chain_id,
        stable_only,
        ..IndexView::default()
    };
    let Some((_, agg)) = ChainAgg::get_by_id(chain_id * 2 + u64::from(stable_only)) else {
        return v;
    };
    if agg.pools == 0 {
        return v;
    }
    v.pools = agg.pools;
    v.tvl_usd = agg.tvl_usd;
    v.tvl_weighted_apr_bps = if agg.tvl_usd == 0 {
        None
    } else {
        i64::try_from(agg.weighted / agg.tvl_usd as i128).ok()
    };
    let name = if stable_only {
        "stable_apr"
    } else {
        "chain_apr"
    };
    v.median_apr_bps = nth_id(name, composite_range(chain_id), (agg.pools - 1) / 2)
        .and_then(Stat::get)
        .map(|s| s.apr_bps);
    v
}

/// Pools of a chain, a page at a time (at most 100): `(keys, cursor)`.
#[btb::query]
pub fn pools(chain_id: u64, limit: u32, cursor: Vec<u8>) -> (Vec<String>, Vec<u8>) {
    let page = Pool::by_chain_id(chain_id)
        .limit(limit.clamp(1, 100) as usize)
        .after(cursor)
        .page();
    (
        page.rows.into_iter().map(|(_, p)| p.key).collect(),
        page.next.unwrap_or_default(),
    )
}

/// One token's grade.
#[btb::query]
pub fn grade(token: String) -> Option<Grade> {
    Grade::get_by_token(&token).map(|(_, g)| g)
}

/// One non-pool yield with its risk.
#[btb::query]
pub fn yield_rate(key: String) -> Option<(YieldRate, Risk)> {
    YieldRate::get_by_key(&key).map(|(_, y)| {
        let r = yield_risk(&y);
        (y, r)
    })
}
