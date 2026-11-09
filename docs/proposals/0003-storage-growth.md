# 0003: Keep node storage small: compression, pruning, tiers, snapshots, smaller transactions

Status: **Draft; phase A built** (zstd storage, see below). Issue: to be opened by a maintainer as a Design Proposal. Phases C to G touch state storage, sync or transaction formats and need the Safety section and review before code.

## Summary

Today every validator keeps every block and every past version of the state forever, uncompressed. Measured on a 4-validator chain, an **idle** network grows about **0.83 GB per node per day** (about 300 GB a year) and a busy one about 657 bytes per transfer. Follow what the internet does: compress what is kept, keep only what is needed where it is needed, put old data on cheap erasure-coded storage, start new nodes from verified snapshots, and send fewer random bytes per transaction. **Recommendation:** A (compression, built) and B (fewer empty blocks) now; C (prune old state versions) and D (snapshots and state sync) next; E (history tiers) after the public testnet; F (short account numbers) and G (signature aggregation) as consensus changes later; H (stateless validators) as research.

## Measurements

4 validators, release build, RocksDB, on one machine. Sizes are the database files (`du --apparent-size`), node 0.

| | Before (no compression) | After phase A (zstd) |
| --- | --- | --- |
| Idle state growth per block | 1,369 B | 575 B |
| Idle history growth per block | 1,279 B | 1,100 B |
| Idle total per day (3.9 blocks/s) | ~0.83 GB | ~0.57 GB |
| 10,000 transfers: state | +0.83 MB | +0.32 MB |
| 10,000 transfers: history | +5.88 MB | +2.10 MB |
| **Per transfer** | **657 B** | **238 B (2.8x smaller)** |
| Throughput in the same test | ~2,650 tx/s | ~2,700 tx/s |

Offline compression of the raw files (whole file, not per block): state 7.2x (zstd 3) to 7.5x (zstd 22), history 3.8x to 4.5x (xz 9e); per 4 KiB block only 2.1x to 2.7x, with or without a trained dictionary. Hashes, keys and signatures (random bytes) are most of a transfer and do not compress. Hashing speed here: SHA-256 1.07 GB/s (SHA hardware), BLAKE3 5.07 GB/s on one core and 14.1 GB/s on all; for 64-byte items the two are equal. Validators ran at about 10% CPU at 2,600 tx/s: hashing is not the bottleneck.

## Where the bytes come from

| Data | Today | Grows with |
| --- | --- | --- |
| Live state (balances, contracts, tables, names) | Paid for by storage deposits | Real usage: bounded by what people pay to keep |
| Old state versions | Merkle nodes are content-addressed and never deleted ([STATE.md](../STATE.md)) | **Every block, even empty ones** |
| Block history | RocksDB nodes keep every block, receipt and account index | Every block and every transaction |
| Empty blocks | One every 250 ms (`idle_block_ms`) so crons run | Time, not usage |

## Ideas taken from outside blockchains

| Idea | Where it is used | For BTB |
| --- | --- | --- |
| Compression with large blocks | every database | phase A |
| Do less when idle | timers, power management | phase B |
| Keep only the latest value per key (log compaction) | Kafka | phase C |
| Keyframes plus deltas | video codecs, git packs, rsync | phases C and D: a snapshot per epoch, only changes between |
| Hot, warm and cold tiers; erasure coding instead of copies | Facebook f4 (replication 3.6x → 2.1x), S3 → Glacier | phase E: old history and snapshots on storage nodes |
| Columnar storage, delta encoding, bit packing | Parquet, ClickHouse, Gorilla (96% of timestamps in 1 bit) | phase E: archived history by column |
| Store each piece once | git, IPFS, backup deduplication (FastCDC) | phase E: contract code, sites and files by hash |
| Short ids instead of long names | database row ids, Solana lookup tables | phase F |
| Many signatures in one | BLS aggregation | phase G |
| Do not store what you can be proven | Ethereum's stateless clients (Verkle witnesses) | phase H |
| Data with a lifetime | caches, DNS TTL | phase C: tables with an expiry, deposit refunded |

Not used: neural (LLM) compression. It reaches 12x to 15x on text, but BTB data is mostly hashes and numbers, it is very slow, and every node would need bit-identical results.

## Phases

### A. Compressed storage (built)

`btb-runtime`'s RocksDB store uses zstd on every level and on the write-ahead log, 32 KiB blocks, and zstd level 9 with a trained dictionary on the bottom level. Local to each node: state roots, blocks and consensus are unchanged, nodes with and without it agree, and files written before stay readable. `zstd` is BSD-licensed (dual BSD/GPL-2.0, used under BSD).

### B. Fewer empty blocks

`idle_block_ms` (node config) already sets how often an idle chain makes a block; 1,000 ms was measured to work (one block per second, also after restarting every validator). Lowering the default for public networks cuts idle growth about 4x but changes the wall-clock meaning of `every = "N blocks"` crons while the chain is idle. Options: keep crons in blocks and document the idle rate; or let the leader skip empty blocks unless a cron is due (a cron index already exists), so idle chains make almost no blocks. Decision needed before changing defaults.

