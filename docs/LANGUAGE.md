# Writing BTB Contracts: Rust First, Any Language Welcome

BTB contracts run on a metered **RISC-V VM**. Anything that compiles to RISC-V can run on BTB, the same "one VM, many languages" approach the Internet Computer uses with WebAssembly. **Rust is the first-class language**, with an SDK in the style of NEAR's `near-sdk`, and the developer experience is modeled on Convex, so contracts read like a backend that humans and AI agents can understand at a glance.

> Draft. APIs below are illustrative and will change during Phase 1.

## Languages

| Language | Status | How |
| --- | --- | --- |
| **Rust** | First-class (Phase 1) | `btb-sdk` crate with `#[btb::contract]`, `#[table]`, `#[query]`, `#[mutation]`, `#[cron]` macros |
| C / C++ | Supported via LLVM (Phase 3) | Header file with the host-call interface |
| Zig | Supported via LLVM (Phase 3) | Thin SDK over the host-call interface |
| TypeScript | Planned | Compiled or run in a RISC-V-hosted interpreter; the main draw for Convex developers |
| Others (Go via TinyGo, Nim, …) | Community | Any toolchain that emits RISC-V and follows the host-call ABI |
| BTB DSL | Later, optional | A small Rust-like language where tables and queries are syntax; compiles to the same target |

Every language talks to the chain through the same **host-call interface** (storage, tables, tokens, context, cross-chain), so features land for all languages at once.

## Four function kinds

