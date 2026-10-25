# Validator node: `btb-node`

`crates/btb-node` is the binary that turns the pieces into a network: it runs one validator, wiring `btb-consensus` (the `Replica`) to `btb-chain` (the `Executor` and `Mempool`) over libp2p/QUIC, and serves a local JSON-RPC API.

Status: a working multi-validator testnet node with persistent storage, evidence-to-slashing and block sync. Also built: an embedded block explorer and a faucet ([TESTNET.md](TESTNET.md)). Not done: a non-validator following the chain, a public deployment, state sync from snapshots (see [Limits](#limits)).

## Run a local 4-validator testnet

```
cargo build --release -p btb-node            # add --features rocksdb to keep chain state on disk
./target/release/btb-node init-testnet --validators 4 --dir /tmp/btb-net
for i in 0 1 2 3; do ./target/release/btb-node run --config /tmp/btb-net/node$i/node.json & done
curl -s 127.0.0.1:8545/status                # node 0; nodes 1..3 use ports 8546..8548
```

`init-testnet [--chain-id N] [--base-port 9000] [--rpc-base-port 8545]` writes `genesis.json`, `faucet.key` (an account holding everything except the validators' 1,000,000 BTB each) and `nodeN/{validator.key,node.json}`. Validator `i` listens on UDP `base-port + i`. Each validator bonds 100,000 BTB; the epoch is 600 blocks. Edit `genesis.json` (`epoch_len`, `min_stake`, `gas_price`, allocations) before the first start; the validator set and balances are committed in the genesis hash, and the consensus signature domain depends on it.

Without `--features rocksdb` the node keeps chain state in memory and **refuses a `data_dir`** (the consensus safety state would survive a restart while the chain would not). With the feature, `data_dir` holds the chain state (`chain/`, RocksDB), the block history (`blocks/`, RocksDB, see [Block history](#block-history)) and the crash-safety files below, and a restarted node resumes from disk and catches up.

### Config (`node.json`)

| Field | Meaning |
| --- | --- |
| `key_file` | `{"secret": hex}`; the same Ed25519 key signs consensus messages and is the libp2p identity |
| `genesis` | path to `genesis.json` |
| `listen` | `/ip4/127.0.0.1/udp/9000/quic-v1` |
| `peers` | other validators as `/ip4/.../udp/.../quic-v1/p2p/<peer id>`; `btb-node peer-id --key FILE` prints a peer id |
| `rpc` | bind address, loopback only |
| `reporter_key_file` | optional `{"secret": hex}` of the account that signs and pays for `SubmitEvidence` (default: the validator key's own account) |
| `data_dir` | see above |
| `tick_ms` (20), `round_timeout_ms` (2000), `idle_block_ms` (250) | timing |

## Networking

- **Identity.** The libp2p key is the validator Ed25519 key, so the peer id is a function of the consensus key. QUIC's TLS handshake authenticates it; there is no extra handshake. A peer is a validator iff its peer id is derived from a key in the current set; everyone else is disconnected and ignored.
- **Messages.** Each consensus message is one request on a QUIC stream (libp2p request-response, acknowledged with one byte). A frame is `kind:u8 || body`; consensus frames carry the epoch (`u64`) before the message so ended epochs can still serve laggards. Transactions are flooded to all peers on first admission to a mempool.
- **Bounds.** Frames up to 20 MiB (a sync response of 64 blocks of at most 256 KiB payload), length checked before allocation; 512 outstanding requests per peer; a token bucket per peer (default 2000 messages/s, burst 4000; a message costs `1 + bytes/4096`); a peer that sends 20 malformed frames in 10 s is ignored for 30 s. `SyncRequest.from` is rewritten to the authenticated sender so nobody can aim responses at a third party.
- Connections are retried every 500 ms; messages to a disconnected peer are dropped (consensus and sync retransmit).

## How a block is made

1. The driver thread owns the `Replica`, `ChainState`, `Mempool` and `Executor`; network frames, RPC calls and a `tick_ms` timer arrive on one channel, so execution order is commit order.
2. The replica is told not to propose empty blocks by itself (`set_hold_empty_proposals`). The leader proposes immediately when the mempool has transactions, or when an earlier non-empty block still needs the next block's certificate (2-chain commit), and otherwise at most every `idle_block_ms` (crons need blocks). Transactions already seen in an unfinished proposal are not proposed again for a while.
3. The payload is `timestamp:u64 || [tx]` (empty bytes for an empty block), at most 256 KiB. Before voting every replica runs the payload check hook (`set_payload_check`): it decodes, is within the block limits, has no repeated transaction and every transaction passes the stateless checks (chain id, kind, signature, gas limits). A block failing it gets no vote.
4. The chain block is **derived at commit time**: height and parent hash from the chain head, `state_root` from the head state, the proposer's timestamp (not below the parent's). It cannot be fixed at proposal time because with a pipelined 2-chain commit the parent is not executed yet. Every committed consensus block gives exactly one chain block. If the executor rejects a block (it cannot, after the check above) every node falls back to executing it empty, identically.
5. The block is executed (one atomic batch), the mempool is pruned, receipts are indexed.

### Epochs

The chain fixes the validator set per epoch ([CHAIN.md](CHAIN.md#staking-epochs-and-slashing)). The node runs one `Replica::new_epoch` per epoch, built from `ChainState::epoch_record(epoch)` (set in consensus order, anchor). When the last block of an epoch is executed, the next instance starts; the old one stays as an ended replica (last 8 epochs) that answers sync and catch-up for that epoch's laggards. Messages are routed by their epoch tag.

### Block sync

A laggard normally catches up through the ended consensus instances of its epoch (the last 8 epochs). Block sync removes that bound: a node that has not committed for a second asks a peer (round robin over the validators it knows) for committed blocks **by height**, and it can do that for an arbitrarily old height as long as the peers still have that history (all of it with RocksDB, 4096 heights in memory).

- **Request:** frame kind 3, `from_height:u64` (the node's next unexecuted height). **Response:** kind 4, `epoch:u64`, consecutive consensus blocks of that one epoch (up to 16, plus up to 2 more that act as proof), and a QC for the last block. For the epoch's last block, the stored closing proof (the tail block(s) above the epoch end and the QC that certifies them) is appended. A peer answers an unknown or too-new height with an empty response.
- **Verification** (`sync::verify`), nothing from the peer is trusted: the response must be for the epoch the node is in; the first block must have the node's next height and extend the last block it executed (or the epoch's anchor); every block's payload must match its hash and every block must link to the previous one by hash, height, round and justify; and block `i` counts as committed only if block `i + 1` has the next round and a quorum certificate for it exists (the justify of block `i + 2`, or the response's QC), which is the 2-chain commit rule. That one QC is verified signature by signature against the epoch's validator set in the epoch's signature domain. The validator set is **derived by the node itself** by executing the chain epoch by epoch (`ChainState::epoch_record`), so epoch records are never taken from a peer. A committed block commits its hash-linked ancestors. Truncated or altered payloads, broken links, QCs of another epoch, forged or short QCs, wrong start and responses with no proof are rejected (and counted in `btb_status` `sync.rejected`). Only the peer asked, once, may answer; others are counted as `unsolicited` and ignored.
- The verified blocks are executed in order, the epoch switches as usual when the last block of an epoch executes (a consensus instance that block sync overtook is dropped), and the node asks again at once. Near the chain's tip a response proves nothing yet (the newest blocks lack their certificate); the node's own consensus instance takes over there.
- `btb_status` `sync`: `requests`, `blocks_synced`, `served`, `rejected`, `unsolicited`.

A node that is no longer in the allowed set of the peers it asks (removed from the validator set by slashing or low stake, then absent for long) is disconnected by them; following the chain as a non-validator is not done.

### Crash safety

Two files per node in `data_dir`, each replaced atomically (temp file, fsync, rename):

- `safety-<epoch>.bin`: the consensus `SafetyState`. A failed save aborts the node; a corrupt file refuses to start instead of resetting (a reset could double vote).
- `exec.bin`: last executed consensus block, with a write-ahead entry around each execution.

The replica persists "block h committed" before it emits the `Commit`, so a crash in between would lose h. The safety file therefore records the commit point of what was *executed*, never ahead of the chain; after a restart the replica re-commits (re-fetching blocks by sync) whatever the chain still lacks, and the node skips heights already executed. An interrupted execution is settled from the chain head height.

**Without persistent storage a restarted validator is a brand-new replica with the same key and can equivocate.** Only run in-memory nodes for tests.

With RocksDB the chain state, history and safety files all live under `data_dir`: kill a node (even with `kill -9`) and start it again on the same directory and it resumes, then catches up with block sync or its peers' consensus instances. Restarting **all** validators at once works too (the tail comes back from the block store as described above).

### Block history

Every executed block is stored with its receipts, the consensus block it came from (payload, justify QC) and, for the last block of an epoch, the epoch's closing proof (below). Written after execution, in one batch per block.

| Node | Where | Kept |
| --- | --- | --- |
| built with `--features rocksdb`, `data_dir` set | RocksDB at `<data_dir>/blocks` | **everything**, never pruned; survives restarts |
| in memory | a bounded `MemKv` | the newest **4096** heights; older blocks and their receipt index entries are deleted as new ones arrive, so `btb_getBlock` for them is an error and block sync cannot serve them |

The history is separate from the chain state and not part of consensus. A crash between executing a block and writing it can leave one height without a record (reads return `null`, sync skips it). The store also remembers the last 512 proposals of round leaders (and indexes executed blocks by consensus id) so that a node can answer consensus `SyncRequest`s for blocks its replica forgot in a restart; without that, every validator restarting at the same moment would lose the certified-but-uncommitted tip for good.

### Evidence and slashing

When a replica verifies double signing (`Evidence::DoubleVote` or `DoubleProposal`, see [CONSENSUS.md](CONSENSUS.md)), the node files it on chain:

- A validator node signs a `SubmitEvidence { epoch, evidence }` transaction with the reporter account (`reporter_key_file`, default the validator key's account; it pays `EVIDENCE_GAS` and receives the reward) and submits it like any transaction: mempool, gossip, block. See [CHAIN.md](CHAIN.md#staking-epochs-and-slashing) for the checks and the slash.
- **One submission per offender and epoch** per node (the chain slashes an operator once per epoch whatever the evidence, so a second one would only burn gas). The node never reports itself.
- Validators take turns: rank `(my index - offender index) mod n` waits `(rank - 1) * 150 ms`; before filing, a node checks the chain (`slashed_in_epoch`) and whether another node's valid `SubmitEvidence` for that offender was seen in gossip or a proposal in the last 3 s, and stands down if so. A node gives up on a report after 120 s. Failures (reporter cannot pay) are logged, not retried.
- `btb_status` has `evidence_submitted` (tx ids this node filed).

The slashed validator is jailed, so the next epoch's set (chosen at the epoch's last block) leaves it out; consensus continues with the remaining validators. The cluster test shows this with a byzantine double voter.

## JSON-RPC / HTTP

`POST /` with JSON-RPC 2.0, positional params, bytes as hex; `GET /status`. Binds to loopback only; there is no authentication or TLS (put a proxy in front).

| Method | Params | Result |
| --- | --- | --- |
| `btb_sendTransaction` | `[tx_hex]` (wire encoding, see [CHAIN.md](CHAIN.md#transactions)) | `{id}` |
| `btb_getReceipt` | `[tx_id]` | receipt (`status`, `gas_used`, `fee`, `output`, `height`) or `null` |
| `btb_getBlock` | `[height or "latest"]` | header fields, state roots, transaction ids |
| `btb_getBalance` | `[address]` | `{balance, nonce}` (balance as a string) |
| `btb_query` | `[contract, function, args_hex?, caller?]` | `{output, gas_used}`, read-only via `Executor::query` on the head state |
| `btb_getEpoch` | `[epoch?]` | validator set in consensus order (operator, key, stake at epoch start, `jailed` now), start height, anchor |
| `btb_getBlocks` | `[before?, limit?]` | newest-first block summaries (height, hash, time, tx count, proposer, gas, fees); `limit` at most 100 |
| `btb_getTransaction` | `[tx_id]` | the transaction (kind, from, to, amount, function, nonce, gas limit) with block, index and receipt, or `null` |
| `btb_getAccount` | `[address]` | balance, nonce, bonded and unbonding stake, validator info (jailed), whether it is a contract |
| `btb_getAccountTxs` | `[address, limit?, before_height?]` | newest-first transactions that involve the account (sender, recipient or contract), with status and fee |
| `btb_status` | `[]` | height, epoch, round (view), leader, peers, mempool, `last_commit_latency_ms` (submit to commit for transactions submitted at this node), consensus and network counters, evidence |

`GET /explorer` serves the embedded block explorer page ([TESTNET.md](TESTNET.md)). The explorer methods read the block history: the block store keeps an account index (`'A' || address || height || tx index`, one entry per account a transaction involves, deleted with the block when a memory node prunes).

`btb_getBlock` and `btb_getReceipt` read the block history below; for a height a memory node has pruned, `btb_getBlock` returns an error naming the range it keeps (a height not reached yet gives `null`). Queries run on the driver thread between consensus events, bounded by `query_gas`.

## Measured finality

`cargo test -p btb-node --test cluster -- --nocapture` runs 4 validators in one process over real QUIC on localhost (debug build). `FINALITY` lines are submit-to-commit as seen by the submitting node, cold submits spread over all four nodes:

| Setup | median | max |
| --- | --- | --- |
| localhost, 4 validators | ~50 ms | ~95 ms |
| across epoch boundaries (8-block epochs) | ~50 ms | ~60 ms |
| 100 ms one-way delay injected on every message (`test_one_way_delay_ms`) | ~0.7 s | ~0.8 s |

Best case is about five one-way delays: gossip to the leader, proposal, votes, the next proposal, its votes. So ~1 s holds up to roughly 200 ms one-way latency. The injected delay is an emulation on one machine; separate hosts over the internet are not measured yet. With one validator of four down, rounds it leads cost a round timeout (5 sequential transfers took about 2.8 s with a 500 ms timeout).

## Tests

`crates/btb-node/tests/cluster.rs` (about 30 s in total, run one cluster at a time):

- four nodes agree on a transfer and a deploy plus counter calls (same block hash and state root);
- a killed validator does not stop finality; restarted on its data it catches up through sync across epoch boundaries and shows no equivocation;
- a validator offline for 11 epochs of 4 blocks (more than the 8 ended instances peers keep) catches up with block sync, with identical state and blocks;
- a byzantine double voter gets one `SubmitEvidence` per node at most, exactly one accepted, is jailed and left out of the next epoch's set, and finality continues;
- the sync server's answers verify with an independently built validator context; forged, tampered, truncated, unsolicited and out-of-range sync frames are rejected or ignored without harming progress;
- with `--features rocksdb` (`cargo test -p btb-node --features rocksdb -- --test-threads=1`): the same offline-node scenario with state and history in RocksDB (history of every height present after the restart, receipts from before the restart), then a restart of all four nodes from disk;
- a validator that sends garbage, floods and signs two votes per round does not stop progress; honest nodes record double-vote evidence and count dropped frames;
- consensus instances rotate every epoch (8-block epochs);
- finality with emulated WAN latency.

Unit tests cover the payload check, frame parsing, the safety and execution files, the block record encoding and pruning, and `sync::verify` (valid chains, gaps in rounds, missing proof, forged and short QCs, QCs signed in another epoch, truncated and altered payloads, dropped blocks, wrong start).

Manual check, 4 processes built with `--features rocksdb` (`init-testnet`, epoch length 8): `kill -9` of node 3, 45 s offline (17 epochs, 139 blocks behind), restart on the same data directory. It block-synced 149 blocks, rejoined consensus, and all four nodes returned the same block at the same height; `btb_getBlock` for old heights worked on every node from disk.

## Limits

- Epoch changes work for validators that stay in the set. A node that is not in an epoch's set only observes: it does not follow the chain as a non-validator, so joining a running network at a boundary still needs state sync from a snapshot, which does not exist. Block sync has no bound on how far a *validator* can be behind, but it needs peers that kept the history (RocksDB nodes keep all of it; memory nodes 4096 heights) and a replay of every block.
- The closing proof of an epoch is built from tail blocks the node saw and from its replica's newest QC; a node that never saw them (and did not receive the epoch through block sync) cannot serve the last block of that epoch to others, who then ask another peer.
- A failed or lost `SubmitEvidence` is not retried; another validator's report, or the offender's next offence, covers it. The evidence window is the unbonding period.
- The block history is not atomic with chain state: a crash can leave one height missing. Peers' network addresses are configured by hand in `peers`; new validators' addresses are not discovered.
- Validator votes are all-to-all (`O(n^2)`), fine for tens of validators.
