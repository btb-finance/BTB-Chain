# BTB Tokenomics

BTB has a **fixed supply of 1,000,000,000 tokens** created at genesis. Nothing is ever minted after that. Native amounts are `u64` base units with 9 decimals (1 BTB = 10^9 units, the whole supply 10^18 units); see [CHAIN.md](CHAIN.md#units). Tokens circulate from apps to the nodes that host and execute them.

## Principle: whoever does the work gets paid

Every request pays the node that served or executed it:

| Request | Who pays | Who earns | Settles |
| --- | --- | --- | --- |
| Query / subscription | App (or user) via voucher | Hosting node that served it | Batched on-chain |
| Website / file serving | App via voucher | Hosting node | Batched on-chain |
| Mutation | Sender (or app-sponsored) | Validators 80%, relay host 20% (the relay host is the registered hosting or edge node the sender signed into the transaction; without one, validators get 100%) | In the block |
| Cron job | Contract's prepaid balance | Block executor | In the block |
| Oracle price read in a mutation or cron | The user or bot reading it (a per-read fee of 10,000 gas on top of the read) | Validators that reported the price (validator pool) | In the block |
| Oracle price read in a query | Nobody (queries are not billed) | Served by hosting nodes | None |
| Randomness | Mutation's gas (`300 + 2/byte`; the full gas limit is charged) | Validators | In the block |
| Large-file storage (blob layer) | Uploader prepays per GB per period | Storage nodes that pass storage challenges | Per period |
| File retrieval | App or user via voucher | Storage and edge nodes that served the bytes | Batched on-chain |
| Cross-chain signing and relaying | The requesting app | Validators (signing), relayers (delivery) | In the block |
| State storage | Contract locks a deposit | Refunded on delete | On-chain |

See [Scaling and earning](SCALING.md) for how these micro-payments let anyone, down to a phone, earn by doing a small part of the work.

## DEX fees

The native DEX ([CHAIN.md](CHAIN.md#native-dex)) charges a **clearing fee** of `dex_fee_bps` (genesis parameter, default 10 = 0.10%, maximum 10%) on what each side of a batch auction receives, plus rounding dust, plus the normal gas for placing or cancelling an order. The clearing is work done by validators in `end_block`, so BTB fees go to the validator pool and are paid out with the epoch rewards; fees in a user token wait in the DEX fee account until a distribution rule exists (for example swapping them to BTB in a batch). The DEX escrows funds in a vault and returns every unit: it creates no BTB and no token, and the supply invariant (and the token-supply tests) hold across clearings.

## Payment channels for per-request fees

Paying for each query with an on-chain transaction would require consensus and defeat instant reads. Instead:

1. The app (or user) locks BTB in a payment channel with one on-chain transaction.
2. Each request carries a small signed voucher raising the running total owed to that host.
3. The host serves the request instantly, with no consensus.
4. Periodically (for example daily) the host submits the latest voucher and collects everything in one transaction.

The payer signs every voucher, so a host can never claim more than it actually served.

**On-chain rules** (details in [CHAIN.md](CHAIN.md#payment-channels)). A channel is unidirectional, payer to one payee. A voucher carries the chain id, the channel id and the *cumulative* amount, signed by the payer, so the highest voucher replaces all lower ones and a voucher is useless on another chain or in another channel. The payee closes with its best voucher at any time and receives that amount; the rest of the deposit returns to the payer. If the payee goes silent, the payer can start a timeout close after the channel's expiry; the payee then has a challenge period to submit a higher voucher, which wins, and after it the recorded amount (zero if none) is paid and the remainder refunded. BTB locked in channels is part of the supply invariant; nothing is minted or burned. Off chain, `btb-voucher` (Rust) and `@btb/client` (TypeScript) create and verify vouchers and keep the running totals on both sides, refusing anything that does not strictly grow.

**Who signs:** by default the app pays through a session key with a spending limit, so end users never see fees — like a normal website. Wallets can optionally pay directly with an auto-approve allowance.

## Pricing

Queries are metered in the VM like mutations — per instruction, per row read, per byte returned — so every host computes the same gas for the same request. Hosts compete on price per gas unit, and the SDK routes requests to fast, cheap hosts.

## Large-file storage (blob layer)

Implemented in `btb-chain` (`storage.rs`), details in [STORAGE.md](STORAGE.md). The uploader's `Store` transaction **prepays an escrow** of `(k + m) pieces x periods x pay`, where `pay = price_per_kib_period x piece KiB`. Each time a storage node proves it still holds its piece (a Merkle proof of a challenged 1 KiB chunk), `pay` moves from the escrow to the node. A node that misses a challenge loses `slash_bps` of that piece's collateral, **moved into the file's escrow** (it funds the replacement node; whatever is unspent is refunded to the uploader when the file ends), and the period's payment stays in the escrow. Nodes lock a deposit of `deposit_per_kib` per KiB they hold plus a minimum, so the stake is proportional to the data. Nothing is minted: node deposits and escrows are counted in the supply invariant like balances and stake. Fees for the proof transactions are ordinary gas. Retrieval payments (vouchers) are not built.

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

## Validator staking and slashing

Validators are chosen by stake, not appointed. Operators bond BTB (`Stake`), the top `N` above a minimum form the validator set of the next **epoch**, and the set is fixed for that epoch. Mechanics and parameters are in [CHAIN.md](CHAIN.md#staking-epochs-and-slashing); the economic rules are:

- **No new BTB, ever.** Staking, rewards and slashing only move existing units between balances, bonded stake, unbonding stake, the validator fee pool and the slash vault. The supply invariant (`balances + deposits + bonded + unbonding + host bonds == 1,000,000,000 BTB`) is checked after every block.
- **Host bond and relay share.** A hosting or edge node locks a bond (default 10,000 BTB, genesis parameter) to register, and gets it back after an unbonding period once it unregisters. The relay share (20% of a fee) can be rebated by a sender to itself through a second bonded account; this is deliberately accepted as a priced, supply-neutral fee discount open to anyone who locks the bond (see [CHAIN.md](CHAIN.md#fees)).
- **Unbonding.** `Unstake` moves stake into unbonding, which is released at the end of the epoch in which it was requested plus the unbonding period (testnet default: 14 days). Unbonding stake earns nothing, has no vote, and stays slashable.
- **Rewards.** Mutation and cron fees land in the validator pool (80% of a relayed user transaction's fee, 100% of one with no registered relayer, all of a cron's; the relay share is paid to the relayer's account in the block, see [CHAIN.md](CHAIN.md#fees)). At the end of each epoch the whole pool is paid to that epoch's validators, pro rata by stake. A validator jailed during the epoch gets nothing; its share goes to the others. The rounding remainder (less than one unit per validator) goes one unit each to the first validators in the set, so payouts sum exactly to the pool. This is accounting only: while fees are small, rewards are small, which is what the [early security budget](#early-security-budget) reserve is for.
- **Slashing.** Provable double signing (two votes, or two proposals, for one round, the two kinds of consensus `Evidence`) slashes **5%** of the offender's bonded stake and of each unbonding entry, once per epoch, if the evidence arrives within the unbonding window of the epoch it is about. The offender is jailed for good.
- **Where slashed BTB goes.** 10% of the slashed amount is paid to whoever submitted the evidence (nothing if the offender reports itself). The other 90% goes to the **slash vault**, a reserved address nobody holds a key for: it is locked, not burned, so the supply invariant is unchanged and no one can profit from, or be paid by, the vault. It is deliberately *not* redistributed to the other validators, so that nobody gains from provoking or framing an equivocation, and *not* minted away or destroyed, so the fixed supply stays exactly 1,000,000,000. Unlocking the vault (for example into a community fund) would need an explicit protocol change.
- **Why a fixed percentage.** Double signing is the one fault that can break safety, so it is punished even if it did no harm; a small fraction of the stake is enough to make it unprofitable once stake is worth more than a single block's rewards, and a fixed rate keeps the rule simple. A graduated rate (higher when many validators offend together) and downtime penalties are future work.

## Early security budget

With no minting, validators earn only from fees. While usage is low, fees may not attract enough honest validators. A reserve pool carved from the genesis supply (10–20% proposed) can pay decreasing validator rewards over the first years without breaking the 1B cap.

## Genesis allocation

To be decided. Open items:

- [ ] Validator reserve pool size and release schedule
- [ ] Final staking parameters (epoch length, validator count, minimum stake, unbonding period, slash rate) after testnet data
- [ ] Ecosystem / developer grants
- [ ] Team and early contributors (with vesting)
- [ ] Public distribution
