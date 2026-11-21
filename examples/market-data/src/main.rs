//! Example contract: market data on chain (proposal 0004, phase A).
//!
//! A **provider** publishes raw facts for any exchange (companies, delayed quotes, daily bars,
//! fundamentals). Everything derived (change, P/E, dividend yield, market cap, 20-day high and
//! drawdown, SMA, sector median P/E and the stock's premium to it) is computed **at read time in
//! the query**, from the published rows, with integer math only, so anyone can check it.
//!
//! One call returns the whole stock: `stock("PSX:OGDC", include)`, where `include` is a bit mask
//! of [`INC_COMPANY`], [`INC_QUOTE`], ... (`0` = everything), like `?include=quote,ratios`.
//!
//! Units (no floats anywhere):
//! * prices and per-share amounts: `i64` in 1e-4 of the local currency (`312.55` = `3_125_500`);
//! * money totals (revenue, profit, value traded): `i64` in whole local currency units;
//! * percentages: basis points (`-0.08 %` = `-8`); ratios (P/E): 1e-4 (`8.5x` = `85_000`);
//! * times: `as_of` is the provider's UTC unix seconds; `height` the block it was published in.
//!
//! Build: `./build.sh` (writes `market-data.elf`).
#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use btb_sdk::btb;
use btb_sdk::codec::{Decode, DecodeError, Encode, Reader};
use btb_sdk::ensure;
use btb_sdk::host::{storage_get, storage_set};
use btb_sdk::prelude::*;
use btb_sdk::table::Order;

/// Fixed-point scale of prices and ratios.
pub const SCALE: i64 = 10_000;
/// Most rows one publish call may carry.
pub const MAX_BATCH: usize = 500;
/// How many daily bars `stock` returns at most.
pub const MAX_BARS: u32 = 400;

pub const INC_COMPANY: u32 = 1;
pub const INC_QUOTE: u32 = 2;
pub const INC_PRICES: u32 = 4;
pub const INC_FUNDAMENTALS: u32 = 8;
pub const INC_RATIOS: u32 = 16;
pub const INC_ANALYTICS: u32 = 32;
pub const INC_SECTOR: u32 = 64;

const PROVIDER: &[u8] = b"provider";

// ---------------------------------------------------------------------------------------------
// Tables (raw facts, written only by the provider)
// ---------------------------------------------------------------------------------------------

/// One exchange: the local mechanics every stock of it shares.
#[btb::table]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exchange {
    /// Short code used in symbols, e.g. `PSX`, `NSE`, `TADAWUL`.
    #[primary]
    pub code: String,
    /// ISO 10383 MIC, e.g. `XKAR`.
    pub mic: String,
    /// ISO 3166 alpha-2, e.g. `PK`.
    pub country: String,
    /// ISO 4217, e.g. `PKR`.
    pub currency: String,
    /// IANA zone, e.g. `Asia/Karachi`.
    pub timezone: String,
}

/// One listed company. `symbol` is `EXCHANGE:TICKER`.
#[btb::table]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Company {
    #[primary]
    pub symbol: String,
    #[index]
    pub exchange: String,
    /// Sector id, e.g. `oil-gas-exploration-companies`.
    #[index]
    pub sector: String,
    pub name: String,
    pub sector_name: String,
    pub shares_outstanding: u64,
    pub free_float: u64,
    /// e.g. `["KSE100", "KSE30"]`.
    pub index_memberships: Vec<String>,
    pub listing_status: String,
    pub fiscal_year_end: String,
    pub website: String,
    /// SHA-256 of the long profile (description, address, people), kept on storage nodes.
    pub profile_hash: Vec<u8>,
    pub source: String,
    pub as_of: u64,
    pub height: u64,
}

/// The latest (delayed or end-of-day) quote of a symbol, replaced on every publish.
#[btb::table]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quote {
    #[primary]
    pub symbol: String,
    pub price: i64,
    pub previous_close: i64,
    pub open: i64,
    pub high: i64,
    pub low: i64,
    pub volume: u64,
    pub value_traded: i64,
    pub high_52w: i64,
    pub low_52w: i64,
    /// Local market marker, e.g. `XD` (ex-dividend), `XB` (ex-bonus); empty if none.
    pub marker: String,
    /// `trading`, `suspended`, `halted`, ...
    pub trading_status: String,
    /// Trading date `YYYYMMDD` (exchange local).
    pub session: u32,
    pub delayed: bool,
    pub source: String,
    pub as_of: u64,
    pub height: u64,
}

