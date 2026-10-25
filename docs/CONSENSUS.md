# Consensus: `btb-consensus`

BTB Chain orders blocks with a **Jolteon-style 2-chain HotStuff** protocol (pipelined HotStuff-2 family): proof-of-stake, stake-weighted quorums, a timeout-certificate pacemaker, and no forks once a block is final. The `btb-consensus` crate is the **consensus core only**: a deterministic state machine with no networking, no clock, no execution and no disk. The node (Phase 4) wires it to libp2p, a real timer and the executor in `btb-chain`.

Status: core implemented and tested in a deterministic simulator, including [execution attestation](#execution-attestation-validators-certify-state-roots) (a quorum certificate certifies the post-execution state root). Epoch changes (validator-set rotation) are implemented and simulated; see [Epochs](#epochs-and-validator-set-changes). Still open for Phase 4: real networking, measuring ~1 s finality on real links, and signature aggregation. The staking and slashing logic that consumes the evidence lives in `btb-chain` ([CHAIN.md](CHAIN.md#staking-epochs-and-slashing)).

## What it guarantees

With total faulty stake **below 1/3** of the validator set:

- **Safety (always, even with no network guarantees):** no two honest replicas ever commit different blocks at the same height. A committed block is final; there are no reorgs.
- **Liveness (after the network becomes synchronous):** honest replicas keep committing. Rounds led by faulty validators time out and are skipped.
- **Accountability:** the two provable ways to misbehave in this protocol, signing two votes or two proposals in one round, produce self-contained `Evidence` records that anyone can verify against the validator set.

## Interface

```rust
let mut replica = Replica::new(config, validator_set, signing_key, Some(Box::new(storage)))?;
let actions = replica.start();                    // leader of round 1 proposes
let actions = replica.on_message(msg);            // network input
let actions = replica.on_timeout(now);            // logical clock input, call every tick
let actions = replica.propose(payload_bytes);     // queue app payload for the next led round
```

Every call returns `Vec<Action>`:

| Action | Meaning |
| --- | --- |
| `Send { to, msg }` | Send one message to validator `to` |
| `Broadcast(msg)` | Send to every other validator (the replica handles its own copy internally) |
| `Commit(block)` | The block is final. Emitted once per height, in height order, parents first |
| `Evidence(e)` | Newly found, verified misbehaviour. Also broadcast once so everyone learns it |

Rules for the embedder:

- **No wall clock.** `Tick` is a logical `u64`; you pick its unit (for example 50 ms). `Config::base_timeout` is in ticks. Time never moves backwards (`now` is clamped).
- **Payloads are opaque bytes.** A block carries `payload` plus `payload_hash = SHA-256(payload)`. Consensus orders payloads and never interprets them. If the leader has nothing queued it proposes an empty block (needed to keep the 2-chain commit rule moving).
- **Input is untrusted.** `on_message` validates everything and drops what is invalid; it never panics. `Message::decode` is total on arbitrary bytes. The simulator and `codec_roundtrip_and_hostile_bytes_never_panic` fuzz both.
- **Crash safety.** Pass a `SafetyStorage`. The replica saves `last_voted_round`, `locked_round`, `last_proposed_round`, the high QC and the commit point *before* returning any action that depends on them, so a restarted validator cannot vote or propose twice in a round. Blocks are not persisted here; a restarted replica re-fetches them through `SyncRequest`/`SyncResponse` (the executor owns the durable chain).
- **Determinism.** All maps are ordered; there are no floats, threads or system randomness. The same inputs always give the same actions.

## Protocol

Time is divided into **rounds**. Round `r` has one **leader**, `leader(r)`, a pure function of `r` and the validator set.

**Types** (all signatures Ed25519, all signed bytes domain separated and bound to the chain id and validator set through the genesis id):

- `Block { round, height, parent, justify, proposer, payload_hash, payload, exec }`: `justify` is the QC for `parent`; `exec` is the execution attestation (below), covered by the block id.
- `Vote { round, block_id, voter, sig }`: a signature over `(round, block_id)`. It also carries the proposer's signature on the block (`proposal_sig`) so conflicting votes expose an equivocating leader.
- **QC** (quorum certificate): votes from validators holding **more than 2/3 of total stake** for the same block in the same round. Stored as a sorted list of `(validator, signature)`; BLS aggregation can replace this later without changing the rules.
- `Timeout { round, high_qc, voter, sig }`: a signature over `(round, high_qc.round)`.
- **TC** (timeout certificate): timeouts from more than 2/3 of stake for round `r`, plus the highest QC among them (its round must equal the highest signed `qc_round`).

**Happy path (one round per message delay, pipelined):**

1. The leader of round `r` proposes a block extending its highest QC (`justify`), broadcasts it.
2. Every replica that accepts it broadcasts its vote to **all** validators (not only the next leader). Any replica that sees votes holding more than 2/3 of stake forms the QC itself, so a crashed next leader cannot veto the block. The cost is `O(n^2)` vote messages per round; fine for the validator counts in Phase 4 (aggregation and a leader-collects mode are future optimisations).
3. A QC for round `r` moves everyone to round `r + 1`, whose leader proposes with that QC as `justify`.

**Failure path:** if a replica sees no progress for `base_timeout << min(consecutive_failures, max_backoff_shift)` ticks it sends a signed `Timeout` for its round and stops voting in that round. A replica that sees timeouts from more than 1/3 of stake for a round it has not left joins them (at least one honest validator timed out, so the round is really stuck). With timeouts from more than 2/3 of stake any replica forms a TC and moves to the next round. The TC carries the highest QC any timing-out validator had; the next leader must build on a block at least that high. Timeouts are retransmitted every `base_timeout` ticks while stuck, and a replica that receives a stale timeout answers with its newest QC/TC (`Catchup`), so a validator coming back from a partition rejoins quickly.

**Leader selection** (`Config::leader_policy`), both deterministic and identical on every replica:

- `RoundRobin` (default): `index = round mod n`. Ignores stake. Every validator leads equally often. Liveness needs two consecutive honest-led rounds to recur, so it requires fewer than `n/2` faulty validators *by count* (on top of < 1/3 faulty stake): with half the seats faulty and alternating, no two honest leaders ever follow each other and nothing commits. Use `StakeWeighted` when stakes are unequal.
- `StakeWeighted`: smooth weighted round-robin over a fixed cycle where each validator owns a number of slots proportional to its stake (rounded to `1/(32n)` of the total, at least one slot). Interleaved and with bounded gaps, so a large faulty holder cannot take consecutive rounds. Chosen over hash-based sampling on purpose: pseudo-random schedules have unbounded unlucky runs and the simulator found real stalls with them.

Sync: a replica that learns of a block it lacks (from a QC, a TC or a proposal) asks the sender with `SyncRequest` and receives up to 64 hash-chained ancestors; blocks are accepted only if they hash to something already certified or requested, so a response cannot inject unrequested data. Buffered orphans are bounded (`max_pending_blocks`) and requests are retried at other peers.

## Safety rules

These three rules are the whole safety argument; every honest replica applies them exactly.

**Voting rule.** Vote for a proposal with block `B` in round `r` only if all hold:

1. `r` equals the replica's current round and `r > last_voted_round` (one vote per round; sending a timeout counts as having voted in that round).
2. `B.justify.round >= locked_round` (**locking**).
3. `B.justify.round + 1 == r`, **or** the proposal carries a valid TC for round `r - 1` and `B.justify.round >= TC.high_qc.round` (the leader may skip a failed round only with proof, and only while extending the highest QC the timeouts reported).
4. The proposer is `leader(r)`, the signature is valid, `B.parent == B.justify.block_id`, the height is the parent's plus one, the payload hash and size are correct, and the parent chain is known.

`last_voted_round` is persisted before the vote leaves the replica.

**Locking rule.** `locked_round` is the highest QC round the replica has ever seen (in a proposal, TC, timeout or one it formed). It never decreases. (This is the 2-chain analogue of HotStuff's "lock on the 2-chain": with a 2-chain commit the lock moves to the 1-chain, i.e. the highest QC.)

**Commit rule (2-chain).** When the replica holds a QC for block `B'` whose parent is `B` and `B'.round == B.round + 1` (consecutive rounds), it commits `B` and all uncommitted ancestors, in height order. Seeing the QC for `B'` is enough; the block `B'` itself does not need to be committed yet.

### Why this is safe

*Quorum intersection.* Any two sets of validators with more than 2/3 of the stake overlap in stake greater than 1/3, which exceeds the faulty stake, so the overlap contains an honest validator.

**Lemma 1: one certified block per round.** An honest validator votes at most once per round (rule 1, persisted). Two QCs for different blocks in one round would put an honest validator in both quorums, voting twice.

**Lemma 2: a commit pins every later certified block.** Let `B` (round `r`) be committed because `QC(B')` exists, with `B'` in round `r + 1` and parent `B`. Then every certified block with round greater than `r` descends from `B`.

*Proof.* Suppose not, and let `C` be the lowest-round counter-example, in round `s > r`. By Lemma 1, `s != r + 1`, because the only certified block of round `r + 1` is `B'`, which descends from `B`; so `s > r + 1`. A quorum voted for `B'` and a quorum voted for `C`; they share an honest validator `v`. When `v` voted for `B'` it had seen `B'.justify = QC(B)`, so `locked_round(v) >= r` (locking rule). `v` voted for `B'` in round `r + 1` and for `C` in the later round `s`, and the lock never decreases, so when it voted for `C` the voting rule required `C.justify.round >= r`. Let `P` be `C`'s parent and `j = C.justify.round`, so `r <= j < s` and `P` is certified. If `j = r`, then `P = B` (Lemma 1), so `C` descends from `B`. If `j = r + 1`, then `P = B'`, a descendant of `B`. If `r + 1 < j < s`, `P` is certified in a lower round than `s`, so by minimality it descends from `B`, and so does `C`. Each case contradicts the choice of `C`. Hence no counter-example exists. (End of proof.)

**Theorem: no two honest replicas commit different blocks at one height.** Every committed block is certified. Take two committed blocks `B1` (round `r1`) and `B2` (round `r2`) with `r1 <= r2`. If `r1 = r2` they are the same block (Lemma 1). Otherwise `B2` is certified in a round above `r1`, so by Lemma 2 it descends from `B1` and has a larger height. So same height means same block. (End of proof.)

Where the other rules matter: rule 3 (TC and consecutive-round conditions) is about **liveness**, ensuring a new leader cannot be forced to extend a stale branch and honest voters do not reject it; safety rests only on one vote per round (rule 1), the lock (rule 2) and the commit rule. Rule 4 (validity checks) keeps garbage out of the chain.

*Timeouts and restarts.* Sending a timeout sets `last_voted_round >= r`, so a replica never votes in a round after announcing it gave up on it. This keeps the story of a round simple: a timeout certificate for round `r` and a QC for a block of round `r` may both exist (some replicas voted before timing out), and the protocol is safe with both, since Lemma 2 does not look at TCs. Persisted safety state means a crash and restart cannot reset any of these counters.

*Liveness sketch.* After the network becomes synchronous: timeouts and retransmissions make all honest replicas converge on the same round (f+1 amplification, TCs, `Catchup`), and exponential backoff makes the round timer exceed the network delay. In a round with an honest leader every honest replica has the same high QC (it was delivered inside the TC), all accept the proposal, vote, and a QC forms. Two consecutive honest-led rounds commit a block (the votes are broadcast, so the QC of the second block forms without a third honest leader). If fewer than half of the schedule's seats are faulty, such a pair exists in every cycle.

## Execution attestation: validators certify state roots

A QC covers the block id, i.e. the *payload* (the transactions), not what executing the transactions produces. Without more, a hosting node's Merkle proof could only be anchored to a root that every node derived locally. Execution attestation closes that gap without changing the safety rules above: it adds one more condition to the voting rule and one field to the block.

**The field.** `Block::exec` (at most 128 bytes, opaque to this crate, hashed into the block id) says "the execution result at height `k` is `X`". The embedder (`btb-node`) fills it with `(k, d_k, receipts_root_k)`, where `d_k` is a hash chain over all state roots up to `k` (`docs/NODE.md`). Installing the hooks (`Replica::set_exec_hooks(provider, check)`) switches the rule on.

**Which `k`: the committed ancestor, a function of the ancestry.** Execution lags consensus: when `B` is proposed its parent is not committed yet (2-chain), so `B` cannot attest its parent's result. It attests the newest block that `B`'s own ancestry commits. Define, for every linked block `P`:

```
attested(P) = height of P.parent        if P.round == P.parent.round + 1   (QC(P) commits P.parent)
            = attested(P.parent)        otherwise
attested(genesis block of the epoch) = the epoch's anchor height

k(B) = attested(B.parent)               (Replica::exec_height(&B.parent))
```

Equivalently, `k(B) = exec_height(B.parent)` is the highest block committed by the QCs that appear in `B`'s chain, `B.justify = QC(P)` last. Properties:

1. *Well defined.* It depends only on rounds and heights along `B`'s ancestry, so proposer and voters compute the same `k` (the replica keeps it per linked block, `Replica::exec_height`). It is never chosen by the proposer; a block cannot name another height.
2. *Committed.* Any replica that holds `B.justify = QC(P)` and the chain below it has, by the commit rule, already emitted `Commit` for every block up to `k(B)`. So an honest replica that is about to vote on `B` has been *told* to commit height `k(B)`; it has only to *execute* it.
3. *Monotone and bounded lag.* `k` never decreases along a chain and `k(B) <= B.height - 2`. In the common case (consecutive rounds) `k(B) = B.height - 2`; every skipped round holds it back until two consecutive rounds recur.
4. *Epoch boundaries.* The first blocks of epoch `e + 1` have `k` = the anchor, the last block `L` of epoch `e`. `L` itself is attested by epoch `e + 1`'s first block (its validators executed `L` to start the epoch). The tail blocks of epoch `e` (empty payload, still carry `exec`) attest up to `L - 1`. Hence every height is attested by the epoch whose validators were in office when it became committed, or by the next one for `L`.

**The voting rule gains condition 5.** Vote only if, in addition to 1 to 4: `check(B, k(B))` returns `Accept`, i.e. `B.exec` names exactly `k(B)` and the result the voter computed itself by executing height `k(B)`.

- `Reject` (a different result, a different height, a malformed or missing claim): no vote, ever; the round times out if the leader is faulty (counted in `Stats::exec_rejected`).
- `Defer` (the voter has not executed `k(B)` yet): the proposal is parked (one per round) and `Replica::resume` re-runs the vote once the embedder executed the commits. A deferral does not touch `last_voted_round` (it is not a vote), so safety is unaffected, and a deferred round simply times out like a slow leader if execution never catches up.

**The proposer** takes `provider(k)` and may only propose when its own execution has reached `k` (otherwise `pump` returns without taking a payload from the queue; `resume` retries). In a happy path the leader of round `r + 1` forms QC(`r`) and executes the newly committed block in the same step, so there is no delay.

### Why a QC certifies the state root

**Theorem (attested roots are correct).** Let fewer than 1/3 of the stake be faulty and let `QC(B)` exist, with `B.exec` naming `(k, X)`. Then `X` is the result of executing the committed chain up to height `k`, as computed by an honest replica.

*Proof.* `QC(B)` holds more than 2/3 of the stake, so (quorum intersection) at least one honest validator `v` voted for `B`. By condition 5, `check(B, k(B))` accepted, so `B.exec = (k(B), X)` with `X` equal to `v`'s own execution of height `k(B)`. Block `k(B)` is committed in `B`'s ancestry (property 2), and by the Safety Theorem every honest replica commits the same chain, so the block at that height is the same for all of them. Execution is deterministic ([NODE.md](NODE.md#how-a-block-is-made)), so every honest executor obtains `X`. (End of proof.)

*Corollaries.* (a) A byzantine leader that attests a wrong root gets no vote from any honest replica, so with fewer than 1/3 faulty stake it cannot form a QC, and the chain continues in the next round (the block is simply never certified). (b) Because `d_k` is a hash chain over the roots of heights `1..=k`, one certificate also binds every earlier root; a proof for height `h <= k` ships the roots `h..=k` and `d_{h-1}` (`docs/HOSTING.md`). (c) A committed block was certified, so every honest node that executes it can compare the claim with its own result. A mismatch means the node's execution is broken or more than 1/3 of the stake is faulty, and `btb-node` stops with `state divergence` instead of building on it.

**Liveness.** Condition 5 adds waiting but no new way to be stuck. After GST, honest replicas receive the same QCs, hence emit the same `Commit` actions, hence execute the same heights; the leader of an honest round has executed `k` before it proposes (it holds the same commits as the voters, see property 2), and every honest voter's `check` is `Accept` after at most its own execution time. Execution time per block is bounded by gas, and the round timer already grows by backoff until it exceeds the network and execution delay. A voter whose execution is permanently stuck behind (it needs block sync) counts toward the faulty or offline stake for as long as it lags, as before.

**Sync and restart.** Block sync checks the same thing: a synced block must carry a well-formed claim for an earlier height (`btb_node::sync::verify`), and executing it compares the claim with the node's own digest chain (`state divergence` otherwise). A restarted node rebuilds its record of recent digests from the block store and, for heights without a record, from the chain state's Merkle roots (the receipts of such a height are then unknown: the node does not vote on claims about that single height until the commit point moves past it). Nothing about attestation is persisted in the consensus safety file: `k(B)` is recomputed when the replica relinks the chain.

## Epochs and validator-set changes

The validator set is fixed inside an **epoch** and changes only at an epoch boundary. An epoch is a *fresh consensus instance*, not a reconfiguration of a running one:

- `Config::epoch` and `Config::end_height` say which epoch a `Replica` serves and the height of its last block (the chain uses `end_height = (epoch + 1) * epoch_len`). `Replica::new_epoch(config, set, key, storage, anchor)` creates the instance; `Replica::new` is epoch 0 with no end.
- `Anchor { height, id }` is the finalized chain the epoch extends: the height of the previous epoch's last block and a 32-byte `id` that **every validator of the new epoch derives identically from the finalized chain** (btb-chain uses the header hash of that block, `EpochRecord::anchor`). It is the block the round-0 certificate certifies, so the first block of the epoch has `parent = anchor.id` and height `anchor.height + 1`. Because it comes from the chain and not from consensus internals, a node that joins at the boundary needs nothing from the old epoch's consensus.
- Rounds restart at 1 in each epoch. All signatures are bound to `genesis = H(chain id, epoch, validator set digest)` (`Context::for_epoch`), so a vote, proposal or timeout from one epoch (or one set) is invalid in any other. The anchor is deliberately *not* in that hash: a chain that knows an epoch's set can rebuild the context and verify `Evidence` from that epoch without knowing any consensus block id.
- **Last block of an epoch.** With the 2-chain rule the block `L` at `end_height` is committed when a QC exists for a child `L'` in the next round. So blocks above `end_height` ("tail" blocks) are allowed in the epoch's instance, but only to certify `L`: their payload must be empty (voters reject a tail block with a payload, a leader proposes an empty one) and **they are never committed**. When `L` is committed the replica emits `Action::Commit(L)` and `epoch_ended()` becomes true; commits above `end_height` are dropped.
- **After the end.** An ended replica never votes, proposes or times out again. It keeps answering `SyncRequest` and answers a validly signed `Timeout` with `Catchup` (its newest QC), so a validator that lagged behind can still finish the epoch: it receives QC(`L'`), requests the tail blocks, commits `L` and ends too. The node keeps the ended instance around for a while (a few epochs of unbonding is plenty in practice; state sync is the fallback).
- **Switching.** The node executes `L` in `btb-chain`, which fixes the next set (`ChainState::epoch_record(epoch + 1)`: validators in consensus order, `start_height`, `anchor`), and starts `Replica::new_epoch` for the nodes in it with a new `SafetyStorage`. Nodes that left simply drop their replica; nodes that joined start from the same anchor. Messages carry no epoch tag on the wire; the node routes by the epoch of the instance (an envelope), and a message sent to the wrong epoch fails signature checks and is dropped.

### Why epoch changes are safe

*Within an epoch* nothing changes: the safety theorem above holds for the epoch's instance with its own set, as long as the faulty stake of that set is below 1/3. Safety across the boundary needs three more facts.

1. **The last block of epoch `k` is unique.** By the theorem, all honest replicas of epoch `k` that commit height `end_height` commit the same block `L`. Tail blocks are never committed, so no other block at `end_height + 1` or above can be final in epoch `k`.
2. **The new epoch extends exactly `L`.** The next set and the anchor are functions of the *executed* chain up to `L` (`btb-chain`), which is deterministic. Every validator of epoch `k + 1` therefore starts from the same `(set, anchor)`; a validator that starts from a different chain computes a different anchor, a different `genesis` and so a different signature domain, and its messages are ignored. There is no moment at which two honest validators run the same epoch with different sets.
3. **No cross-epoch signature reuse.** The domain separation above means that votes of epoch `k` cannot be replayed into epoch `k + 1` to form a QC (even for the same validators), and a QC cannot be assembled from signers of two different sets. Each instance therefore sees only its own quorums: Lemma 1 and Lemma 2 apply per instance, and chain order across epochs is by height.

Together: the finalized chain is the concatenation, for each epoch, of the blocks that epoch's instance commits (heights `anchor.height + 1 ..= end_height`), each starting from the previous epoch's `L`. Two honest nodes cannot disagree about a height in the same epoch (the theorem), and the epochs are chained through the unique `L`.

*Liveness at the boundary.* Epoch `k` needs two consecutive honest-led rounds after `L` is certified, the ordinary liveness condition (the tail block is an empty proposal). Epoch `k + 1` starts as soon as `2/3+` of *its* stake has executed `L`; validators that are slow to execute or sync `L` count as faulty for that epoch, so the usual bound (< 1/3 of the new set's stake faulty or offline at the start) applies. Laggards of epoch `k` catch up through the ended replicas as described above.

*What this does not give you.* A validator that leaves a set can still sign for the epochs it was in. It is accountable for exactly as long as the evidence window (the unbonding period) lasts, because its stake stays locked and slashable; after that, signatures of old epochs are a long-range attack vector and new nodes must sync from a recent trusted checkpoint (weak subjectivity), as in any proof-of-stake chain.

## Equivocation evidence

| Evidence | What is proven | Produced when |
| --- | --- | --- |
| `DoubleVote { voter, round, a, b }` | The voter signed votes for two different blocks in one round | Two votes by one validator for different blocks are seen, directly or inside a verified QC |
| `DoubleProposal { proposer, round, a, b }` | The proposer signed two different block ids for one round | Two proposals are received, or votes/timeouts carry two different proposer signatures for the round |

`Evidence::verify(&Context)` checks both signatures against the offender's key from the validator set and that the ids differ; it needs no other state, so a slashing module can verify it on-chain. `btb-chain` does exactly that: a `SubmitEvidence { epoch, evidence }` transaction rebuilds the context of `epoch` from the stored validator set (`Context::for_epoch(chain id, epoch, set, None)`) and slashes the offender (see [TOKENOMICS.md](TOKENOMICS.md#validator-staking-and-slashing)). Evidence is deduplicated per `(kind, offender, round)`, broadcast once when first found, and re-verified by every receiver (forged evidence is rejected). Invalid signatures are *not* evidence (anyone can forge a bad signature), they are simply dropped.

## Resource bounds

Messages are rejected before any expensive work where possible: membership and range checks first, then signatures, then QC/TC verification (cached by digest, and QCs reuse signatures already verified as individual votes). Per-round bookkeeping is dropped `retain_rounds` behind the commit point; pending blocks, queued payloads, sync batch size and the future-round window are all capped. Rounds above `2^62` are rejected so round arithmetic cannot overflow.

## Testing

`crates/btb-consensus/src/sim.rs` is a deterministic network simulator: seeded delays and reordering, message loss, partitions, crashed and restarted nodes, and Byzantine replicas (equivocating leader, double voter, invalid signatures, random and mutated garbage bytes). Every message is encoded and decoded on delivery. On every commit of every honest replica it checks that heights are gap-free, each block extends the previous commit, and no two replicas disagree at any height.

`cargo test -p btb-consensus` runs unit tests for every rejection path and scenario tests (crash up to `f`, equivocating leader, double voter, bad signatures and garbage, minority partition, even split and heal, crash with restart) plus a seeded property test over randomised scenarios (safety, liveness after the network heals, catch-up, sound evidence, no honest validator ever accused). `cargo test -p btb-consensus --release -- --ignored --nocapture` runs the long soak over thousands of seeds.

`crates/btb-consensus/src/exec_tests.rs` and the simulators cover execution attestation with a fake execution function (a digest chain over the committed blocks): unit tests for deferral, resume, rejection of wrong, missing and oversized claims; scenario tests where a `WrongExec` leader (validly signed proposals with a wrong root) never gets a vote and the chain continues; a majority of such leaders (liveness lost on purpose) still gets no wrong root certified; crash/restart with loss under stake-weighted leaders; and a seeded property test over `random_scenario`. On every vote the simulator asserts that no honest replica votes for a wrong-attestation block, and on every commit that the certified claim equals the true execution result. `epoch_sim` runs the same checks across four epochs with complete validator turnover, joiners and laggards.

`crates/btb-consensus/src/epoch_sim.rs` simulates epoch changes: sets that change completely over three epochs (nodes joining and leaving), unequal stakes with `StakeWeighted` leaders, an offline new member below 1/3, a validator that is offline across a boundary and must catch up through the ended replicas, a leaver that disappears right after the boundary, and a lossy network before GST over many seeds. On every commit it checks gap-free heights across epochs, that each epoch's first block extends the anchor, that nothing above an epoch's last height is committed, and that no two replicas ever disagree at a height. Unit tests cover tail blocks, ended replicas and signature separation between epochs.

## Not done yet

- Real networking (libp2p/QUIC), real timers, and the ~1 s finality measurement on real links (the simulator reports latency in ticks only).
- BLS signature aggregation (QCs are lists of Ed25519 signatures today).
- A commit proof that lets a node joining from far behind verify the last block of an old epoch without replaying it (today: sync the executed chain, or use an ended replica's `Catchup` and sync).
- Block and payload persistence, state sync from snapshots, and a vote-collection mode that avoids the `O(n^2)` vote broadcast.
- Byzantine-leader payload checks beyond size and hash (the executor validates transactions).
- Receipts roots are attested only at the claimed height `k` (state roots are chained for every height). Chaining receipts too would let a proof cover the receipt of any height; it is a hash change only.
