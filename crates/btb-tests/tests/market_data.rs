//! End to end: build `examples/market-data` for riscv64 and run it in `btb-vm` over
//! `RuntimeHost`/`MemKv`, with real PSX figures for OGDC (2026-11-20 session). The derived
//! numbers must match what an off-chain API computes from the same facts (P/E 5.12, dividend
//! yield 4.00 %, market cap PKR 1,344,255,171,420, 17.8 % below the sector median P/E).

use btb_codec::{Address, Decode, Encode, Reader};
use btb_runtime::{layout, KvStore, MemKv, RuntimeHost, Tables};
use btb_vm::{CallError, Contract, FunctionKind};
use std::collections::BTreeMap;
use std::sync::OnceLock;

mod common;
use common::{abort_msg, MEM};

const CONTRACT: [u8; 32] = [0x4d; 32];
const GAS: u64 = 200_000_000;
const PROVIDER: Address = Address([0x41; 32]);
const EVE: Address = Address([0x45; 32]);

const INC_QUOTE: u32 = 2;
const INC_PRICES: u32 = 4;
const INC_RATIOS: u32 = 16;
const INC_ANALYTICS: u32 = 32;
const INC_SECTOR: u32 = 64;

fn elf() -> &'static [u8] {
    static ELF: OnceLock<Vec<u8>> = OnceLock::new();
    ELF.get_or_init(|| common::build_example("market-data"))
}

/// `312.55` -> `3_125_500`.
fn px(s: &str) -> i64 {
    let (i, f) = s.split_once('.').unwrap_or((s, ""));
    let f = format!("{f:0<4}");
    i.parse::<i64>().unwrap() * 10_000 + f[..4].parse::<i64>().unwrap()
}

struct Chain {
    c: Contract,
    kv: MemKv,
    tables: Tables,
    height: u64,
}

impl Chain {
    fn new() -> Self {
        let mut kv = MemKv::new();
        // Storage deposits are paid from the contract's balance.
        kv.put(&layout::balance_key(&CONTRACT), &u64::MAX.to_le_bytes())
            .unwrap();
        Chain {
            c: Contract::from_elf(elf()).expect("valid contract"),
            kv,
            tables: common::declared_tables(CONTRACT, elf()),
            height: 1,
        }
    }

    fn run(
        &mut self,
        kind: FunctionKind,
        name: &str,
        who: Address,
        input: &[u8],
    ) -> Result<Vec<u8>, CallError> {
        let mut h = RuntimeHost::new(&self.kv, &self.tables, CONTRACT, who.0, self.height);
        let out = self.c.call(kind, name, input, &mut h, GAS, MEM)?;
        let batch = h.finish().expect("no faults");
        if kind == FunctionKind::Mutation {
            self.kv.commit(batch).unwrap();
            self.height += 1;
        }
        Ok(out.output)
    }

    fn mutate<T: Decode>(&mut self, name: &str, who: Address, input: &impl Encode) -> T {
        let out = self
            .run(FunctionKind::Mutation, name, who, &input.encode())
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        T::decode(&out).expect(name)
    }

    fn query_raw(&mut self, name: &str, input: &impl Encode) -> Vec<u8> {
        self.run(FunctionKind::Query, name, EVE, &input.encode())
            .unwrap_or_else(|e| panic!("{name}: {e:?}"))
    }

