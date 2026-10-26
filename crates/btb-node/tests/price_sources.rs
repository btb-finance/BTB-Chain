//! Price sources: adapters, the SSRF-safe fetch, caps, staleness, outliers and the background
//! fetcher. No external network: every endpoint is a mock on 127.0.0.1 (`mock_exchange`).
//! Security-sensitive: a validator signs what it fetched, so every refusal path is tested.

mod mock_exchange;

use btb_node::actions::{FetchError, FetchPolicy};
use btb_node::price_sources::*;
use mock_exchange::{MockExchange, MockResp};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BTC8: u8 = 8;

fn src(ex: &MockExchange, path: &str, pointer: &str) -> SourceConfig {
    SourceConfig::new("mock", &ex.url(path), pointer)
}

// ------------------------------------------------------------------ adapters on fixtures

#[test]
fn presets_parse_their_documented_answers() {
    let fixtures: [(&str, &str, &str); 4] = [
        (
            "binance-example",
            "BTCUSDT",
            r#"{"symbol":"BTCUSDT","price":"64123.45000000"}"#,
        ),
        (
            "coinbase-example",
            "BTC-USD",
            r#"{"data":{"base":"BTC","currency":"USD","amount":"64123.45"}}"#,
        ),
        (
            "bitstamp-example",
            "btcusd",
            r#"{"high":"65000","last":"64123.45","timestamp":"1700000000","bid":"1"}"#,
        ),
        (
            "okx-example",
            "BTC-USDT",
            r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT","last":"64123.45","ts":"1"}]}"#,
        ),
    ];
    for (name, symbol, body) in fixtures {
        let s = preset(name, symbol).unwrap();
        s.validate().unwrap();
        assert!(s.url.starts_with("https://"), "{name}");
        assert_eq!(
            parse_response(&s, BTC8, 200, body.as_bytes()),
            Ok(6_412_345_000_000),
            "{name}"
        );
    }
    assert!(preset("nope", "BTC").is_err());
    for bad in ["", "BTC/USD", "a b", "x?y=1", "../x", "ÄB"] {
        assert!(preset("binance-example", bad).is_err(), "{bad:?}");
    }
}

