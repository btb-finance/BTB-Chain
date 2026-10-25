# Oracles: Randomness and Real-World Data, Built In

On most chains, contracts cannot get a random number or a price on their own. They rely on a separate oracle network, such as Chainlink, and pay it for every request. **On BTB Chain, oracles are a native feature of the validator set**: randomness, price feeds and web data come from the same validators that run consensus, and arrive as ordinary tables your app can query and subscribe to.

This works because BTB validators already do the three things an oracle network does:

- **Reach agreement** on values (consensus),
- **Hold shared threshold keys** (also used for [cross-chain signing](CROSS_CHAIN.md)), and
- **Run servers** that can fetch outside data (they are a decentralized cloud).

## What BTB provides

| Feature | What it replaces | How BTB does it |
| --- | --- | --- |
| Verifiable randomness | Chainlink VRF | Threshold signature over each block, produced jointly by validators |
| Price feeds | Chainlink Data Feeds | Validators fetch prices from many sources, agree on the median, write it to a system table |
| Real-time data streams | Low-latency feeds such as Chainlink Data Streams or Pyth | Feed rows update every block; apps subscribe like any Convex-style query |
| Custom web data | Chainlink Functions | Contracts request an HTTPS call; validators each fetch it and agree on the result |
| Automation | Chainlink Automation | Native `@cron` and scheduled functions |
| Data for other chains | Oracle networks on each chain | BTB signs reports with its threshold key; contracts on Ethereum, Solana and others verify them |

## Status

| Piece | State |
| --- | --- |
| Threshold-BLS randomness beacon, `random_bytes`, `#[btb::mutation(randomness)]` | **Built** (`btb-oracle`, `btb-vm`, `btb-chain`, `btb-node`), with the key-provisioning caveat below |
| Built-in price feeds, `price`, validators' signed observations, stake-weighted median | **Built**; the observation source is a plug-in function (a mock in tests), real exchange fetching waits for HTTPS actions |
| Distributed key generation and resharing | **Built as a library** (`btb_oracle::dkg`), tested against malicious dealers; **not yet wired to a transport**: today the beacon key is dealt once at genesis by a trusted dealer |
| HTTPS actions with transforms | Not built |
| Signed reports for other chains | Not built |

The two caveats that matter, stated up front:

1. **Key provisioning is an interim trusted dealer.** The genesis tool (`btb-node init-testnet --oracle true`, or any harness calling `btb_oracle::trusted_deal`) deals the beacon key and sees every share. Whoever ran it can predict the beacon forever. The committee is the genesis validator set and does not rotate with the staking epochs. The per-epoch DKG and resharing (below) are implemented and tested but their transport is future work. A mainnet genesis must run the DKG ceremony instead.
2. **The committee is by validator count, not stake.** Each holder has one share; the threshold is `n - floor((n - 1) / 3)` (more than two thirds of the holders). Stake weighting of the beacon (several shares per validator) is future work.

## 1. Verifiable randomness

### Design

Every block height `h` has one **beacon value**, the threshold BLS signature of the committee's group key on

```
"btb.beacon.v1" || chain_id:u64be || epoch:u64be || height:u64be
```

(BLS12-381, public keys in G1, signatures in G2, `blst`, Apache-2.0). It is unique: a BLS signature is a deterministic function of key and message, so any `t` valid partial signatures combine to the same 96 bytes. Anyone can verify it against the group key stored in the genesis state. The 32-byte **seed** is `SHA256("btb.beacon.seed.v1" || signature)`.

**Release rule.** A validator releases its partial signature for height `h` only after its own replica **committed** block `h`. Consequences:

