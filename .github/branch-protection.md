# Branch Protection Rules

Recommended settings for an open project: anyone can contribute through pull requests, while `main` stays safe.

## Main branch: `main`

- [x] Require a pull request before merging
  - [x] Require approvals: **1**
  - [x] Dismiss stale pull request approvals when new commits are pushed
- [x] Require status checks to pass before merging
  - Documentation checks
  - CI workflow (once Rust code lands in Phase 1)
- [x] Require conversation resolution before merging
- [x] Allow force pushes: disabled
- [x] Allow deletions: disabled
- [ ] Require signed commits: optional (encouraged, but not required, so newcomers aren't blocked)

## Repository settings that keep contributing easy

- **Issues:** enabled, blank issues allowed
- **Discussions:** enabled, for questions and open-ended ideas
- **Labels:** create `good first issue`, `help wanted`, `design-proposal`, `phase-1` to `phase-6`
- **Forking:** allowed (default for public repositories)
- **Actions on fork PRs:** require approval for first-time contributors (default)

## How to apply

1. Go to Repository Settings → Branches → Add rule for `main`
2. Configure as above and save
3. Settings → General → Features: enable Issues and Discussions
