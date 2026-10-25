# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- **Beacon key without a trusted dealer.** The validators now generate the threshold-BLS beacon key themselves, on chain: a joint-Feldman DKG for epoch 0 in the first `dkg_window` blocks (when the genesis has no `group_key`) and a resharing in the last `dkg_window` blocks of every epoch, as `Dkg` transactions (`TxKind::Dkg`, tag 14: `Deal` with Feldman commitments and per-recipient shares encrypted to the consensus keys with ChaCha20-Poly1305, `Complain` with a Chaum-Pedersen decryption proof that the chain decides by itself). Qualified set = dealers without an upheld complaint; the group key is stable across epochs; the beacon committee of an epoch is its validator set (the next set is locked at the window start); a failed generation keeps the previous key. Beacons are verified against the key of the block's epoch (`50 30 epoch`). `btb-node` deals, complains and stores shares automatically (`dkg_dir`, mode `0600`, optional `BTB_KEY_PASSPHRASE`) and erases old generations. `init-testnet --oracle true` no longer deals anything; `--insecure-dealer true` keeps the old trusted dealer for local testnets. New `dkg_window` in the oracle genesis (changes the genesis encoding). RPC `btb_oracleDkg`; `btb_oracleConfig` now reports the current epoch's key. Storage placement and challenges use the beacon seed (`50 02`) whenever the block has a beacon (block hash only as the documented fallback). Stake-weighted beacon shares remain future work (per-validator threshold, see ORACLES.md)
- Native DEX order amounts, limit prices and clearing prices are `u128` like token balances (were `u64`, 18.4 whole tokens at 18 decimals); each order amount is at most `MAX_ORDER_AMOUNT = 2^100` (`DexError::AmountTooLarge`). Wire format of `Place` (16 bytes per amount instead of 8), stored order and clearing records, the cancel receipt output (16 bytes) and the DEX page change; clearing uses 256-bit intermediates and a brute-force reference test covers amounts up to 2^100
- `examples/counter`'s cron is now `#[btb::cron(every = "10 blocks")]`, so it runs on chain

### Fixed
- Duplicate transaction inclusion after round timeouts in `btb-node`: a transaction already in an uncommitted ancestor of the block being proposed was proposed again once the time-based in-flight hint had expired; both copies committed, the second failed with `BadNonce` and its receipt replaced the real one. Proposers now exclude the transactions of the uncommitted ancestry exactly (`Replica::uncommitted_payloads`), and the block store never lets an `invalid` duplicate receipt replace the receipt of the real execution

### Added
- **HTTPS actions with transforms and signed reports for other chains** (docs/ORACLES.md sections 3 and 4, docs/CHAIN.md#https-actions-and-reports). New transaction `Action` (tag 13): `Request { contract, function, args, gas_limit, max_fee }` runs a contract's `#[btb::action]` first phase read-only, validates the returned `HttpRequest` (per-contract host allowlist set by the deployer with `SetHosts`, URL, header, body, size and time caps), escrows `max_fee` and queues a pending request. Every validator fetches it off-chain (`btb-node` `actions`: https only, public addresses only checked in the resolver at connect time, redirects re-checked against the allowlist, size and time caps; `ureq` behind the default `https` feature), runs the contract's pure `#[btb::transform("name")]` (`btb_transform_<name>`, no state access, gas-metered) and signs the result; votes ride in `Block::actions` / `Batch.actions`. When validators with at least two thirds of the stake signed identical bytes the chain calls `<name>_callback(id, args, status, data)` in the same block (`STATUS_OK`, `STATUS_FAILED`, `STATUS_DISAGREEMENT`, `STATUS_TIMEOUT` at a 50-block deadline), pays each validator that answered, refunds the rest; callbacks cannot be called by transactions. Host call 42 `report_request` queues up to 1 KiB for the beacon committee to threshold-sign (`btb_oracle::report`, message `"btb.report.v1" || chain_id || contract || id || SHA256(data)`); validators gossip partials, combine them and serve `btb_oracleReport`; Rust verifier `btb_oracle::report::verify` and a Solidity sketch (EIP-2537) in ORACLES.md. SDK: `btb_sdk::oracle::{action, report}`, `#[btb::transform]`, `btb_codec::http`; C: `btb_report_request`; VM: `Contract::call_transform`, `has_transform`, `is_action_callback`; RPC `btb_actionRequests`, `btb_oracleReport`; `examples/actions`. BLS reports only: the threshold-ECDSA variant needs the cross-chain key
- Oracles, first pieces (docs/ORACLES.md): `crates/btb-oracle` (BLS12-381 over `blst`: threshold keys, partial signatures, joint-Feldman DKG with complaints and justifications, resharing, trusted-dealer bootstrap, signed price observations, stake-weighted median); threshold-BLS randomness beacon per block height, released only after the block is committed, with execution waiting for it; host call 40 `random_bytes` for mutations and crons marked `#[btb::mutation(randomness)]` / `#[btb::cron(randomness, ...)]` (marker symbol `btb_randomness_<kind>_<name>`, trap elsewhere, full gas always charged, per-transaction derivation); host call 41 `price` and built-in price feeds (genesis pairs, validators' signed observations in the block payload, median over at least two thirds of the stake; free in queries, per-read fee in mutations); `btb_sdk::oracle`, `examples/oracle`, `btb_oracle*` RPC, `btb-node init-testnet --oracle true`. The beacon key is dealt by a trusted dealer at genesis for now; HTTPS actions and signed reports for other chains are not built. IDEA.md and ORACLES.md now agree on the price-read fee
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
