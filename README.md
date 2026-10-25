# BTB Chain

**The Convex of blockchain.** BTB Chain is a decentralized cloud where a developer writes one project — token, backend logic, database and frontend — runs one deploy command, and gets a live app with a URL. No RPC providers, no indexers, no off-chain servers, no separate web hosting.

[![Website](https://img.shields.io/badge/Website-btb.finance-blue)](https://www.btb.finance)
[![Twitter Follow](https://img.shields.io/twitter/follow/btb_finance?style=social)](https://twitter.com/btb_finance)

> **Status: design phase.** This repository holds the BTB Chain specification and roadmap. Phase 1 (language + VM) is the next milestone. The earlier Arbitrum Orbit L3 design has been retired; it remains available at the `orbit-legacy` tag.

## Why BTB Chain

On most chains, the chain cannot answer basic questions like "who holds this NFT?" or "who created this pool?". Every app needs RPC providers, indexers, cron servers and web hosting bolted on. On BTB Chain, **the chain is the whole backend**:

- **Database-native state** — contracts declare tables and indexes; "all NFTs owned by X" is a plain query.
- **Queries and live subscriptions** — reads are instant and served without consensus; UIs update the moment data changes.
- **Cron jobs and scheduled functions** — contracts schedule their own work, no off-chain keepers.
- **On-chain hosting** — frontends are served by the network itself.
- **Native tokens** — creating a token is one built-in call; every wallet, explorer and DEX sees it automatically.
- **Cross-chain by signature** — BTB contracts can read and write Ethereum, Bitcoin, Solana and more without deploying contracts there.

## At a glance

| | |
| --- | --- |
| Native token | **BTB**, fixed supply of **1,000,000,000**, no minting after genesis |
| Fees | Every request pays the node that does the work (hosting node or block executor) |
| Contract language | BTB's own Rust-like language, compiled to RISC-V |
| VM | RISC-V recompiler with deterministic gas metering |
| Node software | Rust |
| Consensus | Proof-of-stake BFT, ~1 s finality, fast path for owned data |
| Execution | Parallel (Block-STM style) |
| Network | Core validators (consensus) + hosting nodes (queries, subscriptions, websites) |

## Developer experience (target)

```bash
btb init my-dex
btb dev        # local chain with hot reload
btb deploy     # contracts + frontend go live
# → https://my-dex.btb.app
```

```rust
contract Dex {
    table Pool {
        id: u64 @primary,
        token_a: TokenId @index,
        token_b: TokenId @index,
        reserve_a: u128,
        reserve_b: u128,
        creator: Address @index,
    }

    query fn price(pool: u64) -> Decimal {
        let p = Pool.get(pool)?;
        p.reserve_b / p.reserve_a
    }

    mutation fn swap(pool: u64, amount_in: u128, min_out: u128) {
        let p = Pool.get(pool)?;
        let out = amm_out(amount_in, p.reserve_a, p.reserve_b);
        require(out >= min_out, "slippage too high")?;
        // move tokens, update reserves
    }

    @cron(every = 1 hour)
    mutation fn record_price_history() { /* for charts */ }
}
```

```js
const price = useQuery(dex.price, { pool: 7 }); // live, no polling
const swap  = useMutation(dex.swap);
```

## Roadmap

| Phase | Focus | Gate to move on |
| --- | --- | --- |
| 1 | Language + RISC-V VM (local, no chain) | Sample contracts compile and run with exact, repeatable gas |
| 2 | State database: tables, indexes, queries, subscriptions | Query by index and watch a UI update live |
| 3 | Single-node devnet: `btb` CLI, TypeScript SDK, dashboard | An outside developer ships a working app and it feels like Convex |
| 4 | Multi-validator testnet: consensus, networking, parallel execution | Testnet keeps finalizing when validators fail |
| 5 | Platform: cron, hosting nodes, payment channels, native tokens, DEX, cross-chain | A full DEX runs end to end on testnet, frontend included |
| 6 | Mainnet: audits, fixed 1B BTB genesis, operator onboarding | — |

Full details: [ROADMAP.md](ROADMAP.md).

## Documentation

- [Architecture](docs/ARCHITECTURE.md) — state model, network, consensus, hosting
- [Language](docs/LANGUAGE.md) — the BTB contract language and RISC-V target
- [Tokenomics](docs/TOKENOMICS.md) — fixed supply, fees, payment channels
- [Cross-chain](docs/CROSS_CHAIN.md) — chain signatures and native-asset vaults
- [Roadmap](ROADMAP.md)

## Contributing

BTB Chain is at the design stage, so design feedback is the most valuable contribution right now. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Security

Please report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).
