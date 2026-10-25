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

One command deploys everything:

```bash
btb deploy
# → https://my-dex.btb.app
```

### Familiar tools

Write contracts in **Rust**, or any language that compiles to RISC-V. Apps follow a Convex-style layout that developers and AI agents can understand instantly. ([Languages](docs/LANGUAGE.md))

### Fair, simple token

**BTB has a fixed supply of 1,000,000,000.** No minting, ever. Every request pays the node that does the work. ([Tokenomics](docs/TOKENOMICS.md))

## Build it with us

BTB Chain is open source and built in the open. Read the [roadmap](ROADMAP.md), then [jump in](CONTRIBUTING.md).

---

## Tweet ideas

**Single tweet**

> A new chain launches every day.
>
> Build your DEX on EVM → miss Solana users. Port it → still miss Aptos & Sui. Bridge it → risk a hack. And you pay $1000s/month for RPC, indexing & hosting.
>
> We're building BTB Chain to fix all of it. 🧵

**Thread**

1/ A new chain launches every day. Users are everywhere, and your app can only be in a few places at once. We're building BTB Chain to fix that. 🧵

2/ Build a DEX on EVM chains and you miss Solana users. Port it to Solana and you still miss Aptos and Sui. Every chain = new language, new audit, new SDK. Your app can't be universal.

3/ Bridges were supposed to fix this. Instead they've been behind some of the biggest hacks in crypto. Every bridge is one more thing that can lose user funds.

4/ And even on one chain you pay $1000s a month for RPCs, indexers, backend servers and hosting, just because the chain can't answer "who holds this token?"

5/ BTB Chain is the Convex of blockchain. Database, backend, cron jobs and frontend all live on-chain. No RPC bills, no indexer, no servers, no AWS.

6/ BTB apps can hold native assets and send transactions on Ethereum, Solana, Bitcoin and more, without deploying contracts there. Build once, reach every chain.

7/ Write contracts in Rust (or any language that compiles to RISC-V). Fixed supply: 1B $BTB, no minting ever. Every request pays the node that serves it.

8/ It's open source and built in the open. Read the idea, the roadmap, and come build with us: github.com/btb-finance/BTB-Chain

**Short version**

> Every chain wants your app. No team can build on all of them.
>
> BTB Chain: build once, reach EVM, Solana, Sui, Aptos & Bitcoin users. Database, backend & hosting on-chain. Zero RPC bills.
>
> Open source. Come build with us 👇
> github.com/btb-finance/BTB-Chain
