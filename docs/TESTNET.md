# Testnet: faucet, explorer and how to run a public one

> **Status: built, not deployed.** The software for a public testnet exists (validators, faucet, explorer, a local launcher), but **no public testnet is running** and nothing here has been put on a real network or host. The Phase 4 box "Public testnet with faucet and explorer" in the [roadmap](../ROADMAP.md) stays open until one is. This page is the plan and the checklist for doing that; the commands for a local run work today.

## What exists

| Piece | Where | What it does |
| --- | --- | --- |
| Validator node | `crates/btb-node`, [NODE.md](NODE.md) | Consensus, execution, block history, JSON-RPC on loopback |
| Explorer | `GET /explorer` on every node's RPC port | One embedded page (no CDN): latest blocks (polls once a second), block, transaction (receipt, gas, fee), account (balance, stake, recent transactions), validator set per epoch with stake and jailed flag, search |
| DEX page | `GET /dex` on every node's RPC port (`?seed=<64 hex>` picks the browser account) | Order entry with signed transactions (WebCrypto Ed25519), live order book, last clearing prices and your orders; see [examples/native-dex](../examples/native-dex/README.md) |
| Explorer API | `btb_getBlocks`, `btb_getTransaction`, `btb_getAccount`, `btb_getAccountTxs`, `btb_getEpoch` | Read endpoints behind the page; account history is indexed in the node's block store |
| Faucet | `crates/btb-faucet` | HTTP service: `POST /drip` sends a `Transfer` from a funded key through node RPCs |
| Local launcher | `scripts/testnet-local.sh` | 4 validators plus the faucet on `127.0.0.1` |
| Browser smoke | `crates/btb-node/web/smoke.js` | Playwright run of the faucet page and explorer against the local launcher |

## Run it locally

```
scripts/testnet-local.sh                       # builds, starts 4 validators and the faucet
# explorer  http://127.0.0.1:8545/explorer      faucet  http://127.0.0.1:8080/
node crates/btb-node/web/smoke.js              # optional browser check (needs playwright + Chromium)
```

Options: `--dir`, `--validators`, `--base-port` (validator `i` listens on UDP `base-port + i`), `--rpc-base-port` (node `i` serves RPC and the explorer on `rpc-base-port + i`), `--faucet-port`, `--epoch-len`, `--pow-bits`, `--release`, `--no-build`. State is in memory (build `btb-node` with `--features rocksdb` and the generated config gets a `data_dir`), so a stopped local chain is gone.

## The faucet

```
btb-faucet --key faucet.key --rpc 127.0.0.1:8545 --rpc 127.0.0.1:8546 --bind 127.0.0.1:8080 \
           --amount 100 --max-amount 1000 --address-cooldown 86400 --ip-max 10 --ip-window 86400 --pow-bits 18
```

| Endpoint | |
| --- | --- |
| `GET /` | the page (address box, optional in-browser proof of work, confirmation) |
| `GET /info` | faucet account, balance, limits, difficulty |
| `GET /challenge` | a single-use proof-of-work challenge `{challenge, bits}` |
| `POST /drip` | `{"address": "<64 hex>", "amount": <whole BTB, optional>, "pow": {"challenge", "nonce"}}` gives `{tx_id, nonce, amount}` |
| `GET /tx/<id>` | the receipt, or `{"status":"pending"}` |

- **Limits (in memory, lost on restart):** one drip per address per cooldown; at most `ip-max` per IP per window (IPv6 per /64); `max-amount` caps one request. A request that fails before anything is sent does not use up the limits. Responses are `429` with `Retry-After`.
- **Proof of work (off by default):** with `--pow-bits N` a drip must carry `nonce` such that `SHA256(challenge || address || nonce as u64 LE)` starts with `N` zero bits; challenges are single use, bound to the address and expire after 5 minutes. About `2^N` hashes: 18 bits is a moment in a browser, 28 bits is minutes. It slows scripts, it is not a captcha; the per-IP and per-address limits do the real work.
- **Nonces:** one lock covers picking a nonce, signing and submitting. The faucet follows the chain's nonce, goes on past a nonce the node already holds (another user of the key), follows `NonceTooLow`, and starts over from the chain nonce when its pending transfers have not landed for 60 s. Several node endpoints can be given; it fails over.
- **Client IP:** the TCP peer. Behind a reverse proxy pass `--trust-proxy`: the **last** `X-Forwarded-For` entry (the one the proxy appended) is used. Without that flag the header is ignored, so it cannot be spoofed.
- The faucet binds loopback by default and warns on anything else. It speaks plain HTTP.

## Running a public testnet (plan, not done)

**What to host**

