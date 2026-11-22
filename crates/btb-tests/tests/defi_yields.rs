//! End to end: build `examples/defi-yields` for riscv64 and run it in `btb-vm` over
//! `RuntimeHost`/`MemKv`, with real pools from the BTB Finance Discover snapshot of
//! 2026-11-22 04:36 UTC (Base, Ethereum, BNB Chain). The rankings, chain index and fee estimate
//! must equal what plain Rust computes from the same facts; the risk grades follow the formula
//! documented on `pool_risk`. Token grades and non-pool yields are test values, labelled so.

use btb_codec::{Address, Decode, Encode, Reader};
use btb_runtime::{layout, KvStore, MemKv, RuntimeHost, Tables};
use btb_vm::{CallError, Contract, FunctionKind};
use std::collections::BTreeMap;
use std::sync::OnceLock;

mod common;
use common::{abort_msg, MEM};

const CONTRACT: [u8; 32] = [0x59; 32];
/// The chain's default `query_gas`; rankings and indices must fit in it.
const QUERY_GAS: u64 = 50_000_000;
const GAS: u64 = 500_000_000;
const PROVIDER: Address = Address([0x41; 32]);
const EVE: Address = Address([0x45; 32]);

/// The snapshot's `updatedAt`, UTC unix seconds.
const AS_OF: u64 = 1_795_322_209;

const R_LOW_TVL: u32 = 1;
const R_IMPERMANENT_LOSS: u32 = 2;
const R_OUT_OF_RANGE: u32 = 4;
const R_HIGH_APR: u32 = 8;
const R_TOKEN_RISK: u32 = 16;
const R_UNGRADED_TOKEN: u32 = 32;

const KIND_LP: u8 = 1;
const KIND_STAKING: u8 = 2;
const KIND_LENDING: u8 = 3;
const KIND_TBILL: u8 = 4;
const FLAG_HONEYPOT: u32 = 16;

const WETH_CBBTC: &str = "8453:0xec3c3755b9ded4a2960ef50c9b48c1694ce274c2";
const WETH_USDC: &str = "8453:0x6c561b446416e1a00e8e93e221854d6ea4171372";

fn elf() -> &'static [u8] {
    static ELF: OnceLock<Vec<u8>> = OnceLock::new();
    ELF.get_or_init(|| common::build_example("defi-yields"))
}

// ---------------------------------------------------------------------------------------------
// Contract types, mirrored
// ---------------------------------------------------------------------------------------------

type PoolIn = (
    String,
    u64,
    (String, String, u32),
    (String, String),
    (String, String),
    bool,
    (String, u64),
);
type StatIn = (
    String,
    (u64, Option<u64>, Option<u64>),
    (i64, i64),
    (Option<i64>, u32, Option<u32>, Option<i64>),
    Option<i64>,
    u64,
);
type GradeIn = (String, u8, u32, (Option<u64>, Option<u32>), u64);
type YieldIn = (
    String,
    u8,
    (String, String),
    Option<u64>,
    i64,
    u8,
    Option<u64>,
    (String, u64),
);
type TopRow = (String, String, u64, i64, u64, u8, u8);
type YieldOut = (String, u8, String, Option<u64>, i64, Option<u64>, u8);

macro_rules! record {
    ($t:ident { $($f:ident: $ty:ty),* $(,)? }) => {
        #[derive(Debug, Clone, PartialEq, Eq)]
        struct $t { $($f: $ty,)* }
        impl Decode for $t {
            fn decode_from(r: &mut Reader<'_>) -> Result<Self, btb_codec::DecodeError> {
                Ok($t { $($f: Decode::decode_from(r)?,)* })
            }
        }
    };
}
record!(Pool {
    key: String,
    chain_id: u64,
    dex: String,
    version: String,
    fee_pips: u32,
    token0: String,
    token1: String,
    symbol0: String,
    symbol1: String,
    stable: bool,
    source: String,
    as_of: u64,
    height: u64,
});
record!(Stat {
    pool: String,
    apr_bps: i64,
    chain_apr: u128,
    stable_apr: Option<u128>,
    grade_apr: u128,
    chain_grade_apr: u128,
    tier_apr: u128,
    chain_tier_apr: u128,
    chain_id: u64,
    stable: bool,
    pair: String,
    risk_score: u8,
    risk_grade: u8,
    risk_reasons: u32,
    token_points: u8,
    token_reasons: u32,
    tvl_usd: u64,
    volume_24h_usd: Option<u64>,
    fees_24h_usd: Option<u64>,
    base_apr_bps: i64,
    reward_apr_bps: i64,
    range_apr_bps: Option<i64>,
    range_days: u32,
    in_range_bps: Option<u32>,
    net_vs_hold_bps: Option<i64>,
    change_24h_bps: Option<i64>,
    as_of: u64,
    height: u64,
});
record!(Hour {
    key: String,
    hour: u64,
    tvl_usd: u64,
    apr_bps: i64,
    volume_24h_usd: Option<u64>,
    fees_24h_usd: Option<u64>,
});
record!(Grade {
    token: String,
    score: u8,
    flags: u32,
    holders: Option<u64>,
    top10_bps: Option<u32>,
    as_of: u64,
    height: u64,
});
record!(Risk {
    score: u8,
    grade: u8,
    reasons: u32,
});
record!(PoolPayload {
    pool: Pool,
    stat: Option<Stat>,
    fees_24h_usd: Option<u64>,
    fees_estimated: bool,
    grade0: Option<Grade>,
    grade1: Option<Grade>,
    risk: Option<Risk>,
    history: Vec<Hour>,
});
record!(IndexView {
    chain_id: u64,
    stable_only: bool,
    pools: u64,
    tvl_usd: u128,
    median_apr_bps: Option<i64>,
    tvl_weighted_apr_bps: Option<i64>,
});

// ---------------------------------------------------------------------------------------------
// The real pools (BTB Finance Discover snapshot, 2026-11-22 04:36 UTC)
// ---------------------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn pool(
    key: &str,
    chain: u64,
    (dex, ver, fee): (&str, &str, u32),
    (t0, t1): (&str, &str),
    (s0, s1): (&str, &str),
    stable: bool,
) -> PoolIn {
    (
        key.into(),
        chain,
        (dex.into(), ver.into(), fee),
        (t0.into(), t1.into()),
        (s0.into(), s1.into()),
        stable,
        ("btb-discover".into(), AS_OF),
    )
}

