# Design proposals

Design changes go through a Design Proposal before code (see [AGENTS.md](../../AGENTS.md) and [CONTRIBUTING.md](../../CONTRIBUTING.md)). A proposal is a short document in this folder that states a problem, compares options, recommends one and says how the result will be tested.

## Process

1. **Write it.** Copy the shape of [0001](0001-consensus-fast-path.md): status, problem, precise definitions, options compared, recommendation with phases, tests, affected files, open questions. Number proposals in order (`NNNN-short-name.md`). No code in a proposal; pseudo-structures are fine.
2. **Open a Design Proposal issue.** Every proposal is also opened as a *Design Proposal* issue on GitHub **by a maintainer**, linking the file. Discussion happens in the issue; the file is updated to match the outcome.
3. **Status.** The first line of each proposal carries one status:
   - **Draft**: under discussion, nothing may rely on it.
   - **Accepted**: a maintainer has agreed in the issue; implementation PRs may cite it. The PR that implements a phase updates the matching files in `docs/` (as always) and ticks the ROADMAP item only when the whole item is done.
4. **Security impact.** Proposals touching the VM, consensus, cryptography, tokenomics or cross-chain must have a "Safety" section and a list of failure-case tests (see AGENTS.md working rules).

## Index

| # | Title | Status |
| --- | --- | --- |
| [0001](0001-consensus-fast-path.md) | Consensus-level fast path for owner-only transactions | Draft |
