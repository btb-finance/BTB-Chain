# BTB Chain Roadmap

BTB Chain ships in six phases. The first three prove the idea on a single machine before any distributed-systems work; Phase 3 is the decisive gate. If building an app on one node feels as good as Convex, BTB can attract the developers and contributors needed for the harder network phases.

Each phase ends with a **gate**: a concrete test that must pass before the next phase starts.

---

## Phase 1 — RISC-V VM and Rust SDK

A RISC-V VM that runs Rust contracts locally, with gas metering. No blockchain yet. Rust compiles to RISC-V with the standard compiler, so no custom language compiler is needed to start.

**Deliverables**

- [ ] RISC-V rv64im interpreter (recompiler later) with per-instruction gas metering
- [ ] ELF loader and contract entry-point convention (one export per query / mutation / cron)
- [ ] Host-call ABI for storage, tables, tokens and context (`caller()`, block height), with a gas cost per call
- [ ] `btb-sdk` Rust crate (no_std) with `#[btb::table]`, `#[query]`, `#[mutation]`, `#[cron]` macros
- [ ] Argument and result encoding shared by all languages
- [ ] Example Rust contracts: counter, token, simple DEX
- [ ] C header for the host-call ABI, proving "any language"
- [ ] Decide: build on an existing RISC-V VM (PolkaVM, CKB-VM) or write our own

**Gate:** sample Rust contracts compile and run with exact, repeatable gas on every machine.

## Phase 2 — State database

Tables, indexes, queries and live subscriptions on one machine.

**Deliverables**

- [ ] Table and index storage on RocksDB
- [ ] Jellyfish Merkle tree state commitment (provable state)
- [ ] Query engine: lookups by primary key and index, filters, pagination
- [ ] Read-set tracking and subscription invalidation (re-run queries when their data changes)
- [ ] Optimistic transactions for mutations (retry on conflict)
- [ ] Storage deposits per byte, refunded on delete

**Gate:** query a table by index and watch a UI update live.

## Phase 3 — Single-node devnet

The full developer experience on one node, so developers can build real apps.

**Deliverables**

- [ ] `btb` CLI: `init`, `dev` (hot reload), `deploy`
- [ ] TypeScript SDK with React hooks (`useQuery`, `useMutation`)
- [ ] Dashboard to browse contract tables like a database
- [ ] Language server (autocomplete, errors, hover types)
- [ ] Native token module (`Token.create`, transfers, holder queries)
- [ ] Example apps: token launcher, simple DEX, NFT market

**Gate:** an outside developer ships a working app and it feels like Convex.

## Phase 4 — Multi-validator testnet

Consensus and networking across independent validators.

**Deliverables**

- [ ] Proof-of-stake BFT consensus (HotStuff family or DAG), ~1 s finality
- [ ] libp2p networking over QUIC
- [ ] Fast path for transactions touching only one owner's data
- [ ] Parallel execution (Block-STM style)
- [ ] Validator staking, rotation and slashing
- [ ] Public testnet with faucet and explorer

**Gate:** testnet keeps finalizing when validators fail or misbehave.

## Phase 5 — Platform features

Everything that makes BTB a decentralized cloud.

**Deliverables**

- [ ] Cron jobs and scheduled functions (prepaid balances, per-block scheduled-gas cap)
- [ ] Hosting nodes: queries, subscriptions, websites, Merkle proofs on answers
- [ ] Payment channels and signed vouchers for per-request fees
- [ ] Fee split for mutations (executor / relay host)
- [ ] Storage nodes ([design](docs/SCALING.md)): erasure-coded file pieces, random piece assignment, storage challenges, automatic repair
- [ ] Edge caching for popular content
- [ ] Native DEX module with batch auctions (front-running protection)
- [ ] Oracles ([design](docs/ORACLES.md)): threshold-BLS randomness beacon, built-in price feeds, HTTPS actions with transforms, signed reports for other chains
- [ ] Cross-chain: threshold signatures and native-asset vaults (Ethereum, Bitcoin, Solana)

**Gate:** a full DEX runs end to end on testnet, frontend included.

## Phase 6 — Mainnet

**Deliverables**

- [ ] Formal specification of the language, VM and consensus
- [ ] Fuzzing and multiple independent security audits
- [ ] Final genesis allocation of the fixed 1,000,000,000 BTB supply
- [ ] Validator and hosting-node operator onboarding (region and provider diversity)
- [ ] Mainnet launch

**After mainnet:** lightweight node app for phones and home devices; state sharding when demand requires it.

---

## Open questions

- [ ] Whether to build an optional BTB DSL later, and its name
- [ ] TypeScript support: compile to RISC-V or run an interpreter inside the VM
- [ ] Fast-path design: owned objects (Sui-style) or a simpler rule
- [ ] Mutation fee split between executor and relay host (80/20 proposed)
- [ ] Genesis reserve for early validator rewards (10–20% proposed) and the rest of the allocation
- [ ] Validator count and minimum stake
- [ ] Reuse of Apache-2.0-converted Convex backend code (subject to license review)
