# btb-sdk

Rust SDK for BTB contracts (`no_std` + `alloc`). Depends on `btb-codec` and `btb-sdk-macros`.

- `btb_sdk::btb`: the attribute macros (`use btb_sdk::btb;`, then `#[btb::query]`, `#[btb::mutation]`, `#[btb::cron]`, `#[btb::action]`, `#[btb::transform("action name")]`, `#[btb::table]`)
- `btb_sdk::host`: safe wrappers over every host call in `btb_vm::host::abi`. Inline-asm `ecall` on `riscv64`, a panic elsewhere so the workspace builds on x86
- `btb_sdk::table::Table`: `insert`, `get`, `update`, `delete`, `scan`, `index_eq` for `#[btb::table]` structs; `host::table_index_scan` reads a secondary index (host call 15)
- feature `runtime` (only with `target_os = "none"`): panic handler that calls `abort`, 128 KiB bump allocator, `mem*` routines in rv64im assembly

See `docs/LANGUAGE.md` and `examples/counter`. End-to-end tests: `crates/btb-tests`.
