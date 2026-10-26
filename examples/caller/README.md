# caller

Example contract: a router that calls other contracts with `call_contract` (`btb_sdk::call`, `btb_sdk::query`). `forward_transfer` and `try_transfer` move tokens held by the router through the [token](../token/README.md) example, whose `caller()` is then the router; `balance_via` is a query that asks the token's query.

The remaining functions are fixtures for the call rules: revert scope (`put_then_abort`, `try_put_then_abort`), depth and re-entrancy (`chain`), caller identity (`ident*`), exact gas (`burn`, `call_burn`), randomness (`roll`, `peek_random`, `call_roll`, `marked_calls_peek`), read-only enforcement (`bad_query`), storage deposits (`grow`, `grow_via`), native transfers by the callee (`pay`, `pay_via`) and the action callback rule (`fetch`, `fetch_callback`, `call_callback`).

```
./build.sh        # writes caller.elf (git-ignored)
cargo test -p btb-tests --test calls
```

Same build setup as [counter](../counter/README.md).
