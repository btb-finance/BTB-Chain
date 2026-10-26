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
| `0x08` usage | `08 A` | metered bytes the contract stores (`u64` LE) |
| `0x09` deposit | `09 A` | BTB locked for those bytes (`u64` LE) |
| `0x0A` token | `0A T` | user-token info: `btb-codec` tuple `(name, symbol, decimals, supply, mint_authority, creator)` |
| `0x0B` token balance | `0B T A` | `u128` LE; absent when zero, so scanning `0B T` lists the holders in ascending address order |
| `0x0C` token nonce | `0C A` | tokens created by contract `A` (`u64` LE); the next token id uses it |
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

## Index scans from contracts

Host call 15, `table_index_scan`, exposes `query::by_index_range` to contracts (see [LANGUAGE.md](LANGUAGE.md)). `RuntimeHost` runs it on its overlay, so rows written earlier in the same call are visible and the scan is tracked: the index key range it covered (cut at the last returned entry when the limit stops it) and a point read for each row it loaded land in the read set. A subscribed query that uses an index therefore re-runs when an entry is added to, removed from or moved within the range, and not for other owners' rows. Gas: 400 + 1 per request byte + 40 per id returned + 1 per cursor byte. An unknown table or index yields an empty page.

## Declared tables and indexes

Contracts declare tables with `#[btb::table]` and indexes with `#[index]` / `#[primary]` on fields ([LANGUAGE.md](LANGUAGE.md#tables-indexes-and-lookups)). The macro exports the schema as marker symbols in the ELF and `btb_runtime::schema::Schema::from_elf` reads it, so every node derives the same indexes from the same code: `btb dev`, `btb deploy` and the chain's `Deploy` (which also validates the schema and fails the deploy if it is inconsistent) all go through `Schema::index_defs()`.

- **Types.** Field types use the text form of the [`btb-codec`](ENCODING.md) types: `u8`..`u128`, `i8`..`i128`, `bool`, `string`, `address`, `bytes`, `vec<T>`, `opt<T>`, `arr<T,N>`, tuples and table names (`Todo`).
- **Extraction.** The extractor for field `k` walks the encoded row from byte 0, skipping fields `0..k` by their encoding (fixed sizes, `u32` length prefixes, option tags) and then reads field `k`. It is bounded by the row length: zero-size vector elements and recursive types are rejected when the schema is validated, nested types are limited to depth 8, and a row that does not parse (truncated, bad tag, a count larger than the row, invalid UTF-8 in a string key) is an `Extract` fault that aborts the write.
- **Value families.** `u8`..`u64` and `bool` index as `U64`; `i8`..`i64` as `I64`; `u128` as `U128`; `string` as `Str`; `address`, `bytes` and `[u8; N]` as `Bytes`; `Option<T>` as `T`, with `None` left out of the index. The contract's `Key` for a lookup must be of the same family (the generated lookups guarantee it).
- **Unique indexes.** `IndexDef::unique()` (what `#[primary]` sets) makes `Tables::insert` and `update` check, before building the batch, that no other row has the value; a clash is `TableError::Duplicate`, which is a `Fault` for the running call, so the whole call commits nothing. The check is a one-entry range read of the index, which is tracked in the read set like any scan.
- **Host-side fallback.** `Tables::register(contract, table, indexes)` with a hand-written extractor still exists, and `Manifest.indexes` specs (`Table:index:addr@offset`) still register tables the ELF does not declare. A table in the ELF always wins over a spec.
- Adding or changing an index does not rebuild entries of rows already stored; schema migration is future work.

## Storage deposits

Metered bytes are `key.len() + value.len()` of every storage (`0x01`), row (`0x02`) and index (`0x04`) entry of the contract. Counters, balances, user-token records (`0x0A` to `0x0C`) and the usage/deposit records are not metered. When a call finishes, `RuntimeHost` compares each written key with its committed value, sums the growth and shrinkage, and settles the net delta:

- Growth locks `delta * DEPOSIT_PER_BYTE` (`layout::DEPOSIT_PER_BYTE`, 10 base units) from the **contract's own balance** into its deposit record, as TOKENOMICS says "the contract locks a deposit". The contract must hold the BTB; an app funds it by transferring to the contract.
- Shrinkage refunds the proportional share of the deposit (`deposit * freed / usage`) to the **contract's balance**. Since the contract paid, it is also repaid.
- If the balance is too low the call returns `Fault::InsufficientDeposit` and no batch is produced, so nothing changes. Overflow gives `Fault::DepositOverflow`.
- Nothing is minted or burned: balance plus deposit of a contract stays constant across storage changes.

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

`SubscriptionManager` stores each subscription (contract, function, args, read set, a SHA-256 fingerprint of the last result). For a committed `WriteBatch`, `affected` returns the subscriptions whose read set contains any written key (a put and a delete both count; a new key inside a scanned range counts too). Points live in a `BTreeMap`; ranges in a list sorted by start with a prefix maximum of ends. A batch of w keys costs about O(w log n + matches) when ranges are narrow, O(w * r) in the worst case of many overlapping wide ranges. `process_commit` re-runs each affected query through a caller closure (no ELF needed here), refreshes its read set, and reports it only if the result differs. The fingerprint is local, not consensus data.

## Optimistic transactions

A mutation runs against a snapshot (`Occ::snapshot`, the commit sequence number) with its writes in an overlay, producing a `Txn` (writes, read set). `Occ::commit` checks the log of write keys committed after the snapshot against the read set (exact keys and ranges). A hit returns `Conflict` and applies nothing; otherwise the batch is applied and the sequence number increases. Outcomes depend only on the log and the commit order, so applying transactions in block order is deterministic. Read-only transactions never conflict. `execute_with_retry(max_retries, f)` re-runs on `Conflict` up to `max_retries` extra times (`Occ::execute_with_retry` does snapshot, execute and commit around a closure). `Occ::prune(seq)` drops old log entries; transactions with an older snapshot then conflict.

## Not yet

Jellyfish Merkle commitment. (A RocksDB `KvStore` exists behind the `rocksdb` feature: `crates/btb-runtime/src/rocks.rs`.)
