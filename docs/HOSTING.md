# Hosting nodes

Status: working on a local multi-validator network (`crates/btb-host`, tests in `crates/btb-host/tests/host.rs`). Not done: payment-channel fees on requests, RocksDB persistence of the follower's state, state sync from snapshots (see [Limits](#limits)). Transaction relay with the 20% fee share: [below](#relaying-transactions). Large files from storage nodes are served ([below](#large-files-from-storage-nodes)); edge caching in front of hosting nodes is [EDGE.md](EDGE.md).

A hosting node is a **non-validator that follows the chain without trusting anyone** and serves the read side of an app: queries, live subscriptions, websites and Merkle proofs. Doubling the number of hosting nodes doubles read capacity without adding work for validators ([SCALING.md](SCALING.md)).

```
validators (consensus)  ──verified block sync──►  btb-host  ──HTTP/SSE──►  browsers, @btb/client
        ▲                                          executes every block,
        └── signed transactions (btb_sendTransaction)   serves queries, sites, proofs
```

## Running one

```
btb-host --genesis genesis.json \
         --validator /ip4/10.0.0.1/udp/9000/quic-v1/p2p/<peer id> [--validator ...] \
         [--http 127.0.0.1:8788] [--default-contract <64 hex>] \
         [--listen /ip4/0.0.0.0/udp/9100/quic-v1] [--serve-followers] \
         [--relayer <64 hex> --relay-rpc HOST:PORT [--relay-rpc ...]]
```

- `--genesis` is the same file the validators use. It fixes the chain id and the epoch-0 validator set, the only thing the node has to be given to trust.
- `--validator` is any number of validators to sync from; the peer id in the address pins the key.
- A validator only serves it if its operator opted in: set `"serve_followers": true` (or `{"max_peers": 32, "rate_per_sec": 10, "rate_burst": 20}`) in the validator's `node.json` ([NODE.md](NODE.md#serving-followers)).
- The HTTP API is read-only and has no TLS or rate limiting of its own: put a reverse proxy in front of it. It binds to the address you give.
- `--serve-followers` makes the hosting node serve block sync to further hosting nodes (a relay tier), with the same limits.

In code: `Host::start(HostConfig::new(genesis, validator_addrs, http_addr))`.

## Following the chain without trust

