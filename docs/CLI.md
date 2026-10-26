# The `btb` command line

`btb` (crate `crates/btb-cli`) is the Phase 3 developer entry point: scaffold a contract project, build it, run it with hot reload and package it for deploy. Build it with `cargo build -p btb-cli` (binary `target/debug/btb`).

```
btb init <name> [--sdk-path DIR | --sdk-git URL]
btb build
btb codegen --ts DIR
btb dev [--port N] [--data DIR]
btb deploy [--rpc URL --key FILE [--gas-limit N]]
btb call --rpc URL --key FILE [--gas-limit N] <contract> <fn> [args]
btb query --rpc URL <contract> <fn> [args]
btb keygen <FILE>
btb store <file> --rpc URL --key FILE [--k N --m N --periods N]
btb fetch <root> --rpc URL [--out FILE]
btb storage-node run --rpc URL --key FILE --dir DIR [--listen ADDR] [--endpoint URL] [--deposit UNITS]
```

`build`, `dev` and `deploy` run inside a project (the directory with `Cargo.toml`). You need the pinned toolchain from `rust-toolchain.toml` with the `riscv64imac-unknown-none-elf` target and `rust-src`; `rustup` installs them on first use.

## `btb init <name>`

Creates `./<name>/` with the same structure as `examples/todo`: `Cargo.toml`, `.cargo/config.toml` (rv64im flags, `build-std`), `link.ld`, `rust-toolchain.toml`, `src/main.rs` (a `Note` table with an `#[index]` on `owner` and `add`, `toggle`, `list_mine`) and `web/index.html` (a live page that calls those three functions). The templates are embedded in the binary. The source is `src/main.rs`, not `lib.rs`: a contract is a `no_std`/`no_main` binary, which is what produces the ELF.

It also writes `.vscode/settings.json`, `.vscode/extensions.json` and `rust-analyzer.toml` so rust-analyzer uses the RISC-V target and `build-std` and expands the `#[btb::*]` macros; see [LSP.md](LSP.md).

The generated `Cargo.toml` depends on `btb-sdk`. Where it finds it is configurable:

- `--sdk-path DIR` writes `btb-sdk = { path = "DIR", ... }`. A relative `DIR` is relative to the new project, e.g. `--sdk-path ../BTB-Chain/crates/btb-sdk`.
- `--sdk-git URL` writes a git dependency (cargo finds the crate in the repository by name).
- Default: `$BTB_SDK_PATH`, else the absolute path of `crates/btb-sdk` in the checkout `btb` was built from. Set one of the above when you move the project or install `btb` elsewhere. `btb-sdk` is not published to crates.io yet.

Indexes are declared in the contract (`#[index]` on a `#[btb::table]` field) and read from the ELF by `btb dev`, `btb deploy` and the chain; the project needs no metadata. The old `[package.metadata.btb] indexes = ["Note:owner:addr@0"]` is **deprecated**: it is still honoured for tables the contract does not declare (and recorded in the deploy manifest), with a warning, and ignored for tables it does. `fields = [...]` (dashboard decode hints) remains optional; hints are derived from the schema by default.

## `btb build`

Runs `cargo build --release` for `riscv64imac-unknown-none-elf` with the environment of the examples' `build.sh` (`RUSTC_BOOTSTRAP=1`; `RUSTFLAGS`, `CARGO_BUILD_TARGET` and `CARGO_TARGET_DIR` are cleared so `.cargo/config.toml` wins), copies the ELF to `target/btb/<name>.elf`, validates it with `btb_vm::Contract::from_elf` and prints the entry points (`query list_mine`, `mutation add`, ...) and the declared tables (`table Note (indexes: owner)`). A rejected ELF or an invalid schema is an error. It also writes `target/btb/<name>.schema.json`: the contract's tables (fields with their `btb-codec` types, `indexes`, `primary`) and functions (`kind`, `name`, `args`, `returns`, `fallible`), the input of `btb codegen`.

## `btb codegen`

`btb codegen --ts DIR` builds the project and writes the typed client for its frontend to `DIR`:

