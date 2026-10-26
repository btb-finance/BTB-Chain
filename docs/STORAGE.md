# Storage nodes

Large files never go on every validator. They are erasure-coded into pieces that live on **storage nodes**; the chain keeps only hashes, assignments, deposits and payments. This page is the design and the reference for what is built: `crates/btb-chain/src/storage.rs` (on-chain rules), `crates/btb-storage` (erasure coding, proofs, piece store and HTTP API, daemon, client), `btb store`, `btb fetch` and `btb storage-node run` in `btb-cli`, and the `btb_storage*` RPC views in `btb-node`. The economics are in [TOKENOMICS.md](TOKENOMICS.md#large-file-storage), the scaling argument in [SCALING.md](SCALING.md#2-storage-split-files-across-many-nodes).

## The flow

```
client                         chain                              storage nodes
  | encode file -> k+m pieces    |                                       |
  | Store{root,size,k,m,periods} |                                       |
  |----------------------------->| escrow n*periods*pay                  |
  |                              | assign piece i -> node (deterministic)|
  |<-- receipt (assignments) ----|                                       |
  | PUT piece i + Merkle path ------------------------------------------>|
  |                              |-- every epoch_blocks: challenge ----->|  (chunk c of piece i)
  |                              |<---- Prove{chunk, paths} (tx) --------|
  |                              | verify against root, pay node         |
  |                              |-- deadline passed: slash, reassign -->|  new node
  |                              |                                       |  rebuilds from k others,
  |                              |<---- Prove (repair) ------------------|  proves
```

## Erasure coding and the file id

`btb_storage::erasure::encode(data, k, m)` cuts the file into `k` equal parts (zero padded) and adds `m` parity pieces with **Reed-Solomon over GF(2^8)** (`reed-solomon-erasure`, MIT). Each piece is `piece_len = ceil(size / k)` rounded up to a multiple of 1 KiB, at least one chunk. **Any `k` of the `k + m` pieces rebuild the file**; up to `m` can be lost. `k, m >= 1` and `k + m <= 128` (the chain limit, `MAX_PIECES`).

The commitment is a two-level SHA-256 Merkle tree (`btb_chain::storage::proof`, used identically by the chain and the storage code):

- chunk leaf `SHA256(0x00 || chunk)`, inner node `SHA256(0x01 || left || right)`, a lone node on an odd level is carried up unchanged; the root of the chunk leaves is the **piece root**;
- file-tree leaf `SHA256(0x02 || piece_root)`; the root over the `k + m` leaves is the **tree root**;
- **file id** (the "root" of `btb store`) = `SHA256("btb.storage.file.v1" || tree_root || size:u64le || k || m)`.

The id binds the content, the size and the shape, so a fetched file is verified by encoding it again and comparing ids. A **piece proof** is the piece's Merkle path to the tree root (`erasure::verify_piece`); a **chunk proof** (`ChunkProof`) is a 1 KiB chunk, its path to the piece root and the piece path, about 1.4 KiB for a 256 KiB piece.

## On-chain rules (`TxKind::Storage`, wire tag 12)

Sub-tags: `0 Register { endpoint, deposit }`, `1 AddDeposit { amount }`, `2 Withdraw { amount }`, `3 Store { root, size, k, m, periods }`, `4 Prove { file, index, proof }`. A refused operation is a revert (nonce used, gas charged, nothing changed). Gas: Register/AddDeposit/Withdraw 20,000, Store 30,000, Prove 40,000.

**Parameters** (`ChainConfig::storage`, in the genesis; defaults for a public network, tests use small ones):

| Parameter | Default | Meaning |
| --- | --- | --- |
| `epoch_blocks` | 600 | Blocks between two challenges of one piece: a payment period |
| `challenge_blocks` | 60 | Blocks a holder has to answer (less than `epoch_blocks`) |
| `repair_blocks` | 300 | Blocks a replacement node has to rebuild and prove a piece |
| `price_per_kib_period` | 100,000 | Payment per KiB of piece per period, base units |
| `deposit_per_kib` | 1,000,000 | Deposit a node must hold per KiB it stores |
| `min_deposit` | 10 BTB | Deposit needed to register |
| `slash_bps` | 5,000 | Share of a piece's collateral lost per missed challenge |

**Nodes.** `Register` locks `deposit` from the sender's balance and publishes an HTTP `endpoint` (up to 256 bytes); registering again updates the endpoint and adds deposit. A node's deposit must always cover `min_deposit + deposit_per_kib * KiB held`; assignment skips nodes without room, and `Withdraw` may only take what is above that. Taking everything while holding no piece removes the node.

**Store.** `Store` pays the whole **escrow** up front: `(k + m) * periods * pay`, with `pay = price_per_kib_period * piece_len / 1024`. The chain assigns the `k + m` pieces and returns the nodes (32 bytes each, in piece order) as the receipt output. Only `root, size, k, m` and the assignments are stored: never file bytes. A root can be stored once while it is live.

**Assignment** is deterministic given a seed. For each piece the chain scans the eligible nodes and takes the lowest `(cooling, pieces of this file already on the node, SHA256("btb.storage.assign.v1" || seed || file || index || node))`. So **no node gets two pieces of the same file unless there are fewer eligible nodes than pieces**, load spreads by hash, and nobody can choose which nodes hold their file. A node that missed a challenge within the last four periods is `cooling` and ranks after every other eligible node, so dead nodes are not handed each other's pieces over and over.

**Challenges.** Each piece has its own schedule (the first round is `epoch_blocks/2 + hash % (epoch_blocks/2)` blocks after the store, then every `epoch_blocks`, so the load is spread over blocks). A round for a held piece opens a challenge: one chunk index, picked from the seed, with a deadline of `challenge_blocks`. The holder sends `Prove { file, index, proof }`; the chain checks that the sender is the holder, that the deadline has not passed, and that the chunk at the challenged index belongs to the file id. Success pays `pay` from the escrow to the node (the payout per proven period) and closes the challenge. Each piece runs `periods` rounds, then is retired: the node's capacity is freed, and when the last piece of a file retires the **remaining escrow goes back to the owner** and the file record is deleted.

**Miss, slash, repair.** If the deadline passes: the holder loses `slash_bps` of the piece's collateral (`deposit_per_kib * KiB of the piece`) from its deposit, **moved into the file's escrow** (it pays the replacement and, at the end, is refunded to the owner if unspent); that period's payment stays in the escrow; the node's capacity is released and it is put on cooldown. The piece is reassigned at once (same rule, excluding the failed node) in state `Repairing` with a repair challenge due in `repair_blocks`. The new node learns of it from the chain, fetches any `k` other pieces from their holders, rebuilds the piece off-chain, checks that the rebuilt pieces reproduce the file id, stores its piece and proves a challenged chunk; that earns the withheld payment. If it fails too, it is slashed and the piece moves again. If no node is eligible the piece is `Vacant` and retried every `repair_blocks`. A piece being repaired skips the payment of the rounds it was away.

**Supply.** Node deposits (`0x41`) and file escrows (`0x43`) are counted in the supply invariant like balances and stake: every block ends with balances + storage deposits + stake + channel funds + node deposits + escrows equal to 1,000,000,000 BTB. Slashing moves deposit to escrow, payment moves escrow to a node, refund moves escrow to the owner; nothing is minted or burned.

State layout (tags `0x40` to `0x47`): see the table in `storage.rs`.

### Randomness

**The seed is the threshold beacon** ([ORACLES.md](ORACLES.md#1-verifiable-randomness)) whenever the block being executed has one: `seed = SHA256("btb.storage.seed.v1" || beacon seed)`, read from the block's seed record (state key `50 02`, which holds `height || seed` and is trusted only when its height is the current block's). It is used for the piece assignment of a `Store` (in the block that carries the transaction) and for the challenge chunks, deadlines and reassignments decided at the end of a block. The validators release the partial signatures of height `h` only after block `h` is committed, so the transactions of block `h` and the block that closes a challenge round are fixed before anyone can know the seed: **a proposer can no longer grind placement or challenged chunks**, and neither can a node that wants a particular file or chunk (a `Store` or a missed challenge cannot be steered by choosing transactions or timestamps).

**Fallback (documented, not silent).** Two cases have no beacon, and only there the old seed is used: a chain without an oracle config, and the first `dkg_window` blocks of a chain whose beacon key is generated on chain. The fallback seed is the **parent block hash** for `Store` (the head while a block runs) and the **hash of the block that closes the round** for the scheduler. It is grindable by a proposer (try other transactions or timestamps), which can only shift placement and challenges, never forge a proof or take money. Every block after the first key exists uses the beacon; nothing else in the protocol differs between the two. Tested: `storage_placement_follows_the_beacon_seed` (`crates/btb-chain/tests/storage_beacon.rs`, which also checks that the challenged chunk is the one derived from the beacon of the block that opened the round).

## The storage node (`btb storage-node run`)

```
btb storage-node run --rpc http://127.0.0.1:8545 --key node.key --dir ./pieces \
    [--listen 127.0.0.1:0] [--endpoint http://host:port] [--deposit UNITS]
```

It opens the piece store, starts the HTTP API, registers on chain if `--deposit` is given and the account is not a node yet (the endpoint defaults to the address it bound), prints `storage node: <endpoint> account <hex>`, and then loops: ask the chain (`btb_storageDuties`) for its pieces, answer open challenges with a chunk proof, rebuild pieces it was assigned in a repair, and delete pieces that are no longer assigned to it. The key signs `Prove` and `Register`; fund the account with the deposit plus gas.

**Piece API** (plain HTTP/1.1, no TLS: put a proxy in front to expose it):

| Request | Meaning |
| --- | --- |
| `PUT /piece/<file>/<index>` | Store a piece; header `X-Btb-Path` is the hex piece path. Accepted only if the chain assigned this piece to this node and the piece belongs to the file id |
| `GET /piece/<file>/<index>` | The piece; `X-Btb-Path` carries its path. The reader verifies it against the file id, so nodes need not be trusted |
| `GET /proof/<file>/<index>/<chunk>` | An encoded `ChunkProof` |
| `GET /health` | `ok` |

Pieces are files `<dir>/<file hex>/<index>.piece`, written atomically.

## The client

`btb store <file> --rpc URL --key FILE [--k 4] [--m 2] [--periods 12]` encodes the file, sends `Store` (paying the escrow), reads the assignment and uploads each piece in parallel to its node, then prints `root: <hex>`. It fails if a piece could not be uploaded (the file is on chain anyway: the missed first challenge makes the chain reassign and a node rebuild it).

`btb fetch <root> --rpc URL [--out FILE]` asks the chain for the holders, downloads pieces until it has `k` that verify against the root (dead or lying nodes are skipped), decodes, encodes the result again and checks that it reproduces the root. It needs no key.

## Files from contracts

A contract can reference files and pay for their storage without touching bytes (the Convex file-storage model). Host calls `file_info` (70) and `file_pin` (71), the Rust wrappers `btb_sdk::files`, the gas and the byte layouts are in [LANGUAGE.md](LANGUAGE.md#files); the example is `examples/files`.

```
client                  hosting node / btb dev        chain                       storage nodes
  | POST /upload (bytes) ->| encode, keep bytes        |                              |
  |<- {root,size,k,m} -----|                           |                              |
  | mutation(root,size,k,m) ---------------------------> file_pin: escrow from the     |
  |                        |                           | contract's balance, assign   |
  | POST /upload/<root>/push ->| pieces ------------------------------------------->  |
  | GET /file/<root> ----->| any k pieces, verified    |                              |
```

- **`file_info(root) -> (size, status, expiry)`** reads the file record (tag `0x42`) and, for a file whose last piece retired, a small **ended-file record** (tag `0x48`: size, created, periods, deleted when the root is stored again), so `expired` is told apart from `unknown`. `expiry = created + periods * epoch_blocks`, the height by which every piece has run its last round. It is a state read (400 gas), tracked in the read set, so a subscription that uses it updates when the file is stored.
- **Who pays.** Either the client sends `StoreFile` (`btb store`, which pays the escrow from its account) or the contract calls **`file_pin`**, which runs exactly the `Store` rules (`apply` with the contract as owner): the escrow `(k + m) * periods * pay` is taken from the contract's balance, the pieces are assigned, and when the file ends the unspent escrow goes back **to the contract**. A pin that fails answers a status and changes nothing (balance too low, not enough nodes, bad shape, already stored); the supply invariant holds as for any `Store`. `file_pin` needs `size`, `k` and `m` because the root cannot be inverted. Re-storing an expired root is allowed.
- **Uploads are off chain.** `btb-host --upload-dir DIR` turns on the upload gateway: `POST /upload[?k=&m=]` encodes the body (up to 64 MiB), keeps it and answers `{root, size, k, m, stored, url}`; `POST /upload/<root>/push` sends the pieces to the assigned nodes once the chain knows the file (409 before), deleting its copy when all arrive. The gateway never spends: someone else pays (client `Store` or contract pin). `GET /file/<root>` serves the verified file. The `@btb/client` helpers are `uploadFile`, `finishUpload` and `fileUrl` ([TS_SDK.md](TS_SDK.md#files)).
- **`btb dev`** emulates all of it with files on disk and the same API ([DEV_SERVER.md](DEV_SERVER.md#file-storage)), with the same roots.

Tests: `crates/btb-chain/tests/files.rs` (unknown root, client-paid store visible to the contract, contract-funded pin and escrow accounting under the supply invariant, failures as statuses, no nodes, expiry and re-store), `crates/btb-tests/tests/dev_multi.rs` and `sdk-ts/test/files.test.ts` (upload, attach, `file_info`, fetch by URL on `btb dev`), `crates/btb-cli/tests/files_e2e.rs` (4 validators, 6 storage-node daemons, a hosting node with the gateway: upload, contract pin, push, serve verified).

## RPC views

`btb_storageConfig`, `btb_storageNodes`, `btb_storageFile [root]` (file, escrow, pieces with holder, endpoint, state, open challenge, proven and missed counts), `btb_storageFiles`, `btb_storageDuties [address]`. Amounts are decimal strings.

## Tests

- `crates/btb-storage`: erasure round trips with every subset of up to `m` missing pieces (including 1 MiB), reconstruction restores the exact pieces and the id, id commits to content, size and shape, piece verification and tampering, store.
- `crates/btb-chain/tests/storage.rs`: proof verification and tamper cases, wire round trip, registration and withdrawal rules, assignment determinism and spread (distinct nodes when enough exist, fair sharing when not), escrow accounting, a proof pays from the escrow, bad proofs and wrong senders are refused and pay nothing, a missed challenge slashes into the escrow and reassigns to a node without another piece of the file, the replacement proves and is paid, a repair that never comes is slashed again, vacant pieces are picked up when a node appears, expiry refunds and frees capacity, escrow is refunded when nodes fail all the way. The executor checks the supply invariant after every block of every test.
- `crates/btb-cli/tests/storage.rs`: a 4-validator cluster, 6 `btb storage-node` processes, `btb store` of a 1 MiB file, two storage nodes killed, `btb fetch` still verifies the file, the chain slashes the dead nodes and live nodes rebuild the two pieces, which then verify against the file id and equal the originals.

## Not done

- Hosting nodes serving large site files by root from storage nodes ([HOSTING.md](HOSTING.md)): the pieces and the client exist, the `btb-host` route does not.
- Retrieval payments (vouchers per byte served by storage and edge nodes), extending the paid duration of a stored file, deleting a file early, a protocol fee, and encryption of fragments before upload (the client can encrypt before `store`; nodes only ever see opaque pieces).
- Sampling several chunks per challenge for large pieces.
- The assignment scans every registered node per piece; an indexed or sortition-tree structure is needed with thousands of nodes. The scheduler caps its work at 256 events per block (later ones wait).
- A node that is dead but registered is only discovered by a missed challenge; there is no faster liveness signal.
