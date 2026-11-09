# 0005: Economics that pay node operators, and rules that stop validators from capturing the chain

Status: **Draft; two rules built** (voting-power cap, gas-price ceiling; see below). Issue: to be opened by a maintainer as a Design Proposal. Touches tokenomics and consensus: needs the Safety section and review.

## Summary

Two questions decided together, because each one shapes the other:

1. **Is running a node worth it?** With BTB at an assumed $1 (a $1B market cap), today's placeholder fee (a transfer costs 0.000002 BTB) pays validators almost nothing. A gas price around 500 (a transfer costs about $0.001) pays a 50-validator set from about 10 transactions per second, and a small reserve (1 to 3% of supply, not 10 to 20%) covers the early years.
2. **Can validators rig it?** On chains where the validators, or a few delegates elected by token holders, vote on fees and parameters (Tron's 27 super representatives, for example), those who collect the fees also set them. BTB must make that impossible by construction: validators never set fees, nobody can hold more than a small share of voting power, fees have hard limits in code, and rules change only through public, slow software upgrades.

**Built in this change:** a per-validator voting-power cap of 5% (protocol constant) and a protocol maximum gas price. **Recommended next:** an algorithmic gas price whose congestion part does not go to the validators who produce the block, a decision on delegation, and the reserve size.

## 1. Does running a node pay?

Assumptions: BTB = $1. A validator costs about $30 a month to run (it is a simple PC: about 130 to 170 MB of memory and about 10% of a core at 2,600 transfers per second in our tests). Each validator stakes 100,000 BTB (the testnet default) and expects about 5% a year on it.

| Validators | Running cost | 5% on stake | Needed per year |
| --- | --- | --- | --- |
| 21 | $7,560 | $105,000 | about $113,000 |
| 50 | $18,000 | $250,000 | about $268,000 |
| 100 | $36,000 | $500,000 | about $536,000 |

The hardware is cheap; the real cost is the return on the locked stake.

| Usage | Fees per year at today's 0.000002 BTB per transfer | Fee per transaction needed for 21 validators |
| --- | --- | --- |
| 1 tx/s | $63 | $0.0036 |
| 10 tx/s | $631 | $0.00036 |
| 100 tx/s | $6,307 | $0.000036 |
| 1,000 tx/s | $63,072 | $0.000004 |

| Gas price | Transfer (2,000 gas) | DEX order (30,000 gas) |
| --- | --- | --- |
| 1 (today) | $0.000002 | $0.00003 |
| **500 (suggested)** | **$0.001** | **$0.015** |
| 5,000 | $0.01 | $0.15 |
| 10,000 (new protocol maximum) | $0.02 | $0.30 |

At a gas price of 500, 10 tx/s brings about $315,000 a year (enough for 50 validators) and 100 tx/s about $3.2 million.

**Reserve.** [TOKENOMICS.md](../TOKENOMICS.md#early-security-budget) proposes 10 to 20% of the supply for early validator rewards. 15% is 150 million BTB, which at the budget above would pay 21 validators for about 1,300 years and 100 validators for about 280. **1 to 3%**, paid out over about five years and decreasing as fees grow, is enough; the rest can fund development, storage and hosting incentives, or the community.

Hosting, edge and storage nodes are paid separately (per request and per stored and proven byte), so their income does not depend on validator fees. Their prices are part of the next revision of this proposal.

Sensitivity: at $0.10 per BTB every dollar figure above is ten times smaller. A gas price that targets a dollar cost needs a price input, and the chain's price oracle is reported by the validators, so it must not be the only input (open question 2).

## 2. Rules against capture

### Why

When the parties who collect fees can also vote to set them, fees drift toward what is best for them. On Tron a small set of super representatives, elected through delegated votes, can vote on network parameters including resource prices. Delegated systems also concentrate: people delegate to the largest, best-known operators, which then hold enough votes to decide alone. BTB should not rely on validators behaving well; the rules should make capture impossible or useless.

### Rule 1: validators never set fees (to build)

The gas price is either fixed in the genesis (today) or computed by a formula in the protocol (proposed): a base price that rises when blocks are fuller than a target and falls when they are emptier, by at most 12.5% per block, as in Ethereum's base fee. **No transaction, vote or parameter lets validators change it.** Changing the formula is a software upgrade that every operator chooses to install.

### Rule 2: validators gain nothing by raising fees (to build)

Ethereum burns its base fee so producers cannot profit from congestion. BTB cannot burn: the supply is fixed at exactly 1,000,000,000 and the chain checks that sum every block. So the congestion part of the fee must go somewhere a block producer does not control and cannot profit from at once, for example spread over many future epochs to all stakers, or to hosting and storage nodes. The current validators keep a small fixed tip and the fees of work they actually do. Where exactly it goes is open question 1.

### Rule 3: hard limits in code (built: ceiling)

- **Built:** `MAX_GAS_PRICE = 10,000` in `btb-chain` (`state.rs`). A genesis above it is refused, so no chain can launch with punitive fees, and raising it needs a software upgrade. A transfer can then cost at most 0.02 BTB.
- To build with Rule 1: the per-block change limit (12.5%).

### Rule 4: nobody holds more than 5% of the votes (built)

- **Built:** `MAX_VOTING_POWER_BPS = 500` in `btb-chain` (`staking.rs`, `cap_voting_power`). When a validator set is fixed for an epoch (and at genesis), each validator's weight is capped at 5% of the set's total by water filling: stake above the cap simply does not count. The cap applies to consensus votes, to rewards (paid by weight, so growing past the cap earns nothing), to oracle weights and to the vault's security bound (the conservative direction: the vault may hold less, never more). Slashing and unbonding use the real bond.
- With the cap, a 2/3 quorum needs **at least 14 validators**. Tron's super representatives reach their 2/3 with 18 of 27; Ethereum and Solana have no per-validator cap.
- A set of 20 validators or fewer cannot satisfy a 5% cap at all, so it is left uncapped (local testnets, tests). **A public network should launch with at least 21 validators** for the cap to apply.
- The cap is a protocol constant: no vote or parameter changes it.

### Rule 5: delegation that cannot become control (to decide)

Two options:

- **No delegation.** Validators stake their own BTB; the minimum stake is set low (the 100,000 BTB testnet default is a placeholder) so many small operators can join with a simple PC. Simple, nothing to capture. Holders who do not run a node earn nothing from staking.
- **Capped delegation.** Holders may delegate, but delegated stake counts toward the same 5% cap, so a pool gains no power past it, and rewards per delegated BTB fall once a validator is near the cap, so delegators are pushed toward smaller validators (Cosmos and Polkadot use similar nudges). More holders take part; more code to audit.

Either way, delegation must never give a voting right over fees or rules (Rules 1 and 6).

### Rule 6: rules change slowly and in public (process)

Every change to fees, caps or economics needs a design proposal, a public notice period (for example 30 days) and a software release that operators install themselves. There is no on-chain parameter vote that takes effect the next day. Users and apps get time to leave if they disagree.

## What BTB already has

Fees go to whoever does the work; the supply is fixed and never minted; consensus is stake-weighted with slashing for double signing; nodes are cheap to run, so a large validator set is affordable; parameters live in the genesis and no transaction changes them.

## Safety

- **Voting-power cap:** consensus-critical. Deterministic integer arithmetic (u128, floor division), never raises a weight, never touches bonded stake. Tests: a whale is capped and small validators keep their stake; no change when nobody is too big; several whales; a 2/3 quorum needs at least 14 validators; sets too small for the cap are unchanged; a genesis with a whale stores a capped epoch set while the whale's bond stays whole. Behaviour of sets of 20 or fewer is unchanged, so existing networks with 4 validators see no difference.
- **Gas-price ceiling:** checked at genesis; tests refuse `MAX_GAS_PRICE + 1` and accept `MAX_GAS_PRICE`.
- **Algorithmic fee (to build):** must use block contents only (no clock, no floats); its destination must keep the supply invariant; producers must not be able to raise their own income by stuffing blocks.

## Open questions

1. Where the congestion part of the fee goes (spread over future epochs to all stakers, to hosting and storage nodes, a mix).
2. Whether the gas price should target a dollar cost, and with which price input (the validators' own oracle should not be the only one).
3. Delegation: none with a low minimum stake, or capped delegation.
4. Reserve size (1 to 3% proposed), schedule and who holds it (a locked protocol account, not a team key).
5. Minimum stake and the launch validator count (at least 21 for the cap to apply).
