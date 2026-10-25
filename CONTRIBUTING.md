# Contributing to BTB Chain

BTB Chain is in its design phase, and input at this stage shapes everything that follows. Contributions are welcome, whether it's:

- Feedback on the design docs and roadmap
- Proposing changes to the architecture, language or tokenomics
- Reporting problems or inconsistencies
- Code for Phase 1 (language and VM) once development starts
- Becoming a maintainer

## Design proposals

Larger changes start as a **design proposal** issue: describe the problem, the proposed change, alternatives considered and the impact on security, performance and developer experience. Accepted proposals are merged into the docs under `docs/`.

## Development process

We use GitHub to host code, track issues and feature requests, and accept pull requests.

1. Fork the repo and create your branch from `main`
2. If you've added code that should be tested, add tests
3. If you've changed behavior or interfaces, update the documentation
4. Ensure the test suite passes and your code is formatted (`cargo fmt`, `cargo clippy` once Rust code lands)
5. Open the pull request

## Reporting issues

Use the [issue tracker](https://github.com/btb-finance/btb-chain/issues). Good reports include a summary, steps to reproduce, what you expected and what actually happened.

Security vulnerabilities must **not** be reported in public issues — see [SECURITY.md](SECURITY.md).

## License

Contributions are licensed under the MIT License, the same license that covers the project.
