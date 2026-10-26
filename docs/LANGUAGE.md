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

Running `btb dev` compiles the `btb/` folder, starts a local chain, hot-reloads on save and generates typed TypeScript bindings for the frontend.

## What works today (Phase 1)

The SDK is the `btb-sdk` crate (`no_std`, plus `alloc`). The working examples are `examples/counter`, `examples/token` (fungible token in contract storage) and `examples/dex` (constant-product AMM, 0.3% fee) `examples/todo` (a per-user list read through an index, the Phase 2 demo), `examples/launcher` (user tokens and a faucet) and `examples/nft` (NFTs and an escrow-based market), built for `riscv64imac-unknown-none-elf` and run in `btb-vm` by `crates/btb-tests`.

```rust
use btb_sdk::btb;
use btb_sdk::host;

#[btb::table]
pub struct Change { pub by: u64, pub total: u64 }   // Table: insert / get / update / delete / scan

#[btb::mutation]
pub fn increment(by: u64) -> u64 { /* host::storage_get / storage_set */ 0 }
```

- `#[btb::query]`, `#[btb::mutation]`, `#[btb::cron]`, `#[btb::action]` export `btb_<kind>_<name>` (name `[a-z0-9_]`, at most 64). Arguments are decoded as the tuple of the parameters and the return value is encoded and passed to `return_data` ([ENCODING.md](ENCODING.md)). No return type means no output. Bad arguments abort the call. At most 8 arguments; no generics, `async` or `self`.
- `#[btb::cron(every = "N blocks")]` (1 to 4294967296) also exports a marker symbol `btb_every_<N>_<name>`; `btb deploy` reads it into the manifest's `crons` (function, every_blocks), which the chain schedules from the deploy height ([CHAIN.md](CHAIN.md#cron-jobs)). Time units such as `"1h"` are rejected at compile time because the chain schedules by block height only. A cron without `every` has no schedule and never runs.
- `#[btb::mutation(randomness)]` and `#[btb::cron(randomness, every = "N blocks")]` (any order) let the function call `btb_sdk::oracle::random::{bytes, u64, range}`; the attribute exports a marker symbol `btb_randomness_<kind>_<name>` that the VM reads from the ELF on every call. On a query or an action it is a compile error. A marked function is always charged its full gas limit. See [ORACLES.md](ORACLES.md#1-verifiable-randomness).
- **HTTPS actions.** `#[btb::action] fn name(args...) -> HttpRequest` is the first phase: it runs **read-only** (a state write traps) inside the `ActionOp::Request` transaction and returns the request the validators make (`HttpRequest::get(url)`, `::post(url, body)`, `.header(..)`, `.limits(max_response_bytes, timeout_ms)`; `btb_sdk::oracle::action`). `#[btb::transform("name")] fn f(resp: HttpResponse) -> T` exports `btb_transform_<name>`: a pure function (no state, clock or randomness; gas-metered) from one validator's response to the value the validators sign (the canonical encoding of `T`); without one the signed value is the raw body of a 2xx response. The result is delivered to `#[btb::mutation] fn name_callback(id: Address, args: Vec<u8>, status: u8, data: Vec<u8>)`, which must start with `action::require_callback()`; its `status` is `action::STATUS_OK`, `STATUS_FAILED`, `STATUS_DISAGREEMENT` or `STATUS_TIMEOUT`. The chain refuses a transaction that calls a callback. A contract's deployer sets the hosts its actions may call with `ActionOp::SetHosts`. An action is requested, never called: queries and `Call` transactions cannot start one. Full design, limits and the SSRF rules: [ORACLES.md](ORACLES.md#3-custom-web-data-https-requests).
- `btb_sdk::oracle::report::request(data)` queues a report (at most 1 KiB) for the beacon committee to sign for other chains; mutations and crons only. See [ORACLES.md](ORACLES.md#4-btb-as-an-oracle-for-other-chains).
- `#[btb::table]` makes the struct encode as the tuple of its fields and implements `btb_sdk::table::Table` (table name = struct name). `#[primary]`, `#[index]` and the generated `by_<field>` lookups in the DEX example below are not implemented; row ids come from `insert`.
- Host calls are wrapped in `btb_sdk::host`: `storage_get/set/remove`, `caller`, `block_height`, `token_balance`, `token_transfer`, `return_data`, `abort`, `table_*`. They only work inside the VM; on other targets they panic. `btb_sdk::oracle` adds `random::{bytes, u64, range}` and `price(pair)` (calls 40 and 41).
- `use btb_sdk::prelude::*` is available, but `Result<T>`, `ensure!`, `Ratio` and `TokenId` in the example below are design sketches that do not exist yet.
- Contracts use `feature = "runtime"` for the panic handler, a bump allocator and `mem*` routines. The VM implements only rv64im, so they must be built without the C and A extensions; see `examples/counter/README.md`.

## Example: a DEX in Rust

```rust
use btb_sdk::prelude::*;

#[btb::table]
pub struct Pool {
    #[primary] pub id: u64,
    #[index]   pub token_a: TokenId,
    #[index]   pub token_b: TokenId,
    pub reserve_a: u128,
    pub reserve_b: u128,
    #[index]   pub creator: Address,
}

#[btb::query]
pub fn price(pool: u64) -> Result<Ratio> {
    let p = Pool::get(pool)?;
    Ok(Ratio::new(p.reserve_b, p.reserve_a))
}

#[btb::query]
pub fn pools_by_creator(creator: Address) -> Vec<Pool> {
    Pool::by_creator(creator).collect()
}

#[btb::mutation]
pub fn swap(pool: u64, amount_in: u128, min_out: u128) -> Result<u128> {
    let mut p = Pool::get(pool)?;
    let out = amm_out(amount_in, p.reserve_a, p.reserve_b);
    ensure!(out >= min_out, "slippage too high");
    token::transfer_in(p.token_a, caller(), amount_in)?;
    token::transfer_out(p.token_b, caller(), out)?;
    p.reserve_a += amount_in;
    p.reserve_b -= out;
    p.save()?;
    Ok(out)
}

#[btb::cron(every = "3600 blocks")]
pub fn record_price_history() -> Result<()> {
    // snapshot prices for charts
    Ok(())
}
```

And the frontend, exactly like Convex:

```tsx
const price = useQuery(api.pools.price, { pool: 7 }); // live, no polling
const swap  = useMutation(api.pools.swap);
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

Oracle calls use the range 40 to 49 so they never collide with other additions. Queries run in read-only mode: calls 2, 3, 7, 10, 12, 13, 16, 17, 21 and 22 trap with `WriteInQuery`, and call 40 traps with `RandomnessForbidden`. Bad guest pointers trap; unknown numbers trap. Cross-chain calls are added later with their own numbers and gas costs. Call 42 (`report_request`) also traps `WriteInQuery` in a query, in an action's first phase and in a transform.

**Oracles (calls 40 to 43).** `random_bytes` derives from the block's beacon: `SHA256("btb.rand.out.v1" || SHA256("btb.rand.tx.v1" || beacon_seed || tx_id) || call_index:u64be || block:u32be)`, so replicas agree, transactions differ and every call in a transaction differs. `btb dev` has no beacon and answers marked functions from a fixed public seed (not random). `price` returns the last published stake-weighted median of the validators' signed observations (`NOT_FOUND` before the first one, `PRICE_PAUSED` while the circuit breaker is open); `price_twap` averages it over a window of blocks. In Rust: `price(pair) -> Option<Price>`, `price_checked(pair) -> Result<Price, PriceError>` (`NotFound | Paused`) and `price_twap(pair, window) -> Result<Twap, PriceError>` (adds `BadWindow`); in C `btb_price`/`btb_price_twap` with `BTB_PRICE_PAUSED` and `BTB_PRICE_BAD_WINDOW`. Design, safety argument and fees: [ORACLES.md](ORACLES.md).

`table_index_scan` (call 15) scans one secondary index of a table between two bounds (`unbounded`, `included` or `excluded`; typed values `u64`, `i64`, `u128`, `bytes`, `string`), ascending or descending, with a `limit` (capped at 256) and an opaque resume cursor, and returns row ids ordered by (value, id). The contract loads rows with `table_get`. It returns the full result length and copies only if it fits in `out_cap`, so retry with a bigger buffer. A malformed request traps. In Rust: `Table::index_eq("owner", Key::Bytes(&addr.0), limit, &cursor)` or `host::table_index_scan`. The scan is recorded in the call's read set as a key range, so a subscribed query re-runs exactly when an entry in that range changes.

**Declaring indexes (temporary).** Contracts cannot declare indexes yet. For now the host registers them per table (`btb_runtime::Tables::register`; `btb-dev` takes `--index Table:index:addr@offset`). A table with no registered index returns no results. A declaration in the contract itself (and in its deploy manifest) replaces this later; see [DEV_SERVER.md](DEV_SERVER.md).

**User tokens (calls 16 to 23).** `btb_sdk::token::Token` wraps them: `Token::create(name, symbol, decimals, initial_supply, mint_authority)`, `.transfer(&to, amount)`, `.balance_of(&holder)`, `.info()`, `.holders(cursor, limit)`, `.mint(&to, amount)`, `.burn(amount)`; `contract_address()` gives a contract its own address (for naming itself as mint authority). They are separate from native BTB (calls 6 and 7): creating a token never mints BTB. `status` is `0` ok, `1` invalid name/symbol/decimals, `2` insufficient balance, `3` unknown token, `4` not the mint authority, `5` overflow, `6` unsupported by the host. Semantic failures return a status and change nothing; a malformed request traps. Balances belong to addresses, and as with native `token_transfer`, a contract moves tokens only from its own balance. Holders are paged by **address** (ascending, non-zero balances only; the last address returned is the cursor), which stays stable while balances change; rank by balance in a contract-owned table if needed. In C see the `btb_token_*` wrappers in `include/btb.h`. Details: [TOKENOMICS.md](TOKENOMICS.md#user-tokens).

Argument and result bytes use the canonical encoding specified in [ENCODING.md](ENCODING.md).

### C

`include/btb.h` is a freestanding C header (only `<stdint.h>`) with the call numbers above, a `static inline` `ecall` wrapper per host call (`btb_storage_read`, `btb_return_data`, ...) and entry-point macros: `BTB_QUERY(name)`, `BTB_MUTATION(name)`, `BTB_CRON(name)`, `BTB_ACTION(name)` define `btb_<kind>_<name>(const uint8_t *ptr, uint64_t len)`. Compile with `clang --target=riscv64-unknown-elf -march=rv64im -mabi=lp64 -nostdlib -ffreestanding -O2 -mno-relax` and link with `ld.lld` and a linker script like `examples/c-counter/link.ld`. See `examples/c-counter/` for a working counter; a test checks the header's call numbers against `btb_vm::host::abi`. For oracles it has `btb_random_bytes`, `btb_price`, `btb_report_request` and `BTB_RANDOMNESS(mutation, name);` (or `cron`), which exports the `btb_randomness_<kind>_<name>` marker next to `BTB_MUTATION(name)`.
