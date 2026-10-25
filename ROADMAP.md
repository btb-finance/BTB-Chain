# BTB Chain Roadmap

BTB Chain ships in six phases. The first three prove the idea on a single machine before any distributed-systems work; Phase 3 is the decisive gate. If building an app on one node feels as good as Convex, BTB can attract the developers and contributors needed for the harder network phases.

Each phase ends with a **gate**: a concrete test that must pass before the next phase starts.

---

## Phase 1 — RISC-V VM and Rust SDK

A RISC-V VM that runs Rust contracts locally, with gas metering. No blockchain yet. Rust compiles to RISC-V with the standard compiler, so no custom language compiler is needed to start.

**Deliverables**

- [x] RISC-V rv64im interpreter (recompiler later) with per-instruction gas metering
- [x] ELF loader and contract entry-point convention (one export per query / mutation / cron / action)
- [x] Host-call ABI for storage, tables, tokens and context (`caller()`, block height), with a gas cost per call
- [x] `btb-sdk` Rust crate (no_std) with `#[btb::table]`, `#[query]`, `#[mutation]`, `#[cron]` macros
- [x] Argument and result encoding shared by all languages
- [x] Example Rust contracts: counter, token, simple DEX
- [x] C header for the host-call ABI, proving "any language"
- [x] Decide: build on an existing RISC-V VM (PolkaVM, CKB-VM) or write our own: keep our own interpreter for now, see [docs/VM_DECISION.md](docs/VM_DECISION.md)

**Gate:** sample Rust contracts compile and run with exact, repeatable gas on every machine.

## Phase 2 — State database

Tables, indexes, queries and live subscriptions on one machine.

**Deliverables**

- [x] Table and index storage on RocksDB
  - Tables, indexes and key layout in `crates/btb-runtime` over any `KvStore`; RocksDB backend behind the `rocksdb` feature, tested to give results identical to `MemKv` (tables, index queries, deposits, `examples/counter`) and to persist across reopen; `btb-dev --features rocksdb --data DIR` keeps todos across restarts
- [x] Jellyfish Merkle tree state commitment (provable state)
- [x] Query engine: lookups by primary key and index, filters, pagination
  - Contracts reach it through host call 15 (`table_index_scan`: index range, order, limit, cursor); row filters are in the Rust API only
- [x] Read-set tracking and subscription invalidation (re-run queries when their data changes)
- [x] Optimistic transactions for mutations (retry on conflict)
- [x] Storage deposits per byte, refunded on delete

**Gate:** query a table by index and watch a UI update live. **Met.**

Gate demo: `examples/todo` (reads its rows through an index) served by `btb-dev` with a live page; run by `crates/btb-tests/tests/dev_gate.rs` (SSE stream gets a new event after `add` and `toggle`) and a Playwright smoke check. Phase 2 is complete; contract-declared indexes are still temporary host-side registrations (see [docs/DEV_SERVER.md](docs/DEV_SERVER.md)).

## Phase 3 — Single-node devnet

The full developer experience on one node, so developers can build real apps.

**Deliverables**

- [x] `btb` CLI: `init`, `dev` (hot reload), `deploy`
  - `crates/btb-cli`, see [docs/CLI.md](docs/CLI.md); `deploy` writes a local bundle, and with `--rpc URL --key FILE` sends it to a node as a signed `Deploy` transaction; also `call`, `query`, `keygen`