type Facts = (
    (u64, Option<u64>, Option<u64>),
    (i64, i64),
    (Option<i64>, u32, Option<u32>, Option<i64>),
    Option<i64>,
);

fn stat(key: &str, (money, apr, range, change): Facts) -> StatIn {
    (key.into(), money, apr, range, change, AS_OF)
}

fn real_pools() -> Vec<PoolIn> {
    vec![
        pool(
            "8453:0xa41bc0affba7fd420d186b84899d7ab2ac57fcd1",
            8453,
            ("Aerodrome", "V3", 11),
            (
                "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
                "0xfde4c96c8593536e31f229ea8f37b2ada2699bb2",
            ),
            ("USDC", "USDT"),
            true,
        ),
        pool(
            WETH_CBBTC,
            8453,
            ("Aerodrome", "V3", 85),
            (
                "0x4200000000000000000000000000000000000006",
                "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf",
            ),
            ("WETH", "cbBTC"),
            false,
        ),
        pool(
            WETH_USDC,
            8453,
            ("Uniswap", "V3", 3000),
            (
                "0x4200000000000000000000000000000000000006",
                "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
            ),
            ("WETH", "USDC"),
            false,
        ),
        pool(
            "1:0xe63e32b2ae40601662f760d6bf5d771057324fbd97784fe1d3717069f7b75d45",
            1,
            ("Uniswap", "V4", 5),
            (
                "0x6c3ea9036406852006290770bedfcaba0e23a0e8",
                "0xdc035d45d973e3ec169d2276ddab16f1e407384f",
            ),
            ("PYUSD", "USDS"),
            true,
        ),
        pool(
            "1:0x2d90ebffc87bbe29e4d78e7627adfe9d0f3d7978",
            1,
            ("Aerodrome Slipstream", "V3", 10),
            (
                "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                "0xdac17f958d2ee523a2206206994597c13d831ec7",
            ),
            ("USDC", "USDT"),
            true,
        ),
        pool(
            "56:0xf43fdb854021ddeb41e06ac1d6e5df475197038ba5d3cba147f469a56870cd1b",
            56,
            ("Pancakeswap Infinity", "V3", 70),
            (
                "0xeec6574eabba52bac3f0277f2cd5ac7e67197886",
                "0x55d398326f99059ff775485246999027b3197955",
            ),
            ("KII", "USDT"),
            false,
        ),
        pool(
            "56:0x9485ff32b6b4444c21d5abe4d9a2283d127075a2",
            56,
            ("PancakeSwap", "V3", 100),
            (
                "0x4fa7c69a7b69f8bc48233024d546bc299d6b03bf",
                "0x55d398326f99059ff775485246999027b3197955",
            ),
            ("quq", "USDT"),
            false,
        ),
        pool(
            "8453:0x038bcd3cd0ea49ebb6cf994306d46c85d1c143af",
            8453,
            ("Aerodrome", "V3", 100),
            (
                "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
                "0xd11b4bba7fb5f9345ac4922cdfa9423f06b1cb07",
            ),
            ("USDC", "Anthropic"),
            false,
        ),
        pool(
            "8453:0xb0f8d3d5981f4de5a1854569266f1e721b9fc01e",
            8453,
            ("Aerodrome", "V3", 100),
            (
                "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
                "0xe6f8fc5cf6a1e97e1538fbd0f1c660e3556a3e6c",
            ),
            ("USDC", "Paper"),
            false,
        ),
    ]
}

fn real_stats() -> Vec<StatIn> {
    vec![
        stat(
            "8453:0xa41bc0affba7fd420d186b84899d7ab2ac57fcd1",
            (
                (463_790, Some(5_066_529), Some(56)),
                (439, 0),
                (Some(52), 8, Some(10_000), Some(1)),
                Some(16),
            ),
        ),
        stat(
            WETH_CBBTC,
            (
                (4_664_007, Some(164_038_200), Some(13_246)),
                (10_366, 0),
                (Some(4_231), 8, Some(10_000), Some(49)),
                Some(663),
            ),
        ),
        stat(
            WETH_USDC,
            (
                (80_717_780, Some(84_669_746), Some(211_666)),
                (9_571, 0),
                (Some(22_510), 8, Some(9_200), Some(66)),
                Some(679),
            ),
        ),
        stat(
            "1:0xe63e32b2ae40601662f760d6bf5d771057324fbd97784fe1d3717069f7b75d45",
            (
                (100_191_204, Some(104_709_007), Some(524)),
                (19, 0),
                (Some(0), 8, None, Some(0)),
                Some(-12),
            ),
        ),
        stat(
            "1:0x2d90ebffc87bbe29e4d78e7627adfe9d0f3d7978",
            (
                (3_780_902, Some(59_906_883), Some(599)),
                (578, 0),
                (Some(19), 8, Some(10_000), Some(0)),
                Some(0),
            ),
        ),
        stat(
            "56:0xf43fdb854021ddeb41e06ac1d6e5df475197038ba5d3cba147f469a56870cd1b",
            (
                (1_734_036, Some(180_103_105), Some(12_607)),
                (26_537, 0),
                (None, 0, None, None),
                Some(919),
            ),
        ),
        stat(
            "56:0x9485ff32b6b4444c21d5abe4d9a2283d127075a2",
            (
                (987_032, Some(102_807_994), Some(8_225)),
                (30_414, 0),
                (None, 0, None, None),
                Some(-6),
            ),
        ),
        stat(
            "8453:0x038bcd3cd0ea49ebb6cf994306d46c85d1c143af",
            (
                (700_301, Some(50_933_752), Some(5_093)),
                (26_547, 0),
                (None, 0, None, None),
                Some(-5),
            ),
        ),
        stat(
            "8453:0xb0f8d3d5981f4de5a1854569266f1e721b9fc01e",
            (
                (1_113_303, Some(40_181_118), Some(4_018)),
                (13_174, 0),
                (None, 0, None, None),
                Some(-8_948),
            ),
        ),
    ]
}

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

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

    /// A chain with the provider set and the real pools and stats published.
    fn loaded() -> Self {
        let mut ch = Chain::new();
        ch.mutate::<()>("init", PROVIDER, &());
        assert_eq!(
            ch.mutate::<u32>("publish_pools", PROVIDER, &(real_pools(),)),
            9
        );
        assert_eq!(
            ch.mutate::<u32>("publish_stats", PROVIDER, &(real_stats(),)),
            9
        );
        ch
    }

    fn run(
        &mut self,
        kind: FunctionKind,
        name: &str,
        who: Address,
        input: &[u8],
        gas: u64,
    ) -> Result<(Vec<u8>, u64), CallError> {
        let mut h = RuntimeHost::new(&self.kv, &self.tables, CONTRACT, who.0, self.height);
        let out = self.c.call(kind, name, input, &mut h, gas, MEM)?;
        let batch = h.finish().expect("no faults");
        if kind == FunctionKind::Mutation {
            self.kv.commit(batch).unwrap();
            self.height += 1;
        }
        Ok((out.output, out.gas_used))
    }

    fn mutate<T: Decode>(&mut self, name: &str, who: Address, input: &impl Encode) -> T {
        let (out, _) = self
            .run(FunctionKind::Mutation, name, who, &input.encode(), GAS)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        T::decode(&out).expect(name)
    }

    /// A query under the chain's default query gas; also returns the gas it used.
    fn query_gas<T: Decode>(&mut self, name: &str, input: &impl Encode) -> (T, u64) {
        let (out, gas) = self
            .run(FunctionKind::Query, name, EVE, &input.encode(), QUERY_GAS)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        (T::decode(&out).expect(name), gas)
    }

    fn query<T: Decode>(&mut self, name: &str, input: &impl Encode) -> T {
        self.query_gas(name, input).0
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
            .run(FunctionKind::Mutation, name, who, &input.encode(), GAS)
            .expect_err(name);
        assert_eq!(abort_msg(e), msg, "{name}");
        assert_eq!(self.snapshot(), before, "{name} left a trace");
    }
}