/// One daily bar. `key` is `SYMBOL|YYYYMMDD`, so a primary range scan returns a symbol's bars
/// in date order.
#[btb::table]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bar {
    #[primary]
    pub key: String,
    pub date: u32,
    pub open: i64,
    pub high: i64,
    pub low: i64,
    pub close: i64,
    pub volume: u64,
    /// Close adjusted for corporate actions, when the provider has it.
    pub adjusted_close: Option<i64>,
}

/// Trailing-twelve-month fundamentals of a symbol.
#[btb::table]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fundamentals {
    #[primary]
    pub symbol: String,
    /// e.g. `FY2026` or `TTM 2026Q1`.
    pub period: String,
    pub revenue: i64,
    pub net_profit: i64,
    pub eps_ttm: i64,
    pub dps_ttm: i64,
    pub book_value_per_share: Option<i64>,
    pub as_of: u64,
    pub height: u64,
}

// ---------------------------------------------------------------------------------------------
// Inputs (tuples, so they need no extra encoding code)
// ---------------------------------------------------------------------------------------------

/// `(code, mic, country, currency, timezone)`
pub type ExchangeIn = (String, String, String, String, String);
/// `(symbol, sector, name, sector_name, (shares_outstanding, free_float), index_memberships,
/// (listing_status, fiscal_year_end, website), (profile_hash, source, as_of))`
pub type CompanyIn = (
    String,
    String,
    String,
    String,
    (u64, u64),
    Vec<String>,
    (String, String, String),
    (Vec<u8>, String, u64),
);
/// `(symbol, (price, previous_close, open, high, low), (volume, value_traded),
/// (high_52w, low_52w), (marker, trading_status), session, delayed, (source, as_of))`
pub type QuoteIn = (
    String,
    (i64, i64, i64, i64, i64),
    (u64, i64),
    (i64, i64),
    (String, String),
    u32,
    bool,
    (String, u64),
);
/// One row of a sector board: `(symbol, price, change_bps, pe)`.
pub type BoardRow = (String, i64, Option<i64>, Option<i64>);
/// `(symbol, date, open, high, low, close, volume, adjusted_close)`
pub type BarIn = (String, u32, i64, i64, i64, i64, u64, Option<i64>);
/// `(symbol, period, (revenue, net_profit), (eps_ttm, dps_ttm), book_value_per_share, as_of)`
pub type FundamentalsIn = (String, String, (i64, i64), (i64, i64), Option<i64>, u64);

// ---------------------------------------------------------------------------------------------
// The one-call payload
// ---------------------------------------------------------------------------------------------

/// Derived valuation, computed from the quote and fundamentals. `None` when undefined (no
/// earnings, no price), never a trap.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Ratios {
    /// Price / EPS, 1e-4.
    pub pe: Option<i64>,
    /// Price / book, 1e-4.
    pub pb: Option<i64>,
    /// DPS / price, bps.
    pub dividend_yield_bps: Option<i64>,
    /// DPS / EPS, bps.
    pub payout_bps: Option<i64>,
    /// Net profit / revenue, bps.
    pub net_margin_bps: Option<i64>,
}

/// Derived price analytics, computed from the quote and the bars.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Analytics {
    /// `price - previous_close`, 1e-4.
    pub change: i64,
    pub change_bps: Option<i64>,
    /// `price * shares_outstanding`, whole currency units.
    pub market_cap: Option<i128>,
    /// `free_float / shares_outstanding`, bps.
    pub free_float_bps: Option<i64>,
    pub high_20d: Option<i64>,
    /// From the 20-day high to the price, bps (zero or negative).
    pub drawdown_20d_bps: Option<i64>,
    pub sma_20: Option<i64>,
    pub sma_50: Option<i64>,
    pub avg_volume_20d: Option<u64>,
    /// From the 52-week high, bps (zero or negative).
    pub from_52w_high_bps: Option<i64>,
    /// Bars the averages were computed over.
    pub bars_used: u32,
}