    fn snapshot(&self) -> BTreeMap<Vec<u8>, Vec<u8>> {
        self.kv
            .entries()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    fn fails(&mut self, name: &str, who: Address, input: &impl Encode, msg: &str) {
        let before = self.snapshot();
        let e = self
            .run(FunctionKind::Mutation, name, who, &input.encode())
            .expect_err(name);
        assert_eq!(abort_msg(e), msg, "{name}");
        assert_eq!(self.snapshot(), before, "{name} left a trace");
    }
}

type ExchangeIn = (String, String, String, String, String);
type CompanyIn = (
    String,
    String,
    String,
    String,
    (u64, u64),
    Vec<String>,
    (String, String, String),
    (Vec<u8>, String, u64),
);
type QuoteIn = (
    String,
    (i64, i64, i64, i64, i64),
    (u64, i64),
    (i64, i64),
    (String, String),
    u32,
    bool,
    (String, u64),
);
type BarIn = (String, u32, i64, i64, i64, i64, u64, Option<i64>);
type FundIn = (String, String, (i64, i64), (i64, i64), Option<i64>, u64);

const SECTOR: &str = "oil-gas-exploration-companies";

fn company(sym: &str, name: &str, shares: u64, ff: u64) -> CompanyIn {
    (
        sym.into(),
        SECTOR.into(),
        name.into(),
        "OIL & GAS EXPLORATION COMPANIES".into(),
        (shares, ff),
        vec!["KSE100".into()],
        ("listed".into(), "June".into(), String::new()),
        (vec![0; 32], "psx".into(), 1_795_000_000),
    )
}

fn quote(sym: &str, price: &str, prev: &str, as_of: u64) -> QuoteIn {
    let p = px(price);
    (
        sym.into(),
        (p, px(prev), p, p, p),
        (1_000, 1_000),
        (p, p),
        (String::new(), "trading".into()),
        20_261_120,
        true,
        ("psx".into(), as_of),
    )
}

/// OGDC and three sector peers, each with a P/E chosen so the sector median is 6.23.
fn seeded() -> Chain {
    let mut ch = Chain::new();
    ch.mutate::<()>("init", PROVIDER, &());
    let ex: Vec<ExchangeIn> = vec![(
        "PSX".into(),
        "XKAR".into(),
        "PK".into(),
        "PKR".into(),
        "Asia/Karachi".into(),
    )];
    ch.mutate::<u32>("publish_exchanges", PROVIDER, &(ex,));
    let cos = vec![
        company(
            "PSX:OGDC",
            "Oil & Gas Development Company Limited",
            4_300_928_400,
            645_139_260,
        ),
        company("PSX:PPL", "Pakistan Petroleum Limited", 2_720_000_000, 0),
        company("PSX:MARI", "Mari Energies Limited", 1_200_000_000, 0),
        company("PSX:POL", "Pakistan Oilfields Limited", 283_000_000, 0),
    ];
    assert_eq!(ch.mutate::<u32>("publish_companies", PROVIDER, &(cos,)), 4);

    let mut ogdc = quote("PSX:OGDC", "312.55", "312.79", 1_795_150_000);
    ogdc.1 = (
        px("312.55"),
        px("312.79"),
        px("313.6"),
        px("315.48"),
        px("311.91"),
    );
    ogdc.2 = (1_236_159, 218_451_790);
    ogdc.3 = (px("352"), px("246"));
    ogdc.4 .0 = "XD".into();
    let qs = vec![
        ogdc,
        quote("PSX:PPL", "186.9", "186", 1_795_150_000),
        quote("PSX:MARI", "640", "650", 1_795_150_000),
        quote("PSX:POL", "630", "625", 1_795_150_000),
    ];
    assert_eq!(ch.mutate::<u32>("publish_quotes", PROVIDER, &(qs,)), 4);

    let bars: Vec<BarIn> = [
        (
            20_261_116, "318.44", "321.06", "315.87", "316.21", 3_035_626,
        ),
        (20_261_117, "314", "317", "313.55", "314.04", 1_037_552),
        (20_261_118, "314.04", "314.35", "312", "312.73", 948_535),
        (20_261_119, "312.8", "314.5", "312.01", "312.79", 536_360),
        (20_261_120, "313.6", "315.48", "311.91", "312.55", 1_245_401),
    ]
    .iter()
    .map(|&(d, o, h, l, c, v)| ("PSX:OGDC".into(), d, px(o), px(h), px(l), px(c), v, None))
    .collect();
    assert_eq!(ch.mutate::<u32>("publish_bars", PROVIDER, &(bars,)), 5);

    // EPS so that P/E = 6.23 (PPL), 8 (MARI), 7 (POL); OGDC's TTM EPS is 61.04, DPS 12.5.
    let f = |s: &str, eps: &str, dps: &str| -> FundIn {
        (
            s.into(),
            "TTM".into(),
            (449_190_752_000, 242_373_646_000),
            (px(eps), px(dps)),
            None,
            1_795_000_000,
        )
    };
    let funds = vec![
        f("PSX:OGDC", "61.04", "12.5"),
        f("PSX:PPL", "30", "4"),
        f("PSX:MARI", "80", "20"),
        f("PSX:POL", "90", "50"),
    ];
    assert_eq!(
        ch.mutate::<u32>("publish_fundamentals", PROVIDER, &(funds,)),
        4
    );
    ch
}

/// Read the fixed head of a payload asked for with one `include` bit: every other block must be
/// absent, and the reader is left at the asked-for block.
fn skip_to<'a>(bytes: &'a [u8], upto: usize) -> Reader<'a> {
    let mut r = Reader::new(bytes, 1 << 20);
    assert_eq!(String::decode_from(&mut r).unwrap(), "PSX:OGDC");
    // exchange, company, quote, prices, fundamentals, ratios, analytics, sector
    for i in 0..upto {
        if i == 3 {
            assert_eq!(u32::decode_from(&mut r).unwrap(), 0, "prices not asked for");
        } else {
            assert_eq!(
                u8::decode_from(&mut r).unwrap(),
                0,
                "block {i} not asked for"
            );
        }
    }
    r
}

fn opt(r: &mut Reader<'_>) -> Option<i64> {
    Option::<i64>::decode_from(r).unwrap()
}

