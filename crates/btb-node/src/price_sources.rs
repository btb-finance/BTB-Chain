//! Production price sourcing for a validator (`docs/ORACLES.md`, section 2).
//!
//! A validator fetches each pair from several public exchange REST endpoints, takes the median of
//! the sources that are fresh and not outliers, and signs that as its observation. The chain then
//! takes the stake-weighted median over validators, so the final price is a median of medians.
//!
//! * **Adapters.** A source is `url + JSON pointer + scaling`: the pointer names the price in the
//!   JSON answer (a string such as `"65000.12"` or a number), it is parsed as a decimal text into
//!   an integer with the pair's decimals (no floats on the way in), then `* mul / div`. Presets
//!   ([`preset`]) fill in the common shapes; **they are examples**: endpoints, symbols and
//!   terms of use change, check them before relying on one.
//! * **Fetching** uses the same SSRF-safe client as HTTPS actions ([`crate::actions::fetch`]):
//!   https only, public addresses only (checked on the connection's own addresses), redirects
//!   only within the source's host, size and time caps.
//! * **Never blocks the node.** A background thread fetches on an interval and keeps the last
//!   good value of every source with its timestamp; the driver only reads that table.
//! * **Staleness, outliers, quorum.** [`aggregate`] drops values older than `max_age_ms`, needs
//!   at least `min_sources` of the rest, rejects values more than `outlier_bps` from the median
//!   and needs `min_sources` survivors again. Otherwise the validator reports nothing for that
//!   pair (and the chain keeps its previous price, whose height shows its age). Use three
//!   sources or more: with two, a single liar makes both rejected (a safe failure).

use crate::actions::{fetch, FetchError, FetchPolicy};
use btb_chain::actions::{host_allowed, parse_url};
use btb_codec::http::HttpRequest;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_SOURCES_PER_PAIR: usize = 16;
pub const DEFAULT_MAX_BYTES: u32 = 64 * 1024;
pub const DEFAULT_TIMEOUT_MS: u32 = 3_000;
const MAX_BYTES_LIMIT: u32 = 1024 * 1024;
const MAX_TIMEOUT_MS: u32 = 30_000;

/// One endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceConfig {
    pub name: String,
    pub url: String,
    /// JSON pointer (RFC 6901) to the price in the answer, for example `/data/amount`.
    pub pointer: String,
    /// The parsed price is multiplied by `mul` and divided by `div` (both at least 1): for
    /// example `div = 100` for an endpoint that reports cents instead of dollars.
    pub mul: u128,
    pub div: u128,
    pub max_bytes: u32,
    pub timeout_ms: u32,
}

impl SourceConfig {
    pub fn new(name: &str, url: &str, pointer: &str) -> Self {
        SourceConfig {
            name: name.into(),
            url: url.into(),
            pointer: pointer.into(),
            mul: 1,
            div: 1,
            max_bytes: DEFAULT_MAX_BYTES,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let bad = |m: &str| format!("price source {:?}: {m}", self.name);
        if self.name.is_empty() || self.name.len() > 64 {
            return Err(bad("needs a name of 1 to 64 characters"));
        }
        parse_url(&self.url).map_err(|e| bad(&format!("url: {e:?}")))?;
        if !self.pointer.starts_with('/') {
            return Err(bad("pointer must start with `/`"));
        }
        if self.mul == 0 || self.div == 0 {
            return Err(bad("mul and div must be at least 1"));
        }
        if self.max_bytes == 0 || self.max_bytes > MAX_BYTES_LIMIT {
            return Err(bad("max_bytes must be 1..=1048576"));
        }
        if self.timeout_ms == 0 || self.timeout_ms > MAX_TIMEOUT_MS {
            return Err(bad("timeout_ms must be 1..=30000"));
        }
        Ok(())
    }

    /// The host allowlist of this source: its own host (and port, when not the default).
    fn hosts(&self) -> Result<Vec<String>, FetchError> {
        let u = parse_url(&self.url).map_err(|_| FetchError::Blocked)?;
        let default = if u.https { 443 } else { 80 };
        Ok(vec![if u.port == default {
            u.host
        } else {
            format!("{}:{}", u.host, u.port)
        }])
    }
}

/// Built-in source shapes. **Examples**, not endorsements: the URLs and answer formats belong
/// to third parties and may change or be unavailable in your region; verify before use.
/// `symbol` is the exchange's own market name (`BTCUSDT`, `BTC-USD`, `btcusd`, ...).
pub fn preset(name: &str, symbol: &str) -> Result<SourceConfig, String> {
    if symbol.is_empty()
        || symbol.len() > 24
        || !symbol
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(format!(
            "preset symbol {symbol:?}: use letters, digits, - and _"
        ));
    }
    let (url, pointer) = match name {
        "binance-example" => (
            format!("https://api.binance.com/api/v3/ticker/price?symbol={symbol}"),
            "/price",
        ),
        "coinbase-example" => (
            format!("https://api.coinbase.com/v2/prices/{symbol}/spot"),
            "/data/amount",
        ),
        "bitstamp-example" => (
            format!("https://www.bitstamp.net/api/v2/ticker/{symbol}/"),
            "/last",
        ),
        "okx-example" => (
            format!("https://www.okx.com/api/v5/market/ticker?instId={symbol}"),
            "/data/0/last",
        ),
        other => return Err(format!("unknown price preset {other:?}")),
    };
    Ok(SourceConfig::new(
        &format!("{name}:{symbol}"),
        &url,
        pointer,
    ))
}

