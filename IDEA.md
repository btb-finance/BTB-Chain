# The Idea Behind BTB Chain

## The problem

**A new chain launches almost every day.** Users, liquidity and attention are scattered across dozens of ecosystems, and each one speaks a different language.

### 1. Your app can't reach everyone

Say you build a DEX.

- Deploy it on EVM chains, and you miss **Solana** users.
- Port it to Solana, and you still miss **Aptos** and **Sui** users.
- Each ecosystem means a different language (Solidity, Rust programs, Move), a different wallet, a different SDK and a different audit.

Building and maintaining the same app on every chain is slow, expensive and error-prone. So most apps pick one or two chains and give up on the rest. **Your app can't be universal.**

### 2. Cross-chain is risky

The usual fix is bridges and wrapped tokens. But bridges have been behind some of the largest hacks in crypto, with billions of dollars lost. Every bridge adds a new contract to trust, a new key to protect and a new way to lose user funds.

### 3. You pay thousands of dollars just to run an app

Even on one chain, the chain itself is only one piece of your app. You also pay for:

| What | Why you need it | Typical cost |
| --- | --- | --- |
| RPC providers | To read and send transactions reliably | Hundreds to thousands of $ per month |
| Indexers | Because the chain can't answer "who holds this token?" or "who created this pool?" | Hundreds to thousands of $ per month |
| Backend servers | For scheduled jobs, notifications, price updates | Monthly cloud bills |
| Web hosting | For the frontend | Monthly cloud bills, plus a central point of failure |

A "decentralized" app ends up depending on a stack of centralized services, and a small team spends thousands of dollars a month before it has a single user.

## We need to fix all of it

**BTB Chain is the Convex of blockchain: one place where your whole app lives, and from which it can reach every chain.**

### One app, every chain

BTB validators jointly hold threshold keys that control normal addresses on Ethereum, Solana, Bitcoin and other chains. Your BTB app can **hold native assets and send transactions on other chains without deploying contracts there**.

Build your DEX once on BTB, and users from EVM chains, Solana, Aptos, Sui and Bitcoin can all deposit real assets and trade. No wrapped tokens, no separate deployments. ([Cross-chain design](docs/CROSS_CHAIN.md))

### Safer cross-chain by design

No bridge contracts to hack. Vaults are controlled by the validator set, and vault value is capped below the stake validators would lose by cheating. ([Cross-chain risks](docs/CROSS_CHAIN.md#limits-and-risks))

### No RPC, no indexer, no servers, no hosting bills

On BTB, **the chain is the backend**:

- **Database built in.** Tables and indexes are part of the chain, so "who holds this token?" is a plain query. No indexer.
- **Free, instant queries and live updates.** Hosting nodes answer reads and push changes to your UI. No RPC subscription.
- **Cron jobs on-chain.** Contracts schedule their own work. No backend servers.
- **Frontends hosted on-chain.** Deploy your website with your contracts. No AWS.
- **Oracles built in.** Random numbers, live prices and web data come straight from BTB's validators, which already run servers and reach agreement. No separate oracle network and no oracle fees per request. ([Oracles](docs/ORACLES.md))

One command deploys everything:

```bash
btb deploy
# → https://my-dex.btb.app
```

### Familiar tools

Write contracts in **Rust**, or any language that compiles to RISC-V. Apps follow a Convex-style layout that developers and AI agents can understand instantly. ([Languages](docs/LANGUAGE.md))

### Anyone can host, anyone can earn

Every action on BTB costs a tiny amount of BTB that goes to whoever did the work: a bot reading a price pays the validators that reported it, a page view pays the node that served it, a stored file pays the nodes holding it.

Big files are never copied to every machine. A 100 GB NFT artwork is split into 100 small pieces stored on 100 different nodes, and any 34 pieces can rebuild it. So **anyone can host a small part of the network, even from a phone, and earn from it**. ([Scaling and earning](docs/SCALING.md))

### Fair, simple token

**BTB has a fixed supply of 1,000,000,000.** No minting, ever. Every request pays the node that does the work. ([Tokenomics](docs/TOKENOMICS.md))

## Build it with us

BTB Chain is open source and built in the open. Read the [roadmap](ROADMAP.md), then [jump in](CONTRIBUTING.md).
