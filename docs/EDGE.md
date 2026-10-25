# Edge nodes

Status: working (`crates/btb-edge`, tests in `crates/btb-edge/tests/edge.rs`). Not done: relaying live subscriptions to clients, proof checking of site hashes inside the edge, bandwidth-priced requests (see [Limits](#limits)).

An edge node is a CDN node for BTB. It sits **in front of hosting nodes** ([HOSTING.md](HOSTING.md)), caches popular content close to users, and earns a fee for every request it answers beyond a free quota ([SCALING.md](SCALING.md), [TOKENOMICS.md](TOKENOMICS.md)). It is not a validator and does not follow the chain; it never has to be trusted, because everything it caches is either checked by the edge before it is kept or carries a proof the client checks.

```
user ──▶ edge ──▶ hosting node(s)   (site files, queries, proofs; failover across several)
          │  └──▶ storage nodes     (large files by root, k of n pieces)
          └─────▶ node RPC          (payment channels, where a file's pieces are, Close tx)
```

## Run

```
btb-edge --upstream host1:8788,host2:8788 --rpc http://node:8545 \
         [--listen 127.0.0.1:8090] [--mem-mb 256] [--disk-dir DIR --disk-mb 4096] \
         [--site-ttl-secs 5] [--max-object-mb 1024] [--max-subscriptions 1024] \
         [--key edge-key.json --price 10 --free-requests 100 --free-window-secs 3600 \
          --redeem-min 1000 --redeem-every-secs 3600] \
         [--relayer <64 hex>]
```

Library: `Edge::start(EdgeConfig, Services)`; `Services` takes the upstream (`Upstream` trait, `HttpUpstream` for real hosts), a `StorageSource`, node `Rpc` and a `Submitter`, so everything is testable with fakes.

## What is cached, and how it stays correct

| Route | Cached as | Verified by the edge | Lifetime |
| --- | --- | --- | --- |
| `GET /site/<contract>/<path>` | body by SHA-256 | SHA-256(body) must equal the host's `X-BTB-Content-Hash`, else the next host is tried (`tampered_rejected`) | Content: forever (LRU by bytes). The `path -> hash` mapping is re-checked after `site_ttl` with `If-None-Match`: an unchanged file costs a `304`, a republished one is fetched |
| `GET /file/<root>`, and site paths the host answers with `X-BTB-Storage-Root` | body by storage root | `btb-storage` fetch: each piece's Merkle path against the root, any `k` of `n`, decode, re-encode, compare the id | forever (LRU by bytes), `Cache-Control: immutable` |
| `GET /c/<contract>/query/<fn>?args=&caller=[&proof=1]` | body per (contract, fn, args, caller, proof, **height**) | nothing: proof bundles are passed through and **clients verify them** like against any hosting node | until an upstream subscription reports a change |
| `/proof/...`, `/api/site/...` | not cached | none | relayed with failover |

The site hash is the one the host advertises and the proof route covers (`/proof/<contract>/site/<path>`). The edge checks the bytes against it; a client that wants the hash anchored to a validator-attested state root fetches the proof through the edge and checks it as it would against a hosting node.

### Query invalidation

Query results depend on state, so "latest" needs a signal. On the first cache fill of a (contract, fn, args, caller) the edge opens one upstream SSE subscription (`/c/<contract>/subscribe/<fn>`) and keeps its result valid while the stream is alive:

- The first event is the result at registration time. If it differs from what was cached, the cache entry was fetched before a block the stream already knew about and is dropped.
- Every later event means a block changed the result: the "latest" entries (plain and `proof=1`) are dropped and the next request refetches. A generation counter keeps a fetch that raced with a block from being stored as latest.
- When the stream ends or errors, the entries are dropped and the next request subscribes again: **the edge never serves a "latest" it can no longer vouch for.**
- Old heights stay addressable: `?height=N` is answered from cache if the edge still holds that height, else `404`. `X-BTB-Height` tells at which block the answer was computed (a result that no block changed is still current).
- At most `max_subscriptions` streams (one thread each); beyond that queries are passed through uncached.

### Other behavior

- **Request coalescing.** Concurrent misses for the same object share one upstream fetch (`coalesced` counts the waiters).
- **Ranges.** `Range: bytes=a-b`, `a-` and `-n` give `206` with `Content-Range`; unsatisfiable ranges `416`; several ranges are answered with the whole body. Ranges are cut from the cached object, so a big file is fetched once.
- **`ETag` / `304`.** Site files: the SHA-256; storage files: the root; queries: SHA-256 of the body. `If-None-Match` answers `304`.
- **Budgets.** `mem_bytes` bounds memory (LRU by bytes). With a disk tier, content-addressed objects evicted from memory are written to disk (`SHA-256` trailer checked on every read, so bit rot is a miss) and the disk tier is itself LRU by bytes; it survives restarts. Queries are memory only. An object larger than the memory budget goes to disk only, or is served without being kept.
- **Failover.** `HttpUpstream` tries hosts in order; a transport error, a `5xx` or a failed verification moves on to the next host and sets the failing one aside for a cooldown. A `404`/`400` is an answer, not a reason to fail over.
- **Headers.** `X-BTB-Cache: HIT | MISS | REVALIDATED | COALESCED`; site files keep the host's `nosniff` and CSP sandbox headers.

## Payments

Optional (`EdgeConfig::pay`, or `--key` with `--price`). Each client address gets `free_requests` per `window` for nothing. Beyond that a request must carry `X-BTB-Voucher: <hex of the 120-byte voucher>` ([`btb-voucher`](TOKENOMICS.md#payment-channels-for-per-request-fees)), otherwise the edge answers `402` with `X-BTB-Price` and its payee address.

- The voucher must be for **this chain**, for a channel whose **payee is the edge's account** and which is open and not closing. The edge looks the channel up through node RPC (`btb_channel`, once per channel; again when a voucher exceeds the known deposit, so top-ups are picked up).
- It must be signed by the channel's payer key and **strictly increase** both amount and nonce over the best voucher the edge holds (`Ledger::accept`): replays, reordered and lower vouchers are refused with `402`. Amounts are cumulative.
- The request is served when the cumulative amount covers everything charged so far plus `price` per request. A request with a voucher is always a paid request; a bad voucher is refused even inside the free quota.
- The edge keeps only the best voucher per channel. `Edge::redeem()` (and a background task every `redeem_every`) sends `ChannelOp::Close` with it, signed with the edge's key, for channels that earned at least `redeem_min`. The channel is then gone; further vouchers for it are refused. A payer that disappears can still be settled any time before its timeout close finishes ([CHAIN.md](CHAIN.md#payment-channels)).
- `/health` and `/metrics` are free.

## Transaction relay

With `--relayer <address>` (and `--rpc`), or `EdgeConfig::relay`, the edge forwards client-signed transactions: `GET /relay` returns `{relayer, chain_id}`, `POST /tx` (hex body or `{"tx": hex}`) checks the signature, the chain id and that the transaction names this relayer, then sends it through node RPC (`btb_sendTransaction`) and answers `{id, relayer}`; `GET /tx/<id>` returns the receipt from the node. Same rules as a hosting node ([HOSTING.md](HOSTING.md#relaying-transactions)): the client signs the relayer, the edge cannot change it, the account must be registered on chain (`Host::Register`) to be paid, and the executor then pays it 20% of the fee ([CHAIN.md](CHAIN.md#fees)). These routes are free (not behind the voucher quota); the `relayed` counter is in `/metrics`. Point `--rpc` at validators or RPC nodes that accept transactions.

## Metrics

`GET /metrics` (Prometheus text): `requests`, `hits`, `misses` (a revalidated file is a hit; a request that waited on another's fetch is a miss and also `coalesced`), per kind `site_*`, `file_*`, `query_*`, `bytes_out`, `upstream_requests`, `upstream_bytes`, `storage_fetches`, `storage_bytes`, `storage_failures`, `tampered_rejected`, `invalidations`, `subscriptions_opened`, `passthrough`, `free_served`, `paid_served`, `payment_required`, `vouchers_accepted`, `vouchers_rejected`, `relayed`, `redeem_txs`, `redeem_failures`, `redeemed_units`, and gauges `cache_memory_bytes`, `cache_memory_entries`, `cache_disk_bytes`, `subscriptions`, `unsettled_units`. `GET /status` has a short JSON summary.

## Security notes

- An upstream host cannot make the edge serve wrong bytes for a hash or a root: both are checked before caching. A host that lies about the *hash of a path* (serves other valid content for it) is caught only by a client that checks the Merkle proof of the path; the edge relays proofs untouched for that.
- Disk files are checked against their own SHA-256 trailer on every read.
- Voucher checks are those of `btb-voucher` (strict Ed25519, chain and channel binding). The edge never signs anything except the `Close` transaction.
- Request size and connection limits are minimal (`max_connections`, header caps, 10 s read timeout); put TLS and a rate limiter in front.

## Limits

- Live subscriptions are not relayed: subscribe on a hosting node (`/c/<contract>/subscribe/<fn>` answers `501`).
- Site `path -> hash` freshness is time-based (`site_ttl`), not event-based: a republished file shows up within the TTL.
- A price is flat per request, not per byte.
- A storage-backed file is fetched whole into memory before it is served.
- The edge checks site content against the host's advertised hash, not against a state root: see the security note.
- Each distinct "latest" query holds one upstream stream and one thread.
