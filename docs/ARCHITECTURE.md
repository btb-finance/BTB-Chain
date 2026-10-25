# BTB Chain Architecture

BTB Chain is a decentralized cloud: core validators keep the chain in agreement, hosting nodes serve the traffic, and contracts behave like a backend with a real database.

## Core principles

1. **The chain is the backend.** No RPC providers, indexers or outside servers are needed to build a complete app.
2. **Blocks inside, requests outside.** Validators batch transactions to agree on order; developers and users only see fast requests and live queries.
3. **Whoever does the work gets paid.** Every request (query, page load, mutation, cron run) pays the node that served or executed it.
4. **Fixed supply.** 1,000,000,000 BTB at genesis, no minting ever.
5. **Safety in the language.** Determinism, gas metering and money-safety rules are enforced by the compiler.
6. **Speed from architecture.** Performance comes from parallel execution, fast consensus and a purpose-built state database.

## Network

```
            Users and apps
   (wallets, browsers, TypeScript SDK)
                  │  requests + signed vouchers
                  ▼
  ┌───────────────────────────────────┐      ┌──────────────────┐
  │  Hosting nodes  (thousands)       │◄────►│  Blob storage    │
  │  queries, subscriptions,          │      │  erasure-coded   │
  │  websites, files, proofs          │      │  large files     │
  └───────────────────────────────────┘      └──────────────────┘
        │ relay mutations      ▲ state sync + proofs
        ▼                      │
  ┌───────────────────────────────────┐
  │  Core validators  (~50–150)       │
  │  consensus, mutations, cron,      │
  │  full state, cross-chain signing  │
  └───────────────────────────────────┘
```

### Core validators

- Run consensus, execute mutations and cron jobs, keep full state.
- Stake BTB and run serious hardware, typically in data centers.
- Must be spread across regions and providers to stay decentralized.
- Jointly hold threshold keys for cross-chain signing (see [CROSS_CHAIN.md](CROSS_CHAIN.md)).

### Hosting nodes

- Serve queries, live subscriptions, websites and files, like a CDN.
- Sync state from validators and attach Merkle proofs so answers can be verified.
- Relay mutations to validators and earn a share of their gas.
- Anyone can run one; BTB scales traffic by adding hosting nodes without slowing consensus.

## Consensus: blocks inside, requests outside

Validators are run by parties who don't trust each other, so they must agree on one order of transactions to prevent double spends. Batching that agreement into blocks makes it cheap. Users and developers never see blocks:

- **Fast path.** Transactions touching only one user's own data (sending your own tokens) need validator signatures but no global ordering, confirming in about half a second.
- **Streaming consensus.** A pipelined BFT or DAG protocol proposes batches continuously; shared objects like DEX pools confirm in under a second.
- **Parallel execution.** Transactions that touch different data run at the same time (Block-STM style). Table declarations tell the scheduler what each transaction touches.
- **Request/response SDK.** `await dex.swap(...)` returns the result; batching, confirmation and retries are hidden.

## State model

BTB state is a database, not a raw key-value store.

| Feature | Design |
| --- | --- |
| Tables and indexes | Contracts declare tables with `@primary` and `@index` fields. The chain maintains indexes as consensus state. The writer pays gas for index updates; indexes per table are capped and priced. |
| Native tokens | A chain feature, not separate contracts. Creation costs a fee plus a storage deposit. Holders, supplies and creators are queryable. |
| Queries | Read-only, answered by any hosting node without consensus. Optional Merkle proof against the latest block; apps choose fast or verified. |
| Subscriptions | An app subscribes to a query; the node pushes new results whenever a block changes the data it read. |
| Mutations | Change state, go through consensus, pay gas. Optimistic execution with retry on conflict. |
| Cron / scheduled functions | Scheduled by block height or block timestamp, never a local clock. Paid from the contract's prepaid balance; jobs pause when it is empty. A per-block scheduled-gas cap prevents pile-ups. |
| Hosting | Frontend files stored by contracts, served over HTTP by hosting nodes. |
| Blob storage | Large files split across many nodes with erasure coding; only the hash is on-chain. |
| Storage deposits | BTB locked per byte stored, refunded on delete. Prevents state bloat. |
| Outside data | Validator-agreed oracle mechanism keeps execution deterministic. |

## Front-running protection

Block producers choose transaction order and earn fees, so they could front-run large swaps. The native DEX module uses **batch auctions**: all swaps on a pool in one block settle at the same price, so ordering gives no advantage.

## Tech stack

| Layer | Choice | Why |
| --- | --- | --- |
| Contract language | Own Rust-like language | Tables, queries, mutations, cron and tokens as syntax |
| Compile target | RISC-V, 64-bit integer-only | Simple, near-native, ZK-friendly, deterministic |
| VM | RISC-V recompiler with gas metering | Near-native speed with exact metering |
| Node software | Rust | Safe, fast, strongest blockchain ecosystem |
| Consensus | Proof-of-stake BFT (HotStuff family or DAG) | ~1 s finality, no forks |
| Execution | Parallel, Block-STM style | Non-conflicting transactions run at once |
| Storage | RocksDB + Jellyfish Merkle tree | Fast storage with provable state |
| Networking | libp2p over QUIC | Battle-tested peer-to-peer |
| Cryptography | Ed25519, BLAKE3, BLS (validator votes), threshold ECDSA/Schnorr/EdDSA (cross-chain) | Fast and widely trusted |
| App access | WebSocket + HTTP gateway on hosting nodes | Queries, subscriptions, hosted sites |

## Prior art

- **Convex** (convex.dev) — reactive queries, transactions and developer experience. Its backend is source-available under FSL-1.1-Apache-2.0; each release converts to Apache 2.0 two years after publication. Any code reuse requires a license review.
- **Internet Computer** — on-chain hosting, query calls, timers, threshold signatures.
- **Sui / Aptos** — owned-object fast path, Block-STM parallel execution, Move's money-safety ideas.
- **NEAR** — sharding, storage staking, chain signatures.
- **Nervos CKB / PolkaVM** — RISC-V VMs in production.
- **THORChain** — native cross-chain swaps via vaults.
