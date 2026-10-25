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
  │  Hosting + edge nodes (thousands, │◄────►│  Storage nodes   │
  │  incl. phones): queries, subs,    │      │  erasure-coded   │
  │  websites, cached files, proofs   │      │  file pieces     │
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

### Storage nodes and edge nodes

- **Storage nodes** hold erasure-coded pieces of large files (a 100 GB file becomes 100 pieces of ~3 GB, any 34 rebuild it), prove they still hold them through random challenges, and earn per period.
- **Edge nodes**, including laptops and phones, cache popular content near users, relay requests and hold small pieces, earning per request.

See [SCALING.md](SCALING.md) for how every layer scales and how each role earns.

## Consensus: blocks inside, requests outside

Validators are run by parties who don't trust each other, so they must agree on one order of transactions to prevent double spends. Batching that agreement into blocks makes it cheap. Users and developers never see blocks:

- **Fast path.** Transactions touching only one user's own data (sending your own tokens) need validator signatures but no global ordering, confirming in about half a second.
- **Streaming consensus.** A pipelined BFT or DAG protocol proposes batches continuously; shared objects like DEX pools confirm in under a second. The consensus core is a Jolteon-style 2-chain HotStuff state machine in `crates/btb-consensus`; its safety rules, leader schedule and evidence records are in [CONSENSUS.md](CONSENSUS.md).
- **Parallel execution.** Transactions that touch different data run at the same time (Block-STM style): built in `btb-chain` as speculative execution with a multi-version memory, validation and re-execution, identical to sequential execution by construction and by differential tests ([CHAIN.md](CHAIN.md#parallel-execution)). Transactions whose accounts are statically known and closed skip conflict tracking. This is an execution-level path inside an ordered block; the consensus-level fast path above (no global ordering) is not built yet.
- **Request/response SDK.** `await dex.swap(...)` returns the result; batching, confirmation and retries are hidden.

## Execution layer

The deterministic part a consensus engine drives is `crates/btb-chain`: signed transactions (Deploy, Call to a mutation or cron, Transfer), a genesis that allocates the fixed supply, blocks executed in order with per-transaction gas, fees accounted into a validator pool (80%) and a relay pool (20%), cron jobs run by block height, and a Merkle state root after every block. Queries never enter it. See [CHAIN.md](CHAIN.md).

## State model

BTB state is a database, not a raw key-value store. Tables, indexes and everything else live in one ordered keyspace with a fixed key layout; see [STATE.md](STATE.md).

| Feature | Design |
| --- | --- |
| Tables and indexes | Contracts declare tables with `@primary` and `@index` fields. The chain maintains indexes as consensus state. The writer pays gas for index updates; indexes per table are capped and priced. |
| Native tokens | A chain feature, not separate contracts. Creation costs a fee plus a storage deposit. Holders, supplies and creators are queryable. |
| Queries | Read-only, answered by any hosting node without consensus. Optional Merkle proof against the latest block; apps choose fast or verified. |
| Subscriptions | An app subscribes to a query; the node pushes new results whenever a block changes the data it read. |
| Mutations | Change state, go through consensus, pay gas. Optimistic execution with retry on conflict. |
| Cron / scheduled functions | Scheduled by block height (`btb-chain`: every N blocks) or block timestamp, never a local clock. Paid from the contract's prepaid balance; jobs pause when it is empty. A per-block scheduled-gas cap prevents pile-ups. |
| Hosting | Frontend files stored by contracts, served over HTTP by hosting nodes. |
| Blob storage | Large files split across many nodes with erasure coding; only the hash is on-chain. |
| Storage deposits | BTB locked per byte stored, refunded on delete. Prevents state bloat. |
| Oracles | Randomness from a threshold-BLS beacon each block; price feeds written to a system table by median of validator reports; HTTPS actions agreed by validators through transform functions. See [ORACLES.md](ORACLES.md). |

## Front-running protection

Block producers choose transaction order and earn fees, so they could front-run large swaps. The native DEX module uses **batch auctions**: all swaps on a pool in one block settle at the same price, so ordering gives no advantage.

## Tech stack

| Layer | Choice | Why |
| --- | --- | --- |
| Contract languages | Rust first (SDK with attribute macros); any language that compiles to RISC-V | Familiar tools, like NEAR and ICP |
| Compile target | RISC-V, 64-bit integer-only | Simple, near-native, ZK-friendly, deterministic |
| VM | RISC-V recompiler with gas metering (reference interpreter: `crates/btb-vm`) | Near-native speed with exact metering (see [VM_DECISION.md](VM_DECISION.md)) |
| Node software | Rust | Safe, fast, strongest blockchain ecosystem |
| Consensus | Proof-of-stake BFT (HotStuff family or DAG) | ~1 s finality, no forks |
| Execution | Parallel, Block-STM style | Non-conflicting transactions run at once |
| Storage | RocksDB + Jellyfish Merkle tree | Fast storage with provable state |
| Networking | libp2p over QUIC | Battle-tested peer-to-peer |
| Cryptography | Ed25519, BLAKE3, BLS (validator votes), threshold ECDSA/Schnorr/EdDSA (cross-chain) | Fast and widely trusted |
| App access | WebSocket + HTTP gateway on hosting nodes | Queries, subscriptions, hosted sites |

## Prior art

BTB combines ideas that have each been proven separately. [PRIOR_ART.md](PRIOR_ART.md) explains each project in detail: how it works, what BTB takes from it, and what BTB does differently.

| Project | What BTB takes |
| --- | --- |
| Convex | Developer experience, reactive queries, query/mutation split, crons |
| Internet Computer | On-chain hosting, free queries, app-pays fees, timers, threshold signatures, many languages on one VM |
| NEAR | Rust-first SDK with attribute macros, storage staking, chain signatures |
| Sui / Aptos | Owned-object fast path, Block-STM parallel execution, money-safe types |
| Nervos CKB / PolkaVM | RISC-V as the contract target, recompilation to native code |
| THORChain | Validator-controlled vaults for native cross-chain assets |