/// How the stock compares with its sector on the same exchange.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SectorView {
    pub sector: String,
    pub companies: u32,
    /// Companies with a positive P/E, the ones the median is over.
    pub valid_pe: u32,
    pub median_pe: Option<i64>,
    /// `pe / median_pe - 1`, bps.
    pub pe_premium_bps: Option<i64>,
}

/// Everything about one stock in one answer. Blocks not asked for are `None` / empty.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StockPayload {
    pub symbol: String,
    pub exchange: Option<Exchange>,
    pub company: Option<Company>,
    pub quote: Option<Quote>,
    /// Newest first.
    pub prices: Vec<Bar>,
    pub fundamentals: Option<Fundamentals>,
    pub ratios: Option<Ratios>,
    pub analytics: Option<Analytics>,
    pub sector: Option<SectorView>,
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
record!(Ratios {
    pe,
    pb,
    dividend_yield_bps,
    payout_bps,
    net_margin_bps
});
record!(Analytics {
    change,
    change_bps,
    market_cap,
    free_float_bps,
    high_20d,
    drawdown_20d_bps,
    sma_20,
    sma_50,
    avg_volume_20d,
    from_52w_high_bps,
    bars_used
});
record!(SectorView {
    sector,
    companies,
    valid_pe,
    median_pe,
    pe_premium_bps
});
record!(StockPayload {
    symbol,
    exchange,
    company,
    quote,
    prices,
    fundamentals,
    ratios,
    analytics,
    sector
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

/// Hand the provider role to another account.
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
    ensure!(n > 0 && n <= MAX_BATCH, "batch must hold 1..=500 rows");
    Ok(())
}

/// `EXCHANGE:TICKER` with a known exchange; returns the exchange code.
fn exchange_of(symbol: &str) -> btb_sdk::Result<String> {
    let Some((ex, ticker)) = symbol.split_once(':') else {
        btb_sdk::bail!("symbol must be EXCHANGE:TICKER");
    };
    ensure!(
        !ticker.is_empty() && ticker.len() <= 32 && !ticker.contains('|'),
        "bad ticker"
    );
    ensure!(Exchange::get_by_code(ex).is_some(), "unknown exchange");
    Ok(String::from(ex))
}

fn valid_ohlc(open: i64, high: i64, low: i64, close: i64) -> bool {
    low > 0 && low <= open && low <= close && high >= open && high >= close
}

/// Add or replace exchanges.
#[btb::mutation]
pub fn publish_exchanges(rows: Vec<ExchangeIn>) -> btb_sdk::Result<u32> {
    check_batch(rows.len())?;
    for (code, mic, country, currency, timezone) in rows {
        ensure!(
            !code.is_empty() && code.len() <= 16 && !code.contains(':'),
            "bad exchange code"
        );
        let row = Exchange {
            code: code.clone(),
            mic,
            country,
            currency,
            timezone,
        };
        match Exchange::get_by_code(&code) {
            Some((id, _)) => {
                row.update(id);
            }
            None => {
                row.insert();
            }
        }
    }
    Ok(storage_bump(b"n_exchanges"))
}

/// Add or replace companies.
#[btb::mutation]
pub fn publish_companies(rows: Vec<CompanyIn>) -> btb_sdk::Result<u32> {
    check_batch(rows.len())?;
    let n = rows.len() as u32;
    for (
        symbol,
        sector,
        name,
        sector_name,
        (shares, ff),
        idx,
        (status, fye, web),
        (hash, src, as_of),
    ) in rows
    {
        let exchange = exchange_of(&symbol)?;
        ensure!(ff <= shares, "free float above shares outstanding");
        let row = Company {
            symbol: symbol.clone(),
            exchange,
            sector,
            name,
            sector_name,
            shares_outstanding: shares,
            free_float: ff,
            index_memberships: idx,
            listing_status: status,
            fiscal_year_end: fye,
            website: web,
            profile_hash: hash,
            source: src,
            as_of,
            height: block_height(),
        };
        match Company::get_by_symbol(&symbol) {
            Some((id, _)) => {
                row.update(id);
            }
            None => {
                row.insert();
            }
        }
    }
    Ok(n)
}

/// Replace quotes. A quote older than the stored one (by `as_of`) is skipped, so a replayed or
/// late batch cannot roll a price back. Returns how many were written.
#[btb::mutation]
pub fn publish_quotes(rows: Vec<QuoteIn>) -> btb_sdk::Result<u32> {
    check_batch(rows.len())?;
    let mut written = 0;
    for (
        symbol,
        (price, prev, open, high, low),
        (volume, value),
        (h52, l52),
        (marker, status),
        session,
        delayed,
        (src, as_of),
    ) in rows
    {
        ensure!(Company::get_by_symbol(&symbol).is_some(), "unknown symbol");
        ensure!(valid_ohlc(open, high, low, price) && prev > 0, "bad prices");
        let row = Quote {
            symbol: symbol.clone(),
            price,
            previous_close: prev,
            open,
            high,
            low,
            volume,
            value_traded: value,
            high_52w: h52,
            low_52w: l52,
            marker,
            trading_status: status,
            session,
            delayed,
            source: src,
            as_of,
            height: block_height(),
        };
        match Quote::get_by_symbol(&symbol) {
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

/// Add or correct daily bars.
#[btb::mutation]
pub fn publish_bars(rows: Vec<BarIn>) -> btb_sdk::Result<u32> {
    check_batch(rows.len())?;
    let n = rows.len() as u32;
    for (symbol, date, open, high, low, close, volume, adj) in rows {
        ensure!(Company::get_by_symbol(&symbol).is_some(), "unknown symbol");
        ensure!((19_000_101..=29_991_231).contains(&date), "bad date");
        ensure!(valid_ohlc(open, high, low, close), "bad prices");
        let key = bar_key(&symbol, date);
        let row = Bar {
            key: key.clone(),
            date,
            open,
            high,
            low,
            close,
            volume,
            adjusted_close: adj,
        };
        match Bar::get_by_key(&key) {
            Some((id, _)) => {
                row.update(id);
            }
            None => {
                row.insert();
            }
        }
    }
    Ok(n)
}

/// Add or replace fundamentals.
#[btb::mutation]
pub fn publish_fundamentals(rows: Vec<FundamentalsIn>) -> btb_sdk::Result<u32> {
    check_batch(rows.len())?;
    let n = rows.len() as u32;
    for (symbol, period, (revenue, profit), (eps, dps), bvps, as_of) in rows {
        ensure!(Company::get_by_symbol(&symbol).is_some(), "unknown symbol");
        ensure!(dps >= 0, "negative dividend");
        let row = Fundamentals {
            symbol: symbol.clone(),
            period,
            revenue,
            net_profit: profit,
            eps_ttm: eps,
            dps_ttm: dps,
            book_value_per_share: bvps,
            as_of,
            height: block_height(),
        };
        match Fundamentals::get_by_symbol(&symbol) {
            Some((id, _)) => {
                row.update(id);
            }
            None => {
                row.insert();
            }
        }
    }
    Ok(n)
}

fn storage_bump(key: &[u8]) -> u32 {
    let n = storage_get(key)
        .and_then(|b| u32::decode(&b).ok())
        .unwrap_or(0)
        + 1;
    storage_set(key, &n.encode());
    n
}

// ---------------------------------------------------------------------------------------------
// Math (integer only; every division guarded)
// ---------------------------------------------------------------------------------------------

/// `a * m / b`, rounded half away from zero, in i128; `None` if `b == 0` or it overflows i64.
fn mul_div(a: i64, m: i64, b: i64) -> Option<i64> {
    if b == 0 {
        return None;
    }
    let n = a as i128 * m as i128;
    let d = b as i128;
    let q = n / d;
    let r = n % d;
    let q = if 2 * r.abs() >= d.abs() {
        q + if (n < 0) == (d < 0) { 1 } else { -1 }
    } else {
        q
    };
    i64::try_from(q).ok()
}

/// `(a / b - 1)` in bps.
fn change_bps(a: i64, b: i64) -> Option<i64> {
    if b <= 0 {
        return None;
    }
    mul_div(a - b, 10_000, b)
}

/// Lower median of a sorted, non-empty slice.
fn median(sorted: &[i64]) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    Some(sorted[(sorted.len() - 1) / 2])
}

fn bar_key(symbol: &str, date: u32) -> String {
    format!("{symbol}|{date:08}")
}

/// Newest `n` bars of `symbol`, newest first.
fn recent_bars(symbol: &str, n: u32) -> Vec<Bar> {
    let lo = format!("{symbol}|");
    let hi = format!("{symbol}}}"); // '}' sorts right after '|'
    Bar::by_key_range(lo..hi)
        .order(Order::Desc)
        .limit(n.min(MAX_BARS) as usize)
        .map(|(_, b)| b)
        .collect()
}

fn ratios(q: &Quote, f: &Fundamentals) -> Ratios {
    let pos = |v: Option<i64>| v.filter(|x| *x > 0);
    Ratios {
        pe: if f.eps_ttm > 0 {
            mul_div(q.price, SCALE, f.eps_ttm)
        } else {
            None
        },
        pb: f
            .book_value_per_share
            .filter(|b| *b > 0)
            .and_then(|b| mul_div(q.price, SCALE, b)),
        dividend_yield_bps: pos(Some(q.price)).and_then(|p| mul_div(f.dps_ttm, 10_000, p)),
        payout_bps: if f.eps_ttm > 0 {
            mul_div(f.dps_ttm, 10_000, f.eps_ttm)
        } else {
            None
        },
        net_margin_bps: if f.revenue > 0 {
            mul_div(f.net_profit, 10_000, f.revenue)
        } else {
            None
        },
    }
}

fn analytics(q: &Quote, c: Option<&Company>, bars: &[Bar]) -> Analytics {
    let take = |n: usize| &bars[..bars.len().min(n)];
    let avg = |s: &[Bar]| -> Option<i64> {
        if s.is_empty() {
            return None;
        }
        let sum: i128 = s.iter().map(|b| b.close as i128).sum();
        mul_div(i64::try_from(sum).ok()?, 1, s.len() as i64)
    };
    let b20 = take(20);
    let high_20d = b20.iter().map(|b| b.high).max();
    let shares = c.map(|c| c.shares_outstanding).unwrap_or(0);
    Analytics {
        change: q.price - q.previous_close,
        change_bps: change_bps(q.price, q.previous_close),
        market_cap: (shares > 0).then(|| q.price as i128 * shares as i128 / SCALE as i128),
        free_float_bps: c
            .filter(|c| c.shares_outstanding > 0)
            .and_then(|c| mul_div(c.free_float as i64, 10_000, c.shares_outstanding as i64)),
        high_20d,
        drawdown_20d_bps: high_20d.and_then(|h| change_bps(q.price.min(h), h)),
        sma_20: (bars.len() >= 20).then(|| avg(take(20))).flatten(),
        sma_50: (bars.len() >= 50).then(|| avg(take(50))).flatten(),
        avg_volume_20d: (!b20.is_empty())
            .then(|| b20.iter().map(|b| b.volume as u128).sum::<u128>() / b20.len() as u128)
            .and_then(|v| u64::try_from(v).ok()),
        from_52w_high_bps: change_bps(q.price.min(q.high_52w), q.high_52w),
        bars_used: bars.len() as u32,
    }
}

fn pe_of(symbol: &str) -> Option<i64> {
    let (_, q) = Quote::get_by_symbol(symbol)?;
    let (_, f) = Fundamentals::get_by_symbol(symbol)?;
    ratios(&q, &f).pe
}

fn sector_view(c: &Company, own_pe: Option<i64>) -> SectorView {
    let mut companies = 0u32;
    let mut pes: Vec<i64> = Vec::new();
    for (_, peer) in Company::by_sector(&c.sector) {
        if peer.exchange != c.exchange {
            continue; // never mix currencies
        }
        companies += 1;
        if let Some(pe) = pe_of(&peer.symbol) {
            // Insertion keeps `pes` sorted (sectors are small; avoids the core sort routines).
            let at = pes.partition_point(|x| *x <= pe);
            pes.insert(at, pe);
        }
    }
    let median_pe = median(&pes);
    SectorView {
        sector: c.sector.clone(),
        companies,
        valid_pe: pes.len() as u32,
        median_pe,
        pe_premium_bps: match (own_pe, median_pe) {
            (Some(p), Some(m)) => change_bps(p, m),
            _ => None,
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------------------------

/// Everything about one stock in one call. `include` is a mask of `INC_*` (0 = everything);
/// `bars` caps the price history (at most 400, newest first).
#[btb::query]
pub fn stock(symbol: String, include: u32, bars: u32) -> Option<StockPayload> {
    let inc = if include == 0 { u32::MAX } else { include };
    let want = |b: u32| inc & b != 0;
    let company = Company::get_by_symbol(&symbol).map(|(_, c)| c)?;
    let quote = Quote::get_by_symbol(&symbol).map(|(_, q)| q);
    let fund = Fundamentals::get_by_symbol(&symbol).map(|(_, f)| f);
    // Analytics need 50 bars for the SMA even when fewer are returned.
    let need_bars = if want(INC_ANALYTICS) {
        bars.max(50)
    } else {
        bars
    };
    let history = if want(INC_PRICES) || want(INC_ANALYTICS) {
        recent_bars(&symbol, need_bars)
    } else {
        Vec::new()
    };
    let r = match (&quote, &fund) {
        (Some(q), Some(f)) => Some(ratios(q, f)),
        _ => None,
    };
    Some(StockPayload {
        symbol: symbol.clone(),
        exchange: want(INC_COMPANY)
            .then(|| Exchange::get_by_code(&company.exchange).map(|(_, e)| e))
            .flatten(),
        quote: want(INC_QUOTE).then(|| quote.clone()).flatten(),
        prices: if want(INC_PRICES) {
            history
                .iter()
                .take(bars.min(MAX_BARS) as usize)
                .cloned()
                .collect()
        } else {
            Vec::new()
        },
        fundamentals: want(INC_FUNDAMENTALS).then(|| fund.clone()).flatten(),
        ratios: want(INC_RATIOS).then(|| r.clone()).flatten(),
        analytics: want(INC_ANALYTICS)
            .then(|| {
                quote
                    .as_ref()
                    .map(|q| analytics(q, Some(&company), &history))
            })
            .flatten(),
        sector: want(INC_SECTOR).then(|| sector_view(&company, r.as_ref().and_then(|r| r.pe))),
        company: want(INC_COMPANY).then_some(company),
    })
}

/// Quotes of many symbols in one call (at most 100); unknown symbols are left out.
#[btb::query]
pub fn quotes(symbols: Vec<String>) -> Vec<Quote> {
    symbols
        .into_iter()
        .take(100)
        .filter_map(|s| Quote::get_by_symbol(&s).map(|(_, q)| q))
        .collect()
}

/// Symbols of an exchange, a page at a time: `(symbols, cursor)`.
#[btb::query]
pub fn symbols(exchange: String, limit: u32, cursor: Vec<u8>) -> (Vec<String>, Vec<u8>) {
    let page = Company::by_exchange(&exchange)
        .limit(limit.clamp(1, 500) as usize)
        .after(cursor)
        .page();
    (
        page.rows.into_iter().map(|(_, c)| c.symbol).collect(),
        page.next.unwrap_or_default(),
    )
}

/// Sector board: `(symbol, price, change_bps, pe)` for every company of a sector on one
/// exchange, plus the sector view.
#[btb::query]
pub fn sector(exchange: String, sector: String) -> (Vec<BoardRow>, Option<SectorView>) {
    let mut rows = Vec::new();
    let mut first: Option<Company> = None;
    for (_, c) in Company::by_sector(&sector) {
        if c.exchange != exchange {
            continue;
        }
        if let Some((_, q)) = Quote::get_by_symbol(&c.symbol) {
            rows.push((
                c.symbol.clone(),
                q.price,
                change_bps(q.price, q.previous_close),
                pe_of(&c.symbol),
            ));
        }
        first.get_or_insert(c);
    }
    let view = first.map(|c| sector_view(&c, None));
    (rows, view)
}
