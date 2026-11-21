//! The contract's types, field for field in the same order (`examples/market-data/src/main.rs`).
//! The canonical encoding is the fields in order, so these decode its answers exactly; a test in
//! the contract's suite would catch a mismatch the moment a field moves.

use btb_codec::{Decode, DecodeError, Reader};

macro_rules! record {
    ($t:ident { $($f:ident : $ty:ty),* $(,)? }) => {
        #[derive(Debug, Clone, PartialEq, Eq, Default)]
        pub struct $t { $(pub $f: $ty,)* }
        impl Decode for $t {
            fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
                Ok($t { $($f: Decode::decode_from(r)?,)* })
            }
        }
    };
}

record!(Exchange {
    code: String,
    mic: String,
    country: String,
    currency: String,
    timezone: String
});
record!(Company {
    symbol: String, exchange: String, sector: String, name: String, sector_name: String,
    shares_outstanding: u64, free_float: u64, index_memberships: Vec<String>, listing_status: String,
    fiscal_year_end: String, website: String, profile_hash: Vec<u8>, source: String, as_of: u64, height: u64,
});
record!(Quote {
    symbol: String,
    price: i64,
    previous_close: i64,
    open: i64,
    high: i64,
    low: i64,
    volume: u64,
    value_traded: i64,
    high_52w: i64,
    low_52w: i64,
    marker: String,
    trading_status: String,
    session: u32,
    delayed: bool,
    source: String,
    as_of: u64,
    height: u64,
});
record!(Bar {
    key: String, date: u32, open: i64, high: i64, low: i64, close: i64, volume: u64,
    adjusted_close: Option<i64>,
});
record!(Fundamentals {
    symbol: String, period: String, revenue: i64, net_profit: i64, eps_ttm: i64, dps_ttm: i64,
    book_value_per_share: Option<i64>, as_of: u64, height: u64,
});
record!(Ratios {
    pe: Option<i64>, pb: Option<i64>, dividend_yield_bps: Option<i64>, payout_bps: Option<i64>,
    net_margin_bps: Option<i64>,
});
record!(Analytics {
    change: i64, change_bps: Option<i64>, market_cap: Option<i128>, free_float_bps: Option<i64>,
    high_20d: Option<i64>, drawdown_20d_bps: Option<i64>, sma_20: Option<i64>, sma_50: Option<i64>,
    avg_volume_20d: Option<u64>, from_52w_high_bps: Option<i64>, bars_used: u32,
});
record!(SectorView {
    sector: String, companies: u32, valid_pe: u32, median_pe: Option<i64>, pe_premium_bps: Option<i64>,
});
record!(StockPayload {
    symbol: String, exchange: Option<Exchange>, company: Option<Company>, quote: Option<Quote>,
    prices: Vec<Bar>, fundamentals: Option<Fundamentals>, ratios: Option<Ratios>,
    analytics: Option<Analytics>, sector: Option<SectorView>,
});

pub type ExchangeIn = (String, String, String, String, String);
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
pub type BarIn = (String, u32, i64, i64, i64, i64, u64, Option<i64>);
pub type FundamentalsIn = (String, String, (i64, i64), (i64, i64), Option<i64>, u64);
