# todo

Example BTB contract: a per-user todo list with declared indexes, typed lookups, `Result` errors and a generated TypeScript client.

| Kind | Function |
| --- | --- |
| mutation | `add(text: String) -> Result<u64>` (1..=280 bytes), `toggle(id: u64) -> Result<bool>` (owner only) |
| query | `list_mine() -> Vec<(u64, bool, String)>`: the caller's items via the `owner` index |
| query | `recent(limit: u32) -> Vec<Todo>`: newest first (`.order(Order::Desc).limit(n)`), full rows |
| query | `page_mine(limit, cursor, newest_first) -> (rows, cursor)`: cursor pagination (`.after(cursor).page()`) |
| query | `open_since(from: u64) -> Vec<u64>`: a range lookup on `created` (`by_created_range(from..)`) plus a filter |

The table is `Todo { #[index] owner: Address, done: bool, #[index] created: u64, text: String }`. The two indexes are declared on the fields; `btb dev`, `btb-dev` and the chain read them from the schema `#[btb::table]` exports in the ELF, so there is no `Cargo.toml` metadata and no `--index` flag. `Todo::by_owner(addr)` and `Todo::by_created_range(..)` are generated. `add` and `toggle` use `ensure!` / `bail!`: the message reaches the client as a `ContractError`.

```
./build.sh                                   # writes todo.elf (git-ignored)
cargo run -p btb-dev -- examples/todo/todo.elf   # then open http://127.0.0.1:8787/
node web/smoke.js                            # optional Playwright check (needs target/debug/btb-dev)
```

`api/` is the client `btb codegen --ts api` generates from the contract (`api.ts` for TypeScript, `api.js` + `api.d.ts` for plain pages, `todo.schema.json` is `target/btb/todo.schema.json`). They are checked in and compared with fresh output by `crates/btb-cli/tests/cli.rs` (`BTB_UPDATE_GOLDEN=1 cargo test -p btb-cli --test cli golden` regenerates them; then run `node examples/sync-sdk.mjs`). `web/index.html` has no build step: it inlines `@btb/client` and `api/api.js`, subscribes with `client.subscribe(api.list_mine, [], render)` and posts `client.mutation(api.add, [text])`. Like the other examples this crate is its own workspace and only builds for `riscv64imac-unknown-none-elf`; see `examples/counter/README.md` for why.
