# AGENTS.md

Guidance for AI coding agents (and humans) working on BTB Chain. Read this first.

## What this project is

BTB Chain is a Layer 1 blockchain designed as **"the Convex of blockchain"**: a decentralized cloud where an app's database, backend functions, scheduled jobs and frontend all live on-chain. Contracts run on a metered **RISC-V VM**; **Rust** is the first-class contract language, and any language that compiles to RISC-V can be used.

The project is in its **design phase**, heading into **Phase 1 (VM + Rust SDK)**. Most of the repository today is specification.

## Where things are

| Path | Contents |
| --- | --- |
| `IDEA.md` | The problem BTB solves and why, plus tweet drafts |
| `README.md` | Overview and quick summary |
| `ROADMAP.md` | Six phases, deliverables as checkboxes, gates between phases |
| `docs/ARCHITECTURE.md` | Network, consensus, state model, tech stack |
| `docs/LANGUAGE.md` | Contract languages, Rust SDK, project layout |
| `docs/TOKENOMICS.md` | Fixed 1B BTB supply, fees, payment channels |
| `docs/CROSS_CHAIN.md` | Chain signatures and vaults |
| `docs/PRIOR_ART.md` | What BTB takes from Convex, ICP, NEAR, Sui/Aptos, CKB/PolkaVM, THORChain |
| `CONTRIBUTING.md` | How to contribute, including AI-assisted work |

Planned code layout (from Phase 1):

```
crates/
├── btb-vm/        # RISC-V rv64im interpreter, gas metering, host-call ABI
├── btb-sdk/       # Rust contract SDK (no_std) and macros
├── btb-runtime/   # tables, indexes, queries, subscriptions, scheduler
├── btb-node/      # consensus, networking, hosting gateway
└── btb-cli/       # `btb init`, `btb dev`, `btb deploy`
```

## Core concepts to keep consistent

- **Three function kinds:** `query` (read-only, no consensus, subscribable), `mutation` (changes state, consensus, gas), `cron` (scheduled mutation). Never let a query write state.
- **Determinism:** nothing that affects consensus may use floats, system time, thread scheduling or unseeded randomness. The VM implements only integer RISC-V (rv64im).
- **Gas:** every instruction and every host call costs gas. New host calls must define their gas cost.
- **Fixed supply:** 1,000,000,000 BTB, never minted after genesis. No design may introduce inflation.
- **Fees go to whoever does the work:** hosting nodes for queries and serving, executors for mutations and crons.
- **Blocks inside, requests outside:** developers and users see request/response APIs; consensus details stay internal.

## Working rules

- Keep changes small and focused; one idea per pull request.
- When behavior or interfaces change, update the matching file in `docs/` in the same PR.
- Design changes go through a Design Proposal issue before code.
- Rust code: `cargo fmt`, `cargo clippy -- -D warnings` and `cargo test` must pass.
- Security-sensitive areas (VM, consensus, cryptography, tokenomics, cross-chain) need tests for failure cases and an explicit note on security impact in the PR.
- Do not add dependencies with non-permissive licenses. Do not copy Convex backend code unless it has converted to Apache 2.0 (see `docs/PRIOR_ART.md`).
- Commit and PR messages should be clear and in English; disclose AI assistance in the PR template.