/// Headline APR of a stat input: the range APR when measured, else base + reward.
fn headline(s: &StatIn) -> i64 {
    s.3 .0.unwrap_or(s.2 .0 + s.2 .1)
}

fn tvl(s: &StatIn) -> u64 {
    s.1 .0
}

fn chain_of(key: &str) -> u64 {
    key.split_once(':').unwrap().0.parse().unwrap()
}

fn grade_in(token: &str, score: u8, flags: u32) -> GradeIn {
    (
        token.into(),
        score,
        flags,
        (Some(1_000), Some(2_000)),
        AS_OF,
    )
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[test]
fn one_call_returns_a_pool_with_stats_history_and_risk() {
    let mut ch = Chain::loaded();
    let p: Option<PoolPayload> = ch.query("pool", &(WETH_CBBTC.to_string(), 10u32));
    let p = p.expect("pool exists");
    assert_eq!(p.pool.symbol0, "WETH");
    assert_eq!(p.pool.fee_pips, 85);
    let s = p.stat.expect("stat");
    // The measured range APR (42.31 %) is the headline, not the 103.66 % whole-pool APR.
    assert_eq!(s.apr_bps, 4_231);
    assert_eq!(s.base_apr_bps, 10_366);
    assert_eq!(
        (s.tvl_usd, s.range_days, s.in_range_bps),
        (4_664_007, 8, Some(10_000))
    );
    assert_eq!((p.fees_24h_usd, p.fees_estimated), (Some(13_246), false));
    assert_eq!(p.history.len(), 1);
    assert_eq!(p.history[0].hour, AS_OF / 3_600);
    assert_eq!(p.history[0].apr_bps, 4_231);
    // 100 - 5 (TVL under $10M) - 10 (not a stable pair) - 5 - 5 (two ungraded tokens) = 75: B.
    let r = p.risk.expect("risk");
    assert_eq!(
        r,
        Risk {
            score: 75,
            grade: 2,
            reasons: R_LOW_TVL | R_IMPERMANENT_LOSS | R_UNGRADED_TOKEN
        }
    );

    // Grading both tokens well lifts it to 85: A. A honeypot flag on one sinks it to E.
    let weth = "8453:0x4200000000000000000000000000000000000006";
    let cbbtc = "8453:0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf";
    let g = vec![grade_in(weth, 95, 0), grade_in(cbbtc, 90, 0)];
    assert_eq!(ch.mutate::<u32>("publish_grades", PROVIDER, &(g,)), 2);
    let p: Option<PoolPayload> = ch.query("pool", &(WETH_CBBTC.to_string(), 0u32));
    let p = p.unwrap();
    assert_eq!(p.grade0.as_ref().map(|g| g.score), Some(95));
    assert!(p.history.is_empty(), "hours = 0 returns no history");
    assert_eq!(
        p.risk.unwrap(),
        Risk {
            score: 85,
            grade: 1,
            reasons: R_LOW_TVL | R_IMPERMANENT_LOSS
        }
    );
    let mut bad = grade_in(cbbtc, 40, FLAG_HONEYPOT);
    bad.4 = AS_OF + 60;
    assert_eq!(
        ch.mutate::<u32>("publish_grades", PROVIDER, &(vec![bad],)),
        1
    );
    let p: Option<PoolPayload> = ch.query("pool", &(WETH_CBBTC.to_string(), 0u32));
    // 85 - 25 (score under 50) - 40 (honeypot) = 20: E.
    assert_eq!(
        p.unwrap().risk.unwrap(),
        Risk {
            score: 20,
            grade: 5,
            reasons: R_LOW_TVL | R_IMPERMANENT_LOSS | R_TOKEN_RISK
        }
    );

    let missing: Option<PoolPayload> = ch.query("pool", &("8453:0xdead".to_string(), 1u32));
    assert!(missing.is_none());
}

#[test]
fn older_stats_never_roll_a_pool_back_and_newer_ones_add_history() {
    let mut ch = Chain::loaded();
    let mut old = real_stats()[1].clone(); // WETH/cbBTC
    old.3 .0 = Some(1);
    old.5 = AS_OF - 3_600;
    assert_eq!(
        ch.mutate::<u32>("publish_stats", PROVIDER, &(vec![old],)),
        0
    );
    let mut same = real_stats()[1].clone();
    same.3 .0 = Some(2);
    assert_eq!(
        ch.mutate::<u32>("publish_stats", PROVIDER, &(vec![same],)),
        0
    );

    let mut next = real_stats()[1].clone();
    next.3 .0 = Some(4_500);
    next.1 .0 = 5_000_000;
    next.5 = AS_OF + 3_600;
    assert_eq!(
        ch.mutate::<u32>("publish_stats", PROVIDER, &(vec![next],)),
        1
    );
    let p: Option<PoolPayload> = ch.query("pool", &(WETH_CBBTC.to_string(), 400u32));
    let p = p.unwrap();
    assert_eq!(p.stat.unwrap().apr_bps, 4_500);
    let hours: Vec<(u64, i64)> = p.history.iter().map(|h| (h.hour, h.apr_bps)).collect();
    assert_eq!(
        hours,
        vec![(AS_OF / 3_600 + 1, 4_500), (AS_OF / 3_600, 4_231)]
    );
}

#[test]
fn top_pools_rank_by_headline_apr_with_filters() {
    let mut ch = Chain::loaded();
    let stats = real_stats();
    let mut expected: Vec<(String, i64)> =
        stats.iter().map(|s| (s.0.clone(), headline(s))).collect();
    expected.sort_by_key(|x| std::cmp::Reverse(x.1));

    let all: Vec<TopRow> = ch.query("top_pools", &(0u64, 0u8, 0u64, false, 100u32));
    let got: Vec<(String, i64)> = all.iter().map(|r| (r.0.clone(), r.3)).collect();
    assert_eq!(got, expected);
    // WETH/USDC on Base: 225.10 % measured range APR, ranked by it rather than 95.71 %.
    let weth_usdc = all.iter().find(|r| r.0 == WETH_USDC).unwrap();
    assert_eq!(
        (weth_usdc.1.as_str(), weth_usdc.2, weth_usdc.3),
        ("WETH/USDC", 8453, 22_510)
    );
    // 100 - 10 (not stable) - 10 (APR over 200 %) - 10 (two ungraded tokens) = 70: B.
    assert_eq!((weth_usdc.6, weth_usdc.5), (70, 2));

    // Base only, $1M+ TVL, top 3.
    let base: Vec<TopRow> = ch.query("top_pools", &(8453u64, 0u8, 1_000_000u64, false, 3u32));
    let mut exp: Vec<(String, i64)> = stats
        .iter()
        .filter(|s| chain_of(&s.0) == 8453 && tvl(s) >= 1_000_000)
        .map(|s| (s.0.clone(), headline(s)))
        .collect();
    exp.sort_by_key(|x| std::cmp::Reverse(x.1));
    exp.truncate(3);
    assert_eq!(
        base.iter().map(|r| (r.0.clone(), r.3)).collect::<Vec<_>>(),
        exp
    );

    // Stable pairs only.
    let stable: Vec<TopRow> = ch.query("top_pools", &(0u64, 0u8, 0u64, true, 100u32));
    assert_eq!(stable.len(), 3);
    assert!(stable.iter().all(|r| r.1.contains("USD")));

    // Grade B or better: every returned row has grade <= 2, and none is left out.
    let b: Vec<TopRow> = ch.query("top_pools", &(0u64, 2u8, 0u64, false, 100u32));
    assert!(b.iter().all(|r| r.5 <= 2));
    assert_eq!(b.len(), all.iter().filter(|r| r.5 <= 2).count());
}

#[test]
fn best_for_keeps_a_position_under_two_percent_of_the_pool() {
    let mut ch = Chain::loaded();
    // $100,000 needs at least $5M of TVL.
    let rows: Vec<TopRow> = ch.query("best_for", &(100_000u64, 0u8, 0u64, 100u32));
    let mut exp: Vec<(String, i64)> = real_stats()
        .iter()
        .filter(|s| tvl(s) >= 5_000_000)
        .map(|s| (s.0.clone(), headline(s)))
        .collect();
    exp.sort_by_key(|x| std::cmp::Reverse(x.1));
    assert_eq!(
        rows.iter().map(|r| (r.0.clone(), r.3)).collect::<Vec<_>>(),
        exp
    );
    assert_eq!(rows[0].0, WETH_USDC);
    // A larger position than any pool can take finds nothing.
    let none: Vec<TopRow> = ch.query("best_for", &(10_000_000_000u64, 0u8, 0u64, 10u32));
    assert!(none.is_empty());
}

#[test]
fn chain_index_matches_plain_math() {
    let mut ch = Chain::loaded();
    for (chain, stable_only) in [(8453u64, false), (8453, true), (1, false), (56, false)] {
        let rows: Vec<&StatIn> = Vec::from_iter(
            real_stats_static()
                .iter()
                .filter(|s| chain_of(&s.0) == chain && tvl(s) > 0),
        );
        let rows: Vec<&StatIn> = rows
            .into_iter()
            .filter(|s| !stable_only || is_stable(&s.0))
            .collect();
        let mut aprs: Vec<i64> = rows.iter().map(|s| headline(s)).collect();
        aprs.sort();
        let tvl_sum: u128 = rows.iter().map(|s| tvl(s) as u128).sum();
        let weighted: i128 = rows
            .iter()
            .map(|s| headline(s) as i128 * tvl(s) as i128)
            .sum();
        let v: IndexView = ch.query("index", &(chain, stable_only));
        assert_eq!(v.pools as usize, rows.len(), "chain {chain}");
        assert_eq!(v.tvl_usd, tvl_sum);
        assert_eq!(
            v.median_apr_bps,
            aprs.get((aprs.len().max(1) - 1) / 2).copied()
        );
        assert_eq!(
            v.tvl_weighted_apr_bps,
            Some((weighted / tvl_sum as i128) as i64)
        );
    }
    // A newer stat replaces the old one in the totals instead of adding to them.
    let mut s = real_stats()[1].clone(); // WETH/cbBTC on Base: TVL $4,664,007 -> $5,000,000
    s.1 .0 = 5_000_000;
    s.5 = AS_OF + 3_600;
    let before: IndexView = ch.query("index", &(8453u64, false));
    assert_eq!(ch.mutate::<u32>("publish_stats", PROVIDER, &(vec![s],)), 1);
    let after: IndexView = ch.query("index", &(8453u64, false));
    assert_eq!(after.pools, before.pools);
    assert_eq!(after.tvl_usd, before.tvl_usd - 4_664_007 + 5_000_000);
    // Base has five of the real pools.
    let v: IndexView = ch.query("index", &(8453u64, false));
    assert_eq!(v.pools, 5);
    let empty: IndexView = ch.query("index", &(10u64, false));
    assert_eq!(
        (
            empty.pools,
            empty.median_apr_bps,
            empty.tvl_weighted_apr_bps
        ),
        (0, None, None)
    );
}

fn real_stats_static() -> &'static [StatIn] {
    static S: OnceLock<Vec<StatIn>> = OnceLock::new();
    S.get_or_init(real_stats)
}

