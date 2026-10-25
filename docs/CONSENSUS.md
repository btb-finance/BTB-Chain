# Consensus: `btb-consensus`

BTB Chain orders blocks with a **Jolteon-style 2-chain HotStuff** protocol (pipelined HotStuff-2 family): proof-of-stake, stake-weighted quorums, a timeout-certificate pacemaker, and no forks once a block is final. The `btb-consensus` crate is the **consensus core only**: a deterministic state machine with no networking, no clock, no execution and no disk. The node (Phase 4) wires it to libp2p, a real timer and the executor in `btb-chain`.

Status: core implemented and tested in a deterministic simulator. Still open for Phase 4: real networking, measuring ~1 s finality on real links, signature aggregation, validator-set changes between epochs, and slashing logic that consumes the evidence records.

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

- `Block { round, height, parent, justify, proposer, payload_hash, payload }`: `justify` is the QC for `parent`.
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

## Equivocation evidence

| Evidence | What is proven | Produced when |
| --- | --- | --- |
| `DoubleVote { voter, round, a, b }` | The voter signed votes for two different blocks in one round | Two votes by one validator for different blocks are seen, directly or inside a verified QC |
| `DoubleProposal { proposer, round, a, b }` | The proposer signed two different block ids for one round | Two proposals are received, or votes/timeouts carry two different proposer signatures for the round |

`Evidence::verify(&Context)` checks both signatures against the offender's key from the validator set and that the ids differ; it needs no other state, so a slashing module can verify it on-chain. Evidence is deduplicated per `(kind, offender, round)`, broadcast once when first found, and re-verified by every receiver (forged evidence is rejected). Invalid signatures are *not* evidence (anyone can forge a bad signature), they are simply dropped.

## Resource bounds

Messages are rejected before any expensive work where possible: membership and range checks first, then signatures, then QC/TC verification (cached by digest, and QCs reuse signatures already verified as individual votes). Per-round bookkeeping is dropped `retain_rounds` behind the commit point; pending blocks, queued payloads, sync batch size and the future-round window are all capped. Rounds above `2^62` are rejected so round arithmetic cannot overflow.

## Testing

`crates/btb-consensus/src/sim.rs` is a deterministic network simulator: seeded delays and reordering, message loss, partitions, crashed and restarted nodes, and Byzantine replicas (equivocating leader, double voter, invalid signatures, random and mutated garbage bytes). Every message is encoded and decoded on delivery. On every commit of every honest replica it checks that heights are gap-free, each block extends the previous commit, and no two replicas disagree at any height.

`cargo test -p btb-consensus` runs unit tests for every rejection path and scenario tests (crash up to `f`, equivocating leader, double voter, bad signatures and garbage, minority partition, even split and heal, crash with restart) plus a seeded property test over randomised scenarios (safety, liveness after the network heals, catch-up, sound evidence, no honest validator ever accused). `cargo test -p btb-consensus --release -- --ignored --nocapture` runs the long soak over thousands of seeds.

## Not done yet

- Real networking (libp2p/QUIC), real timers, and the ~1 s finality measurement on real links (the simulator reports latency in ticks only).
- BLS signature aggregation (QCs are lists of Ed25519 signatures today).
- Validator-set changes at epoch boundaries and the slashing/staking logic that consumes `Evidence`.
- Block and payload persistence, state sync from snapshots, and a vote-collection mode that avoids the `O(n^2)` vote broadcast.
- Byzantine-leader payload checks beyond size and hash (the executor validates transactions).
