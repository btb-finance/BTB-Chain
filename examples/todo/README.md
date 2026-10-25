# todo

Example BTB contract: a per-user todo list that reads its rows through a secondary index (host call 15, `table_index_scan`).

| Kind | Function |
| --- | --- |
| mutation | `add(text: String) -> u64` (1..=280 bytes), `toggle(id: u64) -> bool` (owner only) |
| query | `list_mine() -> Vec<(u64, bool, String)>`: the caller's items via the `owner` index |

The table is `Todo { owner: Address, done: bool, text: String }`. SDK tables are named after the struct, so the table is `Todo`. The `owner` index is registered on the host side for now (`btb-dev --index Todo:owner:addr@0`, the default); see [docs/DEV_SERVER.md](../../docs/DEV_SERVER.md).

```
./build.sh                                   # writes todo.elf (git-ignored)
cargo run -p btb-dev -- examples/todo/todo.elf   # then open http://127.0.0.1:8787/
node web/smoke.js                            # optional Playwright check (needs target/debug/btb-dev)
```

`web/index.html` has no build step: it subscribes to `list_mine` with `EventSource` and posts `add` / `toggle`. Like the other examples this crate is its own workspace and only builds for `riscv64imac-unknown-none-elf`; see `examples/counter/README.md` for why.