fn is_stable(key: &str) -> bool {
    real_pools().iter().find(|p| p.0 == key).unwrap().5
}

#[test]
fn missing_fees_are_estimated_from_the_base_apr() {
    let mut ch = Chain::loaded();
    let mut s = real_stats()[8].clone(); // USDC/Paper on Base: $1,113,303 at 131.74 % base APR
    s.1 .2 = None;
    s.5 = AS_OF + 60;
    assert_eq!(ch.mutate::<u32>("publish_stats", PROVIDER, &(vec![s],)), 1);
    let p: Option<PoolPayload> = ch.query("pool", &(real_stats()[8].0.clone(), 0u32));
    let p = p.unwrap();
    assert!(p.fees_estimated);
    assert_eq!(p.fees_24h_usd, Some(1_113_303u64 * 13_174 / (10_000 * 365)));
}

#[test]
fn yields_of_every_kind_rank_together_with_pools() {
    let mut ch = Chain::loaded();
    // Test values, not market data.
    let y = |key: &str,
             kind: u8,
             asset: &str,
             venue: &str,
             chain: Option<u64>,
             rate: i64,
             liq: u8,
             tvl: Option<u64>|
     -> YieldIn {
        (
            key.into(),
            kind,
            (asset.into(), venue.into()),
            chain,
            rate,
            liq,
            tvl,
            ("test".into(), AS_OF),
        )
    };
    let rows = vec![
        y(
            "tbill:usd-3m:us-treasury",
            KIND_TBILL,
            "USD 3M",
            "US Treasury",
            None,
            380,
            1,
            None,
        ),
        y(
            "staking:eth:ethereum",
            KIND_STAKING,
            "ETH",
            "Ethereum",
            Some(1),
            290,
            1,
            Some(9_000_000_000),
        ),
        y(
            "lending:usdc:aave-v3-base",
            KIND_LENDING,
            "USDC",
            "Aave V3",
            Some(8453),
            450,
            0,
            Some(400_000),
        ),
    ];
    assert_eq!(ch.mutate::<u32>("publish_yields", PROVIDER, &(rows,)), 3);

    let (y_tbill, r): (YieldRateOut, Risk) = ch
        .query::<Option<(YieldRateOut, Risk)>>(
            "yield_rate",
            &("tbill:usd-3m:us-treasury".to_string(),),
        )
        .unwrap();
    assert_eq!((y_tbill.rate_bps, y_tbill.kind), (380, KIND_TBILL));
    assert_eq!((r.score, r.grade), (95, 1));
    // Lending with $400k: 75 - 15 (TVL under $1M) = 60: C.
    let (_, r): (YieldRateOut, Risk) = ch
        .query::<Option<(YieldRateOut, Risk)>>(
            "yield_rate",
            &("lending:usdc:aave-v3-base".to_string(),),
        )
        .unwrap();
    assert_eq!((r.score, r.grade, r.reasons), (60, 3, R_LOW_TVL));

    // T-bills and staking only.
    let mask = (1u32 << KIND_TBILL) | (1 << KIND_STAKING);
    let only: Vec<YieldOut> = ch.query("top_yields", &(mask, 0u8, 10u32));
    let keys: Vec<&str> = only.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(
        keys,
        vec!["tbill:usd-3m:us-treasury", "staking:eth:ethereum"]
    );
    assert_eq!(only[0].2, "USD 3M on US Treasury");

    // Everything of grade A: the T-bill (95) and staking (75 is B, so out), pools of grade A.
    let a: Vec<YieldOut> = ch.query("top_yields", &(0u32, 1u8, 50u32));
    assert!(a.iter().all(|r| r.6 == 1));
    assert!(a.iter().any(|r| r.0 == "tbill:usd-3m:us-treasury"));

    // All kinds: pools (kind LP) and yields in one list, highest rate first.
    let all: Vec<YieldOut> = ch.query("top_yields", &(0u32, 0u8, 100u32));
    assert_eq!(all.len(), 9 + 3);
    assert!(all.windows(2).all(|w| w[0].4 >= w[1].4));
    // The top is the highest headline APR of the real pools (quq/USDT on BNB Chain, 304.14 %).
    let best = real_stats().into_iter().max_by_key(headline).unwrap();
    assert_eq!(
        (all[0].0.as_str(), all[0].1, all[0].4),
        (best.0.as_str(), KIND_LP, 30_414)
    );
}