- [x] TypeScript SDK with React hooks (`useQuery`, `useMutation`)
- [x] Dashboard to browse contract tables like a database
  - `GET /dashboard` and `/admin/...` in `crates/btb-dev`, see [docs/DEV_SERVER.md](docs/DEV_SERVER.md#dashboard-and-admin-endpoints)
- [x] Language server (autocomplete, errors, hover types)
  - rust-analyzer config in `btb init` templates plus `crates/btb-lsp` (BTB diagnostics, hover, completion), see [docs/LSP.md](docs/LSP.md)
- [x] Native token module (`Token.create`, transfers, holder queries)
  - host calls 16 to 23, `btb_sdk::token::Token`, [examples/launcher](examples/launcher), see [docs/TOKENOMICS.md](docs/TOKENOMICS.md#user-tokens)
- [x] Example apps: token launcher, simple DEX, NFT market
  - [examples/launcher](examples/launcher), [examples/dex](examples/dex), [examples/nft](examples/nft) (escrow-based market), each with a live web page served by `btb dev` and a Playwright smoke script, see [examples/README-web.md](examples/README-web.md)

**Gate:** an outside developer ships a working app and it feels like Convex.

## Phase 4 — Multi-validator testnet

Consensus and networking across independent validators.

**Deliverables**

- [x] Proof-of-stake BFT consensus (HotStuff family or DAG), ~1 s finality
  - consensus core done (simulated): `crates/btb-consensus`, Jolteon-style 2-chain with timeout certificates, stake-weighted quorums and equivocation evidence, see [docs/CONSENSUS.md](docs/CONSENSUS.md); running on real networking in `crates/btb-node`
  - the execution layer it will drive (transactions, genesis, block execution, fees, crons, mempool) is `crates/btb-chain`, see [docs/CHAIN.md](docs/CHAIN.md); not a tick, consensus itself is not done
  - measured on real QUIC links between 4 in-process validators (`cargo test -p btb-node --test cluster -- --nocapture`): submit to final median ~50 ms on localhost, median ~0.7 s (max ~0.8 s) with 100 ms one-way delay injected per message; ~5 one-way delays in the best case, so ~1 s holds up to about 200 ms one-way. Not yet measured: separate machines over the internet, more than 4 validators
- [x] libp2p networking over QUIC
  - `crates/btb-node`: rust-libp2p QUIC (TLS 1.3 authenticates the peer id, which is the validator key), per-peer rate limits and bans, bounded frames, see [docs/NODE.md](docs/NODE.md)
- [ ] Fast path for transactions touching only one owner's data
  - done at the execution level only: transactions whose account footprint is statically closed (transfer-only components) run without conflict tracking, tested against the sequential executor, see [CHAIN.md](docs/CHAIN.md#owner-only-fast-path-execution-level); the consensus-level fast path (confirmation without global ordering) is not built, so no tick
  - design: [Design Proposal 0001](docs/proposals/0001-consensus-fast-path.md) (Draft): lane inside the BFT first, then certified transfers
- [x] Parallel execution (Block-STM style)
  - `crates/btb-chain/src/parallel.rs`: multi-version memory, speculative execution, validation, re-execution, deterministic commit; byte-identical to the sequential executor on random blocks for 1, 2, 4 and 8 workers (`crates/btb-chain/tests/parallel.rs`); crons and end-of-block logic stay sequential, see [CHAIN.md](docs/CHAIN.md#parallel-execution); about 2.5x on 4 cores for conflict-free transfers, no gain on fully conflicting blocks
- [x] Validator staking, rotation and slashing
  - `Stake`/`Unstake`/`RegisterValidator`/`SubmitEvidence` transactions, epochs with the top-N set fixed at each epoch end, unbonding, pro-rata rewards and double-sign slashing in `crates/btb-chain` ([CHAIN.md](docs/CHAIN.md#staking-epochs-and-slashing), [TOKENOMICS.md](docs/TOKENOMICS.md#validator-staking-and-slashing)); epoch changes in `btb-consensus` with the safety argument in [CONSENSUS.md](docs/CONSENSUS.md#epochs-and-validator-set-changes)
- [ ] Public testnet with faucet and explorer
  - faucet + explorer built; public deployment pending. `crates/btb-faucet` (rate limits, optional proof of work, nonce management), the `/explorer` page and read endpoints in `crates/btb-node`, `scripts/testnet-local.sh` and a Playwright smoke run against a local 4-validator net; no public network exists yet, see [docs/TESTNET.md](docs/TESTNET.md)

**Gate:** testnet keeps finalizing when validators fail or misbehave.

## Phase 5 — Platform features

Everything that makes BTB a decentralized cloud.

**Deliverables**

- [x] Cron jobs and scheduled functions (prepaid balances, per-block scheduled-gas cap)
  - `#[btb::cron(every = "N blocks")]` flows into the deploy bundle `crons`; the chain runs it from the contract balance, pauses when empty and defers fairly under the cap ([CHAIN.md](docs/CHAIN.md#cron-jobs)); `btb deploy --rpc` builds the `Deploy` transaction from the bundle (manifest with crons and indexes) and prints the contract address
- [x] Hosting nodes: queries, subscriptions, websites, Merkle proofs on answers ([design](docs/HOSTING.md); `crates/btb-host`, no fees or storage-node files yet)
- [x] Payment channels and signed vouchers for per-request fees
  - `TxKind::Channel` (open, top-up, payee close, payer timeout close with a challenge period) in `btb-chain`, locked funds in the supply invariant, parallel-equivalence tested; `btb-voucher` crate and `sdk-ts/src/voucher.ts` with a cross-language fixture ([CHAIN.md](docs/CHAIN.md#payment-channels)); hosting nodes that sell requests against vouchers come with the hosting-node item
- [ ] Fee split for mutations (executor / relay host)
  - the 80/20 split is accounted in `btb-chain`, but the relay share only accumulates in `relay_pool()`: paying it needs a signed `relayer` field and a hosting-node registry, see the design note in [CHAIN.md](docs/CHAIN.md#fees)
- [ ] Storage nodes ([design](docs/SCALING.md)): erasure-coded file pieces, random piece assignment, storage challenges, automatic repair
- [ ] Edge caching for popular content
- [x] Native DEX module with batch auctions (front-running protection)
  - `TxKind::Dex` in `btb-chain` (BTB and user-token pairs, escrowed limit orders, one uniform clearing price per pair every `dex_batch_blocks`, pro-rata fills, fee to the validator pool), tested against a brute-force reference, for supply conservation, reordering and parallel equivalence ([CHAIN.md](docs/CHAIN.md#native-dex)); page at `GET /dex` of every node ([examples/native-dex](examples/native-dex)); constant-product liquidity in the batch is not done
- [ ] Oracles ([design](docs/ORACLES.md)): threshold-BLS randomness beacon, built-in price feeds, HTTPS actions with transforms, signed reports for other chains
- [ ] Cross-chain: threshold signatures and native-asset vaults (Ethereum, Bitcoin, Solana)

**Gate:** a full DEX runs end to end on testnet, frontend included.

Shown locally so far (not yet on a public testnet, so the gate stays open): on `scripts/testnet-local.sh` (4 validators), `node examples/native-dex/web/smoke.js` deploys the launcher contract, creates two user tokens, and drives three browser tabs through the page served at `GET /dex`: signed limit orders are placed from the browser, a resting ask shows up live in another tab's order book, a crossing bid clears in one batch at one price (shown as the last clearing price, with the fee and the exact token amounts checked over RPC), a cancel refunds the escrow, a token/token pair trades, and the token supplies are conserved across holders, vault and fee account. The Rust side (`cargo test -p btb-chain --test dex`) covers the clearing price against a brute-force reference, uniform price, pro-rata rounding, conservation of BTB and every token, cancel and expiry refunds, hostile inputs, order-independence inside a batch and parallel/sequential equivalence.

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
