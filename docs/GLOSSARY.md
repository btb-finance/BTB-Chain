# Glossary

Short definitions of terms used across the BTB Chain docs, in alphabetical order. Each entry points to the document that describes it in full.

**Action.** A function kind that calls the outside world (for example an HTTPS request) and then writes a result the validators have agreed on. See [Oracles](ORACLES.md#3-custom-web-data-https-requests).

**Batch auction.** The way the native DEX settles swaps: all swaps on a pool in one block get the same price, so transaction ordering gives no front-running advantage. See [Architecture](ARCHITECTURE.md#front-running-protection) and [Chain: Native DEX](CHAIN.md#native-dex).

**Blob storage.** The layer that holds large files. Files are erasure-coded into pieces on storage nodes, and only the hash and piece assignments are on-chain. See [Scaling](SCALING.md#2-storage-split-files-across-many-nodes).

**Chain signatures.** Signatures produced jointly by validators with threshold keys, which control ordinary addresses on other chains such as Ethereum, Bitcoin and Solana. They let BTB contracts act on those chains without deploying contracts there. See [Cross-chain](CROSS_CHAIN.md#writing-chain-signatures).

**Core validator.** A staked node that runs consensus, executes mutations and crons, keeps full state, and takes part in threshold signing and the randomness beacon. See [Architecture](ARCHITECTURE.md#core-validators).

**Cron.** A function kind that is a mutation run on a schedule, by block height or block timestamp and never a local clock. It is paid from the contract's prepaid balance. See [Languages](LANGUAGE.md#four-function-kinds).

**Determinism.** The rule that anything affecting consensus gives the same result on every node: no floats, system time, thread scheduling or unseeded randomness. See [Languages](LANGUAGE.md#design-rules-all-languages).

**Edge node.** A lightweight node, possibly a laptop or phone, that caches popular content near users, relays requests and holds small pieces, earning per request. See [Scaling](SCALING.md#five-ways-to-earn).

**Erasure coding.** Splitting a file into pieces with redundancy so that a subset can rebuild it; for example, any 34 of 100 pieces recover the file. See [Scaling](SCALING.md#2-storage-split-files-across-many-nodes).

**Fast path.** Confirmation for transactions that touch only one user's own data, needing validator signatures but no global ordering. See [Architecture](ARCHITECTURE.md#consensus-blocks-inside-requests-outside).

**Fixed supply.** BTB's total of 1,000,000,000 tokens, created at genesis and never minted afterward. See [Tokenomics](TOKENOMICS.md).

**Gas.** The metered cost of running code: every VM instruction and every host call is charged, so loops cannot stall the chain. See [Languages](LANGUAGE.md#design-rules-all-languages).

**Host call.** A function the VM exposes to contracts for things like database access or token transfers. Each host call has a defined gas cost. See [Languages](LANGUAGE.md#design-rules-all-languages).

**Hosting node.** A node anyone can run that serves queries, live subscriptions and websites, and relays mutations to validators, earning from per-request vouchers. See [Architecture](ARCHITECTURE.md#hosting-nodes).

**Index.** A field-based lookup on a table, maintained by the chain as consensus state, so questions like "all NFTs owned by X" are plain queries. See [Architecture](ARCHITECTURE.md#state-model).

**Mutation.** A function kind that reads and writes state in a transaction. It goes through consensus and pays gas. See [Languages](LANGUAGE.md#four-function-kinds).

**Payment channel.** BTB locked on-chain once, then drawn down by signed vouchers so per-request fees need no consensus. See [Tokenomics](TOKENOMICS.md#payment-channels-for-per-request-fees).

**Query.** A read-only function kind answered by hosting nodes without consensus. Queries cannot write state or use randomness, and frontends can subscribe to them. See [Languages](LANGUAGE.md#four-function-kinds).

**Relayer.** A party that delivers BTB's signed reports and transactions to other chains and is paid a relay fee by the requesting app. See [Scaling](SCALING.md#five-ways-to-earn).

**RISC-V (rv64im).** The 64-bit, integer-only instruction set that BTB contracts compile to. Rust is first-class, and any language that targets RISC-V works. See [Languages](LANGUAGE.md#why-risc-v).

**Storage challenge.** A random, time-limited request for a storage node to prove it still holds part of a piece, answered with the data and a Merkle proof. Failing costs the period's payment and part of the node's deposit. See [Scaling](SCALING.md#keeping-storage-honest-proofs).

**Storage deposit.** BTB locked per byte of state a contract stores, refunded when the data is deleted. It prevents state bloat. See [Tokenomics](TOKENOMICS.md#storage-deposits).

**Storage node.** A node with spare disk that holds erasure-coded file pieces, proves it still has them, and earns per period. See [Architecture](ARCHITECTURE.md#storage-nodes-and-edge-nodes).

**Subscription.** A live query: the node pushes a new result whenever a block changes the data the query read. See [Architecture](ARCHITECTURE.md#state-model).

**Table.** A contract-declared database table with a primary key and optional indexes, which is how BTB state is modeled. See [Architecture](ARCHITECTURE.md#state-model).

**Threshold beacon.** The per-block random value made by validators jointly producing a threshold BLS signature on `(chain, epoch, height)`; each releases its partial only after the block is committed. No single party can predict or choose it. It is available only to mutations and crons marked `randomness`. See [Oracles](ORACLES.md#1-verifiable-randomness).

**Threshold key.** A key split among validators so that no one holds it whole, but a two-thirds quorum can sign together. It is used for cross-chain signing and oracle reports. See [Cross-chain](CROSS_CHAIN.md#writing-chain-signatures).

**Transform function.** Code that strips run-to-run differences (timestamps, request IDs, headers) from an HTTPS response so validators can agree on the result. See [Oracles](ORACLES.md#3-custom-web-data-https-requests).

**Vault.** An address on another chain controlled by the validators' threshold key and holding real assets for deposits and withdrawals. Vault value is capped below the stake validators would lose by cheating. See [Cross-chain](CROSS_CHAIN.md#example-native-cross-chain-swap).

**Voucher.** A small signed message carried with each request that raises the running total a host is owed. The host collects the latest one periodically. See [Tokenomics](TOKENOMICS.md#payment-channels-for-per-request-fees).