#[test]
fn generic_pointer_scaling_and_number_forms() {
    let mut s = SourceConfig::new("g", "https://example.com/x", "/a/1/b");
    let body = br#"{"a":[0,{"b":"0.5"}]}"#;
    assert_eq!(parse_response(&s, 8, 200, body), Ok(50_000_000));
    // a JSON number, with an exponent
    s.pointer = "/p".into();
    assert_eq!(
        parse_response(&s, 2, 200, br#"{"p": 65000.129}"#),
        Ok(6_500_013)
    );
    assert_eq!(parse_response(&s, 0, 200, br#"{"p": 6.5e4}"#), Ok(65_000));
    // scaling: an endpoint reporting cents, one reporting thousands of units
    s.div = 100;
    assert_eq!(
        parse_response(&s, 2, 200, br#"{"p":"650000"}"#),
        Ok(650_000)
    );
    s.div = 1;
    s.mul = 1_000;
    assert_eq!(parse_response(&s, 0, 200, br#"{"p":"65"}"#), Ok(65_000));
    // more decimals than the pair keeps are rounded half up, fewer are padded
    s.mul = 1;
    assert_eq!(parse_response(&s, 2, 200, br#"{"p":"1.005"}"#), Ok(101));
    assert_eq!(parse_response(&s, 2, 200, br#"{"p":"1.004"}"#), Ok(100));
    assert_eq!(parse_response(&s, 8, 200, br#"{"p":"7"}"#), Ok(700_000_000));
}

#[test]
fn malformed_answers_are_rejected() {
    let s = SourceConfig::new("g", "https://example.com/x", "/p");
    let bad: [&[u8]; 14] = [
        b"",
        b"not json",
        b"{\"p\":",
        b"{}",
        b"{\"p\":null}",
        b"{\"p\":true}",
        b"{\"p\":[1]}",
        b"{\"p\":\"abc\"}",
        b"{\"p\":\"-5\"}",
        b"{\"p\":\"+5\"}",
        b"{\"p\":\"0\"}",
        b"{\"p\":\"0.000000001\"}", // rounds to zero at 8 decimals
        b"{\"p\":\"1e100\"}",
        b"{\"p\":\"1,5\"}",
    ];
    for b in bad {
        assert_eq!(
            parse_response(&s, 8, 200, b),
            Err(SourceError::Malformed),
            "{}",
            String::from_utf8_lossy(b)
        );
    }
    // overflow after scaling
    let mut big = s.clone();
    big.mul = u128::MAX;
    assert_eq!(
        parse_response(&big, 8, 200, br#"{"p":"2"}"#),
        Err(SourceError::Malformed)
    );
    // a deeply nested document is refused by the parser, not by a stack overflow
    let deep = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
    assert_eq!(
        parse_response(&s, 8, 200, deep.as_bytes()),
        Err(SourceError::Malformed)
    );
    // non-2xx
    assert_eq!(
        parse_response(&s, 8, 429, br#"{"p":"1"}"#),
        Err(SourceError::Status(429))
    );
}

#[test]
fn source_and_pair_configs_are_validated() {
    let ok = SourceConfig::new("a", "https://example.com/x", "/p");
    assert!(ok.validate().is_ok());
    for bad in [
        SourceConfig {
            name: String::new(),
            ..ok.clone()
        },
        SourceConfig {
            url: "ftp://x/y".into(),
            ..ok.clone()
        },
        SourceConfig {
            url: "https://user@x/y".into(),
            ..ok.clone()
        },
        SourceConfig {
            pointer: "p".into(),
            ..ok.clone()
        },
        SourceConfig {
            mul: 0,
            ..ok.clone()
        },
        SourceConfig {
            div: 0,
            ..ok.clone()
        },
        SourceConfig {
            max_bytes: 0,
            ..ok.clone()
        },
        SourceConfig {
            max_bytes: 10_000_000,
            ..ok.clone()
        },
        SourceConfig {
            timeout_ms: 0,
            ..ok.clone()
        },
        SourceConfig {
            timeout_ms: 600_000,
            ..ok.clone()
        },
    ] {
        assert!(bad.validate().is_err(), "{bad:?}");
    }
    let pair = PairSources {
        pair: "BTC/USD".into(),
        sources: vec![ok.clone()],
        min_sources: 1,
        max_age_ms: 1000,
        outlier_bps: 100,
    };
    assert!(pair.validate().is_ok());
    assert!(PairSources {
        min_sources: 2,
        ..pair.clone()
    }
    .validate()
    .is_err());
    assert!(PairSources {
        min_sources: 0,
        ..pair.clone()
    }
    .validate()
    .is_err());
    assert!(PairSources {
        sources: vec![],
        ..pair.clone()
    }
    .validate()
    .is_err());
    assert!(PairSources {
        sources: vec![ok.clone(), ok.clone()],
        ..pair.clone()
    }
    .validate()
    .is_err());
    assert!(PairSources {
        outlier_bps: 10_001,
        ..pair.clone()
    }
    .validate()
    .is_err());
}

#[test]
fn node_config_json_round() {
    let v: serde_json::Value = serde_json::from_str(
        r#"[{"pair":"BTC/USD","min_sources":2,"max_age_ms":10000,"outlier_bps":150,
             "sources":[{"preset":"binance-example","symbol":"BTCUSDT"},
                        {"name":"mine","url":"https://x.example/p","pointer":"/last","div":100}]}]"#,
    )
    .unwrap();
    let l = from_json_list(&v).unwrap();
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].sources[0].name, "binance-example:BTCUSDT");
    assert_eq!(l[0].sources[1].div, 100);
    assert_eq!(
        (l[0].min_sources, l[0].max_age_ms, l[0].outlier_bps),
        (2, 10_000, 150)
    );
    for bad in [
        r#"[{"sources":[]}]"#,
        r#"[{"pair":"x","sources":[{"preset":"binance-example"}]}]"#,
        r#"[{"pair":"x","sources":[{"name":"a","url":"http://x/","pointer":"p"}]}]"#,
        r#"{"pair":"x"}"#,
    ] {
        let v: serde_json::Value = serde_json::from_str(bad).unwrap();
        assert!(from_json_list(&v).is_err(), "{bad}");
    }
}

// ------------------------------------------------------------------ fetching

#[test]
fn fetches_from_a_local_mock_and_enforces_caps() {
    let policy = FetchPolicy::test_mode();
    let ex = MockExchange::start(|path| match path {
        "/ok" => MockResp::json(r#"{"price":"64000.5"}"#),
        "/huge" => MockResp::json(&format!(
            r#"{{"price":"1","pad":"{}"}}"#,
            "x".repeat(200_000)
        )),
        "/slow" => MockResp {
            delay_ms: 1_500,
            ..MockResp::json(r#"{"price":"1"}"#)
        },
        "/err" => MockResp {
            status: 503,
            ..MockResp::json(r#"{"price":"1"}"#)
        },
        "/junk" => MockResp::json("<html>maintenance</html>"),
        _ => MockResp {
            status: 404,
            ..MockResp::json("{}")
        },
    });
    let mut s = src(&ex, "/ok", "/price");
    assert_eq!(fetch_source(&s, BTC8, &policy), Ok(6_400_050_000_000));
    s.url = ex.url("/huge");
    assert_eq!(
        fetch_source(&s, BTC8, &policy),
        Err(SourceError::Fetch(FetchError::TooLarge))
    );
    // a bigger cap lets the same answer through
    s.max_bytes = 512 * 1024;
    assert_eq!(fetch_source(&s, BTC8, &policy), Ok(100_000_000));
    s.max_bytes = DEFAULT_MAX_BYTES;
    s.url = ex.url("/slow");
    s.timeout_ms = 300;
    let t = Instant::now();
    assert_eq!(
        fetch_source(&s, BTC8, &policy),
        Err(SourceError::Fetch(FetchError::Timeout))
    );
    assert!(
        t.elapsed() < Duration::from_millis(1_400),
        "{:?}",
        t.elapsed()
    );
    s.timeout_ms = DEFAULT_TIMEOUT_MS;
    s.url = ex.url("/err");
    assert_eq!(
        fetch_source(&s, BTC8, &policy),
        Err(SourceError::Status(503))
    );
    s.url = ex.url("/junk");
    assert_eq!(fetch_source(&s, BTC8, &policy), Err(SourceError::Malformed));
    s.url = ex.url("/missing");
    assert_eq!(
        fetch_source(&s, BTC8, &policy),
        Err(SourceError::Status(404))
    );
}

#[test]
fn ssrf_rules_apply_to_price_sources() {
    let ex = MockExchange::start(|_| MockResp::json(r#"{"price":"5"}"#));
    // the default (production) policy: plain http and loopback are both refused, and nothing is
    // even sent
    let strict = FetchPolicy::default();
    let mut s = src(&ex, "/", "/price");
    assert_eq!(
        fetch_source(&s, BTC8, &strict),
        Err(SourceError::Fetch(FetchError::Blocked))
    );
    // https to a loopback address: refused before a connection is made
    s.url = format!("https://127.0.0.1:{}/", ex.port);
    assert_eq!(
        fetch_source(&s, BTC8, &strict),
        Err(SourceError::Fetch(FetchError::Blocked))
    );
    // http allowed but private addresses are not
    let no_private = FetchPolicy {
        allow_http: true,
        ..FetchPolicy::default()
    };
    s.url = ex.url("/");
    assert_eq!(
        fetch_source(&s, BTC8, &no_private),
        Err(SourceError::Fetch(FetchError::Blocked))
    );
    for u in [
        "http://169.254.169.254/latest/meta-data",
        "http://10.0.0.1/x",
        "http://192.168.1.1/x",
    ] {
        s.url = u.into();
        assert_eq!(
            fetch_source(&s, BTC8, &no_private),
            Err(SourceError::Fetch(FetchError::Blocked)),
            "{u}"
        );
    }
    assert_eq!(ex.hits(), 0, "the exchange was never contacted");
    // a redirect to another host is not followed (the allowlist is the source's own host)
    let target = MockExchange::start(|_| MockResp::json(r#"{"price":"9"}"#));
    let tp = target.port;
    let redirecting = MockExchange::start(move |_| MockResp {
        status: 302,
        headers: vec![("Location".into(), format!("http://localhost:{tp}/"))],
        body: vec![],
        delay_ms: 0,
    });
    let s = src(&redirecting, "/", "/price");
    assert_eq!(
        fetch_source(&s, BTC8, &FetchPolicy::test_mode()),
        Err(SourceError::Fetch(FetchError::Blocked))
    );
    assert_eq!(target.hits(), 0);
}

// ------------------------------------------------------------------ combining

#[test]
fn aggregate_is_a_median_with_staleness_and_outlier_rules() {
    // median of three, order does not matter
    assert_eq!(
        aggregate(&[(100, 0), (300, 0), (200, 0)], 2, 1000, 5_000),
        Ok(200)
    );
    // even count: midpoint, floor
    assert_eq!(aggregate(&[(100, 0), (103, 0)], 1, 1000, 5_000), Ok(101));
    // stale values are dropped
    assert_eq!(
        aggregate(&[(100, 0), (300, 5_000), (110, 10)], 2, 1000, 5_000),
        Ok(105)
    );
    assert_eq!(
        aggregate(&[(100, 0), (110, 5_000), (120, 6_000)], 2, 1000, 5_000),
        Err(AggError::NotEnoughFresh)
    );
    assert_eq!(aggregate(&[], 1, 1000, 100), Err(AggError::NotEnoughFresh));
    // an outlier is rejected: 1% band, 3 sources, one lies by 50%
    assert_eq!(
        aggregate(&[(10_000, 0), (10_020, 0), (15_000, 0)], 2, 1000, 100),
        Ok(10_010)
    );
    // exactly on the band edge stays; one unit beyond goes
    assert_eq!(
        aggregate(&[(10_000, 0), (10_000, 0), (10_100, 0)], 3, 1000, 100),
        Ok(10_000)
    );
    assert_eq!(
        aggregate(&[(10_000, 0), (10_000, 0), (10_101, 0)], 3, 1000, 100),
        Err(AggError::NotEnoughAgreeing)
    );
    // two sources that disagree beyond the band: both are out, nothing is reported
    assert_eq!(
        aggregate(&[(100, 0), (200, 0)], 1, 1000, 100),
        Err(AggError::NotEnoughAgreeing)
    );
    // min_sources counts survivors
    assert_eq!(
        aggregate(&[(10_000, 0), (10_010, 0), (20_000, 0)], 3, 1000, 100),
        Err(AggError::NotEnoughAgreeing)
    );
    // huge values do not overflow
    assert!(aggregate(
        &[(u128::MAX, 0), (u128::MAX - 1, 0), (u128::MAX, 0)],
        2,
        1,
        100
    )
    .is_ok());
}

fn pair(sources: usize, min: usize) -> ResolvedPair {
    ResolvedPair {
        id: 0,
        decimals: 8,
        cfg: PairSources {
            pair: "BTC/USD".into(),
            sources: (0..sources)
                .map(|i| SourceConfig::new(&format!("s{i}"), "https://example.com/x", "/p"))
                .collect(),
            min_sources: min,
            max_age_ms: 5_000,
            outlier_bps: 100,
        },
    }
}

fn wait(f: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < end, "timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn fetcher_keeps_last_good_values_and_lets_them_go_stale() {
    let prices = Arc::new([
        AtomicU64::new(1_000_000),
        AtomicU64::new(1_000_100),
        AtomicU64::new(1_000_200),
    ]);
    let down = Arc::new(AtomicBool::new(false));
    let (pr, dn) = (prices.clone(), down.clone());
    let fetcher: Arc<FetchFn> = Arc::new(move |s: &SourceConfig, _| {
        if dn.load(Ordering::Relaxed) {
            return Err(SourceError::Fetch(FetchError::Failed));
        }
        let i: usize = s.name[1..].parse().unwrap();
        Ok(u128::from(pr[i].load(Ordering::Relaxed)))
    });
    let f = PriceFetcher::spawn_with(vec![pair(3, 2)], Duration::from_millis(20), fetcher);
    wait(|| f.value(0) == Some(1_000_100));
    assert_eq!(f.value(1), None, "unconfigured pair");
    // sources go down: the last good values still count while fresh
    down.store(true, Ordering::Relaxed);
    wait(|| f.stats().failed >= 3);
    assert_eq!(f.value(0), Some(1_000_100));
    // and not once they are older than max_age_ms
    assert_eq!(
        f.value_at(0, Instant::now() + Duration::from_millis(5_500)),
        None
    );
    // recovery
    down.store(false, Ordering::Relaxed);
    prices[0].store(1_000_300, Ordering::Relaxed);
    wait(|| f.value(0) == Some(1_000_200));
}

#[test]
fn a_lying_source_is_an_outlier_and_a_liar_majority_is_not_hidden() {
    let lie = Arc::new(AtomicU64::new(1_000_000));
    let l = lie.clone();
    let fetcher: Arc<FetchFn> = Arc::new(move |s: &SourceConfig, _| {
        Ok(match s.name.as_str() {
            "s0" => u128::from(l.load(Ordering::Relaxed)),
            "s1" => 1_000_050,
            _ => 1_000_100,
        })
    });
    let f = PriceFetcher::spawn_with(vec![pair(3, 2)], Duration::from_millis(20), fetcher);
    wait(|| f.value(0).is_some());
    lie.store(9_000_000, Ordering::Relaxed);
    wait(|| f.stats().ok >= 12);
    assert_eq!(
        f.value(0),
        Some(1_000_075),
        "the liar is rejected, the rest averaged"
    );
}

#[test]
fn a_slow_source_never_blocks_the_reader() {
    let fetcher: Arc<FetchFn> = Arc::new(|_, _| {
        std::thread::sleep(Duration::from_secs(3));
        Ok(1)
    });
    let t = Instant::now();
    let f = PriceFetcher::spawn_with(vec![pair(2, 1)], Duration::from_millis(10), fetcher);
    for _ in 0..1000 {
        assert_eq!(f.value(0), None);
    }
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "{:?}",
        t.elapsed()
    );
}

#[test]
fn the_fetch_thread_stops_when_the_source_is_dropped() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = calls.clone();
    let fetcher: Arc<FetchFn> = Arc::new(move |_, _| {
        c.fetch_add(1, Ordering::Relaxed);
        Ok(5)
    });
    let f = PriceFetcher::spawn_with(vec![pair(1, 1)], Duration::from_millis(10), fetcher);
    wait(|| calls.load(Ordering::Relaxed) > 3);
    let src = f.into_source();
    assert_eq!(src.get(0), Some(5));
    drop(src);
    std::thread::sleep(Duration::from_millis(100));
    let n = calls.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(calls.load(Ordering::Relaxed), n);
}

#[test]
fn end_to_end_fetcher_against_mock_exchanges() {
    let mk = |price: &'static str, shape: u8| {
        MockExchange::start(move |_| {
            MockResp::json(&match shape {
                0 => format!(r#"{{"price":"{price}"}}"#),
                1 => format!(r#"{{"data":{{"amount":"{price}"}}}}"#),
                _ => format!(r#"{{"data":[{{"last":"{price}"}}]}}"#),
            })
        })
    };
    let (a, b, c) = (mk("64000.10", 0), mk("64000.30", 1), mk("99999", 2));
    let p = ResolvedPair {
        id: 0,
        decimals: 2,
        cfg: PairSources {
            pair: "BTC/USD".into(),
            sources: vec![
                src(&a, "/", "/price"),
                SourceConfig {
                    name: "b".into(),
                    ..src(&b, "/", "/data/amount")
                },
                SourceConfig {
                    name: "c".into(),
                    ..src(&c, "/", "/data/0/last")
                },
            ],
            min_sources: 2,
            max_age_ms: 10_000,
            outlier_bps: 100,
        },
    };
    let f = PriceFetcher::spawn(vec![p], Duration::from_millis(50), FetchPolicy::test_mode());
    wait(|| f.value(0).is_some());
    assert_eq!(f.value(0), Some(6_400_020));
}
