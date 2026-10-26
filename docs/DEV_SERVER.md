# Dev server (`btb-dev`)

`btb-dev` is a single-node server for trying contracts with the Phase 2 state database. It is the Phase 2 gate demo ("query a table by index and watch a UI update live"). The Phase 3 `btb dev` ([CLI.md](CLI.md)) uses it as a library and adds hot reload; the plain binary has no hot reload, no deploy, no auth. **Development only. Never expose it.**

```
./examples/todo/build.sh                      # builds examples/todo/todo.elf
cargo run -p btb-dev -- examples/todo/todo.elf --port 8787
# open http://127.0.0.1:8787/ in two tabs
```

Flags: `--port N` (default 8787, 0 = any free port), `--index Table:index:addr@offset` (deprecated, repeatable, see below), `--data DIR` (RocksDB; needs `cargo run -p btb-dev --features rocksdb`, default is memory and state is lost on exit).

## HTTP API

| Request | What it does |
| --- | --- |
| `POST /mutation/<name>` | Runs a mutation as an optimistic transaction (up to 3 retries), commits it, then re-runs every subscribed query whose read set was touched |
| `GET /query/<name>` | Runs a query once |
| `GET /subscribe/<name>` | Server-Sent Events. First event: the current result. Then one event each time a commit changes the result (`SubscriptionManager`: read-set invalidation, re-run, compare fingerprints) |
| `GET /dashboard`, `GET /admin/...` | Read-only table browser and JSON API, see [Dashboard](#dashboard-and-admin-endpoints) |
| `GET /` | The todo demo page (`examples/todo/web/index.html`, embedded in the binary) |

Failures return JSON `{"error": "..."}`: 400 (abort or bad arguments), 404 (no such function), 405, 409 (conflict after retries), 500 (storage fault). A failed call changes nothing and pushes nothing.

**Arguments** are the `btb-codec` bytes of the argument tuple ([ENCODING.md](ENCODING.md)), as hex in `?args=` or the POST body. As a convenience, a POST with `Content-Type: application/json` takes a flat array of strings, non-negative integers and booleans (encoded as `String`, `u64`, `bool`): `["buy milk"]`, `[3]`.

**Results** are hex; with `?format=json` the todo demo functions (`list_mine`, `add`, `toggle`) are shown as JSON and everything else as `{"hex": "..."}`. The JSON forms live in `crates/btb-dev/src/demo.rs`.

**Caller identity (dev only).** There is no signature check. The caller is the `X-BTB-Caller` header, or `?caller=` because `EventSource` cannot set headers. 64 hex characters are used as the 32-byte address; any other text (`alice`) is hashed with SHA-256. No value means `anonymous`. A subscription keeps the caller it was opened with and re-runs as that caller.

## Index declarations

Contracts declare their own indexes: `#[index]` / `#[primary]` on the fields of a `#[btb::table]` struct ([LANGUAGE.md](LANGUAGE.md#tables-indexes-and-lookups)). The macro exports the schema in the ELF and the dev server registers the indexes from it when it starts and on every hot reload (`btb_runtime::schema`), so there is nothing to configure: the todo demo's `list_mine` reads through the `owner` index of the contract it is given. A `#[primary]` field is unique: a second row with the same key aborts the call. Rows written before an index was registered are not indexed (a reload that adds an index does not re-index existing rows; restart for a clean state).

The old host-side flag `--index Table:index:kind@offset` (the value is the `addr` or `u64` at byte `offset` of the row) and `[package.metadata.btb] indexes` in `btb dev` projects are **deprecated**. They still register an index for a table the ELF does not declare, and `btb dev` / `btb deploy` print a warning when the metadata is present; for a table the ELF declares they are ignored. Only a contract that declares no table at all gets the old `Todo:owner:addr@0` default.

## State and limits

One contract at a time, at the fixed address `[0xDE; 32]`, funded with 10^12 base units to pay storage deposits. The block height is a counter that goes up by one per committed mutation (starts at 1 on every run). Each call has a 50,000,000 gas limit and 1 MiB of VM memory. Crons and actions are not run.

## Library API (used by `btb dev`)

`Server::start(elf, bind, store, indexes)` starts a server. `server.reload(elf)` validates a new build and swaps it in while keeping all state (the `KvStore`, block height and subscriptions); on an invalid ELF the old contract keeps running. After a swap every open subscription re-runs against the new code and pushes its result if it changed (a stream whose query no longer exists ends). `server.set_page(Some(html))` serves a project's own page at `/` instead of the todo demo. `indexes` is the deprecated list of host-side specs (pass `vec![]`: the ELF's schema registers the rest).

## Dashboard and admin endpoints

`GET /dashboard` serves a single embedded page (inline CSS and JS, no build step, no CDN) that browses the loaded contract like a database. `btb dev` serves it too. It shows:

- **Contract**: address, block height, BTB balance, bytes of state stored (usage), the locked storage deposit, number of storage keys, and the Merkle root. The dev server does not maintain a Merkle tree, so the root is reported as `not tracked` (the `version` is the block height).
- **Tables**: discovered from the key layout (every `0x02` row prefix and `0x03` counter of the contract) with row counts. Click a table for its rows, paged (10/25/100), ascending or descending by id. Each row shows raw hex and, if a field hint exists, decoded values.
- **Indexes**: the registered indexes of the selected table. Pick one and enter a value (an address as 64 hex characters, or a `u64`) to see the matching rows, or leave it empty to list index entries (order-preserving encoded value in hex, row id).
- **Storage keys** (`0x01`) with hex and text, and **tokens** created by the contract with their top holders.
- **Live**: the page opens `GET /admin/events`; when a commit touches the selected table the rows reload, and the list, counts and info refresh on every commit.

**Field hints** tell the dashboard how to decode rows with the [ENCODING.md](ENCODING.md) rules. The row is a tuple of the listed types: `addr bool string bytes u8 u16 u32 u64 u128 i8 i16 i32 i64 i128`. Without hints they are **derived from the contract's schema** whenever every field has one of those types (a table with a `Vec<_>` field has no automatic hint). Override them with `--fields Table:addr,bool,string` (repeatable), or in a `btb dev` project:

```toml
[package.metadata.btb]
fields = ["Note:addr,bool,string"]
```

A row that does not match its hint (wrong type or trailing bytes) shows `decode_error` next to its hex. Without a hint (explicit or derived) only hex is shown. Library: `Server::set_fields(...)`, `parse_fields(...)`.

### `/admin` JSON API (read-only, development only)

Every endpoint is `GET` and returns JSON; other methods return 405. There is no authentication; never expose the server. `limit` is 1..500 (default 25), `cursor` is the opaque hex `next_cursor` of the previous page (`null` when there is no more), `order` is `asc` or `desc`. Row ids and amounts above 32 bits are strings in `decoded`.

| Request | Returns |
| --- | --- |
| `/admin/overview` (also `/admin/tables`) | `contract`, `height`, `balance`, `storage_usage`, `storage_deposit`, `deposit_per_byte`, `storage_keys`, `merkle {tracked, root, version}`, `tables [{name, rows, indexes, fields}]` |
| `/admin/table/<name>?limit&cursor&order` | `{table, total, rows [{id, hex, bytes, decoded?, decode_error?}], next_cursor}` |
| `/admin/table/<name>/<id>` | One row |
| `/admin/index/<table>/<index>?value=V&limit&cursor&order` | With `value`: matching rows as above. Without: `entries [{value (hex of the encoded index value), id}]` |
| `/admin/storage?limit&cursor` | `keys [{key, key_text, hex, bytes}]` |
| `/admin/tokens` | `tokens [{id, name, symbol, decimals, supply, creator, holders [{holder, balance}]}]` |
| `/admin/events` | Server-Sent Events: first the current `height`, then one event per commit that wrote state: `{"height", "tables": [touched table names], "storage", "tokens", "accounts"}` |

The table scan for row counts is linear in the number of rows, which is fine for development data.

## Tests

`crates/btb-tests/tests/dev_gate.rs` is the automated gate: it starts the server on an ephemeral port with the real `todo` contract, opens an SSE stream, posts `add` and `toggle`, and asserts the new event arrives (and that another user's change pushes nothing). `examples/todo/web/smoke.js` is an optional Playwright check of the page in Chromium. `crates/btb-tests/tests/dev_dashboard.rs` covers the dashboard and `/admin` endpoints (tables and counts, paging, decoding, index browse, live event after commit); `crates/btb-dev/web/smoke.js` is the Playwright check of the dashboard page.
