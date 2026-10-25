# BTB Tokenomics

BTB has a **fixed supply of 1,000,000,000 tokens** created at genesis. Nothing is ever minted after that. Tokens circulate from apps to the nodes that host and execute them.

## Principle: whoever does the work gets paid

Every request pays the node that served or executed it:

| Request | Who pays | Who earns | Settles |
| --- | --- | --- | --- |
| Query / subscription | App (or user) via voucher | Hosting node that served it | Batched on-chain |
| Website / file serving | App via voucher | Hosting node | Batched on-chain |
| Mutation | Sender (or app-sponsored) | Block executor 80%, relay host 20% (proposed) | In the block |
| Cron job | Contract's prepaid balance | Block executor | In the block |
| Oracle price read (in a mutation) | The user or bot reading it | Validators that reported the price | In the block |
| Randomness | Mutation's gas | Validators | In the block |
| Large-file storage (blob layer) | Uploader prepays per GB per period | Storage nodes that pass storage challenges | Per period |
| File retrieval | App or user via voucher | Storage and edge nodes that served the bytes | Batched on-chain |
| Cross-chain signing and relaying | The requesting app | Validators (signing), relayers (delivery) | In the block |
| State storage | Contract locks a deposit | Refunded on delete | On-chain |

See [Scaling and earning](SCALING.md) for how these micro-payments let anyone, down to a phone, earn by doing a small part of the work.

## Payment channels for per-request fees

Paying for each query with an on-chain transaction would require consensus and defeat instant reads. Instead:

1. The app (or user) locks BTB in a payment channel with one on-chain transaction.
2. Each request carries a small signed voucher raising the running total owed to that host.
3. The host serves the request instantly, with no consensus.
4. Periodically (for example daily) the host submits the latest voucher and collects everything in one transaction.

The payer signs every voucher, so a host can never claim more than it actually served.

**Who signs:** by default the app pays through a session key with a spending limit, so end users never see fees — like a normal website. Wallets can optionally pay directly with an auto-approve allowance.

## Pricing

Queries are metered in the VM like mutations — per instruction, per row read, per byte returned — so every host computes the same gas for the same request. Hosts compete on price per gas unit, and the SDK routes requests to fast, cheap hosts.

## Storage deposits

Contracts lock BTB per byte of state they store and get it back when the data is deleted. This:

- prevents state bloat from cheap, permanent storage, and
- takes BTB out of circulation as real usage grows.

## User tokens

Anyone can create their own fungible token with the native token module (`Token.create`, host calls 16 to 22, see [LANGUAGE.md](LANGUAGE.md#host-calls)). User tokens are **not BTB**: they have their own id, name, symbol, decimals, `u128` balances and supply, and no operation on them can mint, burn or move BTB. The fixed 1,000,000,000 BTB supply is unaffected; the tests check that native balances plus storage deposits are unchanged by token operations.

- **Id.** `SHA256("btb.token.v1" || creating contract || nonce:u64be)`, where the nonce counts the tokens that contract has created. Deterministic and collision-free per creator.
- **Create.** Name 1 to 64 bytes, symbol 1 to 12 ASCII letters or digits, decimals up to 38. The whole initial supply is credited to the creating contract, which distributes it with `transfer` (a faucet, a sale, an airdrop).
- **Supply.** With no mint authority the supply is fixed forever. With one, only that address can `mint` and `burn` (burn takes from its own balance), every change uses checked math, and supply always equals the sum of balances.
- **Transfers** move tokens from the running contract's own balance, like native `token_transfer`. A failed transfer or an abort leaves no trace.
- **Holders** are queryable by anyone (`token_holders`), paged in ascending address order; accounts with a zero balance are not holders.
- **Cost.** Creation and each call are paid in gas like any call. Token records are not yet part of the storage deposit; a deposit per token and holder entry is future work before open networks.

## Early security budget

With no minting, validators earn only from fees. While usage is low, fees may not attract enough honest validators. A reserve pool carved from the genesis supply (10–20% proposed) can pay decreasing validator rewards over the first years without breaking the 1B cap.

## Genesis allocation

To be decided. Open items:

- [ ] Validator reserve pool size and release schedule
- [ ] Ecosystem / developer grants
- [ ] Team and early contributors (with vesting)
- [ ] Public distribution
