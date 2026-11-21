//! All-Rust frontend for `examples/market-data`: Dioxus compiled to WASM, talking to `btb dev`
//! (or a hosting node) over its HTTP API, with `btb-codec` for arguments and results. No
//! JavaScript is written by hand; `build.sh` inlines the WASM into `../market-data/web/index.html`
//! so `btb dev` serves it from the same origin as the contract.

use btb_codec::{Decode, Encode};
use dioxus::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

mod types;
use types::*;

fn main() {
    dioxus::launch(App);
}

// ---------------------------------------------------------------------------------------------
// Transport: GET /query/<fn>?args=<hex>, POST /mutation/<fn> with a hex body.
// ---------------------------------------------------------------------------------------------

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return Err("odd hex".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

async fn http(
    method: &str,
    path: &str,
    body: Option<String>,
    caller: &str,
) -> Result<String, String> {
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    if let Some(b) = body {
        init.set_body(&b.into());
    }
    let req = web_sys::Request::new_with_str_and_init(path, &init).map_err(|e| format!("{e:?}"))?;
    req.headers()
        .set("X-BTB-Caller", caller)
        .map_err(|e| format!("{e:?}"))?;
    let win = web_sys::window().ok_or("no window")?;
    let resp: web_sys::Response = JsFuture::from(win.fetch_with_request(&req))
        .await
        .map_err(|e| format!("network: {e:?}"))?
        .dyn_into()
        .map_err(|_| "not a response")?;
    let text = JsFuture::from(resp.text().map_err(|e| format!("{e:?}"))?)
        .await
        .map_err(|e| format!("{e:?}"))?
        .as_string()
        .unwrap_or_default();
    if resp.ok() {
        Ok(text)
    } else {
        Err(text)
    }
}

async fn query<T: Decode>(name: &str, args: &impl Encode) -> Result<T, String> {
    let out = http(
        "GET",
        &format!("/query/{name}?args={}", hex(&args.encode())),
        None,
        "viewer",
    )
    .await?;
    T::decode(&unhex(&out)?).map_err(|e| format!("decode {name}: {e:?}"))
}

async fn mutation(name: &str, args: &impl Encode) -> Result<(), String> {
    http(
        "POST",
        &format!("/mutation/{name}"),
        Some(hex(&args.encode())),
        "provider",
    )
    .await
    .map(|_| ())
}

// ---------------------------------------------------------------------------------------------
// Formatting (fixed-point integers to text; still no floats)
// ---------------------------------------------------------------------------------------------

/// `3125500` (1e-4) -> `312.55`.
fn px(v: i64) -> String {
    let sign = if v < 0 { "-" } else { "" };
    let a = v.unsigned_abs();
    let s = format!("{sign}{}.{:04}", a / 10_000, a % 10_000);
    let s = s.trim_end_matches('0');
    if s.ends_with('.') {
        format!("{s}00")
    } else if s.split('.').nth(1).map(|f| f.len()) == Some(1) {
        format!("{s}0")
    } else {
        s.to_string()
    }
}

/// Basis points -> `-0.08 %`.
fn pct(bps: i64) -> String {
    let sign = if bps < 0 { "-" } else { "" };
    let a = bps.unsigned_abs();
    format!("{sign}{}.{:02} %", a / 100, a % 100)
}

/// `1344255171420` -> `1,344,255,171,420`.
fn group(n: i128) -> String {
    let s = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

fn opt<T>(v: Option<T>, f: impl Fn(T) -> String) -> String {
    v.map(f).unwrap_or_else(|| "—".into())
}

fn date(d: u32) -> String {
    format!("{}-{:02}-{:02}", d / 10_000, d / 100 % 100, d % 100)
}

// ---------------------------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------------------------

const CSS: &str = r#"
:root { --bg:#f7f7f5; --fg:#1c1c1a; --muted:#6b6b66; --card:#fff; --line:#e4e4df; --up:#137a3a; --down:#b3261e; --accent:#2b5cd9; }
@media (prefers-color-scheme: dark) { :root { --bg:#151514; --fg:#ececea; --muted:#9a9a94; --card:#1e1e1c; --line:#2e2e2b; --up:#4cc27a; --down:#f0776d; --accent:#7ea1ff; } }
* { box-sizing: border-box; }
body { margin:0; background:var(--bg); color:var(--fg); font:15px/1.45 system-ui, sans-serif; }
main { max-width:960px; margin:0 auto; padding:24px 16px 48px; }
h1 { font-size:20px; margin:0 0 4px; } .sub { color:var(--muted); font-size:13px; margin-bottom:16px; }
form { display:flex; gap:8px; flex-wrap:wrap; margin-bottom:16px; }
input { flex:1; min-width:180px; padding:8px 10px; border:1px solid var(--line); border-radius:8px; background:var(--card); color:var(--fg); font:inherit; }
button { padding:8px 14px; border:1px solid var(--line); border-radius:8px; background:var(--card); color:var(--fg); font:inherit; cursor:pointer; }
button.primary { background:var(--accent); color:#fff; border-color:var(--accent); }
.grid { display:grid; grid-template-columns:repeat(auto-fit, minmax(260px, 1fr)); gap:12px; }
.card { background:var(--card); border:1px solid var(--line); border-radius:12px; padding:14px 16px; }
.card h2 { font-size:13px; text-transform:uppercase; letter-spacing:.04em; color:var(--muted); margin:0 0 8px; }
.price { font-size:32px; font-weight:600; font-variant-numeric:tabular-nums; }
.up { color:var(--up); } .down { color:var(--down); }
dl { display:grid; grid-template-columns:auto auto; gap:4px 12px; margin:0; }
dt { color:var(--muted); } dd { margin:0; text-align:right; font-variant-numeric:tabular-nums; }
table { width:100%; border-collapse:collapse; font-variant-numeric:tabular-nums; font-size:14px; }
th, td { padding:4px 6px; text-align:right; border-bottom:1px solid var(--line); } th:first-child, td:first-child { text-align:left; }
.tag { display:inline-block; padding:1px 6px; border-radius:6px; border:1px solid var(--line); font-size:12px; margin-left:6px; }
.err { color:var(--down); margin:8px 0; } .wide { grid-column:1 / -1; overflow-x:auto; }
"#;

#[component]
fn App() -> Element {
    let mut symbol = use_signal(|| "PSX:OGDC".to_string());
    let mut payload = use_signal(|| None::<StockPayload>);
    let mut error = use_signal(String::new);
    let mut busy = use_signal(|| false);

    let load = move || {
        spawn(async move {
            busy.set(true);
            error.set(String::new());
            match query::<Option<StockPayload>>("stock", &(symbol(), 0u32, 30u32)).await {
                Ok(Some(p)) => payload.set(Some(p)),
                Ok(None) => {
                    payload.set(None);
                    error.set(format!(
                        "{} is not listed. Load the demo data first.",
                        symbol()
                    ));
                }
                Err(e) => error.set(e),
            }
            busy.set(false);
        });
    };

    rsx! {
        style { {CSS} }
        main {
            h1 { "BTB market data" }
            div { class: "sub",
                "One query, "
                code { "stock(symbol)" }
                ", returns everything. Ratios, analytics and the sector view are computed on chain from the provider's facts, in integer math. This page is Rust compiled to WASM."
            }
            form {
                onsubmit: move |e| {
                    e.prevent_default();
                    load();
                },
                input {
                    value: "{symbol}",
                    placeholder: "EXCHANGE:TICKER",
                    oninput: move |e| symbol.set(e.value().trim().to_uppercase()),
                }
                button { class: "primary", r#type: "submit", disabled: busy(), "Load" }
                button {
                    r#type: "button",
                    disabled: busy(),
                    onclick: move |_| {
                        spawn(async move {
                            busy.set(true);
                            match seed().await {
                                Ok(()) => {
                                    busy.set(false);
                                    load();
                                }
                                Err(e) => {
                                    error.set(e);
                                    busy.set(false);
                                }
                            }
                        });
                    },
                    "Load demo data (dev)"
                }
            }
            if !error().is_empty() {
                div { class: "err", "{error}" }
            }
            if let Some(p) = payload() {
                Stock { p }
            }
        }
    }
}

#[component]
fn Stock(p: StockPayload) -> Element {
    let c = p.company.clone();
    let q = p.quote.clone();
    let a = p.analytics.clone().unwrap_or_default();
    let r = p.ratios.clone().unwrap_or_default();
    let ccy = p
        .exchange
        .as_ref()
        .map(|e| e.currency.clone())
        .unwrap_or_default();
    let dir = if a.change < 0 { "down" } else { "up" };
    rsx! {
        div { class: "grid",
            div { class: "card",
                h2 { "{p.symbol}" }
                if let Some(c) = &c {
                    div { "{c.name}" }
                    div { class: "sub", "{c.sector_name}" }
                }
                if let Some(q) = &q {
                    div { class: "price",
                        "{px(q.price)}"
                        span { class: "tag", "{ccy}" }
                        if !q.marker.is_empty() {
                            span { class: "tag", "{q.marker}" }
                        }
                    }
                    div { class: "{dir}", "{px(a.change)} ({opt(a.change_bps, pct)})" }
                    div { class: "sub",
                        "Session {date(q.session)} · "
                        if q.delayed { "delayed" } else { "live" }
                        " · source {q.source} · block {q.height}"
                    }
                }
            }
            if let Some(q) = &q {
                div { class: "card",
                    h2 { "Quote" }
                    dl {
                        dt { "Open" } dd { "{px(q.open)}" }
                        dt { "High / low" } dd { "{px(q.high)} / {px(q.low)}" }
                        dt { "Previous close" } dd { "{px(q.previous_close)}" }
                        dt { "Volume" } dd { "{group(q.volume as i128)}" }
                        dt { "52-week range" } dd { "{px(q.low_52w)} – {px(q.high_52w)}" }
                    }
                }
            }
            div { class: "card",
                h2 { "Valuation" }
                dl {
                    dt { "P/E" } dd { {opt(r.pe, px)} }
                    dt { "P/B" } dd { {opt(r.pb, px)} }
                    dt { "Dividend yield" } dd { {opt(r.dividend_yield_bps, pct)} }
                    dt { "Payout" } dd { {opt(r.payout_bps, pct)} }
                    dt { "Net margin" } dd { {opt(r.net_margin_bps, pct)} }
                }
            }
            div { class: "card",
                h2 { "Analytics" }
                dl {
                    dt { "Market cap" } dd { {opt(a.market_cap, |m| format!("{} {ccy}", group(m)))} }
                    dt { "Free float" } dd { {opt(a.free_float_bps, pct)} }
                    dt { "20-day high" } dd { {opt(a.high_20d, px)} }
                    dt { "Drawdown" } dd { {opt(a.drawdown_20d_bps, pct)} }
                    dt { "From 52-week high" } dd { {opt(a.from_52w_high_bps, pct)} }
                    dt { "SMA 20 / 50" } dd { "{opt(a.sma_20, px)} / {opt(a.sma_50, px)}" }
                    dt { "Avg volume" } dd { {opt(a.avg_volume_20d, |v| group(v as i128))} }
                }
            }
            if let Some(s) = &p.sector {
                div { class: "card",
                    h2 { "Sector" }
                    dl {
                        dt { "Companies" } dd { "{s.companies}" }
                        dt { "Median P/E" } dd { {opt(s.median_pe, px)} }
                        dt { "P/E vs sector" } dd { {opt(s.pe_premium_bps, pct)} }
                    }
                }
            }
            if !p.prices.is_empty() {
                div { class: "card wide",
                    h2 { "Daily prices ({p.prices.len()} sessions, newest first)" }
                    table {
                        tr { th { "Date" } th { "Open" } th { "High" } th { "Low" } th { "Close" } th { "Volume" } }
                        for b in p.prices.iter() {
                            tr { key: "{b.key}",
                                td { "{date(b.date)}" }
                                td { "{px(b.open)}" }
                                td { "{px(b.high)}" }
                                td { "{px(b.low)}" }
                                td { "{px(b.close)}" }
                                td { "{group(b.volume as i128)}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Demo data: real PSX figures for OGDC (2026-11-20) and three sector peers (dev only; the
// caller header makes this browser the provider of a fresh `btb dev`).
// ---------------------------------------------------------------------------------------------

fn p(s: &str) -> i64 {
    let (i, f) = s.split_once('.').unwrap_or((s, ""));
    let f = format!("{f:0<4}");
    i.parse::<i64>().unwrap_or(0) * 10_000 + f[..4].parse::<i64>().unwrap_or(0)
}

async fn seed() -> Result<(), String> {
    // `init` fails once a provider exists; that is fine for a second click.
    let _ = mutation("init", &()).await;
    let ex: Vec<ExchangeIn> = vec![(
        "PSX".into(),
        "XKAR".into(),
        "PK".into(),
        "PKR".into(),
        "Asia/Karachi".into(),
    )];
    mutation("publish_exchanges", &(ex,)).await?;
    let sector = "oil-gas-exploration-companies";
    let co = |sym: &str, name: &str, shares: u64, ff: u64| -> CompanyIn {
        (
            sym.into(),
            sector.into(),
            name.into(),
            "OIL & GAS EXPLORATION COMPANIES".into(),
            (shares, ff),
            vec!["KSE100".into()],
            ("listed".into(), "June".into(), String::new()),
            (vec![0; 32], "psx".into(), 1_795_000_000),
        )
    };
    let cos = vec![
        co(
            "PSX:OGDC",
            "Oil & Gas Development Company Limited",
            4_300_928_400,
            645_139_260,
        ),
        co("PSX:PPL", "Pakistan Petroleum Limited", 2_720_000_000, 0),
        co("PSX:MARI", "Mari Energies Limited", 1_200_000_000, 0),
        co("PSX:POL", "Pakistan Oilfields Limited", 283_000_000, 0),
    ];
    mutation("publish_companies", &(cos,)).await?;
    let q = |sym: &str, last: &str, prev: &str| -> QuoteIn {
        let v = p(last);
        (
            sym.into(),
            (v, p(prev), v, v, v),
            (1_000, 1_000),
            (v, v),
            (String::new(), "trading".into()),
            20_261_120,
            true,
            ("psx".into(), 1_795_150_000),
        )
    };
    let ogdc: QuoteIn = (
        "PSX:OGDC".into(),
        (
            p("312.55"),
            p("312.79"),
            p("313.6"),
            p("315.48"),
            p("311.91"),
        ),
        (1_236_159, 218_451_790),
        (p("352"), p("246")),
        ("XD".into(), "trading".into()),
        20_261_120,
        true,
        ("psx".into(), 1_795_150_000),
    );
    let quotes = vec![
        ogdc,
        q("PSX:PPL", "186.9", "186"),
        q("PSX:MARI", "640", "650"),
        q("PSX:POL", "630", "625"),
    ];
    mutation("publish_quotes", &(quotes,)).await?;
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
    .map(|&(d, o, h, l, c, v)| ("PSX:OGDC".into(), d, p(o), p(h), p(l), p(c), v, None))
    .collect();
    mutation("publish_bars", &(bars,)).await?;
    let f = |s: &str, eps: &str, dps: &str| -> FundamentalsIn {
        (
            s.into(),
            "TTM".into(),
            (449_190_752_000, 242_373_646_000),
            (p(eps), p(dps)),
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
    mutation("publish_fundamentals", &(funds,)).await?;
    Ok(())
}