#[test]
fn contract_is_pure_rv64im() {
    common::assert_only_rv64im(elf());
}

#[test]
fn ratios_match_the_offchain_api() {
    let mut ch = seeded();
    let raw = ch.query_raw("stock", &("PSX:OGDC".to_string(), INC_RATIOS, 0u32));
    assert_eq!(raw[0], 1, "Some");
    let mut r = skip_to(&raw[1..], 5);
    assert_eq!(u8::decode_from(&mut r).unwrap(), 1);
    let pe = opt(&mut r);
    let pb = opt(&mut r);
    let dy = opt(&mut r);
    let payout = opt(&mut r);
    let margin = opt(&mut r);
    assert_eq!(pe, Some(51_204), "P/E 5.1204 (API: 5.12)");
    assert_eq!(pb, None, "no book value published");
    assert_eq!(dy, Some(400), "dividend yield 4.00 % (API: 3.9994 %)");
    assert_eq!(payout, Some(2_048), "payout 20.48 %");
    assert_eq!(margin, Some(5_396), "net margin 53.96 %");
}

#[test]
fn analytics_match_the_offchain_api() {
    let mut ch = seeded();
    let raw = ch.query_raw("stock", &("PSX:OGDC".to_string(), INC_ANALYTICS, 0u32));
    let mut r = skip_to(&raw[1..], 6);
    assert_eq!(u8::decode_from(&mut r).unwrap(), 1);
    assert_eq!(i64::decode_from(&mut r).unwrap(), -2_400, "change -0.24");
    assert_eq!(opt(&mut r), Some(-8), "change -0.08 %");
    assert_eq!(
        Option::<i128>::decode_from(&mut r).unwrap(),
        Some(1_344_255_171_420),
        "market cap"
    );
    assert_eq!(opt(&mut r), Some(1_500), "free float 15 %");
    assert_eq!(opt(&mut r), Some(px("321.06")), "high over the bars held");
    assert_eq!(
        opt(&mut r),
        Some(-265),
        "drawdown -2.65 % (API 5d: -2.65 %)"
    );
    assert_eq!(opt(&mut r), None, "SMA 20 needs 20 bars");
    assert_eq!(opt(&mut r), None, "SMA 50 needs 50 bars");
    assert_eq!(
        Option::<u64>::decode_from(&mut r).unwrap(),
        Some((3_035_626 + 1_037_552 + 948_535 + 536_360 + 1_245_401) / 5)
    );
    assert_eq!(opt(&mut r), Some(-1_121), "11.21 % below the 52-week high");
    assert_eq!(u32::decode_from(&mut r).unwrap(), 5, "bars used");
}

#[test]
fn sector_median_and_premium_match_the_offchain_api() {
    let mut ch = seeded();
    let raw = ch.query_raw("stock", &("PSX:OGDC".to_string(), INC_SECTOR, 0u32));
    let mut r = skip_to(&raw[1..], 7);
    assert_eq!(u8::decode_from(&mut r).unwrap(), 1);
    assert_eq!(String::decode_from(&mut r).unwrap(), SECTOR);
    assert_eq!(u32::decode_from(&mut r).unwrap(), 4, "companies");
    assert_eq!(u32::decode_from(&mut r).unwrap(), 4, "with a valid P/E");
    assert_eq!(opt(&mut r), Some(62_300), "median P/E 6.23 (API: 6.23)");
    assert_eq!(
        opt(&mut r),
        Some(-1_781),
        "-17.81 % vs sector (API: -17.82 %)"
    );
}

#[test]
fn prices_come_newest_first_and_are_capped() {
    let mut ch = seeded();
    let raw = ch.query_raw("stock", &("PSX:OGDC".to_string(), INC_PRICES, 3u32));
    let mut r = skip_to(&raw[1..], 3);
    assert_eq!(u32::decode_from(&mut r).unwrap(), 3);
    let mut dates = vec![];
    for _ in 0..3 {
        let key = String::decode_from(&mut r).unwrap();
        let (date, _o, _h, _l, close, _v, _adj): (u32, i64, i64, i64, i64, u64, Option<i64>) =
            Decode::decode_from(&mut r).unwrap();
        assert!(key.starts_with("PSX:OGDC|"));
        dates.push((date, close));
    }
    assert_eq!(
        dates,
        vec![
            (20_261_120, px("312.55")),
            (20_261_119, px("312.79")),
            (20_261_118, px("312.73"))
        ]
    );
}

#[test]
fn full_payload_is_one_call_and_unknown_is_none() {
    let mut ch = seeded();
    let all = ch.query_raw("stock", &("PSX:OGDC".to_string(), 0u32, 400u32));
    let quote_only = ch.query_raw("stock", &("PSX:OGDC".to_string(), INC_QUOTE, 0u32));
    assert!(
        all.len() > quote_only.len() * 2,
        "everything is in one answer"
    );
    assert!(all.len() < 4_096, "compact: {} bytes", all.len());
    let none = ch.query_raw("stock", &("PSX:NOPE".to_string(), 0u32, 0u32));
    assert_eq!(none, vec![0]);
}

