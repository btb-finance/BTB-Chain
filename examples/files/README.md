# files

Example contract: file references with `btb_sdk::files` (`file_info`, `file_pin`). It keeps a name -> root map, refuses roots that are not stored and paid (`attach`), and can pay for storage itself from its balance (`pin_and_attach`).

```
./build.sh        # writes files.elf (git-ignored)
cargo test -p btb-tests --test files
```

Same build setup as [counter](../counter/README.md). Design: [docs/STORAGE.md](../../docs/STORAGE.md), "Files from contracts".