### C. Prune old state versions

Keep the last `K` state versions (for example one epoch) and delete Merkle nodes no kept version references. Needs reference counting or a mark-and-sweep pass over the kept roots, and an archive mode that never prunes. Hosting nodes that serve proofs at old heights keep a longer window. Optional contract tables with an expiry height (deposit refunded when rows expire).

### D. Snapshots and state sync

At every epoch end the state root is already certified by the validators (execution attestation). A node exports the state at that root in chunks; a new node downloads chunks from several peers, checks each against the certified root with Merkle proofs, then block-syncs only from there. This also lets new validators and hosting nodes join a running network ([NODE.md](../NODE.md#limits)).

### E. History tiers

Validators keep recent blocks (for example two epochs). Older history goes to storage nodes as erasure-coded files (the storage layer already does this for user files), stored by column (heights, nonces, gas and amounts delta-encoded and bit-packed; hashes and signatures in their own columns) and compressed; content (code, site files) deduplicated by hash. Archive nodes and explorers keep everything if they want to. Hash big content (files, snapshots) with BLAKE3; keep SHA-256 for consensus.

### F. Short account numbers (consensus change)

Give each account a sequence number when it first appears; transactions and stored history may name accounts by number (4 to 8 bytes) instead of 32-byte addresses. Assigned in block order, so deterministic.

### G. Signature aggregation (consensus change)

BLS-aggregate the signatures of a block's transactions into one. Interacts with key-slot accounts (proposal 0002) and with verification cost; needs its own design.

Estimated effect of F and G on a transfer's random bytes: from about 160 (two addresses, a key and a signature) to about 10 to 20. Not measured.

### H. Stateless validators (research)

Blocks carry witnesses for the state they touch, so validators need almost no state on disk. Large redesign (witness format, vector commitments).

### I. BLAKE3 and a pure-Rust storage engine (evaluation)

- **BLAKE3.** 5x faster than SHA-256 on large inputs on one core and 13x on all cores (measured above), parallel and with a built-in tree mode for verifying parts of a file. Equal for small items such as Merkle nodes. Plan: BLAKE3 for large content first (storage-node pieces and file ids, snapshots, archived history), which is local to those subsystems. Switching consensus hashes (transaction ids, block hashes, state tree, addresses) changes every id and address: decide once, before mainnet, together with any other hash change, since it is a hard fork afterwards. SHA-256 has hardware instructions on most CPUs; BLAKE3 has none but does not need them.
- **redb instead of RocksDB.** redb is a pure-Rust embedded key-value store (B-tree, ACID, MVCC, MIT/Apache-2.0). Gains: no C++ build (RocksDB needs clang and compiles for minutes; this session also ran out of disk building it), smaller attack surface, simpler crash model. Costs: mainline redb has **no built-in compression**, so the phase A gains would have to come from compressing values ourselves (or a fork such as `shodh-redb` or `redb-turbo`, both young); a B-tree rewrites pages in place, while RocksDB's LSM is built for heavy write streams like block history. `KvStore` is already an interface (`MemKv`, `RocksKv`), so a `RedbKv` can be added next to RocksDB and measured with the same tests (10,000-transfer flood, idle growth, restart of all validators) before choosing. Benchmarks published by the projects themselves disagree by version and hardware.

## Safety

- **A** changes only how each node stores bytes; nothing it stores is used in consensus differently. Tested by the full RocksDB suite (restart of all four nodes from disk included).
- **C and E** must never delete data a node still needs to vote, serve sync or answer proofs it advertises; pruning windows are counted in heights, archive mode is explicit, and a pruned node says so in its RPC errors (as memory nodes already do).
- **D** accepts only chunks proven against a root certified by a quorum of the epoch's validators; a node that cannot verify a chunk asks another peer.
- **F and G** change transaction validity: own proposals, failure-case tests, audit.

## Tests

- A: RocksDB cluster tests (`cargo test -p btb-node --features rocksdb --test cluster`), before/after measurement above.
- C: after pruning, every kept root still verifies and answers proofs; a pruned root gives a clear error; supply and state roots are identical to an unpruned node.
- D: a node synced from a snapshot reaches the same state root as one that replayed from genesis; a forged chunk is rejected.
- E: history fetched back from storage nodes is byte-identical to the original blocks.

## Open questions

1. Idle blocks: lower `idle_block_ms`, or skip empty blocks unless a cron is due?
2. Pruning window `K` for validators and for hosting nodes.
3. Who pays storage nodes for archived history: the protocol (fee share), or archive users?
4. **Observed once, not reproduced:** after restarting all four validators of a local chain that had first been restarted with `idle_block_ms = 1000`, the chain stopped making blocks (all nodes at the same height and round, leader not proposing). Fresh restarts with the default and with 1,000 ms, and after a 10,000-transaction flood, all resumed; the RocksDB restart test passes. Needs investigation before a public testnet.