record!(YieldRateOut {
    key: String,
    kind: u8,
    kind_rate: u128,
    asset: String,
    venue: String,
    chain_id: Option<u64>,
    rate_bps: i64,
    liquidity: u8,
    tvl_usd: Option<u64>,
    source: String,
    as_of: u64,
    height: u64,
});

#[test]
fn pruning_keeps_the_newest_hours() {
    let mut ch = Chain::loaded();
    for i in 1..=5u64 {
        let mut s = real_stats()[1].clone();
        s.5 = AS_OF + i * 3_600;
        assert_eq!(ch.mutate::<u32>("publish_stats", PROVIDER, &(vec![s],)), 1);
    }
    let p: Option<PoolPayload> = ch.query("pool", &(WETH_CBBTC.to_string(), 400u32));
    assert_eq!(p.unwrap().history.len(), 6);
    ch.fails(
        "prune_hours",
        EVE,
        &(WETH_CBBTC.to_string(), 2u32),
        "not the provider",
    );
    assert_eq!(
        ch.mutate::<u32>("prune_hours", PROVIDER, &(WETH_CBBTC.to_string(), 2u32)),
        4
    );
    let p: Option<PoolPayload> = ch.query("pool", &(WETH_CBBTC.to_string(), 400u32));
    let hours: Vec<u64> = p.unwrap().history.iter().map(|h| h.hour).collect();
    assert_eq!(hours, vec![AS_OF / 3_600 + 5, AS_OF / 3_600 + 4]);
    // Other pools' history is untouched.
    let q: Option<PoolPayload> = ch.query("pool", &(WETH_USDC.to_string(), 400u32));
    assert_eq!(q.unwrap().history.len(), 1);
}

