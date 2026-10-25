# Language server

Rust contracts get autocomplete, hover types and ordinary errors from **rust-analyzer**. BTB adds two things on top: project templates that make rust-analyzer work out of the box, and `btb-lsp`, a small language server with BTB-specific checks.

## rust-analyzer in a `btb init` project

`btb init` writes `.vscode/settings.json` (VS Code) and `rust-analyzer.toml` (every other editor). They set:

| Setting | Why |
| --- | --- |
| `cargo.target = riscv64imac-unknown-none-elf` | Analyze for the contract target, not the host |
| `cargo.extraEnv` / `check.extraEnv = RUSTC_BOOTSTRAP=1` | `build-std` in `.cargo/config.toml` is unstable |
| `check.allTargets = false`, `cargo.allTargets = false` | A bare-metal crate has no test harness |
| `procMacro.enable`, `procMacro.attributes.enable` | Expand `#[btb::query]` and friends so generated items resolve |

A test in `crates/btb-cli/tests/cli.rs` scaffolds a project and runs `cargo check` with exactly these settings.

## btb-lsp

`cargo build -p btb-lsp` makes `target/debug/btb-lsp`, an LSP server on stdio (std plus `serde_json`, sources parsed with `syn`). It is best effort and never fails on invalid code: a file that does not parse gets no BTB diagnostics (rust-analyzer reports syntax errors). Run it alongside rust-analyzer, not instead of it.

### Diagnostics

| Code | Severity | Rule |
| --- | --- | --- |
| `btb::query-write` | error | A `#[btb::query]` calls a write API: `storage_set`, `storage_remove`, `table_insert/update/delete`, `token_transfer`, `token_*_raw` writes, `Type::insert/update/delete/transfer/mint/burn/create`, or `.insert()`, `.update(id)`, `.delete(id)`, `.transfer(to, n)`, `.mint(to, n)`, `.burn(n)`. Reads and `Vec::insert` are fine. |
| `btb::float` | error | `f32`, `f64` or a float literal (determinism) |
| `btb::forbidden-std` | error | Any `std` path or `use std::...` (contracts are `no_std`); `std::thread` and `std::time` say why |
| `btb::forbidden-rand` | error | `rand`, `getrandom`, `fastrand`; randomness only comes from the chain beacon |
| `btb::forbidden-time` | error | `SystemTime`, `Instant` |
| `btb::entry-name` | error | Entry point name outside `[a-z0-9_]{1,64}` |
| `btb::duplicate-entry` | error / warning | Same name twice for one kind (error) or across kinds (warning) |

Items under `#[cfg(test)]` are skipped. The query check is syntactic: it does not follow calls into helper functions, and the VM still traps `WriteInQuery` at run time.

### Hover and completion

- Hover on `btb`, `query`, `mutation`, `cron`, `action` or `table` in `#[btb::...]` shows the kind's rules and gas notes.
- Completion after `btb::` lists the macros, after `host::` the host calls (write calls are marked `WRITE`), elsewhere both.

### Editors

- VS Code: [editors/vscode](../editors/vscode) launches `btb-lsp` next to rust-analyzer.
- Others: register `btb-lsp` as an additional server for Rust files (Neovim `vim.lsp.start{cmd={"btb-lsp"}}`, Helix `languages.toml`).

Positions use `char` columns, not UTF-16 units; they agree for ASCII sources.
