# AGENTS.md

Guidance for AI coding agents (and humans) working on BTB Chain. Read this first.

## What this project is

BTB Chain is a Layer 1 blockchain designed as **"the Convex of blockchain"**: a decentralized cloud where an app's database, backend functions, scheduled jobs and frontend all live on-chain. Contracts run on a metered **RISC-V VM**; **Rust** is the first-class contract language, and any language that compiles to RISC-V can be used.

The project is in its **design phase**, heading into **Phase 1 (VM + Rust SDK)**. Most of the repository today is specification.

## Where things are

| Path | Contents |
| --- | --- |
| `IDEA.md` | The problem BTB solves and why (the page to share) |
| `README.md` | Overview and quick summary |
| `ROADMAP.md` | Six phases, deliverables as checkboxes, gates between phases |
| `docs/ARCHITECTURE.md` | Network, consensus, state model, tech stack |
| `docs/DEV_SERVER.md` | `btb-dev`: single-node dev server (HTTP, live queries over SSE), the Phase 2 gate demo |
| `docs/CLI.md` | The `btb` command: `init`, `build`, `dev` (hot reload), `deploy` |
| `docs/CONSENSUS.md` | `btb-consensus`: Jolteon-style 2-chain BFT, safety rules (voting, locking, commit), leader schedule, equivocation evidence, simulator |
| `docs/STATE.md` | Key layout, index encodings, table writes and queries in `btb-runtime` |
| `docs/LSP.md` | rust-analyzer setup and `btb-lsp` (BTB-specific diagnostics, hover, completion) |
| `docs/CHAIN.md` | Transactions, signing, fees, genesis, blocks, crons and the mempool in `btb-chain` |
| `docs/LANGUAGE.md` | Contract languages, Rust SDK, project layout |
| `docs/ENCODING.md` | Canonical binary encoding of arguments and results (all languages) |
| `docs/TS_SDK.md` | TypeScript client (`sdk-ts/`, `@btb/client`): codec, client, React hooks |
| `docs/TOKENOMICS.md` | Fixed 1B BTB supply, fees, payment channels |
| `docs/CROSS_CHAIN.md` | Chain signatures and vaults |
| `docs/ORACLES.md` | Randomness, price feeds, HTTPS actions, reports for other chains |
| `docs/SCALING.md` | How each layer scales; node roles and how each earns; storage proofs |
| `docs/GLOSSARY.md` | Alphabetical definitions of key terms used across the docs |
| `docs/PRIOR_ART.md` | What BTB takes from Convex, ICP, NEAR, Sui/Aptos, CKB/PolkaVM, THORChain, Chainlink |
| `docs/VM_DECISION.md` | Build on PolkaVM / CKB-VM or keep our own `btb-vm`: comparison and recommendation |
| `CONTRIBUTING.md` | How to contribute, including AI-assisted work |

Planned code layout (from Phase 1):

```
crates/
├── btb-vm/        # RISC-V rv64im interpreter, gas metering, host-call ABI
├── btb-codec/     # canonical argument/result encoding (no_std)
├── btb-sdk/       # Rust contract SDK (no_std) and macros
├── btb-runtime/   # kv store, tables, indexes, queries, subscriptions, scheduler
├── btb-consensus/ # deterministic, I/O-free BFT state machine (HotStuff family) + simulator
├── btb-chain/     # transactions, genesis, block execution, fees, crons, mempool
├── btb-dev/       # single-node dev server (HTTP + SSE), Phase 2 demo
├── btb-node/      # consensus, networking, hosting gateway
├── btb-cli/       # `btb init`, `btb dev`, `btb deploy`
└── btb-lsp/       # language server: BTB diagnostics, hover, completion
```

## Core concepts to keep consistent

- **Four function kinds:** `query` (read-only, no consensus, subscribable), `mutation` (changes state, consensus, gas), `cron` (scheduled mutation), `action` (outside HTTPS call whose result validators agree on). Never let a query write state or use randomness.
- **Randomness:** only from the chain's threshold beacon, only in mutations and crons marked `randomness`, never callable by other contracts.
- **Determinism:** nothing that affects consensus may use floats, system time, thread scheduling or unseeded randomness. The VM implements only integer RISC-V (rv64im).
- **Gas:** every instruction and every host call costs gas. New host calls must define their gas cost.
- **Fixed supply:** 1,000,000,000 BTB, never minted after genesis. No design may introduce inflation.
- **Fees go to whoever does the work:** hosting and edge nodes for queries and serving, storage nodes for proven storage, validators for mutations, crons, oracles and signing.
- **Large files never go on every validator:** they are erasure-coded into pieces on storage nodes; only hashes and assignments are on-chain.
- **Blocks inside, requests outside:** developers and users see request/response APIs; consensus details stay internal.

## Working rules

- Keep changes small and focused; one idea per pull request.
- When behavior or interfaces change, update the matching file in `docs/` in the same PR.
- Design changes go through a Design Proposal issue before code.
- Rust code: `cargo fmt`, `cargo clippy -- -D warnings` and `cargo test` must pass.
- Security-sensitive areas (VM, consensus, cryptography, tokenomics, cross-chain) need tests for failure cases and an explicit note on security impact in the PR.
- Do not add dependencies with non-permissive licenses. Do not copy Convex backend code unless it has converted to Apache 2.0 (see `docs/PRIOR_ART.md`).
- Commit and PR messages should be clear and in English; disclose AI assistance in the PR template.