#[test]
fn bad_input_is_refused_and_leaves_no_trace() {
    let mut ch = Chain::new();
    ch.fails(
        "publish_pools",
        PROVIDER,
        &(real_pools(),),
        "not the provider",
    );
    ch.mutate::<()>("init", PROVIDER, &());
    ch.fails("init", EVE, &(), "already initialised");
    ch.fails("publish_pools", EVE, &(real_pools(),), "not the provider");
    ch.fails(
        "publish_pools",
        PROVIDER,
        &(Vec::<PoolIn>::new(),),
        "batch must hold 1..=64 rows",
    );
    let many: Vec<PoolIn> = (0..65).map(|_| real_pools()[0].clone()).collect();
    ch.fails(
        "publish_pools",
        PROVIDER,
        &(many,),
        "batch must hold 1..=64 rows",
    );

    // The key's chain must match the row's chain; one bad row refuses the whole batch.
    let mut wrong = real_pools()[1].clone();
    wrong.1 = 1;
    let mut batch = real_pools();
    batch.push(wrong);
    ch.fails("publish_pools", PROVIDER, &(batch,), "bad pool key");
    let mut pipe = real_pools()[0].clone();
    pipe.0 = "8453:0xab|cd".into();
    ch.fails("publish_pools", PROVIDER, &(vec![pipe],), "bad pool key");
    let mut fee = real_pools()[0].clone();
    fee.2 .2 = 1_000_001;
    ch.fails("publish_pools", PROVIDER, &(vec![fee],), "bad fee");

    ch.fails("publish_stats", PROVIDER, &(real_stats(),), "unknown pool");
    assert_eq!(
        ch.mutate::<u32>("publish_pools", PROVIDER, &(real_pools(),)),
        9
    );
    let mut s = real_stats()[0].clone();
    s.2 .0 = 10_000_001;
    ch.fails("publish_stats", PROVIDER, &(vec![s],), "bad apr");
    let mut s = real_stats()[0].clone();
    s.3 .2 = Some(10_001);
    ch.fails("publish_stats", PROVIDER, &(vec![s],), "bad in-range share");
    let mut s = real_stats()[0].clone();
    s.3 .1 = 0;
    ch.fails(
        "publish_stats",
        PROVIDER,
        &(vec![s],),
        "range apr needs its days",
    );

    ch.fails(
        "publish_grades",
        PROVIDER,
        &(vec![grade_in("8453:0xab", 101, 0)],),
        "bad score",
    );
    ch.fails(
        "publish_grades",
        PROVIDER,
        &(vec![grade_in("0:0xab", 50, 0)],),
        "bad token key",
    );
    ch.fails(
        "publish_grades",
        PROVIDER,
        &(vec![grade_in("0xab", 50, 0)],),
        "bad token key",
    );

    let y: YieldIn = (
        "lp:x:y".into(),
        KIND_LP,
        ("X".into(), "Y".into()),
        None,
        1,
        0,
        None,
        ("t".into(), AS_OF),
    );
    ch.fails(
        "publish_yields",
        PROVIDER,
        &(vec![y.clone()],),
        "bad kind (pools are published as pools)",
    );
    let mut y2 = y.clone();
    y2.1 = KIND_STAKING;
    y2.0 = "staking:eth".into();
    ch.fails("publish_yields", PROVIDER, &(vec![y2],), "bad yield key");
    let mut y3 = y;
    y3.1 = KIND_STAKING;
    y3.0 = "staking:eth:x".into();
    y3.5 = 3;
    ch.fails("publish_yields", PROVIDER, &(vec![y3],), "bad liquidity");

    // Key rotation: the old provider loses the role.
    let next = Address([0x42; 32]);
    ch.fails("set_provider", EVE, &(next,), "not the provider");
    ch.mutate::<()>("set_provider", PROVIDER, &(next,));
    ch.fails(
        "publish_stats",
        PROVIDER,
        &(real_stats(),),
        "not the provider",
    );
    assert_eq!(ch.mutate::<u32>("publish_stats", next, &(real_stats(),)), 9);
    let who: Option<Address> = ch.query("get_provider", &());
    assert_eq!(who, Some(next));
}

#[test]
fn non_evm_ids_keep_their_case() {
    let mut ch = Chain::new();
    ch.mutate::<()>("init", PROVIDER, &());
    // A Solana pool id and mints are base58 (case-sensitive); the provider maps the chain to an id.
    let sol = pool(
        "501:58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2",
        501,
        ("Raydium", "CLMM", 500),
        (
            "So11111111111111111111111111111111111111112",
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        ),
        ("SOL", "USDC"),
        false,
    );
    assert_eq!(
        ch.mutate::<u32>("publish_pools", PROVIDER, &(vec![sol],)),
        1
    );
    let p: Option<PoolPayload> = ch.query(
        "pool",
        &(
            "501:58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2".to_string(),
            0u32,
        ),
    );
    assert_eq!(
        p.unwrap().pool.token1,
        "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
    );
}

#[test]
fn a_full_batch_of_worst_case_rows_fits_in_the_heap() {
    // 64 rows (MAX_BATCH) with the longest keys allowed in practice (V4 pool ids are 66
    // characters) and long labels: the batch limit must hold for them, not only for short rows.
    let mut ch = Chain::new();
    ch.mutate::<()>("init", PROVIDER, &());
    let key = |i: u64| format!("8453:0x{i:064x}");
    let pools: Vec<PoolIn> = (0..64)
        .map(|i| {
            pool(
                &key(i),
                8453,
                ("Pancakeswap Infinity Concentrat", "V4", 3_000),
                (
                    &format!("0x{:040x}", i * 2 + 1),
                    &format!("0x{:040x}", i * 2 + 2),
                ),
                ("ABCDEFGHIJKLMNOPQRSTUVWX", "ABCDEFGHIJKLMNOPQRSTUVWX"),
                i % 2 == 0,
            )
        })
        .collect();
    assert_eq!(ch.mutate::<u32>("publish_pools", PROVIDER, &(pools,)), 64);
    let grades: Vec<GradeIn> = (0..64)
        .map(|i| grade_in(&format!("8453:0x{:040x}", i * 2 + 1), 80, 0))
        .collect();
    assert_eq!(ch.mutate::<u32>("publish_grades", PROVIDER, &(grades,)), 64);
    let stats = |at: u64| -> Vec<StatIn> {
        (0..64)
            .map(|i| {
                let mut s = stat(
                    &key(i),
                    (
                        (u64::MAX / 4, Some(u64::MAX / 4), Some(u64::MAX / 4)),
                        (123_456, 7_890),
                        (Some(98_765), 30, Some(9_999), Some(-1_234)),
                        Some(-5_678),
                    ),
                );
                s.5 = at;
                s
            })
            .collect()
    };
    // Twice: the second batch updates every row (the heavier path: old row, chain totals).
    assert_eq!(
        ch.mutate::<u32>("publish_stats", PROVIDER, &(stats(AS_OF),)),
        64
    );
    assert_eq!(
        ch.mutate::<u32>("publish_stats", PROVIDER, &(stats(AS_OF + 3_600),)),
        64
    );
    let v: IndexView = ch.query("index", &(8453u64, false));
    assert_eq!(v.pools, 64);
}

