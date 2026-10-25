# Scaling and Earning: A Network Anyone Can Join

BTB Chain scales the way the internet does: **work is split into small pieces, each piece is done by whoever is best placed to do it, and every piece is paid for.** No single machine has to store everything or serve everyone. A data center validator, a home server and a phone can all take part and earn BTB, each doing the share of work it can handle.

## The core idea: pay for every unit of work

Every action on BTB costs a small amount of BTB, and that amount goes to the node that did the work.

- A trading bot reads the ETH/USD price inside a mutation → it pays a tiny fee → the validators that fetched and reported the price earn it.
- A user opens an NFT page → the app pays a tiny fee per request → the hosting node that served the page earns it.
- An artist stores a large NFT file → they prepay storage → the nodes holding pieces of that file earn it, month after month, as long as they prove they still have it.

Because fees are paid per use, **demand pays for capacity.** More users means more fees, which attracts more nodes, which adds capacity. Nobody has to pre-buy servers for traffic that may never come.

> Amounts in this document are illustrative. Real prices are set by gas metering and by nodes competing on price.

## Five ways to earn

| Role | What it does | Earns from | Hardware |
| --- | --- | --- | --- |
| **Validator** | Runs consensus, executes mutations and crons, produces randomness, reports oracle prices, signs cross-chain transactions | Gas, oracle fees, randomness fees, cross-chain signing fees | Data-center server, BTB stake |
| **Hosting node** | Answers queries, pushes live subscriptions, serves websites | Per-request vouchers from apps | Home or cloud server |
| **Storage node** | Holds pieces of large files (NFT media, app assets, datasets) and proves it regularly | Monthly storage payments + retrieval bandwidth | Any machine with spare disk |
| **Edge node** | Caches popular content close to users, relays requests, holds small pieces | Per-request vouchers, small storage payments | Laptop, home device, **phone** |
| **Relayer** | Delivers BTB's signed reports and transactions to other chains | Relay fees paid by the requesting app | Any machine |

Many small earnings add up: one person's phone might serve cached NFT images, hold a few small file pieces and relay a few reports, earning a little from each.

## How BTB scales, layer by layer

### 1. Reads: add hosting and edge nodes

Queries, subscriptions and websites don't need consensus. Any hosting or edge node with an up-to-date copy of the relevant data can answer, and attach a proof so the answer can be verified. **Doubling the number of hosting nodes roughly doubles read capacity**, without making validators do any extra work.