| Role | Count | Notes |
| --- | --- | --- |
| Validators | 4 to start, each on its own host and operator if possible | `btb-node run`, built with `--features rocksdb` and a `data_dir`: without it a restarted validator can double sign (see [NODE.md](NODE.md#crash-safety)) |
| Reverse proxy | 1 or more | TLS, rate limits and CORS in front of RPC, explorer and faucet |
| Faucet | 1 | `btb-faucet` pointed at two or more validators' RPC |
| Explorer | none extra | served by any node at `/explorer` (point the proxy at a non-validator-critical node if you can) |

**Ports**

| Port | Protocol | Exposure |
| --- | --- | --- |
| validator p2p, default UDP `9000 + i` | QUIC | open between validators only (firewall to the validators' addresses); unknown peers are dropped anyway |
| node RPC and explorer, default TCP `8545 + i` | HTTP | **loopback only** (the node refuses to bind elsewhere); expose through the proxy |
| faucet, default TCP `8080` | HTTP | loopback; expose through the proxy |

**Keys**

- Each validator key (`validator.key`) lives on its validator host only, mode `0600`, never shared. The same key is the libp2p identity.
- `btb-node init-testnet` writes every key to one machine, which is right for a local run and wrong for a public one. For a public net each operator generates their own key and sends only **public** information to a coordinator, who assembles the genesis. Three commands do this (`crates/btb-node/src/operators.rs`); no secret leaves the machine it was made on:

  ```
  # 1. each operator, on their validator host
  btb-node operator-key --dir ~/btb --name alice --ip <public ip> [--port 9000]
  #    -> ~/btb/validator.key (secret, 0600, never overwritten) and ~/btb/operator.json (public: send it)

  # 2. the coordinator, with every operator.json
  btb-node assemble-genesis --operators alice.json,bob.json,carol.json,dave.json \
           --treasury <account address> --out published [--chain-id 77] [--name btb-testnet] \
           [--stake 100000] [--validator-balance 1000000] [--epoch-len 600]
  #    -> published/genesis.json, published/peers.json and the genesis hash to publish

  # 3. each operator, with the published files and hash
  btb-node node-config --dir ~/btb --genesis genesis.json --peers peers.json --genesis-hash <hash> \
           [--port 9000] [--rpc-port 8545] [--data-dir true]
  btb-node run --config ~/btb/node.json
  ```

  `operator.json` holds the name, public key, account and peer address; its peer address must name the key's own peer id, so a key cannot be paired with someone else's address. `assemble-genesis` refuses duplicate keys or names, a treasury that is a validator account, and balances above the supply; each validator account gets `--validator-balance` BTB with `--stake` bonded, the **treasury** account (a faucet or a multisig; make its key with `btb keygen`) gets the rest of the 1,000,000,000 BTB, and `validate` must pass. `node-config` refuses a genesis whose SHA-256 differs from the published hash and a key that is not a validator in it, listens on all interfaces at `--port`, keeps RPC on loopback and lists every other operator as a peer. **Every validator must start from the byte-identical file**: the genesis covers the validator set, balances and parameters. Oracles are off in an assembled genesis (enable them by hand before publishing if needed).
- Each operator needs the other validators' `peers` entries: `/ip4/<public ip>/udp/<port>/quic-v1/p2p/<peer id>`, where `btb-node peer-id --key validator.key` prints the peer id.

**Genesis distribution**

1. The coordinator publishes `genesis.json` and its hash (the chain id and name too) at a stable URL and in the repository, plus the list of peer addresses.
2. Operators download it, compare the published hash, and start with `chain_id`, `genesis` and `peers` set. A node with a different genesis cannot join: its consensus signature domain differs.
3. Testnet coins: allocate most of the supply to the faucet account, validators' accounts what they need to bond, and nothing else. The faucet key is the only large balance; publish the faucet address.
4. Epochs are 600 blocks by default (about 2.5 minutes at idle); set `epoch_len` in the genesis before the first start if you want another rhythm.
5. A public chain should be restartable: build with `--features rocksdb` and keep the data directories.

**Security notes**

- **RPC has no authentication and no TLS.** It is a dev/testnet API. Never bind it to a public address (the node refuses). Put a reverse proxy in front that: terminates TLS, allows only the methods you need (`btb_getBlock`, `btb_getBlocks`, `btb_getTransaction`, `btb_getAccount`, `btb_getAccountTxs`, `btb_getBalance`, `btb_getReceipt`, `btb_getEpoch`, `btb_status`, `btb_sendTransaction`, `btb_query`), caps request size and rate per IP, and forwards `GET /explorer`. `btb_query` runs contract code on the driver thread between consensus events, so rate limit it hard.
- **The faucet key is a hot wallet.** Anyone who reads `faucet.key` (or the process memory) can move the faucet's whole balance. Keep it on the faucet host only, mode `0600`, run the faucet as an unprivileged user, and **do not reuse it** for anything. Prefer a faucet account funded with a limited amount (move a few million test BTB into a separate account and put only that key on the faucet host) over the genesis account that holds nearly the whole supply. If it leaks, the loss is testnet coins, but a drained faucet ends the testnet's usability; have a rotation procedure (new key, new transfer, restart).
- The faucet rate limits are in memory and per process: restarting resets them, and several faucet instances do not share them. Run one instance, with the proxy adding its own rate limit and `--trust-proxy` set only when a proxy really sits in front.
- Do not expose validators' RPC on the same host name as the faucet page if you can avoid it; a page served from the node (the explorer) and the faucet page both set a strict Content-Security-Policy and use no external scripts, keep it that way.
- Validators need monitoring (height, peers, `last_commit_age_ms` in `btb_status`) and a way to reach each operator. The gate for Phase 4 is that the testnet keeps finalizing when validators fail or misbehave; test that on the public set before inviting anyone.
- Nothing about this is audited. Testnet coins have no value, and the network may be reset at any time.

## Not done

- No public deployment, no domain, no TLS, no hosted genesis. Operator onboarding is the three commands above; peer addresses are still exchanged by hand.
- The explorer polls (once a second) instead of streaming, shows what the node it is served from knows (an in-memory node keeps the newest 4096 heights), and has no contract table browser yet. Blocks cannot be looked up by hash.
- The faucet limits are in memory and per instance; there is no captcha and no account-based quota.
- Joining a running network as a non-validator follower and state sync are not built ([NODE.md](NODE.md#limits)), so a public testnet today means a fixed set of validators from genesis.