#[test]
fn rankings_and_indices_stay_within_the_query_limits() {
    let mut ch = Chain::new();
    ch.mutate::<()>("init", PROVIDER, &());
    // 2,000 synthetic Base pools: far more than one query may decode.
    const N: u64 = 2_000;
    let ids: Vec<u64> = (0..N).collect();
    for chunk in ids.chunks(64) {
        let pools: Vec<PoolIn> = chunk
            .iter()
            .map(|i| {
                pool(
                    &format!("8453:0x{i:040x}"),
                    8453,
                    ("Uniswap", "V3", 500),
                    (
                        &format!("0x{:040x}", i * 2 + 1),
                        &format!("0x{:040x}", i * 2 + 2),
                    ),
                    ("AAA", "BBB"),
                    i % 3 == 0,
                )
            })
            .collect();
        assert_eq!(
            ch.mutate::<u32>("publish_pools", PROVIDER, &(pools,)),
            chunk.len() as u32
        );
        let stats: Vec<StatIn> = chunk
            .iter()
            .map(|i| {
                stat(
                    &format!("8453:0x{i:040x}"),
                    (
                        (50_000 + i * 1_000, Some(1_000_000), Some(100)),
                        ((i * 37 % 9_000) as i64, 0),
                        (None, 0, None, None),
                        None,
                    ),
                )
            })
            .collect();
        assert_eq!(
            ch.mutate::<u32>("publish_stats", PROVIDER, &(stats,)),
            chunk.len() as u32
        );
    }
    let apr = |i: u64| (i * 37 % 9_000) as i64;
    let tvl = |i: u64| 50_000 + i * 1_000;

    // Stable pairs over all chains: no index narrows that, so the ranking reads in APR order and
    // filters (a third of the synthetic pools are stable), the heaviest scan a query can make.
    let (rows, gas): (Vec<TopRow>, u64) =
        ch.query_gas("top_pools", &(0u64, 0u8, 0u64, true, 100u32));
    assert_eq!(rows.len(), 100);
    assert!(rows.windows(2).all(|w| w[0].3 >= w[1].3));
    println!("top_pools, stable pairs on all chains: {gas} gas");
    assert!(gas < QUERY_GAS);
    // A TVL floor no pool reaches reads only the empty top tier.
    let (rows, _): (Vec<TopRow>, u64) =
        ch.query_gas("top_pools", &(8453u64, 0u8, u64::MAX, false, 50u32));
    assert!(rows.is_empty());
    let (rows, gas): (Vec<TopRow>, u64) =
        ch.query_gas("top_pools", &(8453u64, 0u8, 0u64, false, 100u32));
    assert_eq!(rows.len(), 100);
    assert!(rows.windows(2).all(|w| w[0].3 >= w[1].3));
    assert_eq!(rows[0].3, (0..N).map(apr).max().unwrap());
    println!("top_pools, 100 rows: {gas} gas");
    let (rows, _): (Vec<TopRow>, u64) =
        ch.query_gas("top_pools", &(8453u64, 0u8, 0u64, true, 100u32));
    assert_eq!(rows.len(), 100);
    assert_eq!(
        rows[0].3,
        (0..N).filter(|i| i % 3 == 0).map(apr).max().unwrap()
    );

    // The index is exact for all 2,000 pools.
    for stable_only in [false, true] {
        let ids: Vec<u64> = (0..N).filter(|i| !stable_only || i % 3 == 0).collect();
        let mut aprs: Vec<i64> = ids.iter().map(|i| apr(*i)).collect();
        aprs.sort();
        let tvl_sum: u128 = ids.iter().map(|i| tvl(*i) as u128).sum();
        let weighted: i128 = ids.iter().map(|i| apr(*i) as i128 * tvl(*i) as i128).sum();
        let (v, gas): (IndexView, u64) = ch.query_gas("index", &(8453u64, stable_only));
        assert_eq!(v.pools, ids.len() as u64);
        assert_eq!(v.tvl_usd, tvl_sum);
        assert_eq!(v.median_apr_bps, Some(aprs[(aprs.len() - 1) / 2]));
        assert_eq!(
            v.tvl_weighted_apr_bps,
            Some((weighted / tvl_sum as i128) as i64)
        );
        println!("index (stable_only {stable_only}): {gas} gas");
    }

    let (rows, gas): (Vec<YieldOut>, u64) = ch.query_gas("top_yields", &(0u32, 0u8, 100u32));
    assert_eq!(rows.len(), 100);
    println!("top_yields: {gas} gas");
}

#[test]
fn rankings_use_the_stored_risk_until_rescored() {
    let mut ch = Chain::loaded();
    let row = |ch: &mut Chain| -> TopRow {
        let all: Vec<TopRow> = ch.query("top_pools", &(8453u64, 0u8, 0u64, false, 50u32));
        all.into_iter().find(|r| r.0 == WETH_USDC).unwrap()
    };
    // WETH/USDC: $80.7M TVL, not stable, 225.10 % APR, tokens ungraded:
    // 100 - 10 - 10 - 5 - 5 = 70: B.
    assert_eq!((row(&mut ch).6, row(&mut ch).5), (70, 2));
    let weth = "8453:0x4200000000000000000000000000000000000006";
    let usdc = "8453:0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
    let g = vec![grade_in(weth, 95, 0), grade_in(usdc, 98, 0)];
    assert_eq!(ch.mutate::<u32>("publish_grades", PROVIDER, &(g,)), 2);
    // The ranking still holds the score written with the stats; `pool` computes it live.
    assert_eq!(row(&mut ch).6, 70);
    let p: Option<PoolPayload> = ch.query("pool", &(WETH_USDC.to_string(), 0u32));
    let live = p.unwrap().risk.unwrap();
    assert_eq!(
        live,
        Risk {
            score: 80,
            grade: 2,
            reasons: R_IMPERMANENT_LOSS | R_HIGH_APR
        }
    );
    ch.fails(
        "rescore",
        EVE,
        &(vec![WETH_USDC.to_string()],),
        "not the provider",
    );
    let keys = vec![WETH_USDC.to_string(), "8453:0xmissing".to_string()];
    assert_eq!(ch.mutate::<u32>("rescore", PROVIDER, &(keys,)), 1);
    assert_eq!((row(&mut ch).6, row(&mut ch).5), (80, 2));

    // A range that stayed in range under half the time costs 15 more, and the next stats keep
    // the rescored token part.
    let mut s = real_stats()[2].clone();
    s.3 .2 = Some(4_000);
    s.5 = AS_OF + 3_600;
    assert_eq!(ch.mutate::<u32>("publish_stats", PROVIDER, &(vec![s],)), 1);
    assert_eq!(row(&mut ch).6, 65);
    let p: Option<PoolPayload> = ch.query("pool", &(WETH_USDC.to_string(), 0u32));
    assert_eq!(
        p.unwrap().risk.unwrap().reasons,
        R_IMPERMANENT_LOSS | R_HIGH_APR | R_OUT_OF_RANGE
    );
}