- `api.ts`: one `interface` and codec per table (`export interface Todo { owner: Uint8Array; done: boolean; created: bigint; text: string }`) and `export const api = { add: mutationRef<[text: string], bigint>(...), list_mine: queryRef<[], ...>(...), ... }`, using `queryRef` / `mutationRef` / `t` from `@btb/client`;
- `api.js` and `api.d.ts`: the same without a build step, for a page that inlines `@btb/client` (see [TS_SDK.md](TS_SDK.md#typed-clients-from-the-contract-schema)).

Queries and mutations are generated; crons, actions and transforms are not part of the client. A function whose argument or result type the schema cannot describe (a Rust type that is neither a primitive, a tuple, `Vec`, `Option` nor a table struct) is skipped with a warning. The output is a pure function of the schema, so it can be checked in and diffed: `examples/todo/api/` holds the generated files (and `todo.schema.json`), which `crates/btb-cli/tests/cli.rs` compares with fresh output (`BTB_UPDATE_GOLDEN=1` rewrites them).

## `btb dev`

Builds, starts `btb-dev` (as a library) on `127.0.0.1:<port>` (default 8787, `0` = any free port) and serves `web/index.html` at `/`. State is in memory, or in RocksDB with `--data DIR` when `btb` is built with `--features rocksdb`. Every 500 ms it checks the mtime and size of `src/**`, `Cargo.toml` and `web/index.html`. On a change it rebuilds and swaps the new contract into the running server:

- **State is kept**: the store, block height and indexes stay; only the code changes.
- **Subscriptions re-run** against the new code and push a new event if their result changed.
- **Errors do not crash it**: a failed build or an invalid ELF is printed and the previous build keeps running until the next successful save.

Same limits as the dev server: no auth, development only. Never expose it. On reload it also **evolves the schema**: new indexes are backfilled for existing rows, `#[btb::migration]` functions run over stored rows, and an incompatible change without a migration is refused (the old build keeps running) with a printed explanation; see [DEV_SERVER.md](DEV_SERVER.md#schema-evolution-on-reload).

### Several contracts

A project can hold several contracts. List their directories (relative to the project root, each one a contract project made by `btb init`) in the root `Cargo.toml`:

```toml
[workspace]
exclude = ["router", "token"]

[workspace.metadata.btb]
contracts = ["router", "token"]       # [package.metadata.btb] works too
```

`btb build`, `btb dev` and `btb deploy` then act on every listed contract (the first is the primary one). `btb dev` builds all of them, serves them from one server and one dashboard, gives each the stable address `SHA256("btb.dev.contract.v1" || package name)` (printed at startup), and lets them call each other with `call_contract`. It watches each contract directory separately: saving `router/src/main.rs` rebuilds and reloads only the router, state of every contract is kept, subscriptions re-run. Reach a contract at `/c/<package name>/query/<fn>` (see [DEV_SERVER.md](DEV_SERVER.md#several-contracts)); pass the other contract's address as an argument to the router (`dev_address("token")` in Rust, or `GET /contracts`). `web/index.html` of the root (else of the first contract) is served at `/`. A project without `contracts` is the single-contract project `btb init` makes. `btb codegen` and `btb upgrade` still work on one contract at a time (run them in its directory). Test: `multi_contract_dev_cross_calls_and_independent_reloads` in `crates/btb-cli/tests/cli.rs`.

Uploaded files ([DEV_SERVER.md](DEV_SERVER.md#file-storage)) are kept in `target/btb/files`.

## `btb deploy`

Builds and writes a **deploy bundle** to `target/btb/deploy/`:

- `<name>.elf`, the contract;
- `manifest.json`: `name`, `elf`, `size`, `code_hash` (SHA-256 of the ELF, lowercase hex), `hash_algorithm`, `entry_points` (`{kind, name}`), `crons` (`{function, every_blocks}`, from `#[btb::cron(every = "N blocks")]`) and `indexes` (only the deprecated metadata specs for tables the contract does not declare; declared indexes travel inside the ELF, which the chain reads when it validates the `Deploy`).

Without `--rpc` that is all: it prints the hash and where the bundle is.

With `--rpc URL --key FILE` it also deploys the bundle to a node ([NODE.md](NODE.md#json-rpc--http)): the ELF and the manifest's name, crons and indexes become a `Deploy` transaction (binary `Manifest`, see [CHAIN.md](CHAIN.md#transactions)), signed with the key file. It reads `chain_id` from `btb_status` and the nonce from `btb_getBalance`, submits with `btb_sendTransaction`, polls `btb_getReceipt` until the transaction is in a block (a reverted deploy is an error) and prints `deployed: contract <address>` (the address is derived from the sender and the nonce). `--gas-limit N` is only checked against the deploy cost (a deploy always uses exactly its intrinsic gas). The contract account needs a balance for crons to pay their gas, so fund it with a transfer.

`URL` is `host:port` or `http://host:port` (the node's RPC is plain HTTP on loopback; put a proxy in front for remote use).

## `btb upgrade`

```
btb upgrade --rpc URL --key FILE <contract> [--budget-gas N] [--gas-limit N]
btb upgrade --rpc URL <contract> --status
```

Builds the project (like `btb deploy`) and sends an `Upgrade` transaction that replaces the code of the deployed `<contract>` at the same address. The key must be the contract's **upgrade authority**: the deployer by default; `btb deploy --upgrade-authority ADDR` names another account and `btb deploy --immutable` makes the contract impossible to upgrade (an upgrade can also set the policy for later ones; `manifest.json` accepts `"upgrade": "deployer" | "none" | "<address>"`). The chain compares the old and new schema and refuses an incompatible change without a `#[btb::migration]` with the reason; compatible changes and migrations run on chain in the background, paid from `--budget-gas` (default 5,000,000 gas, prepaid at the gas price, the unused part refunded). `--status` prints the progress of the latest upgrade (`migrating`, `cleanup`, `done`, `rolling_back`, `rolled_back` with the error, and per table the rows done and the gas charged; RPC `btb_upgradeStatus`). Details: [CHAIN.md](CHAIN.md#contract-upgrades-and-migrations).

## `btb keygen <FILE>`

Writes a new account key (`{"secret", "public"}` JSON, mode 0600, the same format `btb-node init-testnet` writes for `faucet.key`) and prints the account address. It refuses to overwrite a file. Any `init-testnet` key file works for `--key`.

## `btb call` and `btb query`

`btb call --rpc URL --key FILE [--gas-limit N] <contract> <fn> [args]` signs and sends a `mutation` (default gas limit 2,000,000), waits for the receipt and prints `output` (hex), `gas_used` and `height`. `btb query --rpc URL <contract> <fn> [args]` runs a read-only `btb_query` and prints `output` and `gas_used`; it needs no key.

`--relayer URL` submits through a hosting or edge node instead of straight to the RPC: the CLI reads the node's relayer address (`GET /relay`), signs it into the transaction and `POST`s it to `/tx`; the node forwards it and earns 20% of the fee ([HOSTING.md](HOSTING.md#relaying-transactions)). Nonce, chain id and receipt still come from `--rpc`.

`contract` is the 64-character hex address. `args` is either the canonical encoding as hex ([ENCODING.md](ENCODING.md)) or a JSON array: integers become `u64` (negative ones `i64`), booleans one byte, `"0x"` plus 64 hex characters a raw 32-byte address, any other string a length-prefixed string. Omit it for no arguments. Outputs are hex of the canonical encoding.

## Tests

`crates/btb-cli/tests/cli.rs` runs the real toolchain: `init` into a temp dir (sdk path pointing at this repo) then `build` succeeds and lists the entry points; the deploy manifest hash equals the SHA-256 of the ELF; `dev` on an ephemeral port reloads after a source edit, the new function becomes callable, old state is retained, a broken edit keeps the old build, and an open subscription gets the changed result.

`crates/btb-cli/tests/chain.rs` starts an in-process 4-validator cluster (the `btb-node/tests/lite` helper), builds a copy of `examples/counter` with `every = "3 blocks"` on its cron and runs the real `btb` binary: `deploy --rpc` prints the contract address, `call increment` returns the new total, `query get` reads it and the cron makes it grow on its own; a failing call is a non-zero exit.

## `btb store`, `btb fetch` and `btb storage-node run`

Large files go to storage nodes, not to the chain ([STORAGE.md](STORAGE.md)). `btb store <file> --rpc URL --key FILE [--k 4] [--m 2] [--periods 12]` erasure-codes the file into `k` data + `m` parity pieces, pays the escrow with a `Store` transaction, uploads each piece to the node the chain assigned and prints `root: <hex>`. `btb fetch <root> --rpc URL [--out FILE]` downloads any `k` pieces that verify, decodes and checks the result against the root; it needs no key. `btb storage-node run` starts a storage node (piece store, HTTP API, challenge answering, repair) and, with `--deposit UNITS` and an account that is not registered yet, registers it.
