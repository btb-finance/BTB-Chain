# Branch Protection Rules

Configure the following branch protection rules in GitHub repository settings:

## Main Branch Protection

### Branch name pattern: `main`

- [x] Require a pull request before merging
  - [x] Require approvals (2 minimum)
  - [x] Dismiss stale pull request approvals when new commits are pushed
  - [x] Require review from Code Owners

- [x] Require status checks to pass before merging
  - [x] Require branches to be up to date before merging
  - Required status checks:
    - Documentation checks
    - CI workflow (once Rust code lands in Phase 1)

- [x] Require conversation resolution before merging

- [x] Require signed commits

- [x] Require linear history

- [x] Include administrators

### Additional Settings

- [x] Allow force pushes: Disabled
- [x] Allow deletions: Disabled

## Release Branch Protection

### Branch name pattern: `release/*`

- [x] Require a pull request before merging
  - [x] Require approvals (2 minimum)
  - [x] Require review from Code Owners

- [x] Require status checks to pass before merging
  - All checks from main branch

- [x] Require signed commits

## Development Branch Protection

### Branch name pattern: `develop`

- [x] Require a pull request before merging
  - [x] Require 1 approval
  - [x] Require review from Code Owners

- [x] Require status checks to pass before merging
  - Basic CI checks

# Implementation Steps

1. Go to Repository Settings
2. Navigate to Branches
3. Add rule for each branch pattern
4. Configure settings as specified above
5. Save changes