#[test]
fn history_is_capped_at_two_weeks_per_call() {
    let mut ch = Chain::loaded();
    // 720 more hours of WETH/cbBTC (more than one call may return), 64 rows per batch: rows of
    // one pool in one batch apply in
    // order, each newer than the last.
    let hours: Vec<u64> = (1..=720).collect();
    for chunk in hours.chunks(64) {
        let rows: Vec<StatIn> = chunk
            .iter()
            .map(|h| {
                let mut s = real_stats()[1].clone();
                s.3 .0 = Some(4_000 + *h as i64);
                s.5 = AS_OF + h * 3_600;
                s
            })
            .collect();
        let n = chunk.len() as u32;
        assert_eq!(ch.mutate::<u32>("publish_stats", PROVIDER, &(rows,)), n);
    }
    let (p, gas): (Option<PoolPayload>, u64) =
        ch.query_gas("pool", &(WETH_CBBTC.to_string(), 10_000u32));
    let p = p.unwrap();
    assert_eq!(p.history.len(), 336, "capped at MAX_HOURS");
    assert_eq!(p.history[0].apr_bps, 4_720);
    assert_eq!(p.history[335].hour, AS_OF / 3_600 + 385);
    println!("pool with 720 hours: {gas} gas");
    // The chain totals still count the pool once.
    let v: IndexView = ch.query("index", &(8453u64, false));
    assert_eq!(v.pools, 5);
}

#[test]
fn filtered_rankings_find_good_pools_behind_thousands_of_junk_ones() {
    // What the live snapshot showed: the highest APRs are small, risky pools. 1,000 of them
    // (2,000 % to 50,000 % APR, under $100k TVL) sit above 20 good ones ($50M, 5 % to 24.5 %).
    // A ranking that reads in APR order and filters afterwards never reached the good ones.
    let mut ch = Chain::new();
    ch.mutate::<()>("init", PROVIDER, &());
    let key = |i: u64| format!("{}:0x{i:040x}", if i.is_multiple_of(2) { 8453 } else { 1 });
    let ids: Vec<u64> = (0..1_020).collect();
    for chunk in ids.chunks(64) {
        let pools: Vec<PoolIn> = chunk
            .iter()
            .map(|&i| {
                let good = i >= 1_000;
                pool(
                    &key(i),
                    if i.is_multiple_of(2) { 8453 } else { 1 },
                    ("Uniswap", "V3", 500),
                    (
                        &format!("0x{:040x}", i * 2 + 1),
                        &format!("0x{:040x}", i * 2 + 2),
                    ),
                    ("AAA", "BBB"),
                    good,
                )
            })
            .collect();
        ch.mutate::<u32>("publish_pools", PROVIDER, &(pools,));
        let stats: Vec<StatIn> = chunk
            .iter()
            .map(|&i| {
                let good = i >= 1_000;
                let (tvl, apr) = if good {
                    (50_000_000, 500 + (i - 1_000) as i64 * 100)
                } else {
                    (20_000 + i * 70, 200_000 + (i * 4_801) as i64 % 4_800_000)
                };
                stat(
                    &key(i),
                    (
                        (tvl, Some(1), Some(1)),
                        (apr, 0),
                        (None, 0, None, None),
                        None,
                    ),
                )
            })
            .collect();
        ch.mutate::<u32>("publish_stats", PROVIDER, &(stats,));
    }
    // The good pools, best first: stable, $50M, two ungraded tokens: 100 - 5 - 5 = 90, grade A.
    let mut good: Vec<(String, i64)> = (1_000..1_020u64)
        .map(|i| (key(i), 500 + (i - 1_000) as i64 * 100))
        .collect();
    good.sort_by_key(|x| std::cmp::Reverse(x.1));
    let got = |rows: &[TopRow]| rows.iter().map(|r| (r.0.clone(), r.3)).collect::<Vec<_>>();

    // Unfiltered, the junk leads (by design: highest APR first).
    let all: Vec<TopRow> = ch.query("top_pools", &(0u64, 0u8, 0u64, false, 10u32));
    assert!(all.iter().all(|r| r.4 < 100_000));

    // Grade B or better, TVL >= $1M: exactly the good pools, best first.
    let (b, gas): (Vec<TopRow>, u64) =
        ch.query_gas("top_pools", &(0u64, 2u8, 1_000_000u64, false, 10u32));
    assert_eq!(got(&b), good[..10].to_vec());
    assert!(b.iter().all(|r| r.5 == 1 && r.6 == 90));
    println!("top_pools grade <= B, TVL >= $1M: {gas} gas");
    // The same on one chain (Base holds the even ids).
    let base: Vec<TopRow> = ch.query("top_pools", &(8453u64, 2u8, 1_000_000u64, false, 50u32));
    let good_base: Vec<(String, i64)> = good
        .iter()
        .filter(|g| g.0.starts_with("8453:"))
        .cloned()
        .collect();
    assert_eq!(got(&base), good_base);
    // A TVL floor alone reads only the tiers above it.
    let (t, gas): (Vec<TopRow>, u64) =
        ch.query_gas("top_pools", &(0u64, 0u8, 10_000_000u64, false, 50u32));
    assert_eq!(got(&t), good);
    println!("top_pools TVL >= $10M: {gas} gas");
    // $250,000 needs a $12.5M pool: only the good ones qualify.
    let (bf, gas): (Vec<TopRow>, u64) = ch.query_gas("best_for", &(250_000u64, 0u8, 0u64, 5u32));
    assert_eq!(got(&bf), good[..5].to_vec());
    println!("best_for $250,000: {gas} gas");
    // Stable pairs on a chain still read the stable index.
    let st: Vec<TopRow> = ch.query("top_pools", &(1u64, 0u8, 0u64, true, 50u32));
    let good_eth: Vec<(String, i64)> = good
        .iter()
        .filter(|g| g.0.starts_with("1:"))
        .cloned()
        .collect();
    assert_eq!(got(&st), good_eth);
}
