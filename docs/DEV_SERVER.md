# Dev server (`btb-dev`)

`btb-dev` is a single-node server for trying contracts with the Phase 2 state database. It is the Phase 2 gate demo ("query a table by index and watch a UI update live"). The Phase 3 `btb dev` ([CLI.md](CLI.md)) uses it as a library and adds hot reload; the plain binary has no hot reload, no deploy, no auth. **Development only. Never expose it.**

```
./examples/todo/build.sh                      # builds examples/todo/todo.elf
cargo run -p btb-dev -- examples/todo/todo.elf --port 8787
# open http://127.0.0.1:8787/ in two tabs
```

Flags: `--auth header|signed` (default `header`, see [Signed mode](#signed-mode)), `--port N` (default 8787, 0 = any free port), `--index Table:index:addr@offset` (deprecated, repeatable, see below), `--data DIR` (RocksDB; needs `cargo run -p btb-dev --features rocksdb`, default is memory and state is lost on exit).

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

**Caller identity (dev only).** In the default `header` mode there is no signature check. The caller is the `X-BTB-Caller` header, or `?caller=` because `EventSource` cannot set headers. 64 hex characters are used as the 32-byte address; any other text (`alice`) is hashed with SHA-256. No value means `anonymous`. A subscription keeps the caller it was opened with and re-runs as that caller.

## Signed mode

`btb-dev --auth signed` lets apps test the real auth flow locally; the default `header` mode is unchanged. In signed mode:

- `POST /mutation/<name>` needs `X-BTB-Tx: <hex of a signed Call transaction>` (the format of [CHAIN.md](CHAIN.md#transactions), chain id 1337, to the dev contract `0xDE..DE`, the same function name). Without it the answer is 401. The transaction's arguments are used, `caller()` is its sender (the account, for a session key) and `via_session` is set accordingly.
- `POST /auth/tx` takes a signed `Session` grant or revoke (hex body), `GET /auth/info?account=<hex>` returns `{chain_id, height, nonce}` (height counts mutations).
- The rules are the chain's own (`btb_chain::session`): nonce per account, scope (contract and function), expiry in dev heights, fee budget (the fee charged is `gas_limit * gas_price`, the reservation, since dev has no balances), no sessions from sessions. Refusals are 401 with the reason.
- Queries and subscriptions still take the caller from `X-BTB-Caller`/`?caller=` (reads are not signed on the chain either). `@btb/client`: `new BtbClient({ url, auth: new BtbAuth({ transport: devTransport(url), ... }) })` ([TS_SDK.md](TS_SDK.md#auth-session-keys)).

## Index declarations

Contracts declare their own indexes: `#[index]` / `#[primary]` on the fields of a `#[btb::table]` struct ([LANGUAGE.md](LANGUAGE.md#tables-indexes-and-lookups)). The macro exports the schema in the ELF and the dev server registers the indexes from it when it starts and on every hot reload (`btb_runtime::schema`), so there is nothing to configure: the todo demo's `list_mine` reads through the `owner` index of the contract it is given. A `#[primary]` field is unique: a second row with the same key aborts the call. A hot reload now evolves the schema like a chain upgrade ([below](#schema-evolution-on-reload)).

The old host-side flag `--index Table:index:kind@offset` (the value is the `addr` or `u64` at byte `offset` of the row) and `[package.metadata.btb] indexes` in `btb dev` projects are **deprecated**. They still register an index for a table the ELF does not declare, and `btb dev` / `btb deploy` print a warning when the metadata is present; for a table the ELF declares they are ignored. Only a contract that declares no table at all gets the old `Todo:owner:addr@0` default.

## Schema evolution on reload

On every hot reload the dev server compares the schema of the running build with the new one (`btb_runtime::evolve::diff`, the same code the chain runs for an upgrade, see [CHAIN.md](CHAIN.md#contract-upgrades-and-migrations)) and prints what changed on stderr (`btb dev: schema: ...`):

- **Compatible changes** keep the data: a new table, `Option` fields appended at the end of a row, and **new indexes, which are backfilled for the rows written by earlier builds** (removed indexes have their entries deleted).
- **Incompatible changes** (a field removed, reordered or retyped, a removed primary) need a `#[btb::migration(from = N)]` ([LANGUAGE.md](LANGUAGE.md#schema-evolution-versions-and-migrations)); `btb dev` runs it over every row at once.
- Everything is computed into one write batch and committed atomically. If the change is incompatible without a migration, or the migration fails on a row (error names table and row id), the reload is refused (`btb: reload failed, keeping the old build: ...`), the old build keeps running and the data is untouched. There is no gas, budget or table lock in dev.
- State persisted with `--data` is diffed against the build that was running in this session; a build started fresh on old data trusts that the data matches it.

## State and limits

A single-contract server hosts its contract at the fixed address `[0xDE; 32]`; a multi-contract server (see [Several contracts](#several-contracts)) gives each contract the address `SHA256("btb.dev.contract.v1" || name)`. Every contract is funded with 10^12 base units to pay storage deposits. The block height is a counter that goes up by one per committed mutation (starts at 1 on every run). Each call has a 50,000,000 gas limit and 1 MiB of VM memory. Crons and actions are not run.

## Library API (used by `btb dev`)

`Server::start(elf, bind, store, indexes)` starts a server with one contract; `Server::start_multi(&[ContractSpec { name, elf }], bind, store, indexes, files_dir)` starts several (see below). `server.reload_contract(name, elf)` reloads one of them. `server.reload(elf)` validates a new build and swaps it in while keeping all state (the `KvStore`, block height and subscriptions); on an invalid ELF the old contract keeps running. After a swap every open subscription re-runs against the new code and pushes its result if it changed (a stream whose query no longer exists ends). `server.set_page(Some(html))` serves a project's own page at `/` instead of the todo demo. `indexes` is the deprecated list of host-side specs (pass `vec![]`: the ELF's schema registers the rest).

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

## Several contracts

`Server::start_multi` (and `btb dev` for a project that lists `contracts`, see [CLI.md](CLI.md#btb-dev)) hosts several contracts in one server and one state database:

- **Addresses are stable**: the contract called `name` lives at `SHA256("btb.dev.contract.v1" || name)` (`btb_dev::dev_address`), so the same name keeps its address, and the state kept with `--data`, across restarts and reloads. `GET /contracts` lists `name`, `address` and entry points.
- **Routing**: the first contract is the primary one and answers the unprefixed `/query/<fn>`, `/mutation/<fn>`, `/subscribe/<fn>` and `/admin/...` paths. Reach any contract at `/c/<name or address hex>/query/<fn>` (the same prefix a hosting node uses, so `new BtbClient({ url: "http://127.0.0.1:8787/c/token" })` works) or with `?contract=<name>`. An unknown name is a 404.
- **Cross-contract calls work**: the server resolves `call_contract` targets to its other contracts, with the chain's rules (depth 8, no re-entrancy, read-only frames, no randomness-marked or callback targets). A subscription on a router's query that reads a token re-runs when the token changes.
- **Independent reloads**: `reload_contract(name, elf)` swaps one contract's code. Its state and every other contract (code and state) are untouched; open subscriptions are re-run against the new code. Tables and indexes are registered per contract.
- **Dashboard**: `/dashboard` shows a Contracts list when there is more than one; the tables, storage keys and balance are those of the selected contract (`/admin/...?contract=<name>`), `/admin/contracts` lists them and the SSE events name the contracts they touched.

## File storage

The dev server emulates storage nodes with plain files so apps that use [files from contracts](STORAGE.md#files-from-contracts) run without a cluster. The API is the one a hosting node with the upload gateway has:

| Request | Meaning |
| --- | --- |
| `POST /upload[?k=4&m=2&periods=N&store=0]` (raw body, up to 64 MiB) | keep the bytes and answer `{root, size, k, m, stored, url}`. The root is the real file id (the same `btb store` prints: it erasure-codes the bytes). Unless `store=0` the file is also recorded as stored and paid for `periods` payment periods (default 1000) for free |
| `POST /file/<root>/store[?periods=N]` | the client-paid `StoreFile` of the dev chain: record an uploaded file as stored (free) |
| `POST /upload/<root>/push` | a no-op here (a hosting node pushes the pieces to storage nodes) |
| `GET /file/<root>` | the bytes, with the headers of a hosting node (`ETag`, `Cache-Control: immutable`, sandbox CSP) |
| `GET /admin/files` | roots on disk with `file_info` status and expiry |

Files live in `target/btb/files` under `btb dev` (`--files DIR` for the binary; a temporary directory otherwise). In a contract `file_info` reads the same records the chain would (`status` 0 unknown, 1 stored, 2 expired), so a root that was uploaded with `store=0` is unknown until `/file/<root>/store`. `file_pin` takes the escrow (`StorageConfig` defaults) from the contract's balance and keeps it, since there is no node to pay; a pin the balance cannot cover answers `FILE_PIN_INSUFFICIENT`. A dev payment period is **10 blocks** (a mutation is a block), so `periods = 1` expires after ten mutations and the status turns to expired. Tests: `crates/btb-tests/tests/dev_multi.rs`.
