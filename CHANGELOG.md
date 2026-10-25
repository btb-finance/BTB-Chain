# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2.0.0] - 2026-10-25

### Changed
- BTB Chain is now an independent Layer 1: "the Convex of blockchain", a decentralized cloud with database-native state, queries and live subscriptions, cron jobs, on-chain hosting and native tokens
- New roadmap (ROADMAP.md): six phases from language + VM through mainnet
- Native token BTB with a fixed supply of 1,000,000,000 and no minting

### Added
- Design docs: architecture, contract language (Rust-like, compiled to RISC-V), tokenomics, cross-chain

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
