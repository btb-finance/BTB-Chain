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
| Storage | Contract locks a deposit | Refunded on delete | On-chain |

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

## Early security budget

With no minting, validators earn only from fees. While usage is low, fees may not attract enough honest validators. A reserve pool carved from the genesis supply (10–20% proposed) can pay decreasing validator rewards over the first years without breaking the 1B cap.

## Genesis allocation

To be decided. Open items:

- [ ] Validator reserve pool size and release schedule
- [ ] Ecosystem / developer grants
- [ ] Team and early contributors (with vesting)
- [ ] Public distribution
