# market-data-web

All-Rust frontend for [`examples/market-data`](../market-data): [Dioxus](https://dioxuslabs.com) (MIT/Apache-2.0) compiled to WASM. It calls the contract over the `btb dev` / hosting-node HTTP API and decodes answers with `btb-codec`, the same crate the chain uses. No hand-written JavaScript.

- Search a symbol (`PSX:OGDC`) and see quote, valuation, analytics, sector view and daily prices from **one** `stock()` query.
- **Load demo data (dev)** publishes real PSX figures for OGDC and three sector peers as the provider (dev server header mode only).

## Run

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.130   # must match wasm-bindgen in Cargo.lock
./build.sh                                          # writes ../market-data/web/index.html
cd ../market-data && btb dev                        # open http://127.0.0.1:8787/
```

`build.sh` inlines the JS glue and the WASM into one `index.html` (about 0.9 MB), because `btb dev` serves exactly that file, from the same origin as `/query` and `/mutation`. `wasm-opt` shrinks it further when installed.

`src/types.rs` mirrors the contract's types field for field; the canonical encoding is the fields in order. Sharing one types crate between contract and frontend is the next step.

The same code can later target desktop and mobile with Dioxus' `desktop`/`mobile` features.
