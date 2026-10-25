# Dev server (`btb-dev`)

`btb-dev` is a single-node server for trying contracts with the Phase 2 state database. It is the Phase 2 gate demo ("query a table by index and watch a UI update live"), not the Phase 3 `btb dev`: no hot reload, no deploy, no auth. **Development only. Never expose it.**

```
./examples/todo/build.sh                      # builds examples/todo/todo.elf
cargo run -p btb-dev -- examples/todo/todo.elf --port 8787
# open http://127.0.0.1:8787/ in two tabs
```

Flags: `--port N` (default 8787, 0 = any free port), `--index Table:index:addr@offset` (repeatable, see below), `--data DIR` (RocksDB; needs `cargo run -p btb-dev --features rocksdb`, default is memory and state is lost on exit).

## HTTP API

| Request | What it does |
| --- | --- |
| `POST /mutation/<name>` | Runs a mutation as an optimistic transaction (up to 3 retries), commits it, then re-runs every subscribed query whose read set was touched |
| `GET /query/<name>` | Runs a query once |
| `GET /subscribe/<name>` | Server-Sent Events. First event: the current result. Then one event each time a commit changes the result (`SubscriptionManager`: read-set invalidation, re-run, compare fingerprints) |
| `GET /` | The todo demo page (`examples/todo/web/index.html`, embedded in the binary) |

Failures return JSON `{"error": "..."}`: 400 (abort or bad arguments), 404 (no such function), 405, 409 (conflict after retries), 500 (storage fault). A failed call changes nothing and pushes nothing.

**Arguments** are the `btb-codec` bytes of the argument tuple ([ENCODING.md](ENCODING.md)), as hex in `?args=` or the POST body. As a convenience, a POST with `Content-Type: application/json` takes a flat array of strings, non-negative integers and booleans (encoded as `String`, `u64`, `bool`): `["buy milk"]`, `[3]`.

**Results** are hex; with `?format=json` the todo demo functions (`list_mine`, `add`, `toggle`) are shown as JSON and everything else as `{"hex": "..."}`. The JSON forms live in `crates/btb-dev/src/demo.rs`.

**Caller identity (dev only).** There is no signature check. The caller is the `X-BTB-Caller` header, or `?caller=` because `EventSource` cannot set headers. 64 hex characters are used as the 32-byte address; any other text (`alice`) is hashed with SHA-256. No value means `anonymous`. A subscription keeps the caller it was opened with and re-runs as that caller.

## Temporary: index declarations

Contracts cannot declare indexes yet. The dev server registers them on the host side with `--index Table:index:kind@offset`: the index value is the `addr` (32 bytes, an `Address`) or `u64` (8 bytes LE) at byte `offset` of the encoded row. With no `--index` flag it registers `Todo:owner:addr@0` for the demo. This is a stand-in for contract-side declarations (and a deploy manifest), which will replace the flag; rows written before an index was registered are not indexed.

## State and limits

One contract at a time, at the fixed address `[0xDE; 32]`, funded with 10^12 base units to pay storage deposits. The block height is a counter that goes up by one per committed mutation (starts at 1 on every run). Each call has a 50,000,000 gas limit and 1 MiB of VM memory. Crons and actions are not run.

## Tests

`crates/btb-tests/tests/dev_gate.rs` is the automated gate: it starts the server on an ephemeral port with the real `todo` contract, opens an SSE stream, posts `add` and `toggle`, and asserts the new event arrives (and that another user's change pushes nothing). `examples/todo/web/smoke.js` is an optional Playwright check of the page in Chromium.
