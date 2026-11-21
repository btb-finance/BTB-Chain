# 0005: Open data feeds: any validator adds any feed, readers pay the reporters

Status: **Draft**. Issue: to be opened by a maintainer as a Design Proposal. Touches oracles (consensus-adjacent execution) and tokenomics: the Safety section must be reviewed before code.

## Summary

Today the chain's price feeds are fixed at genesis (at most 64 pairs), every validator must report every pair, and the per-read fee goes to the whole validator pool. Make data an open market, the way independent node operators work on Chainlink: **anyone registers a feed, each validator decides for itself which feeds it serves (and carries the legal responsibility for its own data), readers choose how many reporters they require, and every read pays the validators whose observations made the value.** Feeds that people pay for attract reporters; feeds nobody reads cost nothing to anyone. The chain stays neutral: it aggregates signed observations and moves fees, nothing more.

**Recommendation:** A (open registry and opt-in reporting) and B (per-read fees to reporters) together, then C (reader-chosen quorum and richer values).

## What exists ([ORACLES.md](../ORACLES.md#2-price-feeds-and-real-time-data))

- Pairs `{id, name, decimals}` in the genesis oracle config, at most `MAX_PAIRS = 64`.
- Each validator fetches its configured sources (`price_sources` in `node.json`), signs `(pair, price, height)` with its consensus key, gossips it; proposers carry observations in the block (at most 1024).
- `end_block` keeps each validator's latest observation and, for pairs with news, publishes the **stake-weighted median** if at least **2/3 of the total stake** reported; update rule, circuit breaker and TWAP follow.
- Reads: `price(pair)` free in a query; in a mutation or cron `HOST_PRICE_MUTATION = 200 + 10,000` gas, the 10,000 going to the validator pool.

What is closed: the feed list (genesis), the reporters (all validators, 2/3 of stake required) and the payee (the whole pool, including validators that never reported).

## Definitions

- **Feed.** `{id: u32, name, kind, decimals, owner, read_fee, min_reporters, min_stake_bps, max_age_blocks}`, registered on chain. `kind` is `Price` (one integer, as today) first; `Value` (up to 64 bytes, byte-exact agreement) later.
- **Reporter.** A validator of the current epoch that has **opted in** to a feed (an on-chain flag, so everyone knows who is expected) and signs observations for it. Opting in or out is a transaction; nobody is ever required to report anything.
- **Quorum of a feed.** At least `min_reporters` distinct reporters **and** at least `min_stake_bps` of the *opted-in* stake with a fresh observation. The published value is the stake-weighted median over those fresh observations (the existing `weighted_median`).
- **Contributors of a value.** The reporters whose fresh observations were in the median's input when the value was published.

## Design

### A. Open registry and opt-in reporting

1. **Register.** `FeedOp::Register {name, kind, decimals, read_fee, min_reporters, min_stake_bps, max_age_blocks}`: anyone, with a deposit (default 1,000 BTB, genesis parameter) locked while the feed exists, against spam. Names are unique (`BTC/USD`, `PSX:OGDC`, `WEATHER:KHI`); the owner can change `read_fee` and the quorum (not below the chain floor `min_reporters >= 3`, `min_stake_bps >= 1000`) and can `Close` an unused feed to get the deposit back. Genesis pairs become feeds owned by the chain, unchanged.
2. **Opt in.** `FeedOp::Join {feed}` / `Leave {feed}`, signed by the validator's consensus key. Only current validators; a validator leaving the set stops counting for every feed. At most `MAX_FEEDS_PER_VALIDATOR` (default 256) per validator, so one node's work is bounded.
3. **Report.** Unchanged wire format and checks (consensus-key signature, chain id, freshness, strictly newer), plus: the signer must have joined the feed. Per-block caps stay (`MAX_OBS_PER_BLOCK`), now with a fair share per validator so one validator with many feeds cannot crowd out others.
4. **Publish.** As today (update rule, circuit breaker, TWAP), with the quorum of the feed instead of the fixed 2/3 of total stake. A feed below quorum keeps its last value; its age tells readers how stale it is.
5. **Node side.** `price_sources` already maps pairs to sources; a validator adds a feed by adding its sources and sending `Join`. A validator without sources for a feed simply does not join.

### B. Per-read fees to the reporters