Popular data (a trending NFT collection, a big DEX's prices) gets cached on many edge nodes near users, like a CDN.

### 2. Storage: split files across many nodes

Large files never go on every validator. They go into the **blob storage layer**.

**Example: a 100 GB NFT file**

1. The file is split and erasure-coded into **100 pieces of about 3 GB each**. Erasure coding adds redundancy so that **any 34 of the 100 pieces are enough to rebuild the whole file**.
2. The 100 pieces are assigned to 100 different storage nodes, chosen pseudo-randomly using the chain's [randomness beacon](ORACLES.md#1-verifiable-randomness), so nobody can pick which nodes hold a file.
3. Only a small record goes on-chain: the file's hash, its size, the encoding, which nodes hold which piece, and how long storage is paid for.
4. Each node holding a piece earns from the artist's storage payment.

Even if 66 of the 100 nodes go offline at the same time, the file can still be recovered. When nodes do drop out, the network **repairs** the file by rebuilding the missing pieces from the remaining ones and assigning them to new nodes.

| | Every validator stores it | BTB blob storage |
| --- | --- | --- |
| Copies of a 100 GB file | 100 GB × 100 validators = 10,000 GB | ~300 GB total (3× overhead) |
| Space per node | 100 GB | ~3 GB |
| Survives losing | All but one validator | Up to 66 of 100 nodes |
| Who can store it | Only validators | Anyone with spare disk |

Small data, like NFT metadata JSON files, is cheap enough to keep on hosting nodes directly and is also backed up in blob storage.

### 3. Execution: use every core, then split state

- **Parallel execution.** Transactions touching different data run at the same time on all of a validator's CPU cores (Block-STM style).
- **Fast path.** Transactions touching only one owner's data skip global ordering entirely.
- **State sharding (later).** When one validator set is no longer enough, state can be split across groups of validators that each handle part of the apps, as NEAR does. This is a later step; reads and storage, which make up most traffic, already scale without it.

### 4. Consensus: keep it small and fast

The validator set stays relatively small (about 50 to 150) and fast. It only orders mutations, crons and oracle reports, while everything else happens on the other layers. Streaming (DAG) consensus pipelines batches continuously.

### 5. Micro-payments: batch them

Millions of tiny payments would overload any chain if each were a transaction. BTB uses [payment channels](TOKENOMICS.md#payment-channels-for-per-request-fees): the payer signs small vouchers off-chain, and each node cashes in the latest voucher in one transaction, for example once a day.

## Keeping storage honest: proofs

> **Built:** the flow below is implemented in `btb-chain` (`storage.rs`) and `btb-storage`; see [STORAGE.md](STORAGE.md) for the exact rules. Until the randomness beacon exists, assignment and challenge chunks are derived from block hashes (a proposer can grind them, but cannot forge proofs); each piece is challenged once per period at a time spread by its hash, and a miss slashes part of the piece's collateral into the file's escrow, reassigns the piece and has the new node rebuild it from any `k` others.

Paying people to store data only works if they can't pretend. BTB uses **storage challenges**, similar to Filecoin, Arweave, Storj and Walrus:

1. At random times (chosen by the randomness beacon), the chain challenges a storage node: "prove you have bytes X to Y of piece 37."
2. The node answers with the data and a Merkle proof against the file's hash, within a short time limit.
3. Nodes that answer correctly earn their storage payment for that period. Nodes that fail lose that period's payment and part of their storage deposit, and their piece is reassigned.

Storage nodes lock a **small deposit proportional to the data they hold**, so losing data costs them money. This stake is far smaller than a validator's, which keeps the role open to anyone.

## Can a phone really earn?

Yes, for the right jobs. Phones are not good at holding 100 GB or staying online 24/7, but they are good at small, bursty work close to users.

| Good jobs for a phone | Not suitable for a phone |
| --- | --- |
| Caching popular images and pages for nearby users | Running a validator |
| Holding small file pieces (tens to hundreds of MB) | Storing huge pieces |
| Relaying requests and signed reports | Being the only copy of anything |
| Verifying data as a light client | Guaranteed 24/7 uptime |

The network handles phones' limits by design:

- **Redundancy covers dropouts.** Erasure coding means any single phone going offline never loses data.
- **Pay for what's proven.** Earnings follow passed challenges and served requests, so a phone that's online 30% of the time earns roughly for that 30%.
- **Settings protect the owner.** The node app works only on Wi-Fi, while charging, and within limits the owner sets for disk and data.
- **Reputation.** Nodes with a track record of uptime get assigned more pieces and more requests.

## Example: one day of a popular NFT collection

| Activity | Who pays | Who earns |
| --- | --- | --- |
| 10,000 collectors browse the gallery | App's hosting balance, per request | Hosting and edge nodes that served pages and images |
| Each 100 GB high-resolution artwork stays stored | Artist's prepaid storage | ~100 storage nodes per artwork, including phones holding small pieces |
| A floor-price bot reads the ETH/USD feed | The bot, a small fee per read | Validators reporting the price |
| A random mint picks rare traits | The minter, through gas | Validators producing randomness |
| Royalties are paid out on Ethereum | The app, a cross-chain signing fee | Validators signing; a relayer delivering the transaction |
| Nightly cron updates rarity rankings | Contract's prepaid balance | The block executor |

Every one of these is a separate way to earn, and none needs the same machine to do all of them.

## Risks and limits

| Risk | Mitigation |
| --- | --- |
| Nodes claim to store data they don't have | Random storage challenges with Merkle proofs; deposits lost on failure |
| Many nodes run by one operator (fake decentralization) | Random piece assignment, deposits per node, diversity by network and region |
| Phones drop offline constantly | Erasure coding, automatic repair, pay only for proven work |
| Tiny fees cost more to process than they're worth | Payment channels batch them; one settlement per period |
| Popular content overloads a few nodes | Edge caching spreads it; more demand means more paying requests and more nodes |
| Illegal content in storage | Nodes hold encrypted fragments they can't read; operators can choose content filters for the data they serve publicly; legal policy to be defined |

## Rollout

- **Phase 2–3:** blob storage on a single node; storage fees in the local devnet.
- **Phase 5:** storage nodes with challenges and repair (built, see [STORAGE.md](STORAGE.md)), hosting nodes with vouchers, edge caching.
- **After mainnet:** a lightweight node app for phones and home devices, and state sharding when demand requires it.

See [Architecture](ARCHITECTURE.md) and [Tokenomics](TOKENOMICS.md) for how these pieces fit the rest of BTB.
