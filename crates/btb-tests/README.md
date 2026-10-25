# btb-tests

End-to-end tests. `tests/counter.rs` builds `examples/counter` for `riscv64imac-unknown-none-elf` (needs that target and the `rust-src` component) and runs it in `btb-vm`. The first run takes about 15 seconds because `core` and `alloc` are rebuilt for rv64im.
