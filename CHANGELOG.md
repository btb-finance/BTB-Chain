# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
