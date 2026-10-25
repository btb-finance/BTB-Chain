# counter

Example BTB contract: a persistent counter with a `Change` table.

| Kind | Function |
| --- | --- |
| query | `get() -> u64`, `change(id: u64) -> Option<(u64, u64, u64)>` |
| mutation | `increment(by: u64) -> u64` (aborts on overflow) |
| cron | `tick()` every 10 blocks (`#[btb::cron(every = "10 blocks")]`) |
| query | `increment_in_query(by)`: deliberately illegal, a test fixture showing `WriteInQuery` |

```
./build.sh        # writes counter.elf (git-ignored)
cargo test -p btb-tests -- --nocapture   # builds it and runs it in btb-vm
```

This crate is its own workspace (not a member of the root one) and only builds for `riscv64imac-unknown-none-elf`.

## Why the build looks like this

`btb-vm` runs only rv64im. The stable Rust target `riscv64imac-unknown-none-elf` is the closest one installed, so `.cargo/config.toml`:

- turns off the compressed and atomic extensions (`-c,-zca,-a`; LLVM keeps `zca` even with `-c`);
- uses static relocation, `--no-relax` (no `gp` register in the VM) and `link.ld` to produce a static, non-PIE `ET_EXEC` with separate R+X, R and R+W segments;
- keeps all `btb_*` entry points alive with `--export-dynamic-symbol=btb_*` (no dynamic section is emitted);
- rebuilds `core` and `alloc` with `-Zbuild-std`, because the prebuilt ones contain compressed instructions that get linked in through `Vec`. This needs the `rust-src` component and `RUSTC_BOOTSTRAP=1` on the stable compiler (`build.sh` and the test set it). A custom target JSON would need the same flags.

`crates/btb-tests` sweeps every executable word of the ELF through the VM decoder to prove no compressed, atomic or floating-point instruction slipped in.
