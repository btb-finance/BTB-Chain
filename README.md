# BTB Chain

**The Convex of blockchain.** BTB Chain is a decentralized cloud where a developer writes one project — token, backend logic, database and frontend — runs one deploy command, and gets a live app with a URL. No RPC providers, no indexers, no off-chain servers, no separate web hosting.

[![Website](https://img.shields.io/badge/Website-btb.finance-blue)](https://www.btb.finance)
[![Twitter Follow](https://img.shields.io/twitter/follow/btb_finance?style=social)](https://twitter.com/btb_finance)

**New here? Start with [IDEA.md](IDEA.md): why BTB Chain exists, in five minutes.**

> **Status: design phase.** This repository holds the BTB Chain specification and roadmap. Phase 1 (RISC-V VM + Rust SDK) is the next milestone. The earlier Arbitrum Orbit L3 design has been retired; it remains available on the `orbit-legacy` branch.

## Why BTB Chain

On most chains, the chain cannot answer basic questions like "who holds this NFT?" or "who created this pool?". Every app needs RPC providers, indexers, cron servers and web hosting bolted on. On BTB Chain, **the chain is the whole backend**:

- **Database-native state** — contracts declare tables and indexes; "all NFTs owned by X" is a plain query.
- **Queries and live subscriptions** — reads are instant and served without consensus; UIs update the moment data changes.
- **Cron jobs and scheduled functions** — contracts schedule their own work, no off-chain keepers.
- **On-chain hosting** — frontends are served by the network itself.
- **Native tokens** — creating a token is one built-in call; every wallet, explorer and DEX sees it automatically.
- **Cross-chain by signature** — BTB contracts can read and write Ethereum, Bitcoin, Solana and more without deploying contracts there.
- **Oracles built in** — verifiable randomness, live price feeds and web data come from the validators themselves; no separate oracle network needed.
- **Anyone can earn** — every action pays a tiny BTB fee to whoever did the work; big files are split across many nodes, so anyone can host a small part, even from a phone.

## At a glance

| | |
| --- | --- |
| Native token | **BTB**, fixed supply of **1,000,000,000**, no minting after genesis |
| Fees | Every request pays the node that does the work: validators, hosting, storage or edge nodes |
| Contract languages | **Rust first**, plus any language that compiles to RISC-V (C, C++, Zig, …), like NEAR and ICP |
| VM | RISC-V recompiler with deterministic gas metering |
| Node software | Rust |
| Consensus | Proof-of-stake BFT, ~1 s finality, fast path for owned data |
| Execution | Parallel (Block-STM style) |
| Network | Core validators (consensus) + hosting, storage and edge nodes that anyone can run |

## Developer experience (target)

```bash
btb init my-dex
btb dev        # local chain with hot reload
btb deploy     # contracts + frontend go live
# → https://my-dex.btb.app
```

A BTB app is laid out like a Convex app: backend functions in `btb/`, frontend in `web/`. Contracts are plain Rust with attribute macros, in the style of NEAR's SDK:

```rust
use btb_sdk::prelude::*;

#[btb::table]
pub struct Pool {
    #[primary] pub id: u64,
    #[index]   pub token_a: TokenId,
    #[index]   pub token_b: TokenId,
    pub reserve_a: u128,
    pub reserve_b: u128,
    #[index]   pub creator: Address,
}

#[btb::query]
pub fn price(pool: u64) -> Result<Ratio> {
    let p = Pool::get(pool)?;
    Ok(Ratio::new(p.reserve_b, p.reserve_a))
}

#[btb::mutation]
pub fn swap(pool: u64, amount_in: u128, min_out: u128) -> Result<u128> {
    let mut p = Pool::get(pool)?;
    let out = amm_out(amount_in, p.reserve_a, p.reserve_b);
    ensure!(out >= min_out, "slippage too high");
    // move tokens, update reserves, save
    Ok(out)
}

#[btb::cron(every = "1h")]
pub fn record_price_history() -> Result<()> { Ok(()) }
```

```tsx
const price = useQuery(api.pools.price, { pool: 7 }); // live, no polling
const swap  = useMutation(api.pools.swap);
```

The same four function kinds (query, mutation, cron, action) in every app make BTB projects easy for both humans and AI agents to understand. See [AGENTS.md](AGENTS.md).

## Roadmap

| Phase | Focus | Gate to move on |
| --- | --- | --- |
| 1 | RISC-V VM + Rust SDK (local, no chain) | Sample Rust contracts compile and run with exact, repeatable gas |
| 2 | State database: tables, indexes, queries, subscriptions | Query by index and watch a UI update live |
| 3 | Single-node devnet: `btb` CLI, TypeScript SDK, dashboard | An outside developer ships a working app and it feels like Convex |
| 4 | Multi-validator testnet: consensus, networking, parallel execution | Testnet keeps finalizing when validators fail |
| 5 | Platform: cron, hosting nodes, payment channels, native tokens, DEX, oracles, cross-chain | A full DEX runs end to end on testnet, frontend included |
| 6 | Mainnet: audits, fixed 1B BTB genesis, operator onboarding | — |

Full details: [ROADMAP.md](ROADMAP.md).

## Documentation

- [Architecture](docs/ARCHITECTURE.md) — state model, network, consensus, hosting
- [Languages](docs/LANGUAGE.md) — Rust SDK, any language via RISC-V, Convex-style project layout
- [Tokenomics](docs/TOKENOMICS.md) — fixed supply, fees, payment channels
- [Cross-chain](docs/CROSS_CHAIN.md) — chain signatures and native-asset vaults
- [Oracles](docs/ORACLES.md) — randomness, price feeds and web data built into the validator set
- [Scaling and earning](docs/SCALING.md) — how BTB scales and how anyone, even on a phone, earns by hosting a small part
- [Prior art](docs/PRIOR_ART.md) — what BTB learns from Convex, ICP, NEAR, Sui/Aptos, CKB/PolkaVM, THORChain and Chainlink, in detail
- [AGENTS.md](AGENTS.md) — conventions for AI agents and new contributors
- [Roadmap](ROADMAP.md)

## Build with us

**BTB Chain is built in the open, by everyone: developers, researchers, writers, and the AI tools they work with.** No invitation needed.

- **New here?** Read [CONTRIBUTING.md](CONTRIBUTING.md) and make your first contribution in five steps.
- **Have an idea or a doubt about the design?** Open a [design proposal](https://github.com/btb-finance/btb-chain/issues/new/choose); every voice counts.
- **Want to write code?** Phase 1 (language + RISC-V VM) is next; pick an unchecked item in the [roadmap](ROADMAP.md).
- **Using AI to help?** Welcome. See [Building with AI](CONTRIBUTING.md#building-with-ai).

Be kind and constructive; see the [Code of Conduct](CODE_OF_CONDUCT.md).

## License

Open source under the [MIT License](LICENSE).

## Security

Please report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).
