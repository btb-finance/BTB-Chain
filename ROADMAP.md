# BTB Chain Roadmap

BTB Chain ships in six phases. The first three prove the idea on a single machine before any distributed-systems work; Phase 3 is the decisive gate. If building an app on one node feels as good as Convex, BTB can attract the developers and contributors needed for the harder network phases.

Each phase ends with a **gate**: a concrete test that must pass before the next phase starts.

---

## Phase 1 — Language and VM

A compiler and RISC-V VM that run BTB contracts locally, with gas metering. No blockchain yet.

**Deliverables**

- [ ] Language grammar and lexer/parser
- [ ] Type checker: structs, enums, `Result`/`Option`, generics, integer-only math
- [ ] BTB-specific rules: `contract`, `table`, `query fn`, `mutation fn`, `@cron`
- [ ] Lowering to LLVM or Cranelift IR, emitting RISC-V (rv64im, no floats)
- [ ] RISC-V VM (interpreter first, recompiler later) with per-instruction gas metering
- [ ] Host-call interface for storage, tokens and context (`caller()`, block height)
- [ ] Decide: build on an existing RISC-V VM (PolkaVM, CKB-VM) or write our own

**Gate:** sample contracts compile and run with exact, repeatable gas on every machine.

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
- [ ] Erasure-coded blob storage for large files
- [ ] Native DEX module with batch auctions (front-running protection)
- [ ] Validator-agreed oracle for outside data
- [ ] Cross-chain: threshold signatures and native-asset vaults (Ethereum, Bitcoin, Solana)

**Gate:** a full DEX runs end to end on testnet, frontend included.

## Phase 6 — Mainnet

**Deliverables**

- [ ] Formal specification of the language, VM and consensus
- [ ] Fuzzing and multiple independent security audits
- [ ] Final genesis allocation of the fixed 1,000,000,000 BTB supply
- [ ] Validator and hosting-node operator onboarding (region and provider diversity)
- [ ] Mainnet launch

---

## Open questions

- [ ] Name for the contract language
- [ ] Fast-path design: owned objects (Sui-style) or a simpler rule
- [ ] Mutation fee split between executor and relay host (80/20 proposed)
- [ ] Genesis reserve for early validator rewards (10–20% proposed) and the rest of the allocation
- [ ] Validator count and minimum stake
- [ ] Reuse of Apache-2.0-converted Convex backend code (subject to license review)
