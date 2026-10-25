# token

Example BTB contract: a fungible token kept in contract storage (balances under `bal:<address>`, not the native token host calls). Amounts are `u128`.

| Kind | Function |
| --- | --- |
| mutation | `init(supply: u128)`: once; the caller receives the whole supply, a second call aborts |
| mutation | `transfer(to: Address, amount: u128) -> bool`: aborts on insufficient balance or recipient overflow |
| query | `balance_of(addr: Address) -> u128`, `total_supply() -> u128` |
| query | `transfer_in_query(to, amount)`: deliberately illegal, a test fixture showing `WriteInQuery` |

```
./build.sh        # writes token.elf (git-ignored)
cargo test -p btb-tests --test token -- --nocapture
```

Same build setup as [counter](../counter/README.md) (own workspace, `riscv64imac-unknown-none-elf`, rv64im flags in `.cargo/config.toml`).
