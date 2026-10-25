# Prior Art: What BTB Chain Learns From

BTB Chain is not built from scratch in a vacuum. Each idea in the design has been tried somewhere before, and this document explains, project by project, **what it does, what BTB takes from it, and what BTB does differently.**

The short version:

| Project | What it is | What BTB takes |
| --- | --- | --- |
| [Convex](#convex) | Reactive backend database for web apps | Developer experience, reactive queries, queries/mutations split, crons |
| [Internet Computer (ICP)](#internet-computer-icp) | Blockchain that hosts full apps | On-chain hosting, free query calls, app-pays fees, timers, threshold signatures |
| [NEAR](#near) | Sharded, developer-friendly L1 | Rust-first SDK, storage staking, developer fee share, chain signatures |
| [Sui and Aptos](#sui-and-aptos) | High-throughput Move-based L1s | Owned-object fast path, Block-STM parallel execution, money-safe types |
| [Nervos CKB and PolkaVM](#nervos-ckb-and-polkavm) | Production RISC-V virtual machines | RISC-V as the contract target, recompilation to native code |
| [THORChain](#thorchain) | Native cross-chain swap network | Validator-controlled vaults holding real assets on other chains |
| [Chainlink and randomness beacons](#chainlink-and-randomness-beacons) | Oracle networks for prices, randomness and web data | Price feeds, verifiable randomness and web requests, built into the validator set |

---

## Convex

**What it is.** Convex ([convex.dev](https://www.convex.dev)) is a backend platform for web developers. Instead of setting up a database, an API server, a job queue and WebSocket infrastructure, a developer writes TypeScript functions in a `convex/` folder and runs `npx convex dev`. Convex hosts the database and runs the functions.

**How it works.**

- **Three kinds of functions.** *Queries* read data and cannot change it. *Mutations* read and write data inside a transaction. *Actions* can call the outside world (APIs) but are not transactional.
- **Reactivity.** When a frontend calls `useQuery(...)`, Convex records exactly which documents and index ranges the query read. When a mutation later changes any of that data, Convex re-runs the query and pushes the new result to the client over a WebSocket. The UI is always live with no polling code.
- **Transactions.** Mutations use optimistic concurrency control: they run, and if another mutation changed the same data in the meantime, they are retried automatically. Because of this, mutations must be deterministic, so Convex controls randomness and time inside them.
- **Scheduling.** Functions can schedule other functions for later, and apps can define cron jobs.
- **Developer experience.** Schemas, indexes and functions live in code; a dashboard lets you browse tables like a spreadsheet; generated TypeScript types make the frontend type-safe end to end.
- **Open source status.** The backend (mostly Rust, with user functions running in V8 JavaScript isolates) is source-available under FSL-1.1-Apache-2.0. Each release converts to Apache 2.0 two years after publication. The license forbids building a product that competes with Convex's hosted service using non-converted code.

**What BTB takes.**

- The overall developer experience: one project folder, `btb dev` with hot reload, `btb deploy`, a table dashboard.
- The query/mutation split, and read-set tracking for live subscriptions.
- Deterministic mutations with retry on conflict, which maps naturally onto parallel execution.
- Scheduled functions and cron jobs as first-class features.

**What BTB does differently.**

- **No single operator.** Convex is run by one company; its server decides the order of transactions. BTB replaces that with validators reaching consensus.
- **No V8.** JavaScript engines have no gas metering and are not guaranteed to give bit-identical results on every machine. BTB runs contracts on a metered RISC-V VM.
- **Provable state.** Convex stores data in SQLite or Postgres. BTB stores state in a Merkle tree so answers can be proven.
- **Money is native.** Convex has accounts and billing; BTB has keys, signatures and the BTB token.
- **Actions become oracles.** Calls to the outside world must be agreed by validators (see [Architecture](ARCHITECTURE.md)).
- **License.** BTB code is MIT. Any reuse of Convex code requires checking that the specific code has converted to Apache 2.0.

---

## Internet Computer (ICP)

**What it is.** The Internet Computer, built by DFINITY, is a blockchain designed to host entire applications, including their frontends, rather than just financial logic. Apps run as *canisters*: smart contracts that bundle code and state.

**How it works.**

- **Canisters run WebAssembly.** Developers can write canisters in Motoko (ICP's own language), Rust, TypeScript, Python and other languages that compile to Wasm. This is the model of "one VM, many languages."
- **Query calls vs update calls.** *Update calls* change state and go through consensus (about one to two seconds). *Query calls* are read-only, answered by a single node in milliseconds without consensus. *Certified variables* let a canister attach a signature so a query answer can be verified.
- **Reverse gas model.** Users do not pay fees. Each canister holds a balance of *cycles* (bought with ICP tokens) and pays for its own computation, storage and bandwidth. When the balance runs out, the canister freezes.
- **Web hosting.** Canisters serve HTTP directly, so a website's HTML, JavaScript and images can live on-chain and be loaded in a normal browser.
- **Timers.** Canisters can schedule code to run periodically or after a delay, with no outside service.
- **Threshold signatures (Chain Fusion).** Nodes in a subnet jointly hold ECDSA and Schnorr keys. A canister can control Bitcoin and Ethereum addresses and sign transactions for them, which powers assets like ckBTC.
- **HTTPS outcalls.** Canisters can call web APIs; each node makes the call and the results are agreed on in consensus.
- **Node providers.** Nodes run on dedicated hardware in data centers operated by approved providers, which gives high performance at the cost of more permissioned participation.

**What BTB takes.**

- On-chain hosting of frontends, so an app needs no AWS.
- Free-feeling, consensus-free queries, with optional proofs.
- App-pays fees: BTB's hosting balances and session-key vouchers mean end users never see per-request costs.
- Native timers, which become BTB's `@cron` and scheduled functions.
- Threshold signatures for writing to other chains, and validator-agreed outside calls.
- "Any language" via a common VM target (see [Language](LANGUAGE.md)).

**What BTB does differently.**

- **Database-native state.** ICP canisters manage their own memory; there are no built-in tables, indexes or subscriptions. BTB's chain maintains indexes and pushes live updates, Convex-style.
- **RISC-V instead of WebAssembly**, for simpler recompilation to native code and ZK-friendliness.
- **Permissionless hosting nodes.** Anyone can run a BTB hosting node and earn from vouchers, instead of only approved node providers.
- **Fixed supply.** ICP mints tokens for rewards; BTB's supply is fixed at 1,000,000,000.

---

## NEAR

**What it is.** NEAR Protocol is a proof-of-stake Layer 1 focused on usability and scaling through sharding.

**How it works.**

- **Contracts in Rust (and JavaScript).** Contracts compile to WebAssembly. The main SDK is `near-sdk-rs`, where a contract is an ordinary Rust struct with `#[near]` attribute macros generating the plumbing. There is also a JavaScript/TypeScript SDK.
- **Readable accounts.** Accounts have names like `alice.near`, and an account can hold several keys with different permissions (for example a limited key that can only call one contract, for smooth app logins).
- **Nightshade sharding.** State is split across shards that process transactions in parallel; adding shards adds capacity.
- **Storage staking.** To store data, an account must lock NEAR tokens proportional to the bytes used. Deleting data unlocks them. This keeps state from growing forever.
- **Developer fee share.** A portion of the gas spent calling a contract (30%) goes to the contract's owner; the rest is burned.
- **Chain Signatures.** NEAR accounts can control addresses on other chains using multi-party computation, similar to ICP's Chain Fusion.

**What BTB takes.**

- **A Rust-first SDK with attribute macros**: BTB contracts can be plain Rust with `#[btb::table]`, `#[query]`, `#[mutation]` and `#[cron]` attributes, in the same spirit as `near-sdk`.
- **Storage staking**, as BTB's storage deposits, which also remove tokens from circulation as usage grows under a fixed supply.
- The idea of rewarding the people who make the network useful, which in BTB becomes paying hosting nodes per request.
- Limited-permission keys, which become BTB's session keys for app-sponsored requests.
- Chain signatures for cross-chain writes.

**What BTB does differently.**

- **Fees go to whoever does the work** (executor and hosting nodes) instead of being partly burned; with a fixed supply there is no inflation to offset.
- **Database-native state and live queries.** NEAR contracts use key-value storage and need indexers for questions like "who holds this token?"
- **Two node roles.** BTB scales reads by adding hosting nodes rather than relying mainly on sharding, at least in early phases.

---

## Sui and Aptos

**What they are.** Sui and Aptos are high-throughput Layer 1s that grew out of Meta's Diem project. Both use the Move language, designed specifically for handling digital assets safely.

**How they work.**

- **Move's resource types.** In Move, a coin is a *resource*: the type system forbids copying it or silently dropping it. You can only move it from one place to another. Whole classes of bugs (duplicated tokens, lost funds, reentrancy) become impossible to write.
- **Sui's object model and fast path.** Every asset on Sui is an object, either *owned* by one address or *shared*. A transaction that only touches the sender's own objects (like sending your own coins) cannot conflict with anyone else's, so it skips full consensus ordering and confirms very quickly. Transactions on shared objects (like a DEX pool) go through a DAG-based consensus (Mysticeti).
- **Aptos's Block-STM.** Aptos executes a block's transactions in parallel *optimistically*: it runs them all at once, detects which ones read data that another one wrote, and re-executes only those. The result is the same as running them one by one, but much faster on multi-core machines.
- **Aptos's Jellyfish Merkle tree.** A storage structure that makes state provable while staying efficient on disk.

**What BTB takes.**

- **The fast path** for transactions that touch only one owner's data.
- **Block-STM-style parallel execution.** BTB's table declarations make it even easier to predict which transactions conflict.
- **Money-safe types.** BTB's SDKs treat tokens as values that cannot be copied or dropped, and reentrancy is prevented by design.
- **The Jellyfish Merkle tree** for state commitments.

**What BTB does differently.**

- **No new language required.** Move is powerful but unfamiliar; BTB is Rust-first and open to any language compiling to RISC-V, with money safety enforced by the SDK and VM.
- **Database model** instead of an object model as the main developer abstraction, plus hosting and subscriptions.

---

## Nervos CKB and PolkaVM

**What they are.** Two production projects that chose RISC-V, the open instruction set used by real CPUs, as their smart contract VM.

**How they work.**

- **Nervos CKB-VM.** Nervos CKB runs scripts in CKB-VM, a RISC-V virtual machine written in Rust. Developers can write scripts in any language with a RISC-V compiler (C, Rust and others). Instructions are metered in "cycles."
- **PolkaVM.** Built for the Polkadot ecosystem, PolkaVM is a RISC-V-based VM that *recompiles* RISC-V code into native x86 machine code ahead of execution instead of interpreting it, getting close to native speed while staying sandboxed and metered. It underpins Polkadot's newer smart contract and JAM work.

**Why RISC-V matters.**

- RISC-V has a small base instruction set (about 40 instructions), so the VM is small and easier to audit.
- RISC-V maps cleanly onto x86 and ARM, so recompilation is fast and the result runs near native speed.
- Mainstream compilers (LLVM, GCC) already target RISC-V, which is what makes "any language" practical: Rust, C, C++, Zig and others work out of the box.
- Zero-knowledge proof systems such as RISC Zero and SP1 prove RISC-V execution, opening a path to verifiable light clients.

**What BTB takes.**

- RISC-V (64-bit, integer-only subset) as the single contract target.
- Recompilation to native code on validators, PolkaVM-style, after a simpler interpreter in Phase 1.
- Per-instruction gas metering.
- Possibly code itself: both projects are open source in Rust, and Phase 1 includes deciding whether to build on one of them or write a custom VM.

**What BTB does differently.**

- BTB adds the database, hosting and subscription layers on top of the VM; CKB and PolkaVM are execution engines, not app platforms.

---

## THORChain

**What it is.** THORChain is a network that lets users swap native assets across chains, for example real BTC for real ETH, without wrapped tokens.

**How it works.**

- **Vaults.** THORChain nodes jointly control addresses on Bitcoin, Ethereum and other chains using threshold signature schemes (TSS). No single node can move funds.
- **Observers.** Nodes run full nodes for each connected chain and report deposits; once enough nodes agree, the deposit is recognized.
- **Liquidity pools.** Each external asset is paired with RUNE (THORChain's token) in a pool. A BTC-to-ETH swap goes BTC → RUNE → ETH, and the network then signs an outbound transaction on Ethereum.
- **Security bond.** Node operators bond RUNE, and the protocol aims to keep the value of bonds above the value held in vaults, so stealing would cost more than it gains.

**What BTB takes.**

- The vault model for native cross-chain assets: deposits on other chains, credited on BTB, withdrawals signed by validators.
- Validator-agreed observation of other chains.
- The rule that vault value must stay below slashable stake.

**What BTB does differently.**

- Cross-chain is a **general capability for any BTB contract**, not just a swap protocol: any app can hold assets on or call contracts on other chains.
- Liquidity and pricing are left to apps (like a DEX built on BTB) instead of being fixed into the protocol.

---

## Chainlink and randomness beacons

**What they are.** Blockchains can't see the outside world: a contract can't look up a price, and it can't produce a random number, because every validator must get the same result. *Oracles* fill that gap. Chainlink is the largest oracle network; randomness beacons such as drand, and the built-in beacons of the Internet Computer and Aptos, solve the randomness half.

**How they work.**

- **Chainlink Data Feeds.** Independent node operators each fetch a price from several data sources and report it; the network aggregates the reports (taking the median) and writes the result to a contract on each supported chain. Feeds update when the price moves past a deviation threshold or when a heartbeat interval passes.
- **Chainlink VRF.** A contract requests a random number; a Chainlink node generates it together with a cryptographic proof that it was computed correctly, and the contract verifies the proof before using it.
- **Chainlink Functions and Automation.** Functions runs custom code that calls web APIs across many nodes and aggregates the results; Automation triggers contract functions on a schedule or condition.
- **Randomness beacons.** drand (used by Filecoin and others) is run by a group of organizations that jointly produce a threshold BLS signature every round; the signature is unpredictable until enough members sign, and verifiable by anyone. The Internet Computer and Aptos produce randomness the same way inside their own validator sets, so contracts get it natively, with no outside request.
- **Low-latency feeds.** Newer designs (Chainlink Data Streams, Pyth) publish signed prices off-chain at high frequency and let apps pull the latest signed value into a transaction only when they need it.

**What BTB takes.**

- Median aggregation over many independent sources, with deviation and heartbeat update rules.
- Threshold-signature randomness produced by the validators themselves, like ICP and Aptos, so contracts get randomness with no extra request round-trip.
- Web requests agreed by many nodes, through transform functions (as in ICP's HTTPS outcalls).
- Signed reports that other chains can verify, so BTB can serve its data beyond its own chain.

**What BTB does differently.**

- **No separate network.** The same validators that run consensus provide prices, randomness and web data. There is no second token or second set of operators to trust, and no per-request fee to an outside network.
- **Feeds are tables.** Prices live in a system table that apps query and subscribe to like any other data, so a frontend gets live prices with one `useQuery` call.
- **Automation is already built in** through `@cron` and scheduled functions.
- **Trade-off:** the oracle is exactly as decentralized as BTB's validator set. A chain with few validators has a weaker oracle; Chainlink spreads the work across operators independent of any one chain.

See [ORACLES.md](ORACLES.md) for BTB's full oracle design.

---

## Putting it together

BTB Chain's bet is that these ideas, each proven separately, have never been combined into one platform:

- **Convex's** developer experience and reactive database
- **ICP's** on-chain hosting, free queries and app-pays fees
- **NEAR's** Rust-first SDK, storage staking and chain signatures
- **Sui's and Aptos's** fast path, parallel execution and money-safe types
- **CKB's and PolkaVM's** RISC-V execution, open to any language
- **THORChain's** native cross-chain vaults
- **Chainlink's, ICP's and Aptos's** oracle and randomness designs, built into the validator set

all on a fixed supply of 1,000,000,000 BTB, where every request pays the node that serves it.