A hosting node is a `btb-node` driver whose key is not in the validator set ("observing only"), plus a block hook. It uses exactly the block sync of [NODE.md](NODE.md#block-sync):

1. It asks a validator for committed blocks by height, round robin over the validators it knows.
2. `sync::verify` accepts a response only if it extends what the node already executed, every block links to the previous one, payloads match their hashes, and one quorum certificate (checked signature by signature) proves the 2-chain commit of the leading blocks. The validator set it checks against is **derived by the node itself** by executing the chain epoch by epoch, never taken from a peer.
3. The node **executes** every proven block itself and rebuilds the state, Merkle tree included. A peer can lie by omission (stall, answer late, send nothing) but cannot make the node accept a block that was not committed; a lying peer is skipped and counted in `sync.rejected`.

The test `follower_rejects_forged_sync_from_a_malicious_peer` plays a validator key that answers with a flipped certificate bit, a swapped payload, a certificate signed by outsiders, a wrong epoch and a dropped block: the node rejects each, executes nothing, and then accepts the genuine answer from the same peer.

The node trails the validators by a block or two: the newest committed blocks only become provable when the next ones exist ([NODE.md](NODE.md#block-sync)), and the node polls (`--poll-ms`, default 100 ms) instead of receiving consensus traffic. With the default `idle_block_ms` of 250 ms that is well under a second.

## HTTP API

The query and subscription routes have the shape of `btb-dev` ([DEV_SERVER.md](DEV_SERVER.md)), so `@btb/client` works unchanged. A hosting node serves many contracts, so the contract is a path prefix: point the client at `http://host/c/<contract hex>`.

| Route | Answer |
| --- | --- |
| `GET /c/<contract>/query/<fn>?args=<hex>&caller=<id>` | the result as hex (`text/plain`), header `X-BTB-Height` |
| `GET /c/<contract>/query/<fn>?...&proof=1` | JSON `{result, height, gas_used, incomplete, proof}` (see [Proofs](#proofs)) |
| `GET /c/<contract>/subscribe/<fn>?...` | Server-Sent Events: `data: <hex>` with the current result, then one event per change |
| `GET /query/<fn>`, `GET /subscribe/<fn>` | the same for the `--default-contract`, as `btb-dev` |
| `GET /site/<contract>/<path>` | a published static file (`index.html` for `/` and `dir/`) |
| `GET /api/site/<contract>` | `{files: [{path, sha256}]}` |
| `GET /proof/<contract>?key=<hex>[,<hex>...][&height=N]` | proof bundle for contract storage keys |
| `GET /proof/<contract>/site/<path>[?height=N]` | proof bundle for a site file |
| `GET /proof/raw?key=<hex full state key>[&height=N]` | proof bundle for any state key (balances, rows, ...) |
| `GET /status`, `GET /health` | `btb_status` of the node plus `host: {subscriptions, connections, executed_height}` |
| `GET /relay` | `{relayer, chain_id}`: the address to sign into a transaction (`404` without `--relayer`) |
| `POST /tx` | body: hex of a signed transaction (or `{"tx": hex}`); forwards it to a validator, answers `{id, relayer}` ([below](#relaying-transactions)) |
| `GET /tx/<id>` | the receipt once this node has executed the block, else `null` |
| `POST /c/<contract>/mutation/<fn>` | `501`: mutations are signed transactions: `POST /tx` or a validator's `btb_sendTransaction` |

Arguments, results and the caller identity are those of `btb-dev` (`args` is hex of `btb-codec` bytes; `caller` or `X-BTB-Caller` is 64 hex characters, or any text that is hashed). The caller of a query is **unauthenticated**: a contract must not hide data behind `caller()` in a query. Errors are `{"error": "..."}` with `404` for an unknown contract or function and `400` for an abort. Read routes answer cross-origin (`Access-Control-Allow-Origin: *`): everything served is public chain data.

```ts
import { BtbClient, t, verifyProof } from "@btb/client";
const c = new BtbClient({ url: `http://host:8788/c/${contract}` });
const n = await c.query("get", t.unit, [], t.u64);
c.subscribe("get", t.unit, [], t.u64, (n) => render(n));
const { result, proof } = await c.queryWithProof("get", t.unit, [], t.u64);
await verifyProof(proof, { quorum: { chainId, epochs: { 0: epoch0.validators } } }); // or { trustedRoot }
```

### Queries and subscriptions

Queries run `Executor::query_tracked` on the node's own state, under the same gas cap as validators (`query_gas`), read-only. A subscription runs the query once, records its **read set** (the keys and ranges it read) in the runtime's `SubscriptionManager` and sends the first result. After every executed block the node asks the manager which subscriptions read a key the block wrote, re-runs only those (as the same caller) and pushes the result when it differs from the last one. Registration and the first run happen on the driver thread in one step, so no block can fall between them. A client that stops reading (backlog of 64 events) or whose query starts failing is dropped; `EventSource` reconnects and gets a fresh first event. `X-BTB-Height` / `host.executed_height` tell which block the answer reflects.

Queries and subscription refreshes run on the thread that executes blocks, so a slow query delays sync by its own duration (bounded by gas). Moving reads to a snapshot is future work.

## Proofs

State is committed by a Jellyfish-style Merkle tree ([STATE.md](STATE.md#state-commitment)) keyed by `SHA-256(state key)`, one root per block, every old version still readable. The node returns **proof bundles**:

```json
{ "version": 1, "height": 41, "state_root": "…",
  "entries": [{"key": "…", "value": "…" , "proof": {"siblings": [["…" x15], …], "leaf": ["…", "…"]}}],
  "header": {"height": 42, "parent_hash": "…", "timestamp": 0, "state_root": "…", "tx_root": "…", "hash": "…"},
  "commit": {"epoch": 0, "sync": "<hex: consecutive consensus blocks of the epoch>"},
  "attest": {"epoch": 0, "block": {"round": 7, "height": 44, "parent": "…", "justify": {…}, "proposer": 2,
                                    "payload_hash": "…", "exec": "<claim: height 41, digest, receipts root>", "id": "…"},
             "qc": {"round": 7, "block_id": "…", "votes": [[0, "<sig>"], …]},
             "prev_digest": "…", "roots": ["<state root after 41>", …] } }
```

`entries` say: after block `height`, the state under `state_root` held `value` at `key` (`null`: proven absent). `header` is the header of block `height + 1`, whose `state_root` field **is** the root after `height` ([CHAIN.md](CHAIN.md#blocks)). `commit` is the consensus evidence that block `height + 1` is committed: consecutive consensus blocks from it, whose quorum certificates satisfy the 2-chain rule, i.e. the same evidence block sync checks.

`attest` is the **state root attestation**: a consensus block whose execution claim covers `height`, the quorum certificate for it, and the state roots from `height` up to the claimed height (usually one to three roots). Validators vote for a block only if its claim equals what they executed, so the certificate says that a quorum of the epoch's stake executed the chain to exactly these roots ([CONSENSUS.md](CONSENSUS.md#execution-attestation-validators-certify-state-roots)). The verifier folds the roots into the digest chain `d_h = SHA256("BTB/exec/v1" || d_{h-1} || h || root_h)`, compares `d_k` with the claim, recomputes the block id (so the claim is covered by the signatures), and checks the votes against the epoch's validator set. Heights near an epoch's end are attested by the next epoch's validators (`attest.epoch` says which).

Which version: `?height=N` picks one. Without it the node returns the newest version whose next block is provably committed, which is about three blocks behind the head (block `height + 1` needs two more blocks after it for its certificate). `?proof=1` on a query proves the keys the query read **at the head**; the head's root is named by no header yet, so that bundle has `header: null`. Ask `/proof/...` for the same keys a few blocks later to anchor it.

### What `verifyProof` / `verify_bundle` establish

| Check | Rust (`btb_host::proof::verify_bundle`) | TypeScript (`verifyProof`) |
| --- | --- | --- |
| each value is what the tree under `state_root` holds (or is absent) | yes | yes |
| `state_root` equals one you trust (`trustedRoot`) | caller compares | yes |
| header hash is right and its `state_root` is the proven root | yes | yes |
| header's transactions are the committed payload's | yes | no |
| a quorum of the epoch's validators committed block `height + 1` (its transactions) | yes, with a `Context` you build from a validator set you trust (`context_from_epoch_json`) | no (reports `hasConsensusEvidence`) |
| **a quorum of an epoch's validators attested `state_root`** (they executed the chain to it) | yes: `verify_bundle` (one trusted context) or `verify_bundle_with` (a map epoch to context); `Verified::root_attested` | yes: `verifyProof(bundle, { quorum: { chainId, epochs: { 0: validators } } })`, `rootAttested` |

**What is and is not provable:**

| Answer | Provable today |
| --- | --- |
| a storage key (`storage_get`), a table row by id, a balance, a site file, any other single state key | **yes**, including proof of absence |
| a whole query result | **no**, not as a signed fact: a result is computed by the VM. `?proof=1` gives proofs of every key the query read, so anyone holding the contract code can re-run the query on proven inputs and compare |
| a query that scanned a table or index range | the point reads are proven, ranges are not: the response says `"incomplete": true`. The tree is keyed by hash, so it cannot prove a range is complete; that needs an ordered commitment (future work) |

**Anchoring a root.** The Merkle proofs show that a value is what the tree under `state_root` holds; the attestation shows that `state_root` is the root a quorum of the validators executed. With fewer than a third of the stake faulty, a root that verifies with `root_attested` is the chain's root, and a lying host cannot produce one: it would need signatures from more than two thirds of the stake on a claim that the honest ones would not have signed. What you have to supply is the trust anchor, the validator set of the epoch (`btb_getEpoch`, genesis, or a checkpoint you trust, the usual weak-subjectivity assumption for proof of stake). `trustedRoot` still works and is independent (compare with a validator's `state_root_after` or a second host). In TypeScript, `quorum` rejects a bundle without a valid attestation unless `allowUnattested` is set.

Details that matter:

- A bundle carries an attestation only when the host holds the block that attests the height and the QC for it, which is about three blocks after the height in normal operation (slightly more after skipped rounds). `?proof=1` bundles at the head have none; ask `/proof/...` for the same keys a few blocks later, as before. Without `?height=` the host returns the newest height that has both the commit evidence and the attestation.
- The attestation is for the *state after* block `height`. It does not say the answer to a query was computed correctly from it (see the table above), and it covers receipts only at the claimed height.
- The validator set to check against is the one of `attest.epoch`, not of `height`: the last block of an epoch is attested by the next epoch's validators. `verify_bundle_with` takes the contexts of several epochs.
- A forged root, a forged or short certificate, a claim changed after the fact, or a different validator set fail (`ProofError::BadAttestation` / `ProofError` in TypeScript). A bundle whose epoch you did not supply is not an error by itself in Rust (`root_attested` stays false) and is one in TypeScript unless `allowUnattested`.

## Websites

A contract's deployer publishes static files with the `Site` transaction (`TxKind::Site(SiteOp)`, wire tag 9, `crates/btb-chain/src/site.rs`):

- `Publish { contract, path, content }` creates or replaces a file; `Remove { contract, path }` deletes one. **Only the contract's deployer** (the account that sent the `Deploy`) may do either; anyone else's transaction is included, charged its fee and refused (`Revert::Site(NotDeployer)`), and the files do not change.
- A path is 1 to 128 bytes of `[A-Za-z0-9._~-]` segments separated by single `/` (no leading or trailing slash, no `.` or `..`, no spaces or percent escapes). Malformed paths and files over the limit never enter a block (`Invalid::TooLarge` at admission).
- **Limits:** 96 KiB per file, 256 files per contract. Validators hold every byte forever, so this is for HTML, CSS, JS, small images and fonts. **Large files (video, big images, datasets) belong on storage nodes** ([SCALING.md](SCALING.md): erasure-coded pieces, only hashes on chain), which do not exist yet; the on-chain limit is deliberately small until they do.
- **Cost:** gas `20,000 + 100` per content byte, charged in full also on refusal, paid by the publisher. There is no separate storage deposit: the fee is the price of the space.
- **Content addressing:** a file is stored as `0x32 | contract | path -> SHA-256(content) || content`. The hash is returned as the transaction output, served as `ETag` and `X-BTB-Content-Hash`, listed by `/api/site/<contract>`, and covered by the file's Merkle proof (`/proof/<contract>/site/<path>`), so a page can be checked against a root like any other key.

Serving: `GET /site/<contract>/<path>` (`/site/<contract>/` and `dir/` serve `index.html`). Content types come from the extension (unknown ones are `application/octet-stream`); responses carry `ETag` (and answer `If-None-Match` with `304`), `X-Content-Type-Options: nosniff` and `Content-Security-Policy: sandbox allow-scripts allow-forms allow-popups`. The sandbox gives every site an opaque origin, so a contract's JavaScript cannot read another contract's site, cookies or local storage on the same host; its calls to the node's API are cross-origin requests the API allows. A site that needs same-origin features should be served from its own domain (a reverse proxy mapping a hostname to `/site/<contract>/`).

### Large files from storage nodes

Files above the on-chain limit live on storage nodes ([STORAGE.md](STORAGE.md)). A hosting node serves them by **storage root**, verified before a byte is sent:

- `GET /file/<root hex>`: the file with that root. The node asks the chain where the pieces are (`btb_storageFile`), downloads any `k` of the `n` pieces, checks each piece's Merkle path against the root, decodes, encodes again and compares the id (`btb_storage::client::fetch`). `ETag` is the root and the response is `immutable`; `X-BTB-Storage-Root` names it.
- `GET /site/<contract>/<path>` when `<path>` is not a site file but `<path>.root` is: that small on-chain file holds the 64-hex root, and the node serves the large file under `<path>` with the content type of `<path>`'s extension. The deployer publishes the pointer with a normal `Site` transaction after `btb store`.
- A request with `X-BTB-Storage: redirect` (an [edge node](EDGE.md), which fetches from storage nodes itself) gets `200` with `X-BTB-Storage-Root` and an empty body instead.

The whole file is held in memory while it is fetched and there is no range support here; put an edge node in front for popular or very large files.

## Relaying transactions

A hosting node can accept transactions from browsers and forward them to validators, and earn for it: 20% of the fee of every transaction that names it as `relayer` ([CHAIN.md](CHAIN.md#fees)).

1. The operator creates an account, registers it on chain with a `Host::Register { url }` transaction, and starts the node with `--relayer <that address> --relay-rpc <validator rpc>` (repeatable; first answer wins).
2. A client asks `GET /relay` for the address and **signs it into its transaction** (`@btb/client`: `new BtbClient({url}).submit({seed, chainId, nonce, kind})`; `btb call --relayer http://host:8788 ...`).
3. `POST /tx`. The node decodes the transaction, checks the signature, the chain id and that it names *this node's* relayer, then sends `btb_sendTransaction` to the validators. A transaction naming no relayer or another one is refused with `400`: it would earn this node nothing.

The node never signs and cannot change the relayer: it is part of the signed body, so any change breaks the signature. The node also cannot be sure the fee is paid to it: that depends on its account being registered when the block executes (otherwise the validators get the whole fee), and a client can always bypass it and send to a validator. The endpoint is unauthenticated (anyone may submit; validators charge the sender's fee as for any transaction), so put rate limits in front, as for the read routes.

## Tests

`crates/btb-host/tests/host.rs` runs a real 4-validator cluster (`crates/btb-node/tests/lite`, real QUIC and consensus) and a hosting node:

- the follower stays in sync, its answers equal every validator's, and the state roots it proves equal the validators' recorded roots;
- a proof verifies (with header, quorum evidence and the state root attestation, against a real 4-validator cluster), and a changed value, key, root, sibling, header, payload, certificate, claim, digest or validator set fails; a lie about every root consistently still fails because the signed digest does not fold from it; absence is provable; `?proof=1` proves the keys a query read; a real bundle is checked in as `sdk-ts/test/fixtures/proof.json` (`BTB_UPDATE_FIXTURES=1 cargo test -p btb-host --test host` regenerates it) and verified by Rust and by `sdk-ts/test/proof.test.ts`;
- a subscription pushes after a commit, stays silent for unrelated commits, and is cleaned up when the client leaves;
- a site is served with the right content types and hash after the deployer publishes it, a stranger's publish is refused on chain and the page is unchanged;
- a follower fed forged sync responses by a validator key rejects them all;
- a relay: the client signs the host's relayer address, `POST /tx` forwards the transaction to a validator, the host serves the receipt, and the host account receives exactly 20% of the fee; transactions with no relayer, another relayer, a changed relayer or garbage are refused with `400`, and a host without `--relayer` answers `404`/`501`;
- the unchanged TypeScript client (`sdk-ts/test/host.test.ts`, run by the Rust test when `npm ci` was run in `sdk-ts`) queries, gets a `501` for a mutation, receives subscription pushes and verifies a proof.

`crates/btb-node/tests/followers.rs` covers the validator side: no follower is served unless the operator opted in, a follower gets sync and nothing else (other frames are dropped and counted), the number of followers and their request rate are limited. `crates/btb-chain/tests/site.rs` covers the transaction.

## Limits

- **State is in memory.** A restarted hosting node replays the chain from genesis, which needs validators that still hold the history (RocksDB validators keep all of it, memory validators 4096 heights). Persisting the follower's state in RocksDB and state sync from snapshots are not done.
- **No fees yet.** Hosting nodes are meant to earn per-request fees through payment-channel vouchers ([TOKENOMICS.md](TOKENOMICS.md)); this crate serves requests for free (edge nodes already sell requests against vouchers, [EDGE.md](EDGE.md#payments)). The relay share of mutation fees is paid when clients relay through the node ([above](#relaying-transactions)).
- **Relay is opt-in and unmetered.** Without `--relayer` a hosting node accepts no transactions. With it, `POST /tx` is not rate limited and a host has no bond (see [CHAIN.md](CHAIN.md#fees)). (Validators accept sync requests from followers but not transactions or consensus messages.)
- **Reads share the block-execution thread** (see above), and the HTTP server is a simple thread-per-connection one with a connection cap (`max_connections`, 256).
- **Head proofs are unanchored** until a few blocks later (the attestation and the commit evidence need the blocks after the height); ranges cannot be proven (see [Proofs](#proofs)). State roots *are* validator-attested; a client that does not pass the validator set still gets only the Merkle check.
- **Epoch changes** work as for any observing node; a node absent for longer than validators keep epoch records for evidence would need state sync.
