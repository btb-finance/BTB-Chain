# btb-runtime

Phase 2 state database for BTB Chain. Depends only on `btb-vm`.

- `kv`: `KvStore` trait (get, ordered `scan`, atomic `commit(WriteBatch)`), `MemKv` (BTreeMap) and `Overlay` (uncommitted writes over a shared store; drop it to roll back). A RocksDB backend is planned as another `KvStore` impl behind a cargo feature (for example `rocksdb`). It is NOT added yet, so the crate has no native dependencies.
- `layout`: the deterministic key layout and order-preserving index encodings. See [docs/STATE.md](../../docs/STATE.md).
- `tables`: `Tables` registry (table name + indexes, each an extractor over raw row bytes) with `insert`/`get`/`update`/`delete`. Each write is one `WriteBatch`; extractors run first, so a failure writes nothing.
- `query`: `by_id`, `scan_table`, `by_index_eq`, `by_index_range` with filter, limit, ascending/descending and opaque cursor pagination.
- `host`: `RuntimeHost`, a `btb_vm::Host` over any `KvStore`. Writes go to an overlay; `finish()` returns the `WriteBatch` to commit, or the fault that makes the call invalid.

- `merkle`: Jellyfish-style sparse Merkle tree (16-ary over `SHA-256(key)`, versioned, nodes stored in the `KvStore`): `apply`/`prepare`, `root`, `get_with_proof`, `verify` with inclusion and non-inclusion proofs. Uses `sha2` (MIT/Apache). See [docs/STATE.md](../../docs/STATE.md).

Not done yet in Phase 2: wiring the commitment into block commit, pruning, read-set tracking and subscriptions, optimistic transactions, storage deposits.
