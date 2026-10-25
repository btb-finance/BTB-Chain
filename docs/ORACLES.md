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

## 1. Verifiable randomness

**How it works.** Every block, validators jointly produce a **threshold BLS signature** over the block's height and previous randomness. No single validator knows the result in advance, and nobody can choose it, because it needs signature shares from more than two-thirds of validators. The signature is hashed into the block's random seed. The Internet Computer's random beacon, Aptos's on-chain randomness and the drand network use the same idea.

**Using it.**

```rust
#[btb::mutation(randomness)]
pub fn roll_dice() -> Result<u8> {
    let n = random::range(1, 7); // 1..=6, from this block's seed
    Dice::insert(DiceRoll { player: caller(), value: n })?;
    Ok(n)
}
```

**Safety rules built into the runtime.**

- **Revealed only after ordering.** The seed for a block exists only after the block's transactions are fixed, so nobody can see the outcome and then decide whether to submit.
- **No retry-until-you-win.** Mutations that use randomness are marked `randomness`, cannot be called from other contracts (which could inspect the result and abort), and are charged their maximum gas up front, so a caller cannot make bad outcomes fail by running out of gas.
- **Queries can't use it.** Randomness is only available in mutations and crons, which go through consensus.
- **Unique per call.** Each call derives its value from the block seed plus the transaction hash and a call counter, so two calls never get the same number.

**Trust model.** Randomness is as secure as the chain: biasing it requires more than two-thirds of validators to collude, which would also let them break consensus.

## 2. Price feeds and real-time data

**How it works.**

1. A feed is defined by governance (or by an app, see below): a name like `BTC/USD`, a list of independent sources (exchanges and aggregators), an update rule and quality limits.
2. Each validator fetches every source on its own servers.
3. Each validator proposes its observed value; consensus takes the **median** across validators, so a minority of wrong or malicious validators cannot move it.
4. The agreed value is written to the `oracle.prices` system table, with a timestamp, the number of sources and the spread between them.

**Update rule.** A feed updates when the price moves more than its deviation threshold (for example 0.1%) or when its heartbeat interval passes, whichever comes first. Busy feeds can update every block.

**Using it, Convex-style.** Feeds are just tables, so contracts read them and frontends subscribe to them:

```rust
#[btb::query]
pub fn btc_price() -> Result<Price> {
    oracle::price("BTC/USD") // latest agreed value, with timestamp and spread
}

#[btb::mutation]
pub fn liquidate(position: u64) -> Result<()> {
    let p = oracle::price("ETH/USD")?;
    ensure!(p.age_blocks() <= 5, "price too old");
    // ... liquidation logic
    Ok(())
}
```

```tsx
const btc = useQuery(api.oracle.btcPrice); // live, updates every time the feed does
```

No polling, no third-party subscription, no extra fee per read: queries are served by hosting nodes like any other query.

**Protections.**

- **Many sources, median of medians.** Each validator takes the median across sources; consensus takes the median across validators.
- **Staleness checks.** Every value carries its age; the SDK makes contracts state how old a price they accept.
- **Circuit breakers.** If sources disagree beyond a limit, or a value jumps more than a set percentage in one update, the feed pauses and flags the event instead of publishing a suspicious price.
- **TWAP helpers.** Time-weighted average prices are computed from feed history in the table, for apps that want manipulation-resistant prices.

## 3. Custom web data (HTTPS requests)

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
| Randomness | Mutation's gas (small extra cost) | Validators |
| Built-in price feeds | Apps that read them in mutations, through a per-read fee; queries stay free-feeling | Validators that fetch and report |
| App-defined feeds | The app that created the feed, from its prepaid balance | Validators |
| Custom HTTPS requests | The calling contract's gas | Validators |
| Reports for other chains | The requesting chain's app or relayer | Validators and hosting nodes |

## Risks and limits

| Risk | Mitigation |
| --- | --- |
| Validators collude to fake a price | Same trust as the chain itself: requires two-thirds collusion. Median aggregation tolerates a minority of bad reports. |
| Sources are manipulated (thin markets, a single exchange) | Many independent sources, volume-weighted medians, circuit breakers, TWAPs |
| Stale data during network issues | Every value carries its age; contracts must declare a maximum age |
| One feed's failure halts apps | Feeds pause safely; apps can define fallbacks (another feed or a TWAP) |
| Data licensing and API terms | Built-in feeds use sources whose terms allow this use; app-defined feeds are the app's responsibility |
| Validator load from many feeds and requests | Feeds and requests are paid per use; gas caps per block for oracle work |

## Rollout

Oracles are part of **Phase 5** in the [roadmap](../ROADMAP.md):

1. On-chain randomness (threshold BLS beacon)
2. Built-in price feeds for major assets
3. Custom HTTPS requests with transforms
4. Signed reports for other chains

See [Prior art](PRIOR_ART.md#chainlink-and-randomness-beacons) for how Chainlink, the Internet Computer, Aptos and drand approach the same problems.