/// The sources of one pair and how to combine them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairSources {
    /// The chain's pair, by name (`BTC/USD`) or numeric id; resolved against the genesis.
    pub pair: String,
    pub sources: Vec<SourceConfig>,
    /// Fewest fresh, agreeing sources needed to report (at least 1; use 3 or more in production).
    pub min_sources: usize,
    /// A source value older than this is not used.
    pub max_age_ms: u64,
    /// A value further than this from the median is an outlier and dropped.
    pub outlier_bps: u32,
}

impl PairSources {
    pub fn validate(&self) -> Result<(), String> {
        if self.sources.is_empty() || self.sources.len() > MAX_SOURCES_PER_PAIR {
            return Err(format!(
                "price pair {}: 1 to {MAX_SOURCES_PER_PAIR} sources",
                self.pair
            ));
        }
        if self.min_sources == 0 || self.min_sources > self.sources.len() {
            return Err(format!(
                "price pair {}: min_sources must be 1..={}",
                self.pair,
                self.sources.len()
            ));
        }
        if self.max_age_ms == 0 || self.outlier_bps > 10_000 {
            return Err(format!(
                "price pair {}: bad max_age_ms or outlier_bps",
                self.pair
            ));
        }
        let mut names = std::collections::BTreeSet::new();
        for s in &self.sources {
            s.validate()?;
            if !names.insert(&s.name) {
                return Err(format!(
                    "price pair {}: source {:?} twice",
                    self.pair, s.name
                ));
            }
        }
        Ok(())
    }

    /// Parse one entry of the node config's `price_sources` array.
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let pair = match v.get("pair") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => return Err("price_sources: every entry needs `pair`".into()),
        };
        let mut sources = Vec::new();
        for s in v
            .get("sources")
            .and_then(Value::as_array)
            .ok_or("price_sources: `sources` must be an array")?
        {
            let txt = |k: &str| s.get(k).and_then(Value::as_str);
            let num = |k: &str, d: u64| s.get(k).and_then(Value::as_u64).unwrap_or(d);
            let mut c = if let Some(p) = txt("preset") {
                preset(
                    p,
                    txt("symbol").ok_or("price_sources: a preset needs `symbol`")?,
                )?
            } else {
                SourceConfig::new(
                    txt("name").ok_or("price_sources: a source needs `name`")?,
                    txt("url").ok_or("price_sources: a source needs `url`")?,
                    txt("pointer").ok_or("price_sources: a source needs `pointer`")?,
                )
            };
            if let Some(n) = txt("name") {
                c.name = n.into();
            }
            c.mul = u128::from(num("mul", 1));
            c.div = u128::from(num("div", 1));
            c.max_bytes = u32::try_from(num("max_bytes", u64::from(DEFAULT_MAX_BYTES)))
                .map_err(|_| "price_sources: max_bytes too large")?;
            c.timeout_ms = u32::try_from(num("timeout_ms", u64::from(DEFAULT_TIMEOUT_MS)))
                .map_err(|_| "price_sources: timeout_ms too large")?;
            sources.push(c);
        }
        let n = sources.len();
        let p = PairSources {
            pair,
            sources,
            min_sources: v
                .get("min_sources")
                .and_then(Value::as_u64)
                .map_or(n.min(2), |x| x as usize),
            max_age_ms: v
                .get("max_age_ms")
                .and_then(Value::as_u64)
                .unwrap_or(30_000),
            outlier_bps: v
                .get("outlier_bps")
                .and_then(Value::as_u64)
                .map_or(200, |x| x.min(u64::from(u32::MAX)) as u32),
        };
        p.validate()?;
        Ok(p)
    }
}

/// The `price_sources` array of the node config.
pub fn from_json_list(v: &Value) -> Result<Vec<PairSources>, String> {
    v.as_array()
        .ok_or("price_sources must be an array")?
        .iter()
        .map(PairSources::from_json)
        .collect()
}

// ---------------------------------------------------------------- parsing answers

/// Why a source gave no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceError {
    Fetch(FetchError),
    /// The HTTP status was not 2xx.
    Status(u16),
    /// Not JSON, or the pointer finds nothing, or the value is not a positive decimal that
    /// fits after scaling.
    Malformed,
}

