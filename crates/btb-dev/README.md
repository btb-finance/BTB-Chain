# btb-dev

Single-node dev server for BTB contracts: loads a contract ELF, keeps state in `MemKv` (RocksDB with `--features rocksdb --data DIR`), and serves mutations, queries and live query subscriptions (Server-Sent Events) over plain HTTP on 127.0.0.1. Std-only HTTP, no extra dependencies beyond `sha2`. Development only: the caller identity comes from a header. See [docs/DEV_SERVER.md](../../docs/DEV_SERVER.md).

```
cargo run -p btb-dev -- examples/todo/todo.elf --port 8787
```
