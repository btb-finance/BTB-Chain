# Contributing to BTB Chain

**BTB Chain is built in the open, by everyone.** There is no closed core team and no permission needed to start. Humans and AI-assisted contributors work side by side here: if you have an idea, a question, a fix, or a few hours to spare, you belong in this project.

## Where we are

BTB Chain is in its **design phase**, heading into **Phase 1: language and VM** (see [ROADMAP.md](ROADMAP.md)). That means the most valuable contributions right now are:

- Questioning and improving the design
- Researching how other chains solve the same problems
- Turning roadmap items into clear, buildable tasks
- Prototyping Phase 1 pieces (parser, type checker, RISC-V VM, gas metering)

You don't need to be a blockchain expert. Fresh eyes catch the most important mistakes.

## Ways to contribute

| You like… | You can… |
| --- | --- |
| Ideas and debate | Comment on issues, challenge a design choice, open a [design proposal](https://github.com/btb-finance/btb-chain/issues/new/choose) |
| Writing | Improve the docs, fix unclear explanations, write tutorials and examples |
| Research | Compare approaches (consensus, VMs, fee models) and write up findings in an issue |
| Rust / compilers | Build the BTB language frontend, RISC-V VM, gas metering |
| Databases | Design tables, indexes, queries and subscriptions (Phase 2) |
| TypeScript / frontend | SDK, React hooks, dashboard, example apps (Phase 3) |
| Security | Review designs for attacks before they become code |
| Testing | Write test cases, fuzz targets, and break things on purpose |

## Your first contribution in 5 steps

1. **Pick something.** Browse [open issues](https://github.com/btb-finance/btb-chain/issues), especially ones labeled `good first issue` or `help wanted`, or pick an unchecked item in [ROADMAP.md](ROADMAP.md).
2. **Say hi in the issue.** Comment "I'd like to work on this" so others know. No need to wait for approval; if nobody objects within a day, go ahead.
3. **Fork and branch.** Fork the repo, then create a branch from `main` (for example `docs/fix-tokenomics-typo` or `vm/gas-metering`).
4. **Make your change.** Keep pull requests small and focused; one idea per PR is easiest to review.
5. **Open a pull request.** Fill in the template. A maintainer will review it, usually within a few days.

Small fixes (typos, broken links, clearer wording) can skip straight to step 3.

## Bigger changes: design proposals

For changes to the architecture, language, tokenomics or roadmap:

1. Open a **Design Proposal** issue describing the problem, your proposal, alternatives and impact.
2. Discuss it in the open. Anyone can comment and every voice counts.
3. Once there's rough agreement, open a PR that updates the relevant file in `docs/` or `ROADMAP.md`.

Decisions are made by discussion and consensus in public. When consensus can't be reached, maintainers make the call and explain why in the issue.

## Building with AI

AI tools (Claude, Copilot and others) are welcome collaborators here; parts of this repository were written with Claude. Use them however helps you, with three rules:

1. **You own what you submit.** Read, understand and test everything before opening a PR, whether you or an AI wrote it.
2. **Say so in your PR.** Tick the AI-assistance box in the PR template. It's not a penalty; it helps reviewers know where to look carefully.
3. **Security-critical code gets extra review.** Changes to the VM, consensus, cryptography, tokenomics or cross-chain code need careful human review regardless of who wrote them.

## Code standards (from Phase 1)

- Rust code: `cargo fmt` and `cargo clippy` clean, with tests for new behavior
- Determinism first: no floats, system time or randomness in anything that affects consensus
- Docs updated in the same PR when behavior or interfaces change

## Getting recognized

Every merged contribution counts, whether it's code, docs, research or design. Contributors are listed on the repository's contributors page, and significant contributions are credited in the [CHANGELOG](CHANGELOG.md).

## Reporting issues

Found a problem? [Open an issue](https://github.com/btb-finance/btb-chain/issues/new/choose). Questions are welcome too; there's no such thing as a dumb question at this stage.

Security vulnerabilities must **not** be reported in public issues. See [SECURITY.md](SECURITY.md).

## Code of Conduct

Be kind, assume good intent, and critique ideas rather than people. Read the full [Code of Conduct](CODE_OF_CONDUCT.md).

## License

BTB Chain is open source under the [MIT License](LICENSE). By contributing, you agree your contributions are licensed under the same terms.
