# dex

Example BTB contract: a constant-product AMM (`x * y = k`) over two internal asset balances kept in storage. It is self-contained (no cross-contract calls exist yet), so reserves are bookkeeping, not real tokens. All math is checked `u128`.

| Kind | Function |
| --- | --- |
| mutation | `add_liquidity(a, b) -> u128`: first deposit mints `isqrt(a*b)` shares, later ones `min(a*total/ra, b*total/rb)`; shares are tracked per caller |
| mutation | `swap_a_for_b(amount_in, min_out) -> u128`: 0.3% fee, `out = in*997*rb / (ra*1000 + in*997)` rounded down; aborts with `slippage` if `out < min_out` |
| query | `reserves() -> (u128, u128)`, `quote(amount_in) -> u128`, `shares_of(addr) -> u128` |
| query | `swap_in_query(amount_in)`: deliberately illegal, a test fixture showing `WriteInQuery` |

Aborts: `zero amount`, `zero shares`, `no liquidity`, `slippage`, `overflow`. Removing liquidity and B-to-A swaps are not implemented yet.

```
./build.sh        # writes dex.elf (git-ignored)
cargo test -p btb-tests --test dex -- --nocapture
```

Same build setup as [counter](../counter/README.md).