| Kind | What it does | Goes through consensus? |
| --- | --- | --- |
| `#[btb::query]` | Reads data; cannot change state; frontends can subscribe to it | No (served by hosting nodes) |
| `#[btb::mutation]` | Reads and writes data in a transaction | Yes |
| `#[btb::cron]` | A mutation that runs on a schedule | Yes |
| `#[btb::action]` | Calls the outside world: its first phase returns the HTTPS request every validator makes; the agreed result is delivered to the mutation `<name>_callback` | Yes; validators agree on the transformed response (see [Oracles](ORACLES.md#3-custom-web-data-https-requests)) |

The same split as Convex: queries, mutations, scheduled functions and actions.

## Design rules (all languages)

- **Deterministic.** No floats, no system time, no randomness outside a chain-provided source. The VM only implements the integer RISC-V instruction set.
- **Metered.** The VM charges gas per instruction and per host call; infinite loops cannot stall the chain.
- **Money-safe.** Tokens move only through host calls that check balances; the SDK models them as values that cannot be copied or dropped, and reentrancy is prevented by the runtime.
- **Database-native.** Tables, indexes, queries and mutations are part of the SDK and the host interface, not something each contract reinvents.

## The Convex-style project layout

A BTB app looks like a Convex app: backend functions live in one folder, next to the frontend.

```
my-dex/
├── btb/                 # backend: contracts compiled to RISC-V
│   ├── schema.rs        # tables and indexes
│   ├── pools.rs         # queries and mutations
│   └── crons.rs         # scheduled functions
├── web/                 # frontend, hosted on BTB
│   └── src/App.tsx
└── btb.toml             # project config
```

Running `btb dev` compiles the contract, starts a local chain and hot-reloads on save; `btb codegen --ts web/src/btb/` generates typed TypeScript bindings for the frontend from the contract's schema (see [CLI.md](CLI.md#btb-codegen) and [TS_SDK.md](TS_SDK.md#typed-clients-from-the-contract-schema)). `btb init` creates a single-crate project (`src/main.rs`, `web/index.html`); the multi-file `btb/` folder above is the planned layout.

## What works today (Phase 1)

The SDK is the `btb-sdk` crate (`no_std`, plus `alloc`). The working examples are `examples/counter`, `examples/token` (fungible token in contract storage) and `examples/dex` (constant-product AMM, 0.3% fee) `examples/todo` (a per-user list read through an index, the Phase 2 demo), `examples/launcher` (user tokens and a faucet) and `examples/nft` (NFTs and an escrow-based market), built for `riscv64imac-unknown-none-elf` and run in `btb-vm` by `crates/btb-tests`.

```rust
use btb_sdk::btb;
use btb_sdk::ensure;
use btb_sdk::prelude::*;

#[btb::table]
pub struct Todo {
    #[index] pub owner: Address,   // Todo::by_owner(addr)
    pub done: bool,
    #[index] pub created: u64,     // Todo::by_created_range(a..b)
    pub text: String,
}

#[btb::mutation]
pub fn add(text: String) -> btb_sdk::Result<u64> {
    ensure!(!text.is_empty(), "text must not be empty");
    Ok(Todo { owner: caller(), done: false, created: block_height(), text }.insert())
}

#[btb::query]
pub fn list_mine() -> Vec<(u64, bool, String)> {
    Todo::by_owner(caller()).map(|(id, t)| (id, t.done, t.text)).collect()
}
```

- `#[btb::query]`, `#[btb::mutation]`, `#[btb::cron]`, `#[btb::action]` export `btb_<kind>_<name>` (name `[a-z0-9_]`, at most 64). Arguments are decoded as the tuple of the parameters and the return value is encoded and passed to `return_data` ([ENCODING.md](ENCODING.md)). No return type means no output. Bad arguments abort the call. At most 8 arguments; no generics, `async` or `self`.
- `#[btb::cron(every = "N blocks")]` (1 to 4294967296) also exports a marker symbol `btb_every_<N>_<name>`; `btb deploy` reads it into the manifest's `crons` (function, every_blocks), which the chain schedules from the deploy height ([CHAIN.md](CHAIN.md#cron-jobs)). Time units such as `"1h"` are rejected at compile time because the chain schedules by block height only. A cron without `every` has no schedule and never runs.
- `#[btb::mutation(randomness)]` and `#[btb::cron(randomness, every = "N blocks")]` (any order) let the function call `btb_sdk::oracle::random::{bytes, u64, range}`; the attribute exports a marker symbol `btb_randomness_<kind>_<name>` that the VM reads from the ELF on every call. On a query or an action it is a compile error. A marked function is always charged its full gas limit. See [ORACLES.md](ORACLES.md#1-verifiable-randomness).
- **HTTPS actions.** `#[btb::action] fn name(args...) -> HttpRequest` is the first phase: it runs **read-only** (a state write traps) inside the `ActionOp::Request` transaction and returns the request the validators make (`HttpRequest::get(url)`, `::post(url, body)`, `.header(..)`, `.limits(max_response_bytes, timeout_ms)`; `btb_sdk::oracle::action`). `#[btb::transform("name")] fn f(resp: HttpResponse) -> T` exports `btb_transform_<name>`: a pure function (no state, clock or randomness; gas-metered) from one validator's response to the value the validators sign (the canonical encoding of `T`); without one the signed value is the raw body of a 2xx response. The result is delivered to `#[btb::mutation] fn name_callback(id: Address, args: Vec<u8>, status: u8, data: Vec<u8>)`, which must start with `action::require_callback()`; its `status` is `action::STATUS_OK`, `STATUS_FAILED`, `STATUS_DISAGREEMENT` or `STATUS_TIMEOUT`. The chain refuses a transaction that calls a callback. A contract's deployer sets the hosts its actions may call with `ActionOp::SetHosts`. An action is requested, never called: queries and `Call` transactions cannot start one. Full design, limits and the SSRF rules: [ORACLES.md](ORACLES.md#3-custom-web-data-https-requests).
- `btb_sdk::oracle::report::request(data)` queues a report (at most 1 KiB) for the beacon committee to sign for other chains; mutations and crons only. See [ORACLES.md](ORACLES.md#4-btb-as-an-oracle-for-other-chains).
- `#[btb::table]` makes the struct encode as the tuple of its fields and implements `btb_sdk::table::Table` (table name = struct name; `insert`, `get`, `update`, `delete`, `scan`, `filter`). Row ids come from `insert`. See [Tables, indexes and lookups](#tables-indexes-and-lookups).
- **`Result` errors.** An entry point declared `-> btb_sdk::Result<T>` returns `T` on `Ok` and aborts the call with the message of the `Err` (nothing is committed; the caller sees `aborted: <message>`, which `@btb/client` raises as a `ContractError`). `btb_sdk::ensure!(cond, "msg {}", x)` and `btb_sdk::bail!("msg")` return `Err(btb_sdk::Error)`; `&str`, `String` and `DecodeError` convert with `?` (`opt.ok_or("no such pool")?`). The wire type is `T`; `Result` only changes how failure is reported.
- Host calls are wrapped in `btb_sdk::host`: `storage_get/set/remove`, `caller`, `block_height`, `token_balance`, `token_transfer`, `return_data`, `abort`, `table_*`. They only work inside the VM; on other targets they panic. `btb_sdk::oracle` adds `random::{bytes, u64, range}` and `price(pair)` (calls 40 and 41).
- `use btb_sdk::prelude::*` brings in the macros (`btb`, `ensure!`, `bail!`), `Table`, `Token`, `Address`, `String`, `Vec` and the common host wrappers. `btb_sdk::Result` and `btb_sdk::Error` are not in the prelude, so they do not hide `std`'s `Result`.
- Contracts use `feature = "runtime"` for the panic handler, a bump allocator and `mem*` routines. The VM implements only rv64im, so they must be built without the C and A extensions; see `examples/counter/README.md`.

## Tables, indexes and lookups

A `#[btb::table]` struct declares a table, its secondary indexes and its natural key in one place:

```rust
#[btb::table]
pub struct Todo {
    #[index]   pub owner: Address,
    pub done: bool,
    #[index]   pub created: u64,
    pub text: String,
}

#[btb::table]
pub struct Pool {
    #[primary] pub id: u64,        // unique natural key
    #[index]   pub creator: Address,
    pub reserve_a: u128,
}
```

- `#[index]` makes an ordered secondary index named after the field (at most 8 per table, `#[primary]` included). Indexable field types: `u8`..`u64`, `i8`..`i64`, `bool`, `u128`, `String`, `Address`/`Token`, `[u8; N]`, `Vec<u8>`, and `Option<_>` of those (a `None` row stays out of the index). Floats do not exist; `i128`, vectors and tuples cannot be indexed.
- `#[primary]` is an index whose value is **unique**: inserting or updating a row so that a second row has the same key aborts the call and commits nothing. At most one per table; it cannot be `Option`.
- **The schema travels in the contract.** The macros export marker symbols in the ELF (`btb_schema_table_<Name>_<hex>`, `btb_schema_fn_<kind>_<name>_<hex>`) describing tables, field types, indexes and function signatures in the [`btb-codec`](ENCODING.md) types. `btb dev`, `btb deploy` and the chain read the indexes from there (`btb_runtime::schema`), computing each indexed value by walking the encoded row up to the field (variable-size fields before it are skipped by their encoding), so `Cargo.toml` metadata is not needed. A schema that does not validate (unknown index field, unindexable type, zero-size vector elements, recursive types) makes the deploy fail. `[package.metadata.btb] indexes` still works for tables the contract does not declare, with a deprecation warning.
- **Typed lookups** are generated per indexed field and yield `(row id, row)`:

```rust
use btb_sdk::table::Order;

Todo::by_owner(addr)                          // rows whose owner == addr, ordered by id
Todo::by_created_range(100..)                 // any RangeBounds; ordered by created, then id
Todo::by_owner(addr).order(Order::Desc).limit(20).after(cursor)   // newest 20 after a cursor
Todo::by_owner(addr).page()                   // Page { rows, next: Option<Vec<u8>> }, one host call
Pool::get_by_id(7)                            // Option<(row id, Pool)>, from #[primary]
Todo::filter(|t| t.done)                      // full-table scan with a predicate (costs gas per row)
Todo::by_owner(addr).filter(|(_, t)| !t.done) // Iterator::filter on an index lookup
```

  A lookup is an `Iterator` that fetches 64 ids per host call (`table_index_scan`) and loads each row with `table_get`; `.limit(n)` caps the total and `.cursor()` / `Page::next` is the opaque cursor to pass to `.after(..)` for the next page (return it from a query as `Vec<u8>`). String and byte fields take `&str` / `&[u8]` in `by_<field>`, and `String` / `Vec<u8>` ranges. A query that reads an index range is re-run by subscriptions exactly when an entry in that range changes.

## Example: a DEX in Rust

```rust
use btb_sdk::prelude::*;
use btb_sdk::{ensure, Result};

#[btb::table]
pub struct Pool {
    #[primary] pub id: u64,
    #[index]   pub token_a: Token,
    #[index]   pub token_b: Token,
    pub reserve_a: u128,
    pub reserve_b: u128,
    #[index]   pub creator: Address,
}

#[btb::query]
pub fn price(pool: u64) -> Result<(u128, u128)> {
    let (_, p) = Pool::get_by_id(pool).ok_or("no such pool")?;
    Ok((p.reserve_b, p.reserve_a))
}

#[btb::query]
pub fn pools_by_creator(creator: Address) -> Vec<Pool> {
    Pool::by_creator(creator).map(|(_, p)| p).collect()
}

#[btb::mutation]
pub fn swap(pool: u64, amount_in: u128, min_out: u128) -> Result<u128> {
    let (row, mut p) = Pool::get_by_id(pool).ok_or("no such pool")?;
    let out = amm_out(amount_in, p.reserve_a, p.reserve_b);
    ensure!(out >= min_out, "slippage too high");
    p.reserve_a += amount_in;
    p.reserve_b -= out;
    p.update(row);
    Ok(out)
}

#[btb::cron(every = "3600 blocks")]
pub fn record_price_history() {
    // snapshot prices for charts
}
```

And the frontend, with the client `btb codegen --ts` generates from the contract:

```tsx
import { api } from "./btb/api";

const price = useQuery(api.price, [7n]);   // live, no polling; typed [bigint, bigint]
const swap  = useMutation(api.swap);       // swap(7n, 100n, 90n): Promise<bigint>
```

## Example: native token

```rust
let moon = token::create(TokenSpec {
    name: "Moon",
    symbol: "MOON",
    supply: 1_000_000,   // fixed at creation
    decimals: 18,
})?;
```

## Built for AI agents too

Because every app follows the same layout and the same four function kinds (query, mutation, cron, action), an AI coding agent can read a BTB project and know exactly where data is defined, what can change state, what runs on a schedule and what talks to the outside world. The repository includes an [AGENTS.md](../AGENTS.md) describing these conventions for any agent working on the codebase.

## How it compiles

```
Rust / C / Zig / … source
  → standard compiler (rustc, clang, zig) targeting RISC-V rv64im
  → ELF binary with BTB entry points (one per query / mutation / cron / action)
  → btb deploy: binary + schema uploaded to the chain
  → BTB VM: verified, metered, recompiled to x86 / ARM on validators
```

Entry points are exported ELF function symbols named `btb_query_<name>`, `btb_mutation_<name>`, `btb_cron_<name>` and `btb_action_<name>`, where `<name>` is 1 to 64 characters of `[a-z0-9_]`. A name may appear once per kind, and a contract needs at least one entry point. A fifth family, `btb_transform_<name>`, is not an entry point of the four kinds: it is the pure response transform of the action of the same name, called by validators only (`Contract::call_transform`: a host with no storage, tables, balances, prices or randomness). The mutation `<action>_callback` receives an action's agreed result; the chain refuses to run it from a transaction. The VM calls an entry with `a0` = pointer to the input bytes, `a1` = input length, and a return address that halts cleanly; output is whatever the function passes to `return_data` (empty on a plain return). Queries and actions (their first phase) always run read-only: any state-changing host call traps. Randomness (`random_bytes`) is available only to mutation and cron entry points marked `randomness`: the exported symbol `btb_randomness_<kind>_<name>` (kind `mutation` or `cron`) marks one, and a marker on any other kind, or without its entry point, makes the contract invalid.

No custom compiler is needed for Rust, C or Zig: their existing RISC-V backends do the work. The optional BTB DSL would add its own frontend on top of the same pipeline later.

## Why RISC-V

- About 40 base instructions: a small VM that is easier to secure.
- Mainstream compilers already target it, which is what makes "any language" practical.
- Translates efficiently to x86 and ARM, so contracts run close to native speed.
- ZK-ready: provers such as RISC Zero and SP1 prove RISC-V programs, enabling verifiable light clients later.
- Proven in production by Nervos CKB and Polkadot's PolkaVM, both open source in Rust.

See [PRIOR_ART.md](PRIOR_ART.md) for how other chains approach languages and VMs.

## Host calls

Contracts reach the chain through `ecall`: call number in a7, arguments in a0..a5, result in a0. Each call charges gas before it runs. Current calls (Phase 1, implemented in `crates/btb-vm/src/host.rs`):

| # | Call | Gas |
| - | ---- | --- |
| 1 | `storage_read(key_ptr, key_len, out_ptr, out_cap) -> len or u64::MAX` | 200 + 1/key byte + 1/byte copied |
| 2 | `storage_write(key_ptr, key_len, val_ptr, val_len)` | 500 + 4/(key+val) byte |
| 3 | `storage_remove(key_ptr, key_len)` | 300 + 1/key byte |
| 4 | `caller(out_ptr)` | 10 |
| 5 | `block_height()` | 5 |
| 6 | `token_balance(addr_ptr)` | 50 |
| 7 | `token_transfer(to_ptr, amount) -> 0 ok or 1 insufficient` | 300 |
| 8 | `return_data(ptr, len)` | 10 + 1/byte |
| 9 | `abort(ptr, len)` | 10 + 1/byte |
| 10 | `table_insert(table_ptr, table_len, row_ptr, row_len) -> row id` (monotonic per table, never reused) | 600 + 4/(table+row) byte |
| 11 | `table_get(table_ptr, table_len, id, out_ptr, out_cap) -> len \| u64::MAX` | 250 + 1/table byte + 1/byte copied |
| 12 | `table_update(table_ptr, table_len, id, row_ptr, row_len) -> 0 ok \| 1 missing` | 500 + 4/(table+row) byte |
| 13 | `table_delete(table_ptr, table_len, id) -> 0 ok \| 1 missing` | 300 + 1/table byte |
| 14 | `table_scan(table_ptr, table_len, start_id, out_ptr, out_cap) -> count` (up to out_cap/8 ascending u64 LE ids >= start_id) | 250 + 1/table byte + 10/id returned |
| 15 | `table_index_scan(req_ptr, req_len, out_ptr, out_cap) -> total_len` (read-only; allowed in queries; request and result layout in `abi::TABLE_INDEX_SCAN`) | 400 + 1/request byte + 40/id returned + 1/cursor byte returned |
| 16 | `token_create(req_ptr, req_len, out_ptr) -> status` (user tokens; mutations only; `req` is the `btb-codec` tuple `(name, symbol, decimals: u8, initial_supply: u128, mint_authority: Option<Address>)`; writes the 32-byte token id) | 2000 + 1/request byte |
| 17 | `token_transfer_of(id_ptr, to_ptr, amount_ptr) -> status` (mutations only; `amount_ptr` = 16 bytes `u128` LE; moves from the running contract) | 400 |
| 18 | `token_balance_of(id_ptr, holder_ptr, out_ptr) -> status` (read-only; writes `u128` LE) | 100 |
| 19 | `token_info(id_ptr, out_ptr, out_cap) -> len \| u64::MAX` (read-only; `btb-codec` tuple `(name, symbol, decimals, supply, mint_authority, creator)`) | 150 + 1/byte copied |
| 20 | `token_holders(req_ptr, req_len, out_ptr, out_cap) -> total_len \| u64::MAX` (read-only; request `id[32] \|\| limit:u32 LE \|\| cursor`, holders ascending by address, limit capped at 256; layout in `abi::TOKEN_HOLDERS`) | 300 + 1/request byte + 30/holder returned + 1/cursor byte returned |
| 21 | `token_mint(id_ptr, to_ptr, amount_ptr) -> status` (mutations only; mint authority only) | 500 |
| 22 | `token_burn(id_ptr, amount_ptr) -> status` (mutations only; mint authority only; burns the contract's own balance) | 400 |
| 23 | `contract_address(out_ptr)` (32 bytes) | 10 |
| 40 | `random_bytes(out_ptr, len) -> 0` (at most 1024 bytes derived from the threshold-BLS beacon, the transaction id and a per-transaction call counter; **only** in a mutation or cron marked `randomness`, otherwise traps `RandomnessForbidden`; `RandomnessUnavailable` on a chain without a beacon) | 300 + 2/byte requested, charged first |
| 41 | `price(pair_id, out_ptr) -> 0 ok \| u64::MAX \| 1` (`1` = `PRICE_PAUSED`, the feed's circuit breaker is open and nothing is written; writes 32 bytes: `price: u128 LE`, `height: u64 LE` of the update, `decimals: u8`, 7 zero bytes; read-only; allowed everywhere) | 200 in a query (queries are not billed); 10,200 in a mutation or cron (200 plus a 10,000 per-read fee) |
| 42 | `report_request(data_ptr, len) -> report id \| u64::MAX` (mutations and crons only; a read-only host traps `WriteInQuery`; at most 1024 bytes, longer aborts; queues `data` for the beacon committee to threshold-sign for other chains; at most 8 per call; `u64::MAX` on a chain without an oracle committee) | 5,000 + 10/byte, charged first |
| 43 | `price_twap(pair_id, window, out_ptr) -> 0 ok \| u64::MAX \| 1 \| 2` (writes 32 bytes: `twap: u128 LE`, `window: u32 LE`, `decimals: u8`, 11 zero bytes; time-weighted average over the last `window` blocks, 1 to 10,000; read-only; allowed everywhere; `u64::MAX` unknown pair or no price, `1` = `PRICE_PAUSED`, `2` = `PRICE_BAD_WINDOW`) | 500 in a query (not billed); 20,500 in a mutation or cron (500 plus a 20,000 per-read fee) |
| 50 | `call_contract(req_ptr, req_len, out_ptr, out_cap) -> output_len \| CALL_*` (run a `query` or `mutation` of another contract, see [Calling other contracts](#calling-other-contracts); a mutation from a query traps `WriteInQuery`; a randomness-marked callee or an action callback traps; errors are statuses `>= u64::MAX - 15` that revert only the callee) | 3,000 + 2/request byte, then the callee's exact gas, then 1/byte of output copied |

Oracle calls use the range 40 to 49 and cross-contract calls the range 50 to 59, so they never collide with other additions. Queries run in read-only mode: calls 2, 3, 7, 10, 12, 13, 16, 17, 21 and 22 trap with `WriteInQuery`, call 50 traps with `WriteInQuery` when it names a mutation, and call 40 traps with `RandomnessForbidden`. Bad guest pointers trap; unknown numbers trap. Cross-chain calls are added later with their own numbers and gas costs. Call 42 (`report_request`) also traps `WriteInQuery` in a query, in an action's first phase and in a transform.

**Oracles (calls 40 to 43).** `random_bytes` derives from the block's beacon: `SHA256("btb.rand.out.v1" || SHA256("btb.rand.tx.v1" || beacon_seed || tx_id) || call_index:u64be || block:u32be)`, so replicas agree, transactions differ and every call in a transaction differs. `btb dev` has no beacon and answers marked functions from a fixed public seed (not random). `price` returns the last published stake-weighted median of the validators' signed observations (`NOT_FOUND` before the first one, `PRICE_PAUSED` while the circuit breaker is open); `price_twap` averages it over a window of blocks. In Rust: `price(pair) -> Option<Price>`, `price_checked(pair) -> Result<Price, PriceError>` (`NotFound | Paused`) and `price_twap(pair, window) -> Result<Twap, PriceError>` (adds `BadWindow`); in C `btb_price`/`btb_price_twap` with `BTB_PRICE_PAUSED` and `BTB_PRICE_BAD_WINDOW`. Design, safety argument and fees: [ORACLES.md](ORACLES.md).

`table_index_scan` (call 15) scans one secondary index of a table between two bounds (`unbounded`, `included` or `excluded`; typed values `u64`, `i64`, `u128`, `bytes`, `string`), ascending or descending, with a `limit` (capped at 256) and an opaque resume cursor, and returns row ids ordered by (value, id). The contract loads rows with `table_get`. It returns the full result length and copies only if it fits in `out_cap`, so retry with a bigger buffer. A malformed request traps. In Rust: the generated `Todo::by_owner(addr)` lookups (above), `Table::index_eq("owner", Key::Bytes(&addr.0), limit, &cursor)` or `host::table_index_scan`. Indexes are registered from the schema in the ELF, not by the contract. The scan is recorded in the call's read set as a key range, so a subscribed query re-runs exactly when an entry in that range changes.

**Declaring indexes (temporary).** Contracts cannot declare indexes yet. For now the host registers them per table (`btb_runtime::Tables::register`; `btb-dev` takes `--index Table:index:addr@offset`). A table with no registered index returns no results. A declaration in the contract itself (and in its deploy manifest) replaces this later; see [DEV_SERVER.md](DEV_SERVER.md).

**User tokens (calls 16 to 23).** `btb_sdk::token::Token` wraps them: `Token::create(name, symbol, decimals, initial_supply, mint_authority)`, `.transfer(&to, amount)`, `.balance_of(&holder)`, `.info()`, `.holders(cursor, limit)`, `.mint(&to, amount)`, `.burn(amount)`; `contract_address()` gives a contract its own address (for naming itself as mint authority). They are separate from native BTB (calls 6 and 7): creating a token never mints BTB. `status` is `0` ok, `1` invalid name/symbol/decimals, `2` insufficient balance, `3` unknown token, `4` not the mint authority, `5` overflow, `6` unsupported by the host. Semantic failures return a status and change nothing; a malformed request traps. Balances belong to addresses, and as with native `token_transfer`, a contract moves tokens only from its own balance. Holders are paged by **address** (ascending, non-zero balances only; the last address returned is the cursor), which stays stable while balances change; rank by balance in a contract-owned table if needed. In C see the `btb_token_*` wrappers in `include/btb.h`. Details: [TOKENOMICS.md](TOKENOMICS.md#user-tokens).

### Calling other contracts

A contract can run a function of another contract synchronously with `call_contract` (call 50). In Rust:

```rust
use btb_sdk::{call, query, CallError, Remote};
use btb_sdk::call::GAS_ALL;

// From a mutation or cron: run the mutation `transfer(to, amount)` of the token at `token`.
// There, caller() is this contract and contract_address() is the token.
let moved: bool = call::<_, bool>(token, "transfer", &(to, amount), 200_000)?;

// From anywhere, queries included: run a query.
let bal: u128 = query::<_, u128>(token, "balance_of", &(me,), GAS_ALL)?;

// Or through a handle: Remote::new(token).call("transfer", &(to, amount), gas)
```

`examples/caller` is a router that fronts the `token` example this way. In C, `btb_call_request` builds the request and `btb_call_contract` makes the call (`include/btb.h`; `examples/c-counter` has a `relay` mutation).

**What runs.** The callee runs in a **fresh VM with its own memory** (nothing is shared with the caller but the bytes passed in and out), on the **same transaction state**: it sees the caller's writes and the caller sees its. Its storage, tables, native balance and token balances are its own; `token_transfer`, `token_transfer_of` and the other token calls act as the callee. The request is one block (`abi::CALL_CONTRACT`: target, kind, gas, function name, arguments) because the ABI has five argument registers; the result is copied to `out_ptr` and its length returned. Only `query` and `mutation` can be named: never a cron, an action, or the callback of an action (`<action>_callback`; it traps `CallForbidden`, only the chain delivers those).

**Read-only is enforced through the whole stack.** A mutation (or cron) may call a mutation or a query. A query may call only queries, and so may everything below a query, however deep; naming a mutation from a read-only frame traps the caller with `WriteInQuery`, like a write would. A query callee runs read-only even inside a mutation.

**Randomness is never reachable across calls.** A randomness-marked function (`#[btb::mutation(randomness)]`) cannot be named by another contract: the call traps the caller with `RandomnessForbidden`. A `random_bytes` call in any nested frame traps too (the VM flag is off in every callee), so a marked function that calls an unmarked one cannot lend it the beacon. A marked function is reached only by a transaction or a cron ([ORACLES.md](ORACLES.md#1-verifiable-randomness)).

**Gas.** The call costs `3,000 + 2/request byte` first. `gas` in the request is forwarded to the callee, capped at what the caller has left (`btb_sdk::call::GAS_ALL` forwards everything). The callee's **exact** usage is then charged to the caller; the unused part is never taken. A callee that runs out of gas has used all it was given, fails with `CALL_OUT_OF_GAS` and the caller carries on with the rest. Copying the result costs `1/byte`. The transaction's own gas limit binds the whole stack.

**Errors revert only the callee.** The callee runs on a savepoint of the transaction's write overlay. Any outcome but success (abort, trap, out of gas, a result that does not fit `out_cap`, a deposit it cannot pay, a table fault) drops exactly the writes the callee made, nested calls included, and returns a status to the caller:

| status (`>= u64::MAX - 15`) | meaning |
| --- | --- |
| `CALL_NO_CONTRACT` (`u64::MAX`) | no contract at the target address |
| `CALL_NO_FUNCTION` (`- 1`) | no function of that name and kind |
| `CALL_ABORTED` (`- 2`) | the callee called `abort` |
| `CALL_TRAPPED` (`- 3`) | the callee trapped (bad memory access, `random_bytes`, storage fault, ...) |
| `CALL_OUT_OF_GAS` (`- 4`) | the callee used all the gas forwarded to it |
| `CALL_DEPTH` (`- 5`) | the call would nest deeper than 8 frames |
| `CALL_REENTRANT` (`- 6`) | the target is already on the call stack |
| `CALL_OUTPUT_TOO_LARGE` (`- 7`) | the result did not fit `out_cap` (the callee is reverted: a mutation cannot be retried) |
| `CALL_DEPOSIT` (`- 8`) | the callee's balance cannot pay the storage deposit of what it wrote |
| `CALL_UNSUPPORTED` (`- 9`) | the host cannot run other contracts (`btb dev`, `MemHost`) |

The caller may carry on (its own earlier writes stay) or `abort`, which reverts everything. The Rust wrappers return `Err(CallError::...)`. The result buffer of `call`/`query` is `DEFAULT_OUTPUT_CAP` (2 KiB) bytes; use `call_with_cap`/`query_with_cap` for bigger results.

**Depth and re-entrancy.** At most **8 frames** (the entry point is frame 1). **Re-entering a contract that is already on the call stack, the caller itself included, is refused** (`CALL_REENTRANT`): A calling B calling A fails at the second A. This removes the classic re-entrancy class (state read before an external call, changed by a nested call back into the caller) instead of asking each contract to guard against it. The price is that a callback pattern (A calls B, B calls A back) is impossible; use B's return value, or two transactions. Calling the same contract twice in a row is fine. There is no opt-in guard helper: a later proposal could add an explicit `allow_reentry` marker if a real use case appears.

**Storage deposits.** Each frame's growth is paid by the contract whose bytes grew: when a callee returns successfully its storage growth is locked from the callee's own balance (and shrinkage returns the deposit to it), exactly as for a top-level call. If the callee cannot pay, the call fails with `CALL_DEPOSIT` and is reverted. The caller is never charged for the callee's state. Fund contracts that others will call.

**Not allowed / not done.** No calls from actions' transforms, no delegate-call (the callee never runs with the caller's storage), no value attached to a call (use a token transfer), no cross-chain calls. Hosts without a code resolver (`btb dev`, `MemHost`) answer `CALL_UNSUPPORTED`.

Argument and result bytes use the canonical encoding specified in [ENCODING.md](ENCODING.md).

### C

`include/btb.h` is a freestanding C header (only `<stdint.h>`) with the call numbers above, a `static inline` `ecall` wrapper per host call (`btb_storage_read`, `btb_return_data`, ...) and entry-point macros: `BTB_QUERY(name)`, `BTB_MUTATION(name)`, `BTB_CRON(name)`, `BTB_ACTION(name)` define `btb_<kind>_<name>(const uint8_t *ptr, uint64_t len)`. Compile with `clang --target=riscv64-unknown-elf -march=rv64im -mabi=lp64 -nostdlib -ffreestanding -O2 -mno-relax` and link with `ld.lld` and a linker script like `examples/c-counter/link.ld`. See `examples/c-counter/` for a working counter; a test checks the header's call numbers against `btb_vm::host::abi`. For oracles it has `btb_random_bytes`, `btb_price`, `btb_report_request` and `BTB_RANDOMNESS(mutation, name);` (or `cron`), which exports the `btb_randomness_<kind>_<name>` marker next to `BTB_MUTATION(name)`.
