# Why BTB: compared with Ethereum and Solana

Ethereum and Solana are good at one thing: agreeing on the order of transactions and the resulting balances. Everything else an app needs (reading data, searching it, running scheduled jobs, hosting the website, talking to other chains) is left to outside companies. BTB's goal is not just to be faster. It is to make **the chain the whole backend**, so a developer deploys one project and has nothing else to rent, run or trust.

This page explains where that makes BTB better, how the money flows, and, honestly, where BTB is not ahead yet. The vision is in [IDEA.md](../IDEA.md); numbers here come from the code and from runs on a local network ([QUICKSTART.md](QUICKSTART.md)).

## What an app needs, and who provides it

| An app needs | Ethereum | Solana | BTB |
| --- | --- | --- | --- |
| Smart contracts | Solidity / Vyper, EVM | Rust with Solana's account model | Plain Rust (or any language that compiles to RISC-V) |
| Reading data | Rent an RPC provider (Infura, Alchemy) | Rent an RPC provider (Helius, QuickNode) | **Hosting nodes**, run by anyone, paid per request, answers carry proofs |
| "Who holds this token?", search, sorting | Run or rent an indexer (The Graph, Dune) | Run or rent an indexer | **Tables and indexes in the chain**; a plain query |
| Live updates in the UI | Poll an RPC or run websockets yourself | Same | **Subscriptions** pushed by hosting nodes |
| Scheduled jobs | Keeper bots (Chainlink Automation, Gelato) or your own server | Your own server or a crank service | **Crons in the contract** (`#[btb::cron]`) |
| Website hosting | AWS / Vercel / IPFS pinning | Same | **Sites stored with the contract**, served by hosting nodes |
| Large files | IPFS / Arweave / a cloud bucket | Same | **Storage nodes** with erasure coding and storage proofs |
| Prices, randomness, web data | A separate oracle network (Chainlink, Pyth, VRF) | Pyth, Switchboard | **Built into the validators** |
| Assets on other chains | Bridges and wrapped tokens | Same | **Chain signatures**: validators jointly control native addresses on other chains |

On Ethereum or Solana a small team typically pays for several of these services each month, and each one is a company that can go down, change prices or return wrong data. On BTB they are parts of the same network, paid per use, and verifiable.

## Why "no RPC" matters

An RPC provider is a company you have to trust: if it lies about a balance or is down, your app is wrong or down. BTB replaces it with **hosting nodes**:

- **Anyone can run one** (`btb-host`): it follows the chain and checks every block's signatures itself.
- **Answers are verifiable**: a query can return a Merkle proof tied to a state root that validators holding more than 2/3 of the stake signed. The app checks it with `verifyProof`, so it does not need to trust the node ([HOSTING.md](HOSTING.md)).
- **Read capacity grows with nodes**: reads never touch validators, so twice the hosting nodes means twice the read capacity without slowing consensus.

## Who earns what

Fees go to whoever does the work ([TOKENOMICS.md](TOKENOMICS.md), [SCALING.md](SCALING.md)):

| Work | Who earns | How |
| --- | --- | --- |
| Writes (transfers, mutations, deploys) | Validators 80%, the hosting node that relayed it 20% (validators 100% without one) | Fee per transaction, paid out each epoch by stake |
| Crons and oracles | Validators | Paid by the contract / the caller |
| Reads, live updates, websites | Hosting nodes | Per-request vouchers over payment channels |
| Caching close to users | Edge nodes (down to phones) | Per-request vouchers |
| Large files | Storage nodes | Prepaid, earned only while they prove they hold the data |

Payment channels keep this cheap: an app locks BTB once, signs tiny vouchers off chain per request, and the node cashes in the latest one in a single transaction. On Ethereum and Solana the equivalent money goes to a handful of RPC, indexer and hosting companies; on BTB it goes to many independent operators.

The supply is fixed at 1,000,000,000 BTB with no minting. Validators earn only from fees, with a proposed reserve from the genesis supply for the early years.

## Accounts and names

Ethereum and Solana tie an account to one key forever: lose the key or the seed phrase and the funds are gone, and once an account has sent a transaction its public key is public, so a future quantum computer could derive the private key and empty it. BTB uses a different model ([proposal 0002](proposals/0002-accounts.md)). **Built on chain:** accounts with hidden, rotating keys and `alice.btb` names, including selling and paying by name (`btb name ...`). **Still planned:** passkeys, recovery, `btb1…` addresses and post-quantum signatures.

