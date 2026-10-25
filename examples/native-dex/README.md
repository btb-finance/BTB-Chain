# Native DEX page

A single-file web page for the native DEX module (`crates/btb-chain/src/dex.rs`, [docs/CHAIN.md](../../docs/CHAIN.md#native-dex)). It is embedded in `btb-node` and served at `GET /dex` on every node's RPC port, so it needs nothing but a node:

```
cargo build -p btb-node -p btb-faucet
scripts/testnet-local.sh --no-build          # 4 validators + faucet on loopback
# open http://127.0.0.1:8545/dex
```

The page keeps an Ed25519 seed in the browser (`?seed=<64 hex>` or the **New** button; testnet only), signs `Dex` transactions with WebCrypto and sends them with `btb_sendTransaction`. It shows the block height and the next clearing block, the order book by price level, the last clearing price and recent clearings, your open orders (with **Cancel**) and your closed orders with their average price. It polls the `btb_dex*` RPC methods once a second. Get BTB from the faucet; user tokens come from any contract using the native token module (for example `examples/launcher`: `launch` then `claim`).

How to read it: a pair is shown as *quote per base* (BTB is always the base of a pair with BTB). **Buy base** places a bid, **Sell base** an ask. Orders from every block of a batch window are cleared together at the end of the window at one price, so being first in a block does not help. Funds are escrowed when you place the order and refunded when it ends (filled, expired after the number of batches you chose, or cancelled).

## Smoke test

```
cargo build -p btb-node -p btb-faucet && node examples/native-dex/web/smoke.js
```

Needs the `playwright` package with Chromium and the built launcher ELF (built automatically with `cargo build --release` in `examples/launcher` if missing). It starts a local testnet, funds three accounts from the faucet, deploys the launcher, creates two tokens, and drives three browser tabs: a resting ask appears live in another tab's book, a crossing bid clears at one price with the exact amounts and fee checked over RPC, a cancel refunds, a token/token pair trades, and token supplies are conserved.
