# 0004: Ship upgrades every day without breaking anyone, and without keeping old code alive

Status: **Draft**. Issue: to be opened by a maintainer as a Design Proposal. Touches upgrade rules (consensus) and the CLI; needs the Safety section and review before code.

## Summary

Make upgrading a contract (and the chain itself) something a team can do **every day**, including with AI-written changes, because every upgrade is checked against the contract's real interface and real recent traffic before it goes live, rolls back by itself if it starts failing, and leaves **no old version behind** once it has proven itself. Do the same for the protocol: nodes start from validator-signed snapshots, so old rules can be deleted from the code instead of being carried forever.

**Recommendation:** build the interface check (A) and the dry run (B) first, they catch most breakage and change little; then automatic rollback (C) and atomic app upgrades (D); then protocol rule retirement (E) together with snapshots (proposal 0003, phase D).

## Why

### The cost of never breaking anything

Systems that promise never to break old behaviour end up carrying every old behaviour forever.

- **Microsoft Windows** keeps decades of compatibility code so that old programs still run. Every new feature has to work around old ones, and the old code is attack surface that nobody wants to touch.
- **Ethereum** contracts cannot change at all. Teams work around it with *proxy* contracts, a second contract that forwards calls to a replaceable implementation. Proxies add complexity, storage-layout traps and an admin key, and mistakes in them have caused large losses.
- **Ethereum clients** must still contain the rules of every past network upgrade, because a new node re-executes every block since 2015 and each block must be judged by the rules of its time.

The common cause is the same: **old things must keep working because someone might still use them, and nobody can prove they don't.**

### How the web ships every day

Modern web services deploy many times a day without breaking users. They do not keep every old version; they make each change safe to roll out and cheap to undo:

1. **Know the contract with clients**, and refuse changes that break it (API schemas, typed clients).
2. **Test against real traffic** before going live (shadow traffic, replaying recorded requests).
3. **Roll out, watch error rates, roll back automatically** when they jump (canary releases).
4. **Ship client and server together** where possible, so they never disagree.
5. **Delete the old version** once the new one has proven itself.

BTB can do all five on chain, because the chain sees every call, every result and every client (the site is hosted on chain too).

### Why this matters more with AI

AI makes writing changes cheap, so the limit becomes how safely changes can be shipped. A human or an AI can both forget that some other contract calls a function they renamed. The chain should catch that mechanically, every time, so shipping daily is safe regardless of who wrote the code.

### What BTB already has

