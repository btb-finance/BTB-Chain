# Validator node: `btb-node`

`crates/btb-node` is the binary that turns the pieces into a network: it runs one validator, wiring `btb-consensus` (the `Replica`) to `btb-chain` (the `Executor` and `Mempool`) over libp2p/QUIC, and serves a local JSON-RPC API.

Status: a working multi-validator testnet node. Not done: validators joining or leaving at an epoch boundary (see [Limits](#limits)), a public faucet/explorer, state sync from snapshots.

## Run a local 4-validator testnet

```
cargo build --release -p btb-node            # add --features rocksdb to keep chain state on disk
./target/release/btb-node init-testnet --validators 4 --dir /tmp/btb-net
for i in 0 1 2 3; do ./target/release/btb-node run --config /tmp/btb-net/node$i/node.json & done
curl -s 127.0.0.1:8545/status                # node 0; nodes 1..3 use ports 8546..8548
```

`init-testnet [--chain-id N] [--base-port 9000] [--rpc-base-port 8545]` writes `genesis.json`, `faucet.key` (an account holding everything except the validators' 1,000,000 BTB each) and `nodeN/{validator.key,node.json}`. Validator `i` listens on UDP `base-port + i`. Each validator bonds 100,000 BTB; the epoch is 600 blocks. Edit `genesis.json` (`epoch_len`, `min_stake`, `gas_price`, allocations) before the first start; the validator set and balances are committed in the genesis hash, and the consensus signature domain depends on it.

Without `--features rocksdb` the node keeps chain state in memory and **refuses a `data_dir`** (the consensus safety state would survive a restart while the chain would not). With the feature, `data_dir` holds the RocksDB database (`chain/`) and the crash-safety files below, and a restarted node resumes and catches up.

### Config (`node.json`)

| Field | Meaning |
| --- | --- |
| `key_file` | `{"secret": hex}`; the same Ed25519 key signs consensus messages and is the libp2p identity |
| `genesis` | path to `genesis.json` |
| `listen` | `/ip4/127.0.0.1/udp/9000/quic-v1` |
| `peers` | other validators as `/ip4/.../udp/.../quic-v1/p2p/<peer id>`; `btb-node peer-id --key FILE` prints a peer id |
| `rpc` | bind address, loopback only |
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

### Crash safety

Two files per node in `data_dir`, each replaced atomically (temp file, fsync, rename):

- `safety-<epoch>.bin`: the consensus `SafetyState`. A failed save aborts the node; a corrupt file refuses to start instead of resetting (a reset could double vote).
- `exec.bin`: last executed consensus block, with a write-ahead entry around each execution.

The replica persists "block h committed" before it emits the `Commit`, so a crash in between would lose h. The safety file therefore records the commit point of what was *executed*, never ahead of the chain; after a restart the replica re-commits (re-fetching blocks by sync) whatever the chain still lacks, and the node skips heights already executed. An interrupted execution is settled from the chain head height.

**Without persistent storage a restarted validator is a brand-new replica with the same key and can equivocate.** Only run in-memory nodes for tests.

## JSON-RPC / HTTP

`POST /` with JSON-RPC 2.0, positional params, bytes as hex; `GET /status`. Binds to loopback only; there is no authentication or TLS (put a proxy in front).

| Method | Params | Result |
| --- | --- | --- |
| `btb_sendTransaction` | `[tx_hex]` (wire encoding, see [CHAIN.md](CHAIN.md#transactions)) | `{id}` |
| `btb_getReceipt` | `[tx_id]` | receipt (`status`, `gas_used`, `fee`, `output`, `height`) or `null` |
| `btb_getBlock` | `[height or "latest"]` | header fields, state roots, transaction ids |
| `btb_getBalance` | `[address]` | `{balance, nonce}` (balance as a string) |
| `btb_query` | `[contract, function, args_hex?, caller?]` | `{output, gas_used}`, read-only via `Executor::query` on the head state |
| `btb_getEpoch` | `[epoch?]` | validator set in consensus order, start height, anchor |
| `btb_status` | `[]` | height, epoch, round (view), leader, peers, mempool, `last_commit_latency_ms` (submit to commit for transactions submitted at this node), consensus and network counters, evidence |

Blocks and receipts are kept in memory for the last 4096 blocks (not persisted). Queries run on the driver thread between consensus events, bounded by `query_gas`.

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
- a validator that sends garbage, floods and signs two votes per round does not stop progress; honest nodes record double-vote evidence and count dropped frames;
- consensus instances rotate every epoch (8-block epochs);
- finality with emulated WAN latency.

Unit tests cover the payload check, frame parsing, and the safety and execution files.

## Limits

- Epoch changes work for validators that stay in the set. A node that is not in an epoch's set only observes (no chain sync for non-validators yet), so joining a running network at a boundary needs state sync, which does not exist. A node more than 8 epochs behind cannot catch up.
- Double-vote evidence is logged and exposed in `btb_status`; the node does not yet submit it as a `SubmitEvidence` transaction.
- Block history, receipts and peers' addresses are not persisted; new validators' network addresses are configured by hand in `peers`.
- Validator votes are all-to-all (`O(n^2)`), fine for tens of validators.