- A read in a mutation or cron pays the feed's `read_fee` (gas units, at least the current 10,000) plus the read gas.
- The fee is **credited to the contributors of the value read**, split by stake (rounding remainder to the first contributors, as epoch rewards do), paid out with the epoch rewards. Nobody else earns from it: not validators that did not report, not the feed owner. An optional `owner_share_bps` (at most 10 %) lets a registrant who maintains sources be paid too.
- Reads in **queries** stay free on chain; a node serving them charges through paid queries ([0004 B0](0004-market-data.md#paid-queries-on-validator-rpc), built), so query traffic also pays whoever serves it.
- Nothing is minted: every unit comes from the reader's gas.

### C. Reader-chosen trust, richer values

- `price_checked_with(feed, min_reporters, max_age)`: a contract asks for a stricter quorum than the feed's own and gets `NotEnoughReporters` otherwise.
- `kind = Value`: byte-exact agreement of `min_reporters` (no median) for non-numeric data (a match result, a flag); larger records go through providers ([0004](0004-market-data.md)).
- Per-dataset licence statement (shared with 0004): the registrant states the source and licence type; each validator decides for itself whether to join.

## Responsibility

The protocol aggregates signed observations and moves fees; it does not choose, endorse or check what a feed contains. **Each validator decides which feeds it joins and is responsible for the data it signs and for its own legal requirements**, like independent oracle node operators. A validator can leave any feed at any time. This is the same position ORACLES.md already takes for app-defined feeds.

## Options compared

| | Keep genesis feeds | **Open feeds, opt-in, reporters paid (this)** | Separate oracle network (non-validators report) |
| --- | --- | --- | --- |
| New feeds | Hard fork / genesis | One transaction | One transaction |
| Who reports | All validators, forced | Validators that choose to | Anyone staked as a reporter |
| Who is paid | Whole pool | Only contributors | Reporters |
| Trust | 2/3 of total stake | Feed quorum (≥ 3 reporters, ≥ 10 % of opted-in stake), reader can demand more | A second staking system to secure |
| Work | Little | Moderate (registry, fee routing) | Large |

## Safety

- **Manipulation.** A feed is only as strong as its reporters. With `min_reporters = 3`, three colluding validators can set a value; that is why the quorum is per feed, published with the value (`reporters`, `stake`), and why contracts that move money should demand more (`price_checked_with`) or use TWAPs and the circuit breaker (both kept). The robust-median argument holds over the opted-in set: fewer than half of the reporting stake faulty cannot move the value outside the honest range.
- **Consensus load.** Observation bytes per block stay capped; feeds per validator are capped; verification cost per observation is unchanged. A flood of registrations costs deposits; a feed nobody joins costs nothing.
- **Fee accounting.** Fees are credited at read time to the contributor set stored with the value (bounded: at most the validator set), summed and paid with epoch rewards; the supply invariant must hold (test). A jailed validator's credited fees for the epoch go to the other contributors, as with rewards.
- **No minting**, no change to the 1B supply.
- **Determinism.** Registration, opt-in, quorum, median and fee split are functions of state and block contents only.

Failure-case tests: registration without deposit, duplicate name, quorum below the floor; an observation from a validator that did not join, from a non-validator, stale, replayed or forged is skipped; a feed below quorum keeps its old value; a read in a query charges nothing; a read in a mutation credits exactly the contributors, in stake proportion, and the sum equals the fee; a validator leaving the set stops counting at once; `price_checked_with` refuses a value with too few reporters; supply invariant across reads and epoch payouts; sequential and parallel execution agree.

## Phases

- **A + B:** `btb_chain::oracle` (feed registry under the oracle tag, opt-in set, per-feed quorum, contributor set stored with each value, fee credit), `FeedOp` transaction, `btb_vm` host-call gas for `read_fee`, SDK `price(feed)` unchanged, RPC `btb_feeds`, `btb_feed [id]`, node `Join` from config. Genesis pairs migrate as chain-owned feeds.
- **C:** `price_checked_with`, `kind = Value`, licence statement.

## Affected files

`crates/btb-chain/src/oracle.rs`, `tx.rs`, `crates/btb-vm` (host call, gas), `crates/btb-sdk/src/oracle.rs`, `crates/btb-node` (oracle glue, RPC), docs `ORACLES.md`, `TOKENOMICS.md`, `CHAIN.md`, `NODE.md`, `GLOSSARY.md`.

## Open questions

1. Registration deposit: 1,000 BTB, or scale with `read_fee`?
2. Should the chain floor be stricter for feeds that DeFi contracts read (for example `min_stake_bps >= 3334`)?
3. Owner share: allow it at all, or keep every read fee for the reporters?