#[test]
fn only_the_provider_publishes() {
    let mut ch = seeded();
    let qs = vec![quote("PSX:OGDC", "1", "1", 1_900_000_000)];
    ch.fails("publish_quotes", EVE, &(qs,), "not the provider");
    ch.fails("init", EVE, &(), "already initialised");
    ch.fails("set_provider", EVE, &(EVE,), "not the provider");
}

#[test]
fn bad_rows_are_refused_and_change_nothing() {
    let mut ch = seeded();
    let empty: Vec<QuoteIn> = vec![];
    ch.fails(
        "publish_quotes",
        PROVIDER,
        &(empty,),
        "batch must hold 1..=500 rows",
    );
    let unknown = vec![quote("PSX:NOPE", "1", "1", 1_900_000_000)];
    ch.fails("publish_quotes", PROVIDER, &(unknown,), "unknown symbol");
    let mut bad = quote("PSX:OGDC", "10", "10", 1_900_000_000);
    bad.1 .3 = 1; // high below the price
                  // A good row before a bad one: the whole batch is refused.
    let batch = vec![quote("PSX:PPL", "1", "1", 1_900_000_000), bad];
    ch.fails("publish_quotes", PROVIDER, &(batch,), "bad prices");
    let no_ex = vec![company("LSE:VOD", "Vodafone", 1, 0)];
    ch.fails("publish_companies", PROVIDER, &(no_ex,), "unknown exchange");
    let no_colon = vec![company("OGDC", "x", 1, 0)];
    ch.fails(
        "publish_companies",
        PROVIDER,
        &(no_colon,),
        "symbol must be EXCHANGE:TICKER",
    );
    let ff = vec![company("PSX:X", "x", 1, 2)];
    ch.fails(
        "publish_companies",
        PROVIDER,
        &(ff,),
        "free float above shares outstanding",
    );
}

#[test]
fn an_older_quote_never_rolls_the_price_back() {
    let mut ch = seeded();
    let stale = vec![quote("PSX:OGDC", "100", "100", 1_700_000_000)];
    assert_eq!(ch.mutate::<u32>("publish_quotes", PROVIDER, &(stale,)), 0);
    let raw = ch.query_raw("quotes", &(vec!["PSX:OGDC".to_string()],));
    let mut r = Reader::new(&raw, 1 << 20);
    assert_eq!(u32::decode_from(&mut r).unwrap(), 1);
    assert_eq!(String::decode_from(&mut r).unwrap(), "PSX:OGDC");
    assert_eq!(i64::decode_from(&mut r).unwrap(), px("312.55"));
    let fresh = vec![quote("PSX:OGDC", "320", "312.55", 1_795_200_000)];
    assert_eq!(ch.mutate::<u32>("publish_quotes", PROVIDER, &(fresh,)), 1);
}

#[test]
fn no_earnings_gives_no_pe_instead_of_a_trap() {
    let mut ch = seeded();
    let loss: Vec<FundIn> = vec![(
        "PSX:OGDC".into(),
        "TTM".into(),
        (0, -5),
        (px("-1"), 0),
        Some(0),
        1_796_000_000,
    )];
    ch.mutate::<u32>("publish_fundamentals", PROVIDER, &(loss,));
    let raw = ch.query_raw("stock", &("PSX:OGDC".to_string(), INC_RATIOS, 0u32));
    let mut r = skip_to(&raw[1..], 5);
    assert_eq!(u8::decode_from(&mut r).unwrap(), 1);
    for name in ["pe", "pb"] {
        assert_eq!(opt(&mut r), None, "{name}");
    }
    assert_eq!(opt(&mut r), Some(0), "yield");
    assert_eq!(opt(&mut r), None, "payout");
    assert_eq!(opt(&mut r), None, "margin");
}

#[test]
fn symbols_page_through_an_exchange() {
    let mut ch = seeded();
    let raw = ch.query_raw("symbols", &("PSX".to_string(), 3u32, Vec::<u8>::new()));
    let (page, cursor) = <(Vec<String>, Vec<u8>)>::decode(&raw).unwrap();
    assert_eq!(page, vec!["PSX:OGDC", "PSX:PPL", "PSX:MARI"]);
    let raw = ch.query_raw("symbols", &("PSX".to_string(), 3u32, cursor));
    let (page, cursor) = <(Vec<String>, Vec<u8>)>::decode(&raw).unwrap();
    assert_eq!(page, vec!["PSX:POL"]);
    assert!(cursor.is_empty());
}
