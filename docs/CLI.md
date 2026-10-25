# The `btb` command line

`btb` (crate `crates/btb-cli`) is the Phase 3 developer entry point: scaffold a contract project, build it, run it with hot reload and package it for deploy. Build it with `cargo build -p btb-cli` (binary `target/debug/btb`).

```
btb init <name> [--sdk-path DIR | --sdk-git URL]
btb build
btb dev [--port N] [--data DIR]
btb deploy
```

`build`, `dev` and `deploy` run inside a project (the directory with `Cargo.toml`). You need the pinned toolchain from `rust-toolchain.toml` with the `riscv64imac-unknown-none-elf` target and `rust-src`; `rustup` installs them on first use.

## `btb init <name>`

Creates `./<name>/` with the same structure as `examples/todo`: `Cargo.toml`, `.cargo/config.toml` (rv64im flags, `build-std`), `link.ld`, `rust-toolchain.toml`, `src/main.rs` (a `Note` table with `add`, `toggle`, `list_mine`) and `web/index.html` (a live page that calls those three functions). The templates are embedded in the binary. The source is `src/main.rs`, not `lib.rs`: a contract is a `no_std`/`no_main` binary, which is what produces the ELF.

It also writes `.vscode/settings.json`, `.vscode/extensions.json` and `rust-analyzer.toml` so rust-analyzer uses the RISC-V target and `build-std` and expands the `#[btb::*]` macros; see [LSP.md](LSP.md).

The generated `Cargo.toml` depends on `btb-sdk`. Where it finds it is configurable:

- `--sdk-path DIR` writes `btb-sdk = { path = "DIR", ... }`. A relative `DIR` is relative to the new project, e.g. `--sdk-path ../BTB-Chain/crates/btb-sdk`.
- `--sdk-git URL` writes a git dependency (cargo finds the crate in the repository by name).
- Default: `$BTB_SDK_PATH`, else the absolute path of `crates/btb-sdk` in the checkout `btb` was built from. Set one of the above when you move the project or install `btb` elsewhere. `btb-sdk` is not published to crates.io yet.

The project's `[package.metadata.btb] indexes = ["Note:owner:addr@0"]` lists the temporary host-side index declarations (see [DEV_SERVER.md](DEV_SERVER.md)); `btb dev` registers them and `btb deploy` records them in the manifest.

## `btb build`

Runs `cargo build --release` for `riscv64imac-unknown-none-elf` with the environment of the examples' `build.sh` (`RUSTC_BOOTSTRAP=1`; `RUSTFLAGS`, `CARGO_BUILD_TARGET` and `CARGO_TARGET_DIR` are cleared so `.cargo/config.toml` wins), copies the ELF to `target/btb/<name>.elf`, validates it with `btb_vm::Contract::from_elf` and prints the entry points (`query list_mine`, `mutation add`, ...). A rejected ELF is an error.

## `btb dev`

Builds, starts `btb-dev` (as a library) on `127.0.0.1:<port>` (default 8787, `0` = any free port) and serves `web/index.html` at `/`. State is in memory, or in RocksDB with `--data DIR` when `btb` is built with `--features rocksdb`. Every 500 ms it checks the mtime and size of `src/**`, `Cargo.toml` and `web/index.html`. On a change it rebuilds and swaps the new contract into the running server:

- **State is kept**: the store, block height and indexes stay; only the code changes.
- **Subscriptions re-run** against the new code and push a new event if their result changed.
- **Errors do not crash it**: a failed build or an invalid ELF is printed and the previous build keeps running until the next successful save.

Same limits as the dev server: one contract, no auth, development only. Never expose it. Schema changes that are incompatible with stored rows (a different `Note` layout) are not migrated: restart `btb dev` for a clean state.

## `btb deploy`

There is no chain yet, so `deploy` builds and writes a **deploy bundle** to `target/btb/deploy/`:

- `<name>.elf`, the contract;
- `manifest.json`: `name`, `elf`, `size`, `code_hash` (SHA-256 of the ELF, lowercase hex), `hash_algorithm`, `entry_points` (`{kind, name}`), `crons` (`{function, every_blocks}`, from `#[btb::cron(every = "N blocks")]`) and `indexes`.

It then prints the hash and a note that deploying to a network arrives in Phase 4, when the same bundle becomes the deploy transaction payload.

## Tests

`crates/btb-cli/tests/cli.rs` runs the real toolchain: `init` into a temp dir (sdk path pointing at this repo) then `build` succeeds and lists the entry points; the deploy manifest hash equals the SHA-256 of the ELF; `dev` on an ephemeral port reloads after a source edit, the new function becomes callable, old state is retained, a broken edit keeps the old build, and an open subscription gets the changed result.