`Upgrade` (tag 23, [CHAIN.md](../CHAIN.md#contract-upgrades-and-migrations)) replaces a contract's code at the same address, keeps its data, and checks **data** compatibility from the two ELFs: new tables and appended optional fields pass, breaking layout changes need a declared `#[btb::migration]`, migrations run on chain and a failed migration rolls back. The upgrade authority is the deployer, a named account, or nobody (`--immutable`). What is missing is everything about the **interface** and about **behaviour under real traffic**.

## Design

### A. Interface compatibility check (consensus)

The ELF already lists every entry point with its kind, argument types and return type (`target/btb/<name>.schema.json`, the input of `btb codegen`). At `Upgrade`, the chain diffs old and new:

| Change | Result |
| --- | --- |
| New function, new optional trailing argument | Compatible |
| Function removed, renamed, kind changed, argument or return type changed | **Breaking** |

A breaking upgrade is refused unless the transaction says `breaking: true`. The CLI prints the exact list before sending (`btb upgrade` shows "removes `transfer_from`, changes `balance_of` return u64 → u128"). This is a consent gate, not a ban: owners can still break things on purpose, but never by accident.

The chain also counts, per function, how often it was called (directly or from other contracts) in the last `W` blocks (cheap counters updated on call). The breaking report shows "`transfer_from` was called 1,240 times by 3 contracts in the last day", so the owner knows who would break.

### B. Dry run against real traffic (CLI and hosting nodes, no consensus change)

`btb upgrade --dry-run` takes the last `N` transactions that touched the contract (from a hosting node or the RPC history), executes them on a copy of the head state with the old code and with the new code, and reports every call whose result, revert status, events or gas changed beyond a threshold. Deterministic execution makes this exact: the same input gives the same output on every machine. It runs off chain, so it costs nothing on chain and can run in CI for every AI-written change.

### C. Automatic rollback window (consensus)

After an upgrade the previous code stays stored for a **rollback window** (default about one day of blocks). During it the chain tracks the share of calls to the contract that revert or trap, compared with the same measure before the upgrade. If it rises above a threshold for `K` consecutive blocks (both chosen by the owner within protocol limits), the chain reinstalls the previous code automatically, exactly like a failed migration already does. The owner can also roll back by hand. When the window ends, **the previous version is deleted**. Only one previous version is ever kept.

Data written by the new code during the window must stay readable by the old code: allowed because of rule A (only compatible data changes run without a migration); upgrades with migrations roll back through the existing undo log, whose window is the same.

### D. Atomic app upgrades (consensus)

One `Upgrade` may carry the contract code, its site files and the generated typed client, applied in the same block. The hosted frontend therefore never calls functions the contract no longer has, and never misses new ones. (Site files today are separate `Site` transactions.)

### E. Retiring old protocol rules (consensus, with proposal 0003 phase D)

Network upgrades are activated at an agreed height, and the protocol version is part of the state. After each upgrade, once a snapshot past the activation height is certified by the validators (proposal 0003, phase D), new nodes start from that snapshot instead of replaying older blocks. The code for the old rules can then be **deleted from the node** in the next release. Archive nodes that want to re-execute old history keep using the release that still has those rules; the current release never carries them.

### Who can upgrade

Unchanged: the contract's upgrade authority (deployer, a named account such as a multisig or a governance contract, or nobody). With key-slot accounts (proposal 0002) the authority's admin slot signs upgrades and a spending slot cannot. Options for users to have a say (opt-out windows, voting) are open question 3.

## Safety

- **Interface check (A)** reads only data already in the ELF; both sides are deterministic, so every validator decides the same way. Failure cases to test: removed, renamed, retyped functions; changed kind; a breaking upgrade without and with consent.
- **Dry run (B)** is advisory and off chain; it cannot change state.
- **Rollback (C)** must be deterministic: the error-rate measure counts receipts in blocks, not wall-clock time; the window and threshold are bounded by protocol constants so a contract cannot keep unlimited old code; a rollback is itself recorded as an event. An owner cannot use rollback to undo *other* state (balances moved by users stay moved); only code is reverted, plus migrations through the existing undo log.
- **Atomic upgrades (D)** keep the existing size limits per transaction.
- **Retiring rules (E)** only after the snapshot that follows the activation height is certified by a quorum; a node that must replay older blocks refuses with a clear message naming the release to use.
- Gas: the interface diff and the call counters have fixed costs per function and per call, defined before the phase ships.

## Tests

- A: unit tests of the interface diff over pairs of example contracts; chain tests that a breaking upgrade is refused without consent and applied with it; counters match a hand count.
- B: the dry run reports no differences for an identical ELF and the exact changed calls for a contract whose function was altered.
- C: an upgrade that makes most calls trap is rolled back within `K` blocks; a healthy upgrade is not; the previous code is gone after the window; the supply invariant holds throughout.
- D: contract, site and client change in the same block or not at all.
- E: a node synced from a post-activation snapshot runs without the old rules and reaches the same state root as an archive node.

## Open questions

1. Default rollback window and error-rate threshold, and the protocol bounds on them.
2. Should a breaking upgrade also require a minimum notice period when other contracts depend on the removed function?
3. Should users have a say: for example a short window in which they can withdraw from a contract before a breaking upgrade applies, or a governance vote for contracts that hold others' funds?
4. Where the dry run gets its traffic: the node's own history, a hosting node, or an indexer of the contract's calls.
