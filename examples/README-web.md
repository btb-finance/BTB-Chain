# Example web pages

`launcher`, `dex`, `nft` and `todo` each have `web/index.html`, which `btb dev` serves at `/` (it serves no other files). The three newer pages talk to the contract with [`@btb/client`](../sdk-ts) (hex-encoded canonical arguments, `subscribe` for live data), so the client is **inlined** into each page between `<!-- btb-client:begin -->` and `<!-- btb-client:end -->` as a classic script that defines the global `btb`. There is no build step when you run an example.

`examples/todo` also has a `<!-- btb-api:begin -->` block: the typed client `btb codegen --ts` generated into `examples/todo/api/api.js`, inlined by the same script as `const api` (`client.subscribe(api.list_mine, [], render)`).

Refresh the inlined copy after changing `sdk-ts/src` or a generated `api/api.js`:

```
(cd sdk-ts && npm ci && npm run build) && node examples/sync-sdk.mjs     # --check only reports stale pages
```

Each page has a Playwright smoke script, `web/smoke.js`, that starts `btb dev` in the example directory on a free port and drives two or three browser tabs as different dev users (needs `playwright` with Chromium and `cargo build -p btb-cli`):

```
node examples/launcher/web/smoke.js && node examples/dex/web/smoke.js && node examples/nft/web/smoke.js
```

Caller identity is the dev-only `?caller=` (any text is hashed with SHA-256, 64 hex characters are the address); the pages compute the same address in the browser to show "you" and to address transfers.
