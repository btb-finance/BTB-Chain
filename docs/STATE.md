# State layout

How `btb-runtime` stores everything in one ordered key-value keyspace. Source of truth: `crates/btb-runtime/src/layout.rs`.

`A` is a 32-byte address. Integers are big-endian so byte order equals numeric order. Names (`table`, `index`) are prefixed with their length as `u32` BE, so one name's keys never fall in another's range.

| Tag | Key | Value |
| --- | --- | --- |
| `0x01` storage | `01 A key` | raw bytes |
| `0x02` row | `02 A len table id:u64` | row bytes |
| `0x03` counter | `03 A len table` | next row id (`u64` LE) |
| `0x04` index | `04 A len table len index enc(value) id:u64` | empty |
| `0x05` balance | `05 A` | `u64` LE |
| `0x06` merkle node | `06 node_hash` | node encoding (see State commitment) |
| `0x07` merkle root | `07 version:u64` | 32-byte root |

Row ids are monotonic per table and never reused. The row id is the last 8 bytes of row and index keys, so scanning a prefix returns rows in id order (or in (value, id) order for an index) and a cursor is just the last key seen.

## Index value encodings (order-preserving)

- `u64`: 8 bytes BE. `u128`: 16 bytes BE.
- `i64`: flip the sign bit, then 8 bytes BE (negatives sort first).
- `bytes` / `string`: each `0x00` becomes `0x00 0xFF`, then the terminator `0x00 0x00`. Prefix-free and bytewise ordered.

An index must always yield the same type; the type is not stored.

## Writes and atomicity

An insert, update or delete is one `WriteBatch` holding the row, the counter and every index change. Index extractors run before anything is written. A contract call runs against an overlay and commits one batch only if it finishes without a fault, so a trapped call changes nothing.

## Queries

By id, by index equality or range, with a filter, a limit and cursor pagination in either direction. `next_cursor` is present only when more matching rows exist.

## State commitment

`crates/btb-runtime/src/merkle.rs` commits the state to one 32-byte root per version (block/commit number) with a Jellyfish-style sparse Merkle tree. It is a separate layer: `merkle::apply(kv, version, &WriteBatch)` takes the same state writes and updates the tree; callers decide which keys are committed.

- Path: the 64 nibbles of `SHA-256(key)`; 16 children per node.
- Empty subtree and empty root: 32 zero bytes.
- Leaf: `SHA256(0x00 || key_hash || SHA256(value))`. A subtree with exactly one entry is a single leaf at its shallowest position, so the shape depends only on the set of entries.
- Internal node (two or more entries): `SHA256(0x01 || child_0 .. child_15)`.
- Stored nodes (tag `0x06`, key = node hash): leaf `00 key_hash value_hash value`, internal `01 16*32 bytes` (empty child = zeros). Nodes are content-addressed and never overwritten, so every version stays readable; `0x07 version` maps to its root. Versions must strictly increase. Pruning is not implemented.
- A batch is merged per key (later op wins) and applied in key-hash order, so the root does not depend on the order of writes to different keys.
- `get_with_proof(kv, version, key)` returns the value and a `Proof` (15 siblings per internal node on the path, plus the leaf where the path ends). `verify(root, key, value, &proof)` handles inclusion (`Some(value)`) and non-inclusion (`None`; the path ends at an empty slot or another key's leaf on the same path).

## Read sets

`Overlay` records every read of a call: each `get` adds an exact key, each `scan` adds the key range it covered. A scan stopped by its limit covers only `start..=last returned key` (or `last key..end` in reverse), so writes beyond the cut-off do not invalidate it. `RuntimeHost::finish_tracked` returns the writes and the `ReadSet`. Table gets, scans and index ranges all go through these two calls, so they are covered without extra code.

## Subscriptions

`SubscriptionManager` stores each subscription (contract, function, args, read set, a fingerprint of the last result). For a committed `WriteBatch`, `affected` returns the subscriptions whose read set contains any written key (a put and a delete both count; a new key inside a scanned range counts too). Points live in a `BTreeMap`; ranges in a list sorted by start with a prefix maximum of ends. A batch of w keys costs about O(w log n + matches) when ranges are narrow, O(w * r) in the worst case of many overlapping wide ranges. `process_commit` re-runs each affected query through a caller closure (no ELF needed here), refreshes its read set, and reports it only if the result differs. The fingerprint is a local 128-bit non-cryptographic hash; it is not consensus data.

## Optimistic transactions

A mutation runs against a snapshot (`Occ::snapshot`, the commit sequence number) with its writes in an overlay, producing a `Txn` (writes, read set). `Occ::commit` checks the log of write keys committed after the snapshot against the read set (exact keys and ranges). A hit returns `Conflict` and applies nothing; otherwise the batch is applied and the sequence number increases. Outcomes depend only on the log and the commit order, so applying transactions in block order is deterministic. Read-only transactions never conflict. `execute_with_retry(max_retries, f)` re-runs on `Conflict` up to `max_retries` extra times (`Occ::execute_with_retry` does snapshot, execute and commit around a closure). `Occ::prune(seq)` drops old log entries; transactions with an older snapshot then conflict.

## Not yet

RocksDB backend (behind a cargo feature), Jellyfish Merkle commitment, storage deposits.
