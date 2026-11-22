# defi-yields

Example contract for [proposal 0006](../../docs/proposals/0006-defi-yield-data.md), phase M0: DeFi yield data from one provider (BTB Finance's pool data), with everything derived computed on chain.

- **Provider writes facts** (at most 64 rows per call, provider only, a whole batch is refused if one row is bad, an older row never replaces a newer one):
  - `publish_pools`: pool facts (chain, DEX, fee, tokens, symbols, stable pair or not);
  - `publish_stats`: TVL, volume, fees, base and reward APR, the measured ±5 % range APR, its days, in-range share and result against holding. Each write also records the hour in the pool's history and updates the chain totals;
  - `publish_grades`: token safety scores and danger flags;
  - `publish_yields`: yields that are not pools (staking, lending, tokenized or real T-bills, savings, dividends);
  - `rescore`: refresh the stored risk after grades change; `prune_hours`: keep only the newest hours; `set_provider`: rotate the provider key.
- **Everyone reads, free:**
  - `pool(key, hours)`: facts, latest stats, fee estimate, both token grades, the risk grade (A..E, recomputed live) and up to 336 hours (14 days) of history in one call;
  - `top_pools(chain, max_grade, min_tvl, stable_only, limit)`, `best_for(amount_usd, max_grade, chain, limit)`: ranked by headline APR (the measured range APR when there is one);
  - `top_yields(kind_mask, max_grade, limit)`: pools and other yields in one ranking;
  - `index(chain, stable_only)`: pool count, TVL, median and TVL-weighted APR, exact for any number of pools;
  - `pools(chain, limit, cursor)`, `grade(token)`, `yield_rate(key)`, `get_provider()`.
- **The risk formula** is in the contract (`pool_risk`, `yield_risk`) and documented there: it starts at 100 and subtracts for low TVL, impermanent-loss exposure, a range that often left its range, an APR unlikely to last, and each token's grade and danger flags. A provider can publish wrong facts, never a wrong grade.

Units: money in whole US dollars (`u64`), rates and shares in basis points, pool fees in millionths. Keys: pools `CHAIN_ID:POOL` (`8453:0xec3c...`), tokens `CHAIN_ID:TOKEN`, yields `KIND:ASSET:VENUE`. EVM addresses in lower case; base58 ids (Solana, Sui) keep their case, under a chain id the provider assigns.

**Heap.** A call has the SDK's 512 KiB bump heap, never freed within the call, so batches are 64 rows (worst-case stats rows fit up to 80), rankings decode at most 600 rows and `pool` returns at most 336 hours; the index counts row ids instead of decoding rows. Measurements and the reasons are in the proposal.

Build: `./build.sh` (writes `defi-yields.elf`). Tests: `cargo test -p btb-tests --test defi_yields` loads real pools from the BTB Finance Discover snapshot of 2026-11-22 (Base, Ethereum, BNB Chain) and checks rankings, the chain index and the fee estimate against plain Rust over the same facts, the risk grades against the formula, refusals that leave no trace, and that 2,000 pools stay within the query limits.

Run on a local network (see [QUICKSTART.md](../../docs/QUICKSTART.md)):

```bash
scripts/testnet-local.sh --release            # 4 validators and the faucet
cd examples/defi-yields
$BTB deploy --rpc $RPC --key $KEY             # prints the contract address
curl -s -X POST http://127.0.0.1:8080/drip -d "{\"address\":\"$C\"}"   # storage deposits
$BTB call --rpc $RPC --key $KEY $C init       # the key becomes the provider
$BTB call --rpc $RPC --key $KEY --gas-limit 40000000 $C publish_pools <hex>
$BTB query --rpc $RPC $C index 052100000000000000   # (8453 as u64, false): Base, all pools
```

Arguments are the canonical encoding as hex ([ENCODING.md](../../docs/ENCODING.md)). On 2026-11-22 this published 238 pools from the live snapshot in 30 transactions of 16 rows, before the SDK heap went from 128 to 512 KiB (about 97,000 gas per pool) and answered rankings in 0.2 to 1.2 M gas and the index in about 38,000 gas.
