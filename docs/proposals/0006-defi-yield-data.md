# 0006: DeFi yield data on BTB: BTB Finance as the first data provider

Status: **Draft; phase M0 built** ([`examples/defi-yields`](../../examples/defi-yields), tests `crates/btb-tests/tests/defi_yields.rs`). Builds on [0004](0004-market-data.md) (data providers, provider tables, paid queries) and [0005](0005-open-data-feeds.md) (open feeds, per-read fees to reporters). Nothing here changes consensus; the chain-side gaps it depends on are listed in [Chain-side work](#chain-side-work).

## Summary

BTB Finance (the app at btb.finance) already computes, every hour, data nobody else publishes in one place: about 4,360 concentrated-liquidity pools across 15 chains and many DEXs (index of 2026-11-22) with TVL, volume and fees, the **real ±5% range APR** read from each pool's fee counters, token safety grades, and Smart LP rankings. Today it lives in a private Convex backend and is overwritten every hour.

This proposal publishes that data **on BTB Chain**, two ways:

1. **Provider tables** (the 0004 pattern): a `defi-yields` contract holds pools, hourly pool stats, token grades and a cross-asset yield table. BTB Finance is the first provider and publishes signed batches; **derived numbers (rankings, risk grade, best yield for a budget) are computed on chain**, so anyone can recompute them. Reads are free in queries and paid through paid queries on nodes.
2. **Open feeds** (the 0005 pattern): a small set of headline numbers (yield indices, benchmark rates) as registered feeds that validators opt in to report and that **contracts pay to read**, with the fee going to the reporters.

Who earns: the provider (once 0004 phase B exists), the validators who report feeds, and the hosting nodes that answer queries. It can be BTB Finance or anyone: other providers can publish the same schema, and any validator can join a feed.

## Why this dataset

| | Today (off chain) | On BTB |
| --- | --- | --- |
| Range APR, pool stats | BTB Finance's Convex backend, latest snapshot only, overwritten hourly | Provider table with history (latest rows on chain, older history as blobs with the root on chain) |
| Derived numbers | Computed in the app, unverifiable | Computed by queries and crons from published facts, verifiable |
| Who can use it | The BTB Finance app | Any app or contract on BTB, any client via a hosting node, with Merkle proofs |
| Who earns | Nobody (cost centre) | Provider, reporters, hosting nodes |

It is also the first real-world app for BTB Chain: a dataset with existing daily users on the BTB Finance side.

## Datasets

### A. Provider tables in the `defi-yields` contract

Modelled on `examples/market-data`: integer units, `CHAIN_ID:ID` keys, provider-only `publish_*` mutations of at most 64 rows (see [Heap limit](#heap-limit-what-m0-measured)), whole batch refused on one bad row, an older row never replaces a newer one.

| Table | Key | Columns (units) | Published |
| --- | --- | --- | --- |
| `pools` | `pool_id` = `chain_id:address_or_v4_id` | chain id, dex, version, fee tier (bps x 100), token0, token1 (32-byte address or symbol hash), stable flag | When a pool is first seen; rarely changes |
| `Stat` (latest) and `Hour` (history) | `pool_id`; `pool_id\|hour` | TVL (USD, whole units), volume 24h, fees 24h (USD), base APR (bps), reward APR (bps), range APR (bps), range days, in-range share (bps), net vs hold (bps), source time; the latest row also stores the derived headline APR, pair label and risk | Hourly; history pruned by the provider (`prune_hours`); older history as monthly blobs with the root on chain (not built) |
| `token_grades` | `chain_id:token` | score 0..100, flags bitmask (owner can mint, can blacklist, can change fees, proxy, liquidity locked, ...), holders, top-10 share (bps), scanned at | When a token is scanned (at most daily) |
| `yields` | `kind:asset:venue` | rate (bps), kind (lp, staking, lending, tbill, savings, dividend), currency, liquidity class (instant, days, locked), source, as of | Phase 1: on-chain kinds only (LP, staking, lending, tokenized T-bills); later public rates (T-bills, savings) |

**Derived on chain (integer math, like `stock()` in market-data), as built:**
- `pool(key, hours)`: one call with pool facts, latest stats, fee estimate, the grades of both tokens, the **risk grade A..E** (recomputed live) and up to 336 hours (14 days) of history.
- `top_pools(chain, max_grade, min_tvl, stable_only, limit)` and `best_for(amount, max_grade, chain, limit)` (only pools where the position is at most 2 % of the TVL): ranked by headline APR from an APR index (all chains) or a `chain << 64 | apr` index (one chain, or its stable pairs), decoding at most 600 rows.
- `top_yields(kind_mask, max_grade, limit)`: pools and non-pool yields (staking, lending, T-bills, savings, ...) in one ranking.
- `index(chain, stable_only)`: pool count, TVL, median APR and TVL-weighted APR, **exact for any number of pools**: running totals are kept on every stats write and the median is found by counting row ids in APR order (no rows decoded).
- `rescore(pools)`: after `publish_grades`, refreshes the token part of the risk stored for rankings.

**Risk grade.** A published formula over published facts only: token flags and scores, TVL, in-range share, kind, liquidity class. Because the formula runs on chain, a provider can lie about facts but never about the grade.

### B. Open feeds (headline numbers contracts can buy)

Registered with `FeedOp::Register` (1,000 BTB deposit each), reported by validators that opt in. Keep the set small (256 feeds per validator, a deposit per feed):

| Feed name | Value (decimals) | Sources a validator configures |
| --- | --- | --- |
| `YIELD:STABLE-LP:BASE` | TVL-weighted range APR of stable LP pools on Base (bps, 0) | BTB Finance endpoint (below), plus any other provider that publishes the same index |
| `YIELD:STABLE-LP:ETH` | Same for Ethereum | Same |
| `YIELD:ETH-STAKING` | ETH staking rate (bps) | BTB endpoint plus public staking APIs |
| `YIELD:USDC-LENDING` | USDC supply rate on the largest lending markets (bps) | BTB endpoint plus lending protocols' public APIs |
| `RATE:US-TBILL-3M` | US 3-month T-bill (bps) | Public government data sources |

**The single-source caveat, stated plainly.** A validator's observation is a median of its own sources, and the chain takes the stake-weighted median of validators. If BTB Finance is the only source for an index, every validator relays the same number, so the quorum proves the endpoint said it, not that it is true. Feeds that money depends on should have at least three independent sources (benchmark rates, staking and lending rates have them); BTB-only indices should say so in their name or description until other providers publish them.

### C. What stays off chain

Business leads and project contacts (the sales product in the BTB data business plan) are **not** published: anything in chain state is readable by every node operator for free, and contact data must not be spread that way. Contributions and rewards for that dataset can move on chain later; the data itself stays private.

## The bridge from BTB Finance (no chain change needed)

```
Convex crons (hourly discover refresh, daily range yield, token scans)
   ├─► R2 archive (raw snapshot, kept forever)
   ├─► convex/btbChainPublish.ts  ── @btb/client: callKind + signTransaction ──►  POST /tx on a hosting node
   │        (provider key in Convex env, publish_* batches of <= 500 rows, nonce per batch)        or btb_sendTransaction
   └─► Next.js /api/feeds/<name>  ── {"value": "1234"} ──►  validators' price_sources (url + JSON pointer)
```

1. **Provider publisher** (`convex/btbChainPublish.ts`, Node runtime action). After each refresh, encode rows with the `@btb/client` codec, build `callKind(contract, "publish_pool_stats", args)`, sign with the provider seed, send, and read the receipt with `GET /tx/<id>`. Idempotent per hour (the contract ignores rows older than what it holds), retried on failure, never blocking the app's own refresh.
2. **Feed endpoint** (`src/app/api/feeds/[name]/route.ts`). A tiny JSON document per feed, cached at the CDN for 60 s, so validators can point `price_sources` at it: `{"name": "my-btb", "url": "https://btb.finance/api/feeds/YIELD:STABLE-LP:BASE", "pointer": "/value"}`.
3. **Keys.** A dedicated provider account with a small balance, seed in Convex environment variables only, rotated by a contract `set_provider` call. The contract's provider is an address list so a key can be rotated without downtime.

## Economics

| Who pays | For | Who earns | Status |
| --- | --- | --- | --- |
| A contract (or user) reading a feed in a mutation or cron | `read_fee` per read | Validators whose observations made the value | Built (0005 A+B) |
| An app or client running a query on a node | Gas per query, through vouchers, after a free quota | The node that answered | Built for validator RPC (0004 B0); hosting-node fees not yet |
| A consumer of provider tables | Per read | **The provider** | Not built: needs 0004 phase B (provider registry and per-read fees to providers) |
| Provider | Gas for `publish_*`, storage deposits for rows | Validators | Built |

Until 0004 phase B exists, BTB Finance publishes at its own cost (gas plus deposits) and earns nothing on chain; the value is the showcase, the verified history and the feeds' reporters' income. Measured publishing load (M0, local network): about 97,000 gas per pool and hour (pool facts once, stats every hour), 64 rows per transaction since the SDK heap went to 512 KiB (16 before). All 4,360 pools hourly is about 69 transactions an hour; publishing only pools whose numbers changed, or every 4 hours, divides that.

## Milestones

| # | What | Where | Done when |
| --- | --- | --- | --- |
| M0 ✅ | `examples/defi-yields` contract with tests on real pools from the BTB snapshot | btb-chain | `cargo test -p btb-tests --test defi_yields` passes (13 tests); deployed on a local 4-validator network with `btb deploy`, 238 real pools published and ranked |
| M1 | Local network: `scripts/testnet-local.sh`, deploy the contract, publish one real snapshot from a script | btb-chain | `pool()` returns live numbers with a Merkle proof that `verifyProof` accepts |
| M2 | Convex publisher + R2 archive, pointed at the local or a team-run network | BTBFrontend | Hourly batches land; a missed hour catches up; app unaffected when the chain is down |
| M3 | Feed endpoint, register 5 feeds, 3 or more validators configure sources and join | both | Feeds publish; a demo contract reads `YIELD:STABLE-LP:BASE` in a mutation and the receipt's `feed_fees` reach the reporters |
| M4 | Public page: "on-chain yields" (market-data-web style, or a page in BTB Finance) with a "verified on BTB Chain" proof badge | both | Anyone can check a number against chain state |
| M5 | Public testnet (TESTNET.md checklist) and the chain-side work below | btb-chain | Other providers and validators can join |

## Heap limit: what M0 measured

Contracts run with the SDK's bump heap, **never freed within a call** (`crates/btb-sdk/src/runtime.rs`). Every lookup (key clones, request and result buffers), row read (256-byte buffer, then the exact size), `format!` and growing `Vec` keeps its memory until the call ends; a 300-byte row costs about 1 KB to write. M0 first ran with the SDK's original 128 KiB heap; it was then raised to 512 KiB (the VM gives a call 1 MiB, and every example contract still keeps 300 KiB or more for input). Measured with worst-case rows (66-character V4 pool ids):

| Call | 128 KiB heap | 512 KiB heap |
| --- | --- | --- |
| `publish_stats` (new or update) | 16 rows; fails at 20 | 80 rows; fails at 96 (8.5 M gas for 64 new rows) |
| `publish_pools` | 48 rows; fails at 56 | 128 rows and more |
| `pool` history | not measured | 480 hours; fails at 720 |
| A ranking decoding its full scan | 150 rows, 1.8 M gas | 600 rows, 7.2 M gas (of the 50 M query gas) |

The contract keeps a margin: 64 rows per batch, 600 rows per ranking, 336 hours per `pool`. It is also shaped by the heap: rankings decode one row per pool (pair and risk stored on write), the index keeps running totals and counts row ids instead of decoding rows, updates reuse the previous row instead of reading the pool and grades again, and pruning deletes by row id.

Further options for the SDK, not taken: resetting the heap between rows of a batch mutation, or size-hinted encoding so `Vec` growth does not leave dead buffers.

## Chain-side work

Needed for the full design, none of it consensus-breaking:

1. **TypeScript SDK and CLI for feed operations.** `feedKind(FeedOp)` in `sdk-ts/src/tx.ts` (transaction tag 26) and `btb feed register|update|close|join|leave` in `btb-cli`. Today feed operations need hand-built transactions.
2. **0004 phase B: provider registry and per-read fees to providers.** Without it, providers earn nothing on chain.
3. **0005 phase C: `kind = Value`.** Lets token grades (a score plus a flags bitmask) be a feed, not only a table.
4. **0005 owner share.** Lets a feed's registrant (the provider maintaining the source) earn a share of read fees.
5. **Hosting-node paid queries.** Validator RPC charges per query today; hosting nodes do not yet.
6. **Public testnet** (TESTNET.md).

## Safety and data rules

- Publish only data BTB Finance computes itself from public chain data. Third-party datasets (DefiLlama, CoinGecko, explorers) are not republished unless their licence allows it; each dataset carries a source and licence statement (shared with 0004 and 0005 phase C).
- A wrong row can be corrected only by a newer row from the provider; history keeps both (provider accountability).
- Feeds that contracts use to move money must have several independent sources and contracts should check reporters and age (`price_checked_with`, 0005 C) or use TWAPs.
- The provider key holds only what publishing needs; a compromised key can publish bad facts but cannot change the on-chain formulas, and is rotated by `set_provider`.

## Open questions

1. Publish hourly or every 4 hours, or only changed pools (about 97,000 gas per pool and hour, measured)?
2. How many hours of history to keep on chain before moving to monthly blobs (`pool` returns at most 336)?
3. Should `yields` include off-chain rates (T-bills, savings) from day one, or only after several independent providers exist?
4. Feed `read_fee` levels for the first five feeds.
5. Does BTB Finance run its own validator (and report its own feeds), or only provide data and leave reporting to others?
