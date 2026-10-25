# launcher

Example BTB contract: a token launcher with a faucet, built on the native user-token module (`btb_sdk::token::Token`, host calls 16 to 22). User tokens are separate from native BTB; launching one never mints BTB.

| Kind | Function |
| --- | --- |
| mutation | `launch(name, symbol, decimals, supply, faucet_amount, mintable) -> Token`: creates a token held by the contract and registers the faucet; `mintable` makes the contract the mint authority |
| mutation | `claim(token) -> u128`: once per caller, sends `faucet_amount`; aborts when already claimed or the faucet is empty |
| mutation | `mint_more(token, to, amount)`, `burn_faucet(token, amount)`: launcher only; abort unless the token is mintable |
| query | `balance_of(token, who)`, `info(token)`, `holders(token, cursor, limit)` (ascending address, paged) |
| query | `claim_in_query(token)`: deliberately illegal, a test fixture showing `WriteInQuery` |

```
./build.sh        # writes launcher.elf (git-ignored)
cargo test -p btb-tests --test launcher -- --nocapture
```

Same build setup as [counter](../counter/README.md).
