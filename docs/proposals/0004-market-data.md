# 0004: Market data on BTB: data providers, stock tables and one-call payloads

Status: **Draft; phase A built** (`examples/market-data`, tests `crates/btb-tests/tests/market_data.rs`). Issue: to be opened by a maintainer as a Design Proposal. Phase B touches tokenomics (fees paid to non-validators) and needs the Safety section reviewed before code.

## Summary

We want the same thing a market data API gives today (worked example: the PSX Lens payload `GET /v1/stocks/OGDC?compact=1`, about 31 KB of JSON in 27 blocks: company, quote, prices, ratios, financials, dividends, technicals, sector analytics and more), served from BTB: **one query returns the full stock payload**, for any exchange, in one schema, with a Merkle proof. Third parties (**data providers**) publish the raw facts and **earn when developers use them**. Everything that can be computed from those facts is computed **on chain by crons**, so anyone can check it.

**Recommendation:** A (a market data contract with today's SDK, one trusted provider) now; B (a provider registry with per-read fees paid to providers) next; C (several providers per dataset with a median, slashing) after the public testnet.

## Why the existing oracles are not enough

| Built today ([ORACLES.md](../ORACLES.md)) | Limit for stock data |
| --- | --- |
| Price feeds (`price(pair)`) | At most 64 pairs, set at genesis; one integer per pair; only validators observe and only validators earn |
| HTTPS actions | Result at most 1 KiB; every validator sees the source's API key; no headless browser (frontier exchanges such as PSX need scraping) |
| "App-defined feeds" (ORACLES.md, Economics) | Named in the fee table, not built |

There are about 55,000 listed companies. What is missing is not a fetch mechanism but a **publisher**: someone off chain who collects, cleans and signs the data, and gets paid for it.

## The payload, block by block

Each block of the payload goes into one of three groups.

**P: published by the provider** (raw facts, signed, in a `publish` mutation)

| Block | On BTB |
| --- | --- |
| `company`, `company.profile`, `sector` | Table `companies` (key `exchange:symbol`). Long text (`description`, `address`, people) goes in a content-addressed blob (storage nodes); the row holds its hash |
| `quote` (OHLC, LDCP, volume, `marker` such as `XD`, trading status, `as_of`) | Table `quotes`, one row per symbol, replaced every update |
| `prices` (daily OHLCV) | Table `bars` (key `symbol, trade_date`), the last ~400 sessions on chain; older years as erasure-coded files with the root on chain |
| `financials`, `dividends.history`, `corporate_actions`, `announcements` | Tables `statements`, `dividends`, `corp_actions`; announcement bodies as blobs |
| `freshness`, `attribution`, `source`, `retrieved_at` | Columns on each row: provider, source, source time, the block height it was published at |

**C: computed on chain** (crons over P, deterministic integer math)

`ratios`/`valuation` (P/E, P/S, dividend yield, payout, normalized EPS), `analytics` (drawdowns, 5d/20d highs, market cap), `liquidity` and `liquidity_quality`, `technicals` (moving averages, RSI, volatility), `sector_valuation` and `sector_relative` (sector medians, premiums), `peers` and `peer_relative`, `dividend_sustainability`, `historical_context` (percentiles), `corporate_action_integrity`, `history_gap`, `event_flags`, `derived` (except the next group).

Since anyone can recompute them from the published facts, a provider can only lie about **facts** and never about **math**. That is what makes the data verifiable.

**O: opinion, kept off chain or clearly labelled**

`derived.verdict`, `derived.summary`, `derived.answers`: text judgments. A provider may publish them as its own signed rows, labelled as that provider's view, never as chain-computed data.

### Units (fixes the duplicate and unit issues of the source schema)

- No floats ([AGENTS.md](../../AGENTS.md)). Prices are `i64` in **1e-4 of the local currency** (`312.55` → `3125500`); amounts in the currency's minor units; percentages in **basis points** `i32` (`-0.08%` → `-8`); ratios in 1e-4. The currency (ISO 4217) and the decimals are in the `companies` row.
- Times are block heights plus a UTC `u64` seconds field from the provider. The exchange timezone is a `companies` column. `timestamp_utc`/`timestamp_pkt`/`as_of` collapse into one field.
- One name per fact: `ratios` and `valuation` are the same data (one table); `inside_kse100`/`kse100` become `index_memberships`; `sector_name` is `sector.name`; `quote.metadata`/`quote.market`/`quote.freshness` collapse into the row's columns.
- Symbols are `EXCHANGE:SYMBOL` (`PSX:OGDC`, `NASDAQ:AAPL`); the exchange row holds MIC, country, timezone, currency, trading calendar.

## The API

```rust
#[btb::query]
pub fn stock(symbol: String, include: Vec<Block>) -> StockPayload; // empty include = everything
#[btb::query]
pub fn stocks(symbols: Vec<String>, include: Vec<Block>) -> Vec<StockPayload>; // batch, capped
#[btb::query]
pub fn screen(filter: Screen, page: u32) -> Vec<Row>; // by index
#[btb::query]
pub fn sector(exchange: String, sector: String) -> SectorBoard;

#[btb::mutation]
pub fn publish(dataset: u32, batch: Batch); // provider only
#[btb::cron("after publish")]
pub fn recompute(symbols: Vec<String>); // C blocks
```

`?include=quote,ratios` becomes `include`. Queries are subscribable, so an app gets live updates over SSE ([DEV_SERVER.md](../DEV_SERVER.md)), and through hosting nodes every answer can carry a Merkle proof ([HOSTING.md](../HOSTING.md)). The TypeScript client gets the types from the encoding ([TS_SDK.md](../TS_SDK.md)). A JSON gateway (`btb-host` route) can serve the exact JSON shape for existing clients.

## How providers earn

| Reader | Pays | Provider gets |
| --- | --- | --- |
| A contract, in a mutation or cron (a lending app reading `PSX:OGDC`) | A per-read fee set by the provider, like `price()`'s 10,000 gas | The fee, minus gas, to the provider's account (phase B) |
| An app or AI agent, through queries on a hosting/edge node | Vouchers to the node (`btb-voucher`, [EDGE.md](../EDGE.md)) | A share of the voucher revenue for its dataset (phase B: the node reports reads per dataset with the voucher claims) |
| Anyone reading a full node directly | Nothing | Nothing: data on a public chain is public |

So a provider sells **freshness, coverage, clean schema and verified answers**, not secrecy. Data that must stay paid-only is published encrypted, with keys sold off chain. That is out of scope here.

## Options compared

| | A: one contract, one provider | B: provider registry | C: many providers, median, slashing |
| --- | --- | --- | --- |
| Who can publish | One key set at deploy | Anyone who registers a dataset and stakes | Several per dataset |
| Pay to provider | Off chain | Per read and voucher share, on chain | Same |
| Trust in facts | The provider | The provider, with stake at risk | Median of providers (`btb_oracle::price::weighted_median`), outliers slashed on proof |
| Needs chain changes | No (SDK today) | Yes: a fee recipient other than the validator pool | Yes |

## Recommendation and phases

- **A.** `examples/market-data`: tables, `publish`, recompute crons, `stock()` query, with PSX EOD for ~500 companies from an off-chain Rust ingester. Measure gas per publish and payload size.
- **B.** `DataOp` transaction: register a dataset (schema hash, read price, stake), and a host call `data_read(dataset)` that charges the provider's fee in mutations and is free in queries. Vouchers carry a per-dataset read count; the provider's share is paid on claim.
- **C.** Several providers per dataset, median per field, a fraud proof (two signed batches for the same session that differ, or a published fact contradicting a signed report from an exchange) slashes stake.

## Global coverage (every country)

The schema does not change per country. Adding one is data, not code:

1. **Exchange row** (`exchanges` table): MIC, country, currency and its decimals, timezone, trading calendar and holidays, session times, local markers (PSX `XD`/`XB`, NSE `ex-date`, Tadawul codes), price-limit rules, settlement cycle. Local mechanics are columns in the same shape, never a new schema.
2. **Dataset per exchange** (`psx.eod`, `nse.eod`, `tadawul.eod`, ...): each has its own provider(s), read price and licence statement. One provider may hold many datasets; a country can have competing providers (phase C takes their median).
3. **Same crons**: ratios, technicals and sector medians run per exchange (sector medians never mix currencies). Cross-market views (a global screener) convert at the built-in FX price feeds and say so.

Rollout by region, each a gate: South Asia (PSX, NSE, BSE, DSE, CSE), MENA (Tadawul, ADX, DFM, QSE, EGX), then major markets, then frontier markets (NGX, NSE Kenya, ...) where a scraper-based provider is the only source.

**Scale for ~60,000 companies in ~100 countries:**

| | Estimate | Where it lives |
| --- | --- | --- |
| Live quotes | 60,000 × ~200 B ≈ 12 MB | Validator state |
| Recent bars (50 sessions) | 60,000 × 50 × ~48 B ≈ 150 MB | Validator state |
| Full history, statements, filings | tens of GB | Storage nodes (erasure coded), roots on chain |
| EOD publishes | ~120 transactions of 500 quotes per day | Spread over the day as exchanges close |
| Recompute | ~60,000 symbols per day | Crons under `cron_gas_block_cap`, per exchange after its close |
| Reads | unbounded | Queries on hosting and edge nodes; validators do not serve them |

Validators carry only the live data; the bulk scales with storage and hosting nodes, which earn for it ([SCALING.md](../SCALING.md)). The real limit is **licensing per exchange**, recorded in each dataset's licence statement.

## Phase A as built

`examples/market-data`: tables `Exchange`, `Company`, `Quote`, `Bar` (key `SYMBOL|YYYYMMDD`, so a primary range scan gives a symbol's bars in date order), `Fundamentals`; provider-only `publish_*` mutations; queries `stock`, `quotes`, `symbols`, `sector`. One difference from the text above: the C blocks (ratios, analytics, sector median) are computed **at read time inside the query** rather than by crons. That is free for the reader, always consistent with the facts, and needs no stored copy; crons that store them become worthwhile when a sector has hundreds of companies (the sector median reads every company of the sector). Covered so far: ratios, change, market cap, free float, 20-day high and drawdown, SMA 20/50, average volume, distance from the 52-week high, sector median P/E and premium. Technicals (RSI, volatility), dividend history and percentiles are next.

## Sizes and limits (to measure in A)

- 55,000 quotes × ~200 B ≈ 11 MB of live state; 400 bars × 55,000 × ~48 B ≈ 1 GB, which is why older bars go to storage nodes.
- One `publish` must fit a transaction: batches of ~500 quotes per transaction.
- Recompute is per symbol touched, capped by `cron_gas_block_cap`. A full sector median recompute after EOD spreads over several blocks.

## Safety

- **No inflation:** provider earnings come only from fees readers pay; nothing is minted.
- **Determinism:** C blocks use only integer math, sorted inputs and fixed rounding (half to even) on published rows; no floats, no time.
- **Bad facts:** phase A trusts the provider (documented in each row); B puts stake at risk; C needs a majority of providers to collude.
- **Licensing:** a provider must have the right to publish to a public chain. Vendor data under a redistribution licence usually does not allow it; self-collected exchange data and computed analytics usually do. The provider is responsible, as for app-defined feeds.

Failure-case tests: a non-provider `publish` is refused; a batch with an older `as_of` than the stored row is ignored; recompute gives the same bytes sequentially and in parallel; division by zero (P/E with no earnings) gives `None`, never a trap; a fee read in a query costs nothing; provider earnings plus the validator pool equal what readers paid (supply invariant).

## Affected files

New `examples/market-data`; phase B: `btb-chain` (DataOp, fee routing), `btb-vm` (host call and its gas), `btb-voucher`/`btb-edge` (per-dataset counts); docs `ORACLES.md` (app-defined feeds), `TOKENOMICS.md`, `HOSTING.md`, `GLOSSARY.md` (data provider, dataset).

## Open questions

1. Should the JSON gateway reproduce the source payload exactly, or only the cleaned schema?
2. Per-read fee: set by the provider freely, or bounded by the chain?
3. How much bar history should stay in validator state (400 sessions? 50?) versus storage nodes?
