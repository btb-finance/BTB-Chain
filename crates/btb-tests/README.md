# btb-tests

End-to-end tests. `tests/counter.rs`, `tests/token.rs` and `tests/dex.rs` build `examples/counter`, `examples/token` and `examples/dex` for `riscv64imac-unknown-none-elf` (needs that target and the `rust-src` component) and run them in `btb-vm`. Shared build and rv64im-check helpers live in `tests/common/mod.rs`. The first run takes about 15 seconds per example because `core` and `alloc` are rebuilt for rv64im.

`tests/todo.rs` runs `examples/todo` over `btb-runtime` with an owner index (host call 15). `tests/dev_gate.rs` is the Phase 2 gate: it starts `btb-dev` on an ephemeral port and checks that an SSE subscription pushes the new state after `add` and `toggle`.
