# 0001: Consensus-level fast path for owner-only transactions

Status: **Draft**. Issue: to be opened by a maintainer as a Design Proposal. Roadmap item: Phase 4, "Fast path for transactions touching only one owner's data" ([ROADMAP.md](../../ROADMAP.md)).

## Summary

Let a plain `Transfer` between ordinary accounts confirm after one round trip to a stake quorum, without waiting for a block to be ordered, using the fact that BTB nonces are already per-account versions. Do it in phases: first cheap latency work inside the existing BFT (no new trust assumptions), then certified transfers (FastPay / Sui-style) for accounts only, and only later owner-keyed table rows. **Recommendation: build phase A now, phase B after the testnet has real latency numbers, defer phase C.**

## Problem

Today every transaction waits for [Jolteon-style 2-chain HotStuff](../CONSENSUS.md): a leader proposes, a QC forms, and a block is final after a second QC, roughly two to three message delays plus the wait for the next proposal (target about 1 s, not yet measured on real links). [ARCHITECTURE.md](../ARCHITECTURE.md) promises about 0.5 s for "transactions touching only one user's own data". [CHAIN.md](../CHAIN.md#owner-only-fast-path-execution-level) built only the *execution* half: inside an ordered block, closed transfer components skip conflict tracking. It does not change when a payment is final.

Most payments never conflict with anything, yet they share one global sequence with DEX-like contract traffic. Ordering them wastes latency and consensus bandwidth (`O(n^2)` votes per round, see CONSENSUS.md).

## What "owner-only" means in BTB

A transaction is **owner-only** when everything it can read or write is *exclusively controlled by the signer's key*, so no other transaction can legitimately change it except by crediting it. Precisely, using the state model in [CHAIN.md](../CHAIN.md) and [STATE.md](../STATE.md):

| State | Owner-only? | Why |
| --- | --- | --- |
| Account nonce and balance of the sender | Yes | Only the sender's signature can lower the balance or advance the nonce. |
| Credit to the recipient's balance | Yes, as a *commutative increment* | Adding to a balance never conflicts with another add. Reading the recipient's balance is what would conflict, and `Transfer` does not read it. |
| Contract accounts, fee pools, slash vault, stake records | No | Shared; many senders write them (pools) or the contract controls them (crons spend a contract's balance). |
| Contract-owned rows (`02 A len table id`, STATE.md) | No (today) | The key prefix is the *contract* address `A`. The runtime has no notion of which user "owns" a row. |
| Owner-keyed rows | Only in phase C | A table would have to declare an `owner` column of type address, the runtime would enforce that a call signed by `S` touches only rows with `owner = S` plus pure reads of declared-immutable data, and indexes over it. Not designed here beyond the requirement. |
| User tokens (balances kept in a token contract's tables) | Only in phase C | Same as rows: a transfer touches two owners' rows, plus the token's supply counter if it mints or burns (then never owner-only). |

So in phases A and B, "owner-only" means: `Transfer` whose sender is an ordinary (non-contract, non-pool, non-validator-pool) account. This is the same footprint rule CHAIN.md already uses (a component of `Transfer`s with no pool account); the consensus level adds "the sender is the only account whose state is *read*".

### Detecting it safely

Two mechanisms exist; only one is acceptable for consensus.

1. **Declared access set in the transaction.** The footprint is a function of the transaction body (tag, sender, recipient). A validator can decide eligibility by looking at the bytes, before executing, with no state needed beyond the sender's balance and nonce. This is safe for `Transfer` because its footprint is exact, not a lower bound.
2. **Runtime confinement check.** Run the transaction and see what it touched (what CHAIN.md does after a parallel run). This is correct for *scheduling* (fallback is a sequential rerun) but **not enough for a fast path**: by the time a violation is found, validators have already signed and the owner has a certificate. A `Call` can read or write any account, so its declared footprint is only a lower bound and cannot be certified.

Rule: **only transactions whose declared footprint is exact and owner-only are eligible** (phase B: `Transfer`; phase C: `Call` to contracts that declare, in their manifest, an `owner_only` function whose access set the VM enforces). For phase C the runtime confinement check moves *inside the VM*: a host call outside the declared set aborts the call (revert, fee paid), so the footprint is a hard cap, not a measurement. Declared sets are signed as part of the body, so the owner cannot widen them after the fact.

## Options

### Option 1: prioritised scheduling inside the existing BFT

Keep a single ordering. Add an eligibility lane: proposers fill the front of a block with owner-only transactions, mempools gossip them first, validators vote early on blocks that contain only lane transactions (fewer to execute), and nodes expose a *soft confirmation* ("included in a proposed block with a valid QC").

- Latency: bounded by consensus, about one proposal interval plus two QCs. Better than today under load, never below the consensus floor. Soft confirmation after the first QC is about 1 round, but it is not final until the 2-chain commit.
- Safety: nothing new; the safety theorem in CONSENSUS.md is unchanged.
- Equivocation / locks / recovery: none needed.
- Cost: small. Mempool and proposer policy in `btb-chain` / `btb-node`.
- Limit: it does not remove the ordering delay. It cannot beat the 2-chain commit.

### Option 2: Sui-style Byzantine consistent broadcast with owned-object versions and locks

The owner sends the signed transaction to all validators. Each validator checks the object versions, *locks* them to this transaction (persisted before replying), executes, and returns a signature. 2/3 of stake signs, forming a **certificate**; the owner sends it to validators, who execute and record the effects. Ordering is not needed because owned objects have a single writer. Shared objects still go through consensus; certificates are periodically checkpointed.

Fit for BTB: the "owned object version" is the account **nonce**, which is already strictly sequential. A lock is `(sender, nonce) -> tx hash`. No new object model.

- Latency: one round trip to a quorum for the signatures, one for the certificate; about 2 network delays, no proposal wait. Targets about 0.5 s (ARCHITECTURE.md), to be measured.
- Safety: quorum intersection. Two certificates for the same `(sender, nonce)` would need two quorums of more than 2/3 whose intersection is more than 1/3 of stake and so includes an honest validator that signed only once. Needs less than 1/3 faulty stake, the same assumption as the main protocol.
- Equivocation by the owner: the owner signs `tx1` and `tx2` with the same nonce and sends them to different validators. Neither may reach 2/3; the account is **stuck for the epoch** (its locks are held). This is the price of the design and is the owner's own doing (self-inflicted denial of service, bounded to that account). Honest owners never hit it unless they run two wallets with the same key.
- Lock contention: a wallet resending the same tx is idempotent. An honest retry of a *different* tx at the same nonce is the equivocation case; wallets must keep a single pending tx per nonce.
- Recovery at epoch change: see "Epochs" below. Locks are released at the epoch boundary; certificates from the old epoch are voided if not executed by the last block.
- Cost: the largest. New validator-side lock store, a certificate type, a client protocol, and settlement into blocks.

### Option 3: FastPay-style certificates (accounts only, no general objects)

The same quorum-signature idea with a simpler scope: only balance transfers between accounts, with a per-account sequence number, as in FastPay/Zef/Astro. The recipient does not need to act; each validator updates balances on its own when it sees the certificate.

- Latency: same as Option 2.
- Safety: same argument; the sender's sequence number is the lock.
- Differences from Option 2: no object versions, no "input object" lists, no shared-object handoff. This is what phase B actually needs. It is Option 2 restricted to `Transfer`, with BTB's nonce as the sequence number.
- Cost: medium. Less general than Option 2 but with a much smaller attack surface. Option 2's generality only pays off in phase C.

### Comparison

| | 1. Prioritised BFT | 2. Sui-style | 3. FastPay-style (phase B) |
| --- | --- | --- | --- |
| Confirmation latency | consensus floor (about 1 s target) | about 2 delays (about 0.5 s target) | about 2 delays |
| New trust assumption | none | none (less than 1/3 faulty) | none |
| Owner equivocation | n/a | account stuck until epoch end | same |
| Epoch recovery | none needed | needed (lock release, void certs) | needed |
| Applies to | all lane txs | transfers, owner-keyed rows, tokens | transfers only |
| Interaction with Block-STM | none | settlement as confined components | same |
| Implementation size | small | large | medium |

## Interaction with the rest of the system

### Block-STM and the execution-level fast path

Certified transactions are **settled into a block**: every validator proposes the certificates it has seen as part of the payload, and consensus orders them like any other transaction (a certified `Transfer` is a `Transfer` plus a certificate that proves a quorum has locked it). Execution is unchanged: [`parallel.rs`](../CHAIN.md#parallel-execution) already runs closed transfer components with no conflict tracking, and the same footprint rule that makes them eligible for the consensus fast path decides their component. The differential oracle stays the sequential executor: settling a block with certificates must give byte-identical receipts and state root. Because the sender's lock guarantees no other tx uses that nonce, settlement order among certified transfers of different senders does not matter; of the same sender, nonce order.

The fast-confirmed balance is the sender's *settled balance minus pending certified debits*. Validators track this overlay (their view) for signing decisions only; consensus state is changed only by blocks.

### Receiving a transfer

The recipient's credit commutes with everything except reads of that balance (a contract `Call`, a cron pause check, a stake). Plan: the recipient sees the transfer as **confirmed** (final, cannot be undone) as soon as the certificate exists, but it is **spendable on the fast path only after settlement**, so that a later fast tx by the recipient never depends on an unsettled credit. This keeps each fast tx's check local to settled state plus the sender's own pending debit. Contracts and crons only ever see settled state, as today. (CHAIN.md notes this care point: "receiving a transfer ... would still need care".)

### Gas and fees

`Transfer` costs a fixed `TRANSFER_GAS` = 2,000, so its fee `2,000 * gas_price` is known exactly and a transfer cannot fail after being valid. The lock reserves `amount + fee` from the sender's settled balance (overlay). The fee splits 80/20 between the validator and relay pools as in CHAIN.md, **at settlement**, never at signing, exactly as parallel mode already defers pool credits so pool accounts never become a conflict point. The validators who sign get no per-signature payment; they earn through the validator pool, which is paid pro rata by stake at epoch end. A fast path that avoids ordering must not lower fees: same gas, same price. When a fee market exists, the fast path pays the same base fee.

### Cron and storage deposits

- **Cron:** crons run after the transactions of a block and spend the *contract's* balance, which is shared, so a contract account is never an eligible sender. A cron may read the balance of an account that a certified transfer credits; it sees the settled value, so crons stay deterministic and sequential. No change to [Cron jobs](../CHAIN.md#cron-jobs).
- **Storage deposits:** a plain transfer locks no deposit. Contract writes lock deposits from the *contract's* balance, so they are not owner-only today. Phase C (owner-keyed rows) would lock the deposit from the owner's own balance, which is owner-only and commutes; the deposit is released in the same transaction that deletes the row. The deposit amount must therefore be in the declared access set (the table's per-row deposit is fixed by the manifest), otherwise it is not exact.

### Fixed supply

Unchanged by construction: certificates move balance between accounts and into the pools, never mint. The supply check after every block ([Genesis and the supply invariant](../CHAIN.md#genesis-and-the-supply-invariant)) still sums accounts, pools, deposits and stake. Fast-confirmed amounts are overlay accounting that is never added to the sum; a certificate that is never settled changes nothing. The only way to break supply would be applying a certificate twice, which the nonce rules out (a settled nonce is used; replays are `Invalid`, no state change).

### Epochs

Validator sets are fixed inside an epoch ([CONSENSUS.md](../CONSENSUS.md#epochs-and-validator-set-changes)). Signatures are bound to `genesis = H(chain id, epoch, set digest)`, so a certificate belongs to one epoch and is checked against that epoch's set.

Rules:

1. A certificate carries its epoch and is valid for settlement only in blocks of that epoch (heights up to `end_height`). **A certificate not settled by the last block `L` is void.** After `L`, nothing can execute it.
2. Therefore validators of epoch `k+1` may start with **empty locks**: no old certificate can ever land, so a fresh lock for the same `(sender, nonce)` is safe. This is how a stuck account (owner equivocation) recovers: at the next epoch. Its pending locked funds were never debited in consensus state, so nothing is lost.
3. To keep rule 1 from voiding certificates that honest users already consider final, validators **stop signing fast transactions during the last `W` blocks of an epoch** (a quiescence window); those transactions take the normal ordered path. Any holder of a certificate (sender, recipient or a validator) can submit it, and validators must include certificates they know. A certificate formed before the window is then settled with very high probability, unless no one submits it for `W` blocks.
4. Validators persist every lock before signing, in the same style as `SafetyStorage` (CONSENSUS.md, "Crash safety"), so a restart cannot sign a second tx for the same `(sender, nonce)` in the same epoch.

## Recommendation

1. **Phase A (Phase 4, now): prioritised lane inside the BFT** (Option 1). Add the eligibility function (declared footprint is an exact, owner-only `Transfer`), lane-first proposals, and soft confirmation in the node API. Keep it behind the existing consensus API; no change to `btb-consensus` messages. Measure real-link latency on the testnet.
2. **Phase B (Phase 4, after measurement): certified transfers** (Option 3, shaped so it can grow into Option 2). New `btb-fastpath` core crate in the style of `btb-consensus`: a deterministic state machine with no network, clock or disk, with persisted locks, a `Certificate` type, settlement as payload. Wire into `btb-node`. Ship disabled by default behind a chain parameter until the simulation properties below hold.
3. **Phase C (not scheduled): owner-keyed rows and tokens.** Requires the `owner` column and VM-enforced access sets (see above). Needs its own proposal.

Why not go straight to B: the gain over A is about one second at best and the cost is a second protocol with its own epoch rules. Measure first. Why not stop at A: A cannot go below the consensus floor, and the promise in ARCHITECTURE.md (about 0.5 s) needs B.

The ROADMAP item stays unticked until B is built and tested; CHAIN.md's "Relation to the consensus fast path" and ARCHITECTURE.md's fast-path bullet are updated per phase.

## Tests that prove it (simulator properties)

Run in a deterministic simulator in the style of the `btb-consensus` simulator: random delays, drops, duplicates, reordering, crashes with restart from persisted locks, up to just under 1/3 of stake Byzantine (equivocating, silent, signing anything).

1. **No double spend:** for every `(sender, nonce)` at most one transaction ever gets a certificate; owner equivocation to random validator subsets never yields two.
2. **Supply:** the sum of accounts, pools, deposits and stake equals `TOTAL_SUPPLY` after every settled block, with random certificates, replays and duplicates.
3. **Differential:** a block containing certificates gives byte-identical receipts and state root to the sequential executor running the same transactions in block order (and for 1, 2, 4, 8 workers).
4. **Finality of certificates:** every certificate formed before the quiescence window of its epoch is settled in that epoch, whenever some honest node holds it, and a settled certificate never reverts.
5. **Stuck-account recovery:** an equivocating owner's account cannot transact on the fast path for the rest of the epoch and can in the next one; ordered-path transactions of the same account still work during the stuck period.
6. **Epoch void:** a certificate of epoch `k` is rejected in blocks of epoch `k+1` and later; a validator restarted mid-epoch with persisted locks never signs a second tx for a locked pair; a validator restarted with lost locks is covered by the 1/3 fault budget (documented, tested as a fault).
7. **Eligibility:** `Call`, `Deploy`, `Stake`, transfers from or to pools and the slash vault, and transfers with insufficient balance (counting pending debits) never get a signature; a `Transfer` whose sender has an unsettled fast debit cannot overspend.
8. **Liveness:** with at most 1/3 offline, honest transfers certify; with a minority partitioned, ordered path still progresses.
9. **Hostile input:** certificates and fast messages decode totally on arbitrary bytes (fuzz), bad signatures, wrong epoch, wrong chain id, duplicate signers and under-quorum certificates are rejected without panics.

## Files affected

New: `crates/btb-fastpath` (phase B). Changed, in the same PRs as the code:

- [ARCHITECTURE.md](../ARCHITECTURE.md): the "Fast path" bullet and the "Parallel execution" note.
- [CONSENSUS.md](../CONSENSUS.md): relation of lane and certificates to epochs ("Epochs" and "Not done yet").
- [CHAIN.md](../CHAIN.md): transaction and block payload (certificate list), "Relation to the consensus fast path", "Not done yet".
- [TOKENOMICS.md](../TOKENOMICS.md): confirm that the fee split is unchanged.
- [GLOSSARY.md](../GLOSSARY.md): "certificate", "lock", "quiescence window".
- [ROADMAP.md](../../ROADMAP.md): the Phase 4 item (stays unticked until B is done).
- [NODE.md](../NODE.md): new message types, API for soft confirmation.
- [PRIOR_ART.md](../PRIOR_ART.md): add FastPay/Zef to the Sui and Aptos section.

## Open questions

1. **Quiescence window `W`:** how many blocks, and is a fixed fraction of the epoch better than a fixed number? Interacts with epoch length and with how long clients need to submit a certificate.
2. **Who is paid for certification:** only the validator pool at epoch end, or a per-certificate share to signing validators? A new split needs a TOKENOMICS.md change and must keep the supply fixed.
3. **Recipient spendability:** is "spendable on the fast path only after settlement" too slow for payment chains? A conservative alternative lets a recipient spend a credit once its certificate is itself in a quorum-signed state; that adds a dependency graph and is not proposed.
4. **Mempool and spam:** a lock is free state on every validator. Rate limit per sender (one outstanding lock per account is already the maximum) and a minimum balance for lock creation? Validators must not hold locks for accounts that cannot pay.
5. **Stuck accounts:** is an account freeze until the epoch end acceptable for hot wallets that run the same key in two places? Should there be a signed "cancel lock" order the owner can send with a higher priority fee on the ordered path?
6. **Shared recipients:** can exchanges and other high-fan-in recipients still be accounts, or must they have a contract-level deposit mechanism? (Crediting commutes, so the answer should be accounts.)
7. **Phase C access sets:** how are owner-keyed rows declared in the manifest, and can the existing declared indexes be reused as the access set for scans?
8. **Signature aggregation:** certificates carry up to `n` Ed25519 signatures like QCs today; BLS aggregation is the same future work as in CONSENSUS.md and should be done once for both.
9. **Light clients:** how does a client verify a certificate without running a node (epoch records and set digests are on-chain, so should be fine)?