/// Parse a decimal text (`"65000.12"`, `"6.5e4"`, `"0.0000123"`) into an integer with
/// `decimals` decimal places, rounding half up. `None` for signs, junk, zero or overflow.
pub fn parse_scaled(text: &str, decimals: u8) -> Option<u128> {
    let t = text.trim();
    let (mant, exp) = match t.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i32>().ok()?),
        None => (t, 0),
    };
    if mant.is_empty() || mant.len() > 80 || exp.abs() > 60 {
        return None;
    }
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    if int.is_empty() && frac.is_empty()
        || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let digits: String = int.chars().chain(frac.chars()).collect();
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return None; // zero is not a price
    }
    // value = digits * 10^(exp - frac.len()); the result is value * 10^decimals
    let shift = i64::from(exp) - frac.len() as i64 + i64::from(decimals);
    if digits.len() > 39 {
        return None;
    }
    let m: u128 = digits.parse().ok()?;
    let r = if shift >= 0 {
        m.checked_mul(10u128.checked_pow(u32::try_from(shift).ok()?)?)?
    } else {
        let d = u32::try_from(-shift).ok()?;
        if d > 38 {
            0
        } else {
            let p = 10u128.pow(d);
            (m / p) + u128::from(m % p >= p.div_ceil(2))
        }
    };
    (r > 0).then_some(r)
}

/// Read the price out of an HTTP answer according to `src`.
pub fn parse_response(
    src: &SourceConfig,
    decimals: u8,
    status: u16,
    body: &[u8],
) -> Result<u128, SourceError> {
    if !(200..300).contains(&status) {
        return Err(SourceError::Status(status));
    }
    let doc: Value = serde_json::from_slice(body).map_err(|_| SourceError::Malformed)?;
    let v = doc.pointer(&src.pointer).ok_or(SourceError::Malformed)?;
    let text = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return Err(SourceError::Malformed),
    };
    let scaled = parse_scaled(&text, decimals).ok_or(SourceError::Malformed)?;
    let p = scaled.checked_mul(src.mul).ok_or(SourceError::Malformed)? / src.div;
    if p == 0 {
        return Err(SourceError::Malformed);
    }
    Ok(p)
}

/// Fetch one source and parse it.
pub fn fetch_source(
    src: &SourceConfig,
    decimals: u8,
    policy: &FetchPolicy,
) -> Result<u128, SourceError> {
    let hosts = src.hosts().map_err(SourceError::Fetch)?;
    let mut req = HttpRequest::get(src.url.clone());
    req.max_response_bytes = src.max_bytes;
    req.timeout_ms = src.timeout_ms;
    req.headers
        .push(("accept".into(), "application/json".into()));
    // the allowlist check, with the parsed url, before any network use
    let parsed = parse_url(&src.url).map_err(|_| SourceError::Fetch(FetchError::Blocked))?;
    if !host_allowed(&hosts, &parsed) {
        return Err(SourceError::Fetch(FetchError::Blocked));
    }
    let resp = fetch(&req, &hosts, policy).map_err(SourceError::Fetch)?;
    parse_response(src, decimals, resp.status, &resp.body)
}

// ---------------------------------------------------------------- combining

/// Why a pair has no value now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggError {
    /// Fewer than `min_sources` fresh values.
    NotEnoughFresh,
    /// Fewer than `min_sources` values within `outlier_bps` of the median.
    NotEnoughAgreeing,
}

fn median(sorted: &[u128]) -> u128 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        let (a, b) = (sorted[n / 2 - 1], sorted[n / 2]);
        a + (b - a) / 2
    }
}

/// Combine `(value, age_ms)` samples: drop stale, median, drop outliers, median again.
/// Integer math only; the order of `samples` does not matter.
pub fn aggregate(
    samples: &[(u128, u64)],
    min_sources: usize,
    max_age_ms: u64,
    outlier_bps: u32,
) -> Result<u128, AggError> {
    let mut v: Vec<u128> = samples
        .iter()
        .filter(|(_, age)| *age <= max_age_ms)
        .map(|(p, _)| *p)
        .collect();
    if v.is_empty() || v.len() < min_sources {
        return Err(AggError::NotEnoughFresh);
    }
    v.sort_unstable();
    let m = median(&v);
    v.retain(|p| p.abs_diff(m).saturating_mul(10_000) <= m.saturating_mul(u128::from(outlier_bps)));
    if v.is_empty() || v.len() < min_sources {
        return Err(AggError::NotEnoughAgreeing);
    }
    Ok(median(&v))
}

// ---------------------------------------------------------------- the background fetcher

/// Counters for logs and tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FetchStats {
    pub ok: u64,
    pub failed: u64,
}

