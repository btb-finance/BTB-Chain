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

## Not yet

RocksDB backend (behind a cargo feature), Jellyfish Merkle commitment, read sets and subscriptions, optimistic transactions, storage deposits.