| | Ethereum / Solana | BTB |
| --- | --- | --- |
| What you send to | `0x71C7…` / `7xKX…` | **`alice.btb`** (and `btb1…` with a typo checksum for machines) |
| Account and key | The address *is* the key; it can never change | The account is permanent; its keys can be added, replaced or removed |
| Public keys on chain | Visible after the first transaction, forever | **Never stored**: the chain keeps only a hash of each key |
| Using a key | Same key every time | **One-time keys**: the transaction that reveals a key also switches the account to a new hidden key, so a revealed key never controls funds again |
| Quantum computers | Every account that ever sent a transaction is exposed | Hashes cannot be broken by quantum computers; only a ~1 s window while a transaction is pending remains, closed by post-quantum signatures in the last phase |
| Logging in | Seed phrase, browser wallet | **Passkeys** (Face ID, fingerprint) for everyday spending with a daily limit; no seed phrase |
| Losing a device | Funds lost | Other devices keep working; **guardian recovery** with a 48-hour wait the owner can veto |
| Savings | One key guards everything | A **vault account** behind your name with hidden one-time keys, and an everyday account you top up from it |
| Names | ENS / SNS: separate contracts and fees | **Built into the chain**; a refundable deposit, nothing minted |
| Selling a name | Through a marketplace contract | **Native atomic swap**: list a price, the buyer pays and receives the name in one transaction, no middleman |

Build order: hidden rotating keys and names (**done**), then passkeys, then recovery, then post-quantum signatures. Existing accounts keep working and can move to the new model.

## Speed and capacity

| | Ethereum | Solana | BTB (measured on one machine) |
| --- | --- | --- | --- |
| Time until final (irreversible) | ~13 minutes | ~0.4 s confirmed, ~13 s finalized | ~50 ms on one machine; ~0.7 s with 100 ms simulated network delay |
| Block time | 12 s | ~0.4 s | ~0.25 s (about 4 blocks per second) |
| Simple transfers per second | ~15–30 on L1 | thousands | ~2,500 measured (the load generator was the limit); up to 10,000 per block |

BTB uses a 2-chain BFT consensus (HotStuff/Jolteon family): a block is final once validators with more than 2/3 of the stake have signed it, and every validator re-executes it and compares the state root before voting. No single node can change balances or forge transactions ([CONSENSUS.md](CONSENSUS.md)).

## Developer experience

| | Ethereum / Solana | BTB |
| --- | --- | --- |
| Language | Solidity, or Rust with a special account model | Ordinary Rust; any RISC-V language |
| Project | Contracts here, backend there, frontend somewhere else | One project: contract, database, crons, site |
| Local loop | Several tools and a local node | `btb dev` with hot reload and live queries |
| Deploy | Contracts, then indexer, then servers, then hosting | `btb deploy` |
| Nonces, gas estimation | Often handled by the developer | The CLI and SDK fill them in |

## Where BTB is not ahead (yet)

Being honest about this is part of the design:

- **Not live.** No public testnet or mainnet runs yet ([TESTNET.md](TESTNET.md)). Ethereum and Solana have years in production and billions in value secured.
- **No ecosystem yet.** No wallets, exchanges, users or audited libraries. Network effects are the hardest thing to build.
- **Not audited.** Consensus, VM and cryptography have tests, but no external security audit.
- **Small validator set.** Votes are all-to-all, fine for tens of validators, not thousands. Ethereum has far more independent validators.
- **Fixed validator set and manual peering.** Validators are fixed at genesis, peers' addresses are configured by hand, and there is no snapshot sync for new nodes yet ([NODE.md](NODE.md#limits)).
- **Speed numbers are local.** Finality and throughput above were measured on one machine; a network across continents will be slower.
- **Accounts: passkeys, recovery and post-quantum signatures are not built yet,** and no wallet manages rotating keys for users yet (see [Accounts and names](#accounts-and-names)).
- **Transfers are public**, like on Ethereum and Solana; there is no privacy layer.
- **Raw write speed is below a single server.** Every write must be agreed by validators, so a centralized backend like Convex will always write faster; BTB trades that for no single owner.

## In one sentence

Ethereum and Solana give you a shared ledger and leave you to rent the rest; BTB aims to give you the whole backend, run by many people who are each paid for the part they do, with answers you can verify instead of trust.
