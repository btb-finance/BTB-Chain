# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Native DEX (`crates/btb-chain/src/dex.rs`, docs/CHAIN.md#native-dex): limit-order batch auctions for BTB and user-token pairs. Orders are escrowed and cleared together every `dex_batch_blocks` at one exact rational price per pair (maximum volume, documented tie-breaks, pro-rata fills with supply-safe rounding, `dex_fee_bps` fee), so transaction order inside a batch cannot front-run. New transaction `Dex` (tag 10), genesis parameters `dex_batch_blocks` and `dex_fee_bps`, `btb_dex*` RPC methods and a DEX page at `GET /dex` on `btb-node` (`examples/native-dex`, Playwright smoke test)
- Execution attestation (docs/CONSENSUS.md, docs/NODE.md, docs/HOSTING.md): consensus blocks carry `exec`, a claim about the execution result of the newest block their ancestry commits, and honest validators vote only when it matches their own execution, so a quorum certificate certifies the post-execution state root. Proof bundles carry the attesting block, its QC and the roots (`attest`); `btb_host::proof::verify_bundle`/`verify_bundle_with` and `@btb/client` `verifyProof({ quorum })` check them against an epoch's validator set (`trustedRoot` still works). New `Replica::set_exec_hooks`/`resume`, `Block::exec` (block id domain `BTB/block/v2`), `btb_getBlock` fields `exec_digest`, `receipts_root`, `attests_height`; a node stops with `state divergence` if a certified claim contradicts its execution
- Hosting nodes (`crates/btb-host`, docs/HOSTING.md): a non-validator that follows the chain by verified block sync, executes every block, and serves queries and Server-Sent Events subscriptions in the shape of `btb-dev` (`/c/<contract>/query|subscribe/<fn>`), Merkle proofs of state keys with the committing header and consensus evidence (`/proof/...`, `?proof=1`; verifier in Rust `btb_host::proof` and `@btb/client` `verifyProof`), and static sites at `/site/<contract>/<path>`. New transaction `Site` (tag 9, deployer-only publish/remove of content-addressed files, 96 KiB and 256 files per contract) in `btb-chain`; `Executor::query_tracked`; `btb-node` can serve block sync to non-validator followers (opt-in `serve_followers`, sync requests only, per-follower rate limit and slot cap), exposes `Node::with_chain`/`start_with_hook` and `sync_poll_ms`
- Testnet tooling (not deployed): `crates/btb-faucet` (HTTP faucet with per-address and per-IP limits, optional proof of work, nonce retry), block explorer page at `/explorer` plus `btb_getBlocks`, `btb_getTransaction`, `btb_getAccount`, `btb_getAccountTxs` on `btb-node` (account index in the block store), `scripts/testnet-local.sh`, docs/TESTNET.md
- `crates/btb-consensus`: deterministic, I/O-free Jolteon-style (2-chain HotStuff) BFT state machine with timeout certificates, stake-weighted quorums, leader schedules, crash-safe storage hook, equivocation evidence and a seeded network simulator (crashes, partitions, Byzantine replicas); see docs/CONSENSUS.md
- `crates/btb-chain`: deterministic execution layer (Ed25519-signed Deploy/Call/Transfer transactions, genesis of the fixed supply with 9 decimals, block execution with gas, 80/20 fee pools, height-scheduled crons, supply invariant, mempool); see docs/CHAIN.md
- Host call 15 `table_index_scan` (read-only, 400 gas + 1/request byte + 40/id + 1/cursor byte): contracts query a secondary index by range with order, limit and cursor; SDK `Table::index_eq`, `include/btb.h` constant
- `crates/btb-dev`: single-node dev server (HTTP, Server-Sent Events for live queries) and `examples/todo` with a web page; automated Phase 2 gate test

## [2.0.0] - 2026-10-25

### Community
- Open to everyone: new contributing guide with a five-step first contribution, "Building with AI" rules, Code of Conduct and MIT LICENSE file
- Lighter review rules (one approval, signed commits optional), blank issues allowed, simplified PR template

### Changed
- BTB Chain is now an independent Layer 1: "the Convex of blockchain", a decentralized cloud with database-native state, queries and live subscriptions, cron jobs, on-chain hosting and native tokens
- New roadmap (ROADMAP.md): six phases from RISC-V VM + Rust SDK through mainnet
- Native token BTB with a fixed supply of 1,000,000,000 and no minting
- Contracts are Rust first, and any language that compiles to RISC-V is supported (like NEAR and ICP); a custom DSL is now optional and later

### Added
- Design docs: architecture, languages (Rust SDK, Convex-style project layout), tokenomics, cross-chain
- docs/PRIOR_ART.md: detailed comparison with Convex, ICP, NEAR, Sui/Aptos, Nervos CKB/PolkaVM and THORChain
- AGENTS.md: project conventions for AI agents and new contributors
- docs/ORACLES.md: native oracles (threshold-BLS randomness, price feeds as tables, HTTPS actions, signed reports for other chains); `action` added as a fourth function kind
- docs/SCALING.md: how each layer scales, five node roles and how each earns, erasure-coded storage with challenges, edge nodes on phones

### Removed
- Arbitrum Orbit / AnyTrust L3 system: chain and node configs, Orbit SDK deployment scripts, Hardhat ERC-20 contract, Next.js token UI and Orbit CI workflows (preserved on the `orbit-legacy` branch)

## [1.0.0] - 2025-01-29

### Added
- Initial release of BTB Orbit Chain
- AnyTrust L3 chain configuration
- Validator and sequencer setup
- Token bridge deployment scripts
- Complete CI/CD pipeline
- Development environment setup
- Documentation and contribution guidelines