struct Shared {
    /// `(pair id, source index)` -> last good value and when it was fetched.
    latest: Mutex<BTreeMap<(u32, usize), (u128, Instant)>>,
    stats: Mutex<FetchStats>,
    stop: AtomicBool,
}

/// A pair as the fetcher works on it: chain id and decimals resolved.
#[derive(Debug, Clone)]
pub struct ResolvedPair {
    pub id: u32,
    pub decimals: u8,
    pub cfg: PairSources,
}

/// Resolve configured pairs against the chain's feeds (by name or id).
pub fn resolve(
    cfgs: &[PairSources],
    feeds: &[btb_chain::PairConfig],
) -> Result<Vec<ResolvedPair>, String> {
    cfgs.iter()
        .map(|c| {
            feeds
                .iter()
                .find(|f| f.name == c.pair || f.id.to_string() == c.pair)
                .map(|f| ResolvedPair {
                    id: f.id,
                    decimals: f.decimals,
                    cfg: c.clone(),
                })
                .ok_or_else(|| format!("price_sources: the chain has no pair {:?}", c.pair))
        })
        .collect()
}

/// Owns the fetch thread; dropping the last handle stops it.
pub struct PriceFetcher {
    shared: Arc<Shared>,
    pairs: Vec<ResolvedPair>,
}

impl Drop for PriceFetcher {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// How a source is fetched; replaced in tests of the aggregation path.
pub type FetchFn = dyn Fn(&SourceConfig, u8) -> Result<u128, SourceError> + Send + Sync;

impl PriceFetcher {
    /// Start fetching every `interval`. The first round begins immediately; nothing here ever
    /// blocks the caller.
    pub fn spawn(pairs: Vec<ResolvedPair>, interval: Duration, policy: FetchPolicy) -> Self {
        let f: Arc<FetchFn> = Arc::new(move |s: &SourceConfig, d: u8| fetch_source(s, d, &policy));
        Self::spawn_with(pairs, interval, f)
    }

    pub fn spawn_with(pairs: Vec<ResolvedPair>, interval: Duration, fetcher: Arc<FetchFn>) -> Self {
        let shared = Arc::new(Shared {
            latest: Mutex::new(BTreeMap::new()),
            stats: Mutex::new(FetchStats::default()),
            stop: AtomicBool::new(false),
        });
        let (sh, ps) = (shared.clone(), pairs.clone());
        std::thread::Builder::new()
            .name("btb-price-fetch".into())
            .spawn(move || {
                while !sh.stop.load(Ordering::Relaxed) {
                    let round = Instant::now();
                    std::thread::scope(|scope| {
                        for p in &ps {
                            for (i, s) in p.cfg.sources.iter().enumerate() {
                                let (sh, fetcher) = (&sh, &fetcher);
                                scope.spawn(move || {
                                    if sh.stop.load(Ordering::Relaxed) {
                                        return;
                                    }
                                    match fetcher(s, p.decimals) {
                                        Ok(v) => {
                                            sh.latest
                                                .lock()
                                                .expect("price table")
                                                .insert((p.id, i), (v, Instant::now()));
                                            sh.stats.lock().expect("stats").ok += 1;
                                        }
                                        // keep the last good value; it ages out by itself
                                        Err(_) => sh.stats.lock().expect("stats").failed += 1,
                                    }
                                });
                            }
                        }
                    });
                    while round.elapsed() < interval && !sh.stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            })
            .expect("spawn price fetch thread");
        PriceFetcher { shared, pairs }
    }

    /// This validator's value for `pair` now, or `None` (no pair configured, too few fresh or
    /// agreeing sources). Cheap: reads the table the thread keeps.
    pub fn value_at(&self, pair: u32, now: Instant) -> Option<u128> {
        let p = self.pairs.iter().find(|p| p.id == pair)?;
        let table = self.shared.latest.lock().expect("price table");
        let samples: Vec<(u128, u64)> = (0..p.cfg.sources.len())
            .filter_map(|i| table.get(&(pair, i)))
            .map(|(v, at)| (*v, now.saturating_duration_since(*at).as_millis() as u64))
            .collect();
        aggregate(
            &samples,
            p.cfg.min_sources,
            p.cfg.max_age_ms,
            p.cfg.outlier_bps,
        )
        .ok()
    }

    pub fn value(&self, pair: u32) -> Option<u128> {
        self.value_at(pair, Instant::now())
    }

    pub fn stats(&self) -> FetchStats {
        *self.shared.stats.lock().expect("stats")
    }

    /// Wrap into the node's [`crate::oracle::PriceSource`]; the thread stops when the source
    /// (and all its clones) are dropped.
    pub fn into_source(self) -> crate::oracle::PriceSource {
        let me = Arc::new(self);
        crate::oracle::PriceSource::new(move |pair| me.value(pair))
    }
}