- The transactions that consume `beacon(h)` are final before any partial exists, so their content cannot depend on the randomness.
- Nobody can compute `beacon(h)` early. With fewer than `t` partials every candidate signature is equally likely: the tests show that interpolating `t - 1` partials yields a value that does not verify, that padding with invented partials is rejected, and that the output passes bit-balance and avalanche sanity checks.
- Block `h` executes only when its beacon is known. A committed block waits in a queue (`pending_exec` in the driver) until `t` partials arrive; a node that is late or catching up asks its peers (`Want{height}` answered with the finished beacon, verified before use, stored with the block history). One extra network round trip per block is the price of the rule. Consensus attestation ([CONSENSUS.md](CONSENSUS.md#randomness-beacon-and-execution-lag)) already tolerates execution lag.

**Why the committee cannot bias it.**

- *Grinding by the block producer.* The proposer of `h` fixes the payload without knowing `beacon(h)`; it cannot see the outcome and then decide what to include. It could choose between payloads only by guessing.
- *Grinding by a coalition of validators.* Fewer than `t = n - floor((n - 1)/3)` holders (in particular the `f < n/3` the protocol tolerates) cannot compute the beacon, and holders who withhold their partials can only stall it: with `n - f >= t` honest holders the beacon still appears, so a minority can delay but not steer or censor it (`liveness_with_exactly_f_offline`). Withholding for one height cannot select between two outcomes either, because there is exactly one valid value.
- *Bad partials.* Partials are checked against the holder's public share (`Collector`): optimistically, with one verification of the aggregate, and per partial only if that fails; a holder whose partial fails is ignored for that height. A message is accepted only from the peer whose consensus key is the holder named in it (`malicious_partials_are_rejected_and_do_not_block_liveness`, `forged_misattributed_and_out_of_window_partials_are_ignored`).
- *Key generation bias.* With a trusted dealer the dealer knows everything (see the caveat). With the joint-Feldman DKG below, a rushing adversary can bias the *distribution of the group public key*, which Gennaro, Jarecki, Krawczyk and Rabin showed does not weaken discrete-log signatures such as BLS; the beacon *values* are still unpredictable and unique.
- *Last-revealer effects.* The only party who learns `beacon(h)` first is whoever completes the aggregation, a moment later than every other honest validator, and the value cannot be changed by it.

Trust model: unpredictability needs fewer than `t` colluding holders; liveness needs at least `t` honest ones; both assume the key was provisioned honestly (the caveat above).

### Key provisioning and the DKG

`btb_oracle::dkg` implements, as a deterministic library with no I/O:

- **Joint-Feldman DKG with complaints.** Each dealer `i` broadcasts Feldman commitments `C_{i,k} = a_{i,k} * g1` and privately sends `s_{ij} = f_i(j + 1)` to participant `j`. A participant whose share is missing or does not satisfy `s_{ij} * g1 = sum_k C_{i,k} (j + 1)^k` broadcasts a complaint; the dealer must answer by broadcasting `s_{ij}`; a correct answer clears the complaint, a wrong or missing one disqualifies the dealer. The qualified dealers' contributions add up: group key `sum C_{i,0}`, share `sum s_{ij}`, public share `sum_k sum_i C_{i,k} (j + 1)^k`. Everyone who reads the same board computes the same `GroupKey`; fewer than `t` qualified dealers means the ceremony fails rather than producing a key controlled by a few.
- **Resharing** to a new committee (other size, other threshold) without reconstructing the secret: each old holder deals its own share, its constant commitment must equal its published public share, and the new shares are Lagrange combinations over the qualified old holders. The group key does not change, so old and new committees sign identically.
- **Trusted-dealer bootstrap** (`trusted_deal`).

The intended transport is the chain itself as the broadcast board (commitments, complaints and answers as transactions in the last blocks of an epoch) and the authenticated, encrypted validator channels for the private shares; the validator set of epoch `e + 1` is fixed at the last block of epoch `e`, so the handover would run in the first blocks of epoch `e + 1` with the old committee still signing. None of that transport exists yet; the tests (`crates/btb-oracle/tests/oracle.rs`) drive the library directly, and `crates/btb-chain/tests/oracle.rs` runs a chain with a DKG-generated key and with that key reshared from 4 to 7 holders (same beacon bytes under both committees): honest runs, a bad share with and without a justification, a missing share, false complaints, silent dealers, too few dealers, a reshare from 4 to 7 holders with a lying old holder.

### Using it

```rust
#[btb::mutation(randomness)]
pub fn roll_dice() -> u64 {
    let n = random::range(1, 7); // 1..=6, unbiased, from the beacon
    // ...
    n
}
```

- `random::bytes(n)` (at most 1024), `random::u64()`, `random::range(lo, hi)` (rejection sampling, no modulo bias). They call the host call `random_bytes` ([LANGUAGE.md](LANGUAGE.md#host-calls)).
- **Per-call derivation.** `tx_seed = SHA256("btb.rand.tx.v1" || beacon_seed || tx_id)`; the `c`-th call of the transaction gets `SHA256("btb.rand.out.v1" || tx_seed || c:u64be || j:u32be)` blocks. A cron run uses its receipt id as `tx_id`. Replicas derive identical bytes (`randomness_is_deterministic_across_replicas_and_depends_on_the_beacon`), two transactions or two calls never share bytes.

**Safety rules built into the runtime.**

- **Marker, enforced at run time.** `#[btb::mutation(randomness)]` and `#[btb::cron(randomness, every = "N blocks")]` export the symbol `btb_randomness_<kind>_<name>`. The VM reads it from the ELF on every call (it is not in the manifest and the caller cannot change it). A `random_bytes` call from a query, an action or an unmarked function traps with `RandomnessForbidden` (a read-only host is refused even if flagged). A marker on a query or action, or without a matching entry point, makes the contract invalid at deploy. The macro rejects `#[btb::query(randomness)]` at compile time.
- **Never callable by other contracts.** There are no cross-contract calls; if they are added the callee must run with the flag off. A marked function is reached only by a transaction or a scheduled cron.
- **No retry-until-you-win.** A function that is marked `randomness` is always charged its **full gas limit**, whether it succeeds, aborts or traps, so aborting after a bad draw is never cheaper than keeping it, and a caller cannot make bad outcomes fail by running out of gas (`randomness_costs_the_full_gas_limit_even_when_the_call_fails`). Pick a tight gas limit.
- **Queries can't use it.** The seed is only ever read by marked mutations and crons, which go through consensus.
- **A chain without a beacon** (no oracle config in the genesis) traps marked calls with `RandomnessUnavailable`. `btb dev` has no beacon and serves marked functions from a fixed, public seed so they can be tried out; that output is not random.
- **Gas.** `random_bytes` costs `300 + 2 * len` gas, charged before the call.

## 2. Price feeds and real-time data

### Built

1. A feed is a pair in the genesis oracle config: `{id, name, decimals}` (for example `0`, `BTC/USD`, `8`), up to 64, plus `max_age_blocks` (default 100).
2. Each validator periodically observes every pair and signs `(pair, price, height)` with its **consensus key** (Ed25519, domain-separated, bound to the chain id); `price` is an integer with the pair's decimals, `height` the height the validator had executed. Observations are gossiped to the validators (frame kind 5) and the block proposer puts the ones it holds into its payload (the oracle side-channel; at most 1024 per block).
3. The executor verifies each observation in `end_block`, **skipping** (never failing the block) any that is badly signed, from a non-validator of the current epoch, for an unknown pair, from the future or older than `max_age_blocks`, or not strictly newer than that validator's previous one for the pair (a validator cannot go back in time or change its first statement of a height). Accepted ones replace the validator's latest observation.
4. For every pair that got news, the chain takes the **stake-weighted median** (lower weighted median: the first price at which the cumulative stake reaches half of the reporting stake) over the current epoch's validators' fresh observations, **provided at least two thirds of the total stake reported**. Otherwise the previous price stays, and its height tells consumers how stale it is.
5. The result is stored at `price(pair) = {price, height of the update, decimals, number of reporters}` in the state.

**Why a minority cannot move it.** If less than one third of the stake is faulty and at least two thirds of the stake reports, the faulty reporters hold less than half of the reporting weight, so the weighted median is a value reported by an honest validator or lies between two of them; it never leaves the range the honest validators saw (`weighted_median_is_robust_to_less_than_a_third_of_the_stake` checks this on thousands of random stake and report patterns, `a_minority_of_stake_cannot_move_the_price_outside_the_honest_range` on the chain, and a 4-node cluster test with one validator reporting garbage). A Byzantine proposer can omit observations, which only delays an update (a validator's earlier observation remains valid for `max_age_blocks`); it cannot forge or alter one.

Transactions in a block see the prices of the **previous** block (observations are folded in after the transactions), so a transaction cannot depend on an observation in its own block.

**Reading it.**

```rust
#[btb::query]
pub fn btc_price() -> Option<u128> {
    price(0).map(|p| p.price) // free in a query
}

#[btb::mutation]
pub fn liquidate(position: u64) {
    let Some(p) = price(1) else { abort("no price") };
    if p.age_blocks(block_height()) > 5 {
        abort("price too old"); // the contract states how old a price it accepts
    }
    // ...
}
```

- Host call `price(pair, out_ptr)` returns `price: u128`, `height: u64` (update height), `decimals: u8`; `NOT_FOUND` for an unknown pair or no price yet.
- **Fees (reconciled).** A **mutation or cron pays a per-read fee**: the call costs `HOST_PRICE_MUTATION = 200 + 10,000` gas, which like all gas goes to the validator pool and so to the validators that observed and signed prices; the 10,000 is the fee part. A **query pays nothing**: the call costs `HOST_PRICE_QUERY = 200` gas and queries are not billed. (Earlier drafts said both "no extra fee per read" and "a per-read fee"; this is the single rule.)
- RPC: `btb_oracleConfig`, `btb_oraclePrice [pair]`, `btb_oracleBeacon [height]` ([NODE.md](NODE.md)).

### Planned (needs HTTPS actions)

- **Sources.** Today a validator's observation comes from a configured function (`NodeParams::price_source`; a mock in tests). Real feeds will fetch many exchanges and aggregators on the validator's own server through [HTTPS actions](#3-custom-web-data-https-requests) and take a median across sources before signing, so that each validator's observation is itself a median of medians.
- **Update rule** with a deviation threshold and a heartbeat (today every observation round updates).
- **Circuit breakers**, when sources disagree or a value jumps; **TWAP helpers** over the feed history; and publishing the number of sources and their spread next to the price.

## 3. Custom web data (HTTPS requests)

> **Not built yet.** This section is the design.

Any contract can ask for data that no built-in feed covers, such as a sports score, weather, or an API response.

```rust
#[btb::action]
pub async fn fetch_weather(city: String) -> Result<()> {
    let resp = http::get(format!("https://api.example.com/weather/{city}"))
        .transform(extract_temperature) // keep only the stable part of the response
        .send()
        .await?;
    Weather::upsert(city, resp.parse()?)?;
    Ok(())
}
```

**How it stays deterministic.** Each validator makes the request itself. A *transform* function strips the parts that differ between responses (timestamps, request IDs, headers) and keeps only the value the contract needs. Validators agree on the transformed result: identical results for exact data, or the median for numbers. The Internet Computer's HTTPS outcalls work this way.

**Who pays.** The calling contract pays gas for every validator's request, so expensive or frequent calls cost accordingly.

**Limits.** The source must give consistent answers to all validators within a short window. APIs that require secret keys need extra care: a key given to all validators is visible to all of them.

## 4. BTB as an oracle for other chains

Because BTB validators hold threshold keys for [cross-chain signing](CROSS_CHAIN.md), BTB can **serve its prices and randomness to other chains**:

1. Validators sign a report (for example "ETH/USD = 3,412.50 at block N") with the threshold key.
2. Anyone relays the report to Ethereum, Solana or another chain.
3. A small verifier contract on that chain checks the single threshold signature and uses the value.

This turns BTB's oracle into a product for the whole ecosystem, with fees paid to BTB validators and hosting nodes.

## Economics

Oracle work is paid like everything else on BTB: **whoever does the work gets paid.**

| Service | Who pays | Who earns |
| --- | --- | --- |
| Randomness | Mutation's gas (`300 + 2 * len` per call, and the full gas limit is charged) | Validators |
| Built-in price feeds | Apps that read them **in mutations and crons**, through a per-read fee (10,000 gas on top of the 200 gas read); **queries pay nothing** | Validators that observe and sign prices (through the validator pool) |
| App-defined feeds | The app that created the feed, from its prepaid balance | Validators |
| Custom HTTPS requests | The calling contract's gas | Validators |
| Reports for other chains | The requesting chain's app or relayer | Validators and hosting nodes |

## Risks and limits

| Risk | Mitigation |
| --- | --- |
| Validators collude to fake a price | Same trust as the chain itself: requires two-thirds collusion. Stake-weighted median over at least two thirds of the stake tolerates less than a third of faulty stake. |
| The beacon key was dealt by one party | Interim trusted dealer at genesis (see Status): that party can predict the beacon. Fix: run the DKG ceremony at genesis and per epoch (library done, transport open). |
| The beacon committee is the genesis validator set | It does not rotate with staking epochs yet; if enough genesis holders leave, the beacon stalls (liveness, not safety). Resharing to each epoch's set is the fix. |
| A block waits for its beacon | One extra round trip per block; a missing beacon stalls execution (never produces a wrong one). Peers answer `Want` with verified values. |
| Sources are manipulated (thin markets, a single exchange) | Many independent sources, volume-weighted medians, circuit breakers, TWAPs |
| Stale data during network issues | Every value carries its age; contracts must declare a maximum age |
| One feed's failure halts apps | Feeds pause safely; apps can define fallbacks (another feed or a TWAP) |
| Data licensing and API terms | Built-in feeds use sources whose terms allow this use; app-defined feeds are the app's responsibility |
| Validator load from many feeds and requests | Feeds and requests are paid per use; gas caps per block for oracle work |

## Rollout

Oracles are part of **Phase 5** in the [roadmap](../ROADMAP.md):

1. On-chain randomness (threshold BLS beacon): **built** (trusted-dealer key; DKG transport open)
2. Built-in price feeds: **built** (mock observation source; real sources need step 3)
3. Custom HTTPS requests with transforms: not built
4. Signed reports for other chains: not built

See [Prior art](PRIOR_ART.md#chainlink-and-randomness-beacons) for how Chainlink, the Internet Computer, Aptos and drand approach the same problems.
