# Chain execution layer

`crates/btb-chain` is the deterministic execution layer a consensus engine drives: transactions, genesis, blocks, fees, crons, a mempool and validator staking, on top of `btb-vm` (contracts), `btb-runtime` (state, Merkle root) and `btb-codec` (encoding). It has no networking, no clock and no randomness. Consensus decides the order of blocks; this crate decides what a block does.

```rust
let mut state = ChainState::from_genesis(kv, &genesis)?;     // or ChainState::open(kv)
let (receipts, new_root) = Executor::new().execute_block(&mut state, &block)?;
```

On `Err` (a block-level fault, see below) the state is untouched. On `Ok` everything the block did, the new head and the Merkle tree nodes are committed in one atomic batch.

## Units

BTB has **9 decimals**: 1 BTB = 1,000,000,000 base units. Balances, fees, deposits and amounts are `u64` base units. The fixed supply is 1,000,000,000 BTB = 10^18 base units, which fits in a `u64` (max about 1.8 x 10^19). TOKENOMICS.md does not fix decimals; this is the choice `btb-chain` makes (`BTB_DECIMALS`, `UNITS_PER_BTB`, `TOTAL_SUPPLY`).

## Accounts

An account is a 32-byte address: `SHA256("btb.account.v1" || ed25519_public_key)`. Contract addresses are `SHA256("btb.contract.v1" || deployer || nonce:u64be)`, where `nonce` is the nonce of the deploy transaction, so a contract's address is known before it is deployed. Native balances live in the runtime's balance entries (`0x05`); accounts, contracts and the fee pools share them. Each account also has a nonce (`0x20`), the next one it must use.

## Transactions

Transaction kinds, all encoded with `btb-codec` (little-endian, length-prefixed):

```
tx       = body || public_key[32] || signature[64]
body     = chain_id:u64 || nonce:u64 || tag:u8 [|| relayer[32]] || fields     (relayer present iff tag & 0x80)
Deploy   (0): code:bytes || manifest
Call     (1): contract[32] || kind:u8 || function:string || args:bytes || gas_limit:u64 || max_fee:u64
Transfer (2): to[32] || amount:u64
Stake    (3): validator_key[32] || amount:u64
Unstake  (4): amount:u64
RegisterValidator (5): consensus_key[32] || network_addr:bytes
SubmitEvidence    (6): epoch:u64 || evidence
Storage  (12): op:u8 || fields   (0 Register: endpoint:bytes || deposit:u64 | 1 AddDeposit: amount:u64 | 2 Withdraw: amount:u64 | 3 Store: root[32] || size:u64 || k:u8 || m:u8 || periods:u32 | 4 Prove: file[32] || index:u8 || chunk:bytes || chunk_path:[hash] || piece_path:[hash])
Channel  (8): op:u8 || fields   (0 Open: payee[32] || deposit:u64 || expiry_blocks:u64 | 1 TopUp: channel[32] || amount:u64 | 2 Close: voucher[120] | 3 StartClose: channel[32] || has_voucher:u8 [|| voucher[120]] | 4 Finalize: channel[32])
Cross    (21): op:u8 || fields   (0 Deal | 1 Complain | 2 Signature | 3 Evidence | 4 Attest | 5 Prepare | 6 Withdraw; see Cross-chain keys and vault)
Host     (20): op:u8 || fields   (0 Register: url:bytes (at most 256 bytes) | 1 Unregister)
Action   (13): op:u8 || fields  (0 Request: contract[32] || function:string || args:bytes || gas_limit:u64 || max_fee:u64 | 1 SetHosts: contract[32] || hosts:[string])
Site     (9): op:u8 || fields   (0 Publish: contract[32] || path:string || content:bytes | 1 Remove: contract[32] || path:string)
evidence = 0 || voter:u32 || round:u64 || (id[32] || sig[64]) x2        (DoubleVote)
         | 1 || proposer:u32 || round:u64 || (id[32] || sig[64]) x2     (DoubleProposal)
manifest = name:string || crons:[(function:string, every_blocks:u64)] || indexes:[string]
```

- **Call kinds.** `kind` is 0 query, 1 mutation, 2 cron, 3 action. All four decode, but only **mutation** is valid in a `Call` transaction; query, cron and action are rejected as `Invalid::KindNotAllowed` (an action is requested with the `Action` transaction, tag 13, not called). Queries never go through consensus (run them with `Executor::query` against the head state), actions are not transactions, and crons are scheduled-only: users cannot invoke them by hand, so a cron (for example a payout) runs only on its schedule, with `caller() == contract_address()`.
- **Signature.** Ed25519 (`ed25519-dalek`, strict verification) over `SHA256("btb.tx.sig.v1" || body)`. The body contains the chain id and nonce, so a signature is valid for one chain and one nonce only.
- **Transaction id.** `SHA256("btb.tx.id.v1" || wire bytes)`.
- **Versions and the relayer.** A body may name the hosting node that relayed it (`TxBody::relayer`). Then the tag byte has its high bit set (`tag | 0x80`) and the 32-byte relayer address follows the tag, before the fields. A body with no relayer is **byte for byte the original encoding**, so every old transaction still decodes, keeps its signature and id, and executes (its fee goes wholly to the validator pool). The relayer is inside the signed body: the sender chooses it, and nobody who handles the transaction (the relay host, a proposer, a validator) can add, drop or change it without breaking the signature. See [Fees](#fees).
- **Manifest.** `crons` lists `(function, every_blocks)`: the `cron` entry point `function` runs every `every_blocks` blocks, `every = "N blocks"` (1 to 2^32; at most 16). `indexes` are table index declarations in the same temporary form as `btb dev --index` (`Table:index:addr@offset` or `Table:index:u64@offset`, see [DEV_SERVER.md](DEV_SERVER.md)); the chain registers them from state, so a restarted node rebuilds them. This is a binary struct, not the CLI's `manifest.json`; the CLI bundle's `manifest.json` already carries `crons` (`{function, every_blocks}`), taken from `#[btb::cron(every = "N blocks")]` ([LANGUAGE.md](LANGUAGE.md)); `btb deploy --rpc` converts a bundle into a `Deploy` transaction ([CLI.md](CLI.md)).
- **Limits.** Code up to 1 MiB, arguments up to 1 MiB, function names up to 64 bytes.

## Validity, nonces and failure

Every transaction gets exactly one receipt, in order. There are two kinds of "not successful", and they differ in what they cost:

| Outcome | State | Nonce | Fee |
| --- | --- | --- | --- |
| **Invalid** (wrong chain id, bad signature, kind not allowed, nonce not the expected one, gas limit out of range, `max_fee` below `gas_limit * gas_price`, balance below the most the transaction can take) | unchanged | not used | none |
| **Reverted** (out of gas, abort, unknown function or contract, storage deposit unpayable, bad ELF or manifest in a deploy) | the transaction's own changes are undone | used | gas used x gas price |
| **Success** | applied | used | gas used x gas price |

Nonces are strictly sequential per account: the transaction's nonce must equal the account's current nonce. That rules out replays and gaps. A proposer should not put invalid transactions in a block (the mempool filters them), but if one is there it is skipped deterministically.

## Fees

BTB charges `gas_used * gas_price` base units, where `gas_price` is a genesis parameter (default 1 unit per gas). There is no fee market yet: `max_fee` is only a cap (the transaction is invalid if `gas_limit * gas_price` exceeds it), and unused gas is never charged.

Gas is the VM's gas plus an intrinsic part charged before the VM runs:

| Transaction | Gas |
| --- | --- |
| Transfer | `TRANSFER_GAS` = 2,000 |
| Deploy | `DEPLOY_BASE_GAS` 50,000 + `DEPLOY_BYTE_GAS` 100 per byte of code (always charged in full, also if the deploy reverts) |
| Stake / Unstake | `STAKE_GAS` / `UNSTAKE_GAS` 10,000 |
| RegisterValidator | `REGISTER_VALIDATOR_GAS` 20,000 |
| Channel (open, top-up, finalize) / (close, start-close) | `CHANNEL_GAS` 10,000 / `CHANNEL_VOUCHER_GAS` 30,000 (one signature check) |
| Storage (register, add deposit, withdraw / store / prove) | `REGISTER_GAS` 20,000 / `STORE_GAS` 30,000 / `PROVE_GAS` 40,000 |
| Site (publish / remove) | `SITE_GAS` 20,000 + `SITE_BYTE_GAS` 100 per content byte (publish) / `SITE_GAS` (remove); charged in full also when refused |
| SubmitEvidence | `EVIDENCE_GAS` 100,000 (two signature checks; charged also when the evidence is refused) |
| Call | `CALL_BASE_GAS` 2,000 + `CALL_BYTE_GAS` 2 per byte of function name and arguments, plus the VM gas of the call, up to `gas_limit` |

Stored contract code is paid for by deploy gas, not by a storage deposit (a deposit for code is future work). State a contract writes locks the usual storage deposit from the **contract's** balance (fund a contract with a `Transfer` before calling it).

**Fee split.** Fees are not minted or burned; they move into the validator pool (a reserved address, an ordinary balance entry) and, for relayed transactions, to the relay host:

| Source | Executor (validator) pool | Relay host |
| --- | --- | --- |
| User transaction naming a registered relayer | 80% (the rounding remainder) | 20% (`RELAY_SHARE_BPS` = 2,000) to the relayer's account |
| User transaction with no relayer, an unregistered one, or the sender itself as relayer | 100% | none |
| Cron run | 100% | none |

This is the "80/20" mutation split of [TOKENOMICS.md](TOKENOMICS.md). The **validator pool is paid out at every epoch end** to the epoch's validators, pro rata by stake (see [Staking, epochs and slashing](#staking-epochs-and-slashing)). The relay share is credited to the relayer's account in the block, and `Receipt::relayer` records who earned it. The old `relay_pool()` address stays reserved but is no longer credited.

**Relayers and the hosting-node registry.** A hosting or edge node that forwards client-signed transactions ([HOSTING.md](HOSTING.md#relaying-transactions), [EDGE.md](EDGE.md#transaction-relay)) registers its account with `Host::Register { url }` (tag 20; `Unregister` removes it; `HOST_GAS` 10,000). The registry is `0x60 || address` -> URL, plus the bond `0x61 || address` -> amount and unbonding entries `0x62 || maturity:u64 BE || address` -> amount. At execution the share goes to `body.relayer` only if that account is registered at that moment and is not the sender; otherwise the whole fee goes to the validator pool, so a fee is never burned and a typo or a deregistered host costs the sender nothing extra.

*Host bond.* `Register` locks `host_bond` (genesis parameter, default 10,000 BTB) from the host's balance; without that balance it reverts with `InsufficientBond` (gas is still charged). Re-registering only updates the URL. `Unregister` removes the host from the registry at once (it stops earning) and moves the bond into unbonding for `host_unbonding_blocks` (genesis parameter, default equal to the staking unbonding length, 1,209,600 blocks); `begin_block` then returns it to the balance. Bonded and unbonding amounts are summed by the supply invariant, so the fixed supply still holds. Both parameters are part of the genesis hash.

*Self-rebate: decided rule.* A sender can always bond a second account, name it as `relayer` and receive 20% of its own fees. The chain cannot tell a real relay from a sender that relays through itself (the transaction bytes are identical), and any per-epoch earnings cap proportional to the bond would only throttle honest high-volume hosts while a whale just bonds more. So BTB does not pretend to prevent it. It prices and bounds it: (1) the rebate is only a **fee discount of at most 20%**, never new money (supply neutral; the validators lose that 20% of the fee, nothing else); (2) it costs capital: the bond is locked while registered and for the unbonding period afterwards, so the discount is only worth it for a sender whose fee volume over the lock exceeds the bond's opportunity cost (staking yield); (3) it is open to everybody on equal terms, since running a bonded host is exactly how any honest relay node earns the same share, so the economics are "20% volume discount for anyone who locks 10,000 BTB". The bond also removes the free Sybil case (many throwaway registered accounts, registry spam). The sender-equals-relayer case is still refused as the trivial form.

*Host slashing.* Not implemented. The bond is not slashable today: the only misbehaviour tied to hosts that is provable is a host-signed answer that contradicts an attested Merkle proof, and hosts do not sign their answers yet (answers carry proofs the client checks itself, [HOSTING.md](HOSTING.md)). When signed answers exist, slashing can reuse the evidence pattern of consensus equivocation against the bond (which is why unbonding already delays release).

**Parallel mode.** Relayer credits commute, so they are deferred like the pool credits: each transaction only debits its sender, and the receipts carry the relayer; after the transactions the per-relayer sums are credited once. A block with relayed transactions has them (and any transaction touching a relayer account) on the dynamic path, whose reads are tracked, so the registry check is validated like any other read; a transaction that reads or writes a named relayer's balance makes the block fall back to sequential, exactly as for the pool balances. The differential test (`tests/parallel.rs`) mixes registered, unregistered, self and hot-account relayers, transfers to relayers, and registrations in the same block.

**Design note (implemented above).** The relay share needed three things: (1) an optional signed `relayer: Address` in the transaction body (versioned by the tag's high bit, so old transactions are unchanged); (2) crediting `fee * RELAY_SHARE_BPS / 10000` to that address at execution, with the validator pool as the fallback; (3) a registry of hosting nodes to check against, which is the `Host` transaction. Supply is unchanged: every unit debited from the sender is credited to the validator pool or the relayer.

## Staking, epochs and slashing

Validators are accounts that bond BTB. The chain turns that stake into a validator set per **epoch** and exposes it to the node, which drives `btb-consensus` with it ([CONSENSUS.md](CONSENSUS.md#epochs-and-validator-set-changes)). Economics (rewards, slash destination) are in [TOKENOMICS.md](TOKENOMICS.md#validator-staking-and-slashing). Code: `crates/btb-chain/src/staking.rs`.

**Parameters** (`ChainConfig::staking`, fixed at genesis and part of the genesis hash; defaults for the testnet):

| Parameter | Default | Meaning |
| --- | --- | --- |
| `epoch_len` | 600 blocks | Epoch `e` is blocks `e*E + 1 ..= (e+1)*E` (10 minutes at 1 s) |
| `max_validators` | 100 | `N`: seats in a set |
| `min_stake` | 100,000 BTB | Bonded stake needed to get a seat |
| `unbonding_blocks` | 1,209,600 (14 days at 1 s) | Unbonding period and evidence window; at least one epoch |
| `slash_bps` | 500 (5%) | Share of bonded plus unbonding stake slashed for double signing |
| `reporter_reward_bps` | 1,000 (10%) | Share of the slashed amount paid to the reporter |

**Transactions.** An *operator* is an ordinary account. `RegisterValidator` records the operator's Ed25519 **consensus key** (a valid, non-weak point, unique across validators) and a network address (up to 256 opaque bytes, for example a multiaddr); sending it again updates both. `Stake { validator, amount }` bonds `amount` from the sender's balance; `validator` must be the sender's own public key (the field exists so that delegation can be added without a format change; today anything else is refused as `NotOperator`). `Unstake { amount }` moves bonded stake into unbonding. A refused staking transaction is a *revert* (`Revert::Stake(StakeError)`): it changes nothing, uses the nonce and pays the gas, so spam costs money.

**Stake states.** Bonded stake (`0x26`) counts toward a seat from the next epoch on. Unbonding stake (`0x27`, indexed by maturity in `0x2C`) belongs to nobody for voting, is **still slashable**, and is released into the balance at the start of block `maturity`. `maturity = end of the epoch in which Unstake ran + unbonding_blocks`; aligning to the epoch end means stake unbonded at any point of an epoch outlives the evidence window of that epoch and of every earlier one. Several unstakes in one epoch share one entry.

**Epochs.** The set of epoch 0 is the genesis set. At the **last block of epoch `e`** (height `(e+1)*E`), after the transactions and crons, the executor (1) pays out the validator pool (below), (2) selects the set of epoch `e + 1`: every registered, non-jailed operator with bonded stake >= `min_stake`, ordered by stake descending and **ties by operator address ascending**, the first `N` of them (if nobody qualifies the previous set stays, so consensus never has an empty set), (3) stores it as an `EpochRecord { epoch, start_height, anchor, validators }` where `anchor` is the header hash of that last block, and (4) prunes records whose evidence window closed. The order of `validators` is the consensus validator index. Stake changes during an epoch never alter that epoch's set.

**Node API** (`ChainState`): `current_epoch()` (epoch of the next block), `epoch_record(epoch)`, `consensus_set(epoch)` (a `btb_consensus::ValidatorSet` in index order), `consensus_context(epoch)`, `consensus_chain_id()` (use as `btb_consensus::Config::chain_id`; the chain id the evidence rules use), `validator(operator)`, `staked`, `unbonding`, `unbonding_entries`, `selected_set()` (what the next epoch would get right now), `slash_vault_balance()`, `slashed_in_epoch(epoch, operator)`. After executing a committed block that is a multiple of `epoch_len`, the node reads `epoch_record(next)` and starts `Replica::new_epoch(...)` with `Config { epoch, end_height: (epoch+1)*epoch_len, chain_id: consensus_chain_id(), .. }`, the set from `consensus_set`, and `Anchor { height: record.start_height, id: record.anchor }`.

**Slashing.** `SubmitEvidence { epoch, evidence }` carries a `btb_consensus::Evidence` (`DoubleVote` or `DoubleProposal`). It is accepted when all of these hold, checked in this order, and refused with the named `StakeError` otherwise:

1. the set of `epoch` is stored (`UnknownEpoch`: not started yet, or pruned);
2. `height < (epoch+1)*E + unbonding_blocks` (`Expired`);
3. the offender index is a seat in that set (`UnknownValidator`);
4. `Evidence::verify` succeeds against the context rebuilt from that epoch's set, the chain id and the epoch (`BadEvidence`: wrong or flipped signature, same block twice, wrong round, wrong epoch or chain, a vote signature passed off as a proposal signature);
5. the offender was not already slashed for that epoch (`Duplicate`): **one slash per operator per epoch**, whatever the kind or round of the evidence, so repeating an offence or resubmitting a report cannot compound.

Then, in one step: `slash_bps` of the offender's bonded stake and of **each unbonding entry** is removed (stake already released is out of reach, which is why the window ends when the oldest unbonding stake that could have backed the offence matures); `reporter_reward_bps` of the total goes to the transaction sender (to the vault if the sender is the offender, so self-reporting does not pay); the rest goes to the slash vault; the operator is **jailed** (permanently: no new stake, no re-registration, never selected again; it may still unstake). The offender keeps its seat for the rest of the running epoch, since the set is fixed; it is gone from the next set. The receipt's `output` is the slashed amount (`u64` LE).

**Rewards.** At the epoch end the whole validator pool balance is distributed to the epoch's seats that are not jailed, pro rata by their stake in the set (`pool * stake / total`, floor). The rounding remainder is less than one unit per validator; it is given one unit each to the first validators in set order, so the payouts sum to **exactly** the pool. Payouts are credited to the operators' balances; nothing is minted. If every seat is jailed the pool carries over.

## Payment channels

Unidirectional channels for per-request fees ([TOKENOMICS.md](TOKENOMICS.md#payment-channels-for-per-request-fees)). Code: `crates/btb-chain/src/channel.rs`; vouchers: `crates/btb-voucher` (Rust) and `sdk-ts/src/voucher.ts`. All five operations are one transaction kind, `TxKind::Channel(ChannelOp)` (wire tag 8, sub-tag as in the table above).

- **Open { payee, deposit, expiry_blocks }.** Moves `deposit` from the sender (the *payer*) into the channel. The channel id is `SHA256("btb.channel.v1" || payer || tx_nonce:u64le)` (the receipt output). The payer may time out from height `open_height + expiry_blocks`. Refused: zero deposit, payer = payee, `expiry_blocks` outside `1..=2^32`.
- **TopUp { channel, amount }.** Payer only; adds to the deposit; not while a timeout close is running.
- **Close { voucher }.** Payee only, at any time (also after expiry, and during a challenge period). Pays the voucher's cumulative `amount` to the payee and the rest of the deposit to the payer, then deletes the channel. `amount` above the deposit is refused (`ExceedsDeposit`). A second close finds no channel (`NoSuchChannel`), so a lower voucher replayed after a higher one pays nothing. If a timeout close recorded an amount, the payee gets the larger of the two.
- **StartClose { channel, voucher? }.** Payer only, from the expiry height on. Starts a challenge period of `CHANNEL_CHALLENGE_BLOCKS` (600) blocks. The payer may record a voucher it knows of (checked like a `Close`); without one the recorded amount is 0.
- **Finalize { channel }.** Payer or payee, once the challenge period is over: pays the recorded amount to the payee and the rest to the payer. During the period the payee can `Close` with a higher voucher, which wins; from the deadline on only `Finalize` is accepted (`ChallengeOver`), so the payee must claim in time.
- **Voucher.** `(chain_id, channel_id, amount, nonce)` signed Ed25519 (strict) by the payer's account key over `SHA256("btb.voucher.v1" || chain_id:u64le || channel_id || amount:u64le || nonce:u64le)`. The chain checks the chain id (`WrongChain`), the channel id (`WrongChannel`), the signature against the key in the opening transaction (`BadSignature`) and the amount against the deposit. The on-chain rule is "highest amount wins"; `nonce` is the off-chain counter used by `Spender`/`Ledger` (and `VoucherSpender`/`VoucherLedger` in TypeScript) to reject replayed or reordered vouchers.
- **State.** `0x30` channel record `(payer, payer_key, payee, expiry_height, closing)`, `0x31` locked deposit (`u64` LE). `0x31` is summed into the supply invariant; every refusal is a revert (`Revert::Channel(ChannelError)`: nonce used, fee charged, nothing else changes). Node API: `ChainState::channel(id)`, `channels_locked()`.
- **Not done.** Channels are not yet shared with hosting nodes through a registry, a payee cannot batch several channels in one transaction, and the challenge length is a constant rather than a genesis parameter.

## Native DEX

Batch auctions with one uniform price per pair, so the order of transactions inside a batch gives nobody an advantage ([ARCHITECTURE.md](ARCHITECTURE.md#front-running-protection)). Code: `crates/btb-chain/src/dex.rs`; RPC views: `crates/btb-node/src/dex_rpc.rs`; page and smoke test: `examples/native-dex`. One transaction kind, `TxKind::Dex(DexOp)` (wire tag 10, sub-tag 0 `Place`, 1 `Cancel`).

- **Assets and pairs.** An asset is BTB or a user token of the native token module. A pair is unordered; the asset with the smaller encoding (BTB first, then tokens by id) is the **base**, the other the **quote**, and a price is always *quote per base*, the exact rational `n/d` (no floats, no fixed-point scale). Pair id `SHA256("btb.dex.pair.v1" || base || quote)`.
- **Place { sell, buy, sell_amount, buy_amount, batches }.** Sells `sell_amount` of one asset for at least `buy_amount` of the other. Selling the base is an **ask** (limit `buy_amount / sell_amount`, wants a price at or above it); selling the quote is a **bid** (buys up to `buy_amount` base, escrows `sell_amount` quote, limit `sell_amount / buy_amount`, accepts a price at or below it). The whole sold amount is **escrowed** at once in the DEX vault (`dex_vault()`; BTB in its balance, tokens in its token balance, so BTB escrow is counted by the supply invariant like any balance). The order lives for `batches` clearings (1 to 2^20) and its id (the receipt output) is `SHA256("btb.dex.order.v1" || sender || nonce:u64le)`. Amounts and prices are `u128` (smallest unit of the asset, like token balances; the wire form is 16 bytes little-endian each for `sell_amount` and `buy_amount`) and each of `sell_amount` and `buy_amount` is at most `MAX_ORDER_AMOUNT = 2^100` (`AmountTooLarge` otherwise; over a trillion whole tokens at 18 decimals), which keeps the per-pair sums of the clearing in `u128` for any book of fewer than 2^28 orders (a larger sum clears nothing instead of wrapping). BTB balances are `u64`: a BTB escrow above that can never be afforded (the transaction's maximum cost saturates, so the transaction is invalid and never wrapped), and every conversion between `u128` order amounts and `u64` BTB balances is checked. Refused as a revert (fee charged, nothing changes): zero amount, same asset, bad `batches`, unknown token, balance too low (token; the BTB escrow is part of the transaction's maximum cost and makes it `Invalid::InsufficientBalance`), more than 64 open orders, or an order on the **other side of a pair the sender already trades** (`SelfTrade`: no wash trading against yourself).
- **Cancel { order }.** The owner cancels an open order and gets the remaining escrow back at once (the receipt output is the refunded escrow, 16 bytes little-endian); anyone else, an unknown id or an order already cleared away gets `NoSuchOrder`.
- **Batch window.** Genesis parameter `dex_batch_blocks` (default 1): at the end of every block whose height is a multiple of it (`end_block`, after the transactions and crons, before the epoch logic, always sequential) all open orders of every pair are cleared together. An order placed anywhere inside the window is in that clearing; the clearing never looks at where in the block or window an order arrived.
- **Clearing price.** For a pair let `S(p)` be the base offered by asks with limit `<= p` and `D(p)` the base wanted by bids with limit `>= p`; the matched volume at `p` is `min(S, D)`. The candidate prices are the limit prices present in the batch. The price is chosen by: (1) **maximum volume**, (2) among equals the smallest imbalance `|S - D|`, (3) among equals the **lowest** price. Everything is exact (`u128` limits compare by cross-multiplication in 256 bits; shares and fills use a 256-bit intermediate whose quotient is checked to fit), the result depends only on the *set* of orders, and a brute-force reference checks it on thousands of random books.
- **Fills.** Every eligible order on the short side fills completely; the long side is rationed **pro rata** by remaining base: `floor(base * V / L)`, and the `V - sum(floors)` spare units (fewer than the number of orders) go one each to the largest remainders, ties to the lowest order id. All fills are at the one price. A bid that is filled `f` pays `ceil(f * n / d)` quote, an ask receives `floor(f * n / d)`; so the buyers' payments are at least the sellers' receipts and **the difference (rounding dust) goes to the fee sink with the fee**. Nothing is created or destroyed: `sum paid = sum received + sink`, per asset, checked (a mismatch is a state error that stops the block). A bid's limit is tightened to `escrow / base_remaining` after a partial fill, so rounding can never overdraw its escrow.
- **Fee.** Genesis parameter `dex_fee_bps` (default 10 = 0.10%, at most 1000), taken from what each side *receives* (asks in quote, bids in base). BTB goes to the validator pool (shared at the epoch end like transaction fees); a user token goes to the fee account `dex_fees()` (no key; paying it out is future work). Placing and cancelling also pay the normal gas fee (30 000 and 15 000 gas).
- **Rolling and expiry.** After each clearing an order's `batches` count drops by one; an order that is fully filled (or has nothing left to trade) or whose lifetime is over is closed in the same step, its remaining escrow is refunded and a result record (`Filled` / `Expired` / `Cancelled`, base and quote traded) is kept.
- **State.** `0x34 pair id` open order, `0x35 owner id` index to it, `0x36 pair` assets, `0x37 pair height` one record per clearing with a trade `(n, d, base volume, quote volume, orders)`, `0x38 owner id` result of a closed order. Clearing reads all open orders once per window (cost grows with open orders; a per-account cap of 64 bounds spam, a per-pair index is future work). Closed-order and clearing records are not pruned yet.
- **Queries.** `ChainState::dex_pairs`, `dex_book(pair)`, `dex_orders_of(owner)`, `dex_result` / `dex_results_of`, `dex_clearings(pair, n)`, `dex_last_price(pair)`, `dex_vault_balance(asset)`, `dex_fee_balance(token)`, `tokens`, `token_info`, `token_balance`. The node serves them as `btb_dexConfig`, `btb_dexAssets`, `btb_dexPairs`, `btb_dexPair`, `btb_dexBook`, `btb_dexAccount`, `btb_dexBalance`, and the page at `GET /dex` on every node's RPC port.
- **Parallel execution.** A DEX transaction touches the shared vault and the book, so it is never statically confined: it takes the generic multi-version (Block-STM) path, whose result is the sequential one (tested with 1 to 8 threads, fast path on and off), and the DEX vault and fee account are special addresses that keep plain transfers to them off the fast path. Clearing runs in `end_block`, sequentially.
- **Front-running.** Transaction order inside a block can change which transactions fit and who pays gas, never an outcome: tests run the same orders in many permutations (and an "attacker first or last" case) and require identical balances, books, clearing records and results. What this does not hide: orders are public in the mempool, and an order can be cancelled any time before its clearing (like any resting order).
- **Not done.** A constant-product pool as extra liquidity in the batch (the clearing function takes any set of limit orders, so a pool can be added as a ladder of synthetic orders later), pair/asset listing rules, fee distribution for token fees, pruning of old records, and a gas schedule that grows with the number of open orders.

## Storage nodes

Erasure-coded large files whose pieces live on storage nodes, with deposits, an escrow, deterministic assignment, per-piece challenges with Merkle proofs, slashing, reassignment and payouts. One transaction kind, `TxKind::Storage(StorageOp)` (wire tag 12); state tags `0x40` to `0x47`; a scheduler in `end_block` (after the DEX, before staking) opens challenges and handles deadlines; node deposits and file escrows are part of the supply invariant. Rules, parameters (`ChainConfig::storage`, in the genesis) and the randomness caveat are in [STORAGE.md](STORAGE.md). Storage transactions touch shared records, so they always take the sequential path of parallel execution.

## Oracles

Randomness beacon and price feeds (`crates/btb-chain/src/oracle.rs`; the design, the bias argument and the fee rule are in [ORACLES.md](ORACLES.md)).

- **Genesis.** `ChainState::from_genesis_full(kv, &genesis, &validators, Some(&OracleGenesis { group_key, holders, pairs, max_age_blocks, dkg_window }))` stores the config under `50 01`. `group_key` is empty (the validators generate epoch 0's key on chain in the first `dkg_window` blocks, `crate::dkg`) or, for testnets only, `btb_oracle::GroupKey::encode()` of a dealt key (threshold, group public key and every holder's public share; `holders[i]` is the consensus key of beacon holder `i`), stored as the key of epoch 0 (`50 30 0`). `dkg_window` is at least 6 and at most half the epoch length. It is validated (the shares must lie on one polynomial whose value at 0 is the group key, holders distinct, pairs distinct, `max_age_blocks` in `1..=100000`) and committed in the genesis hash. Without it the chain has no oracles and everything below is off.
- **Block input.** `Block::oracle = OracleInput { beacon, observations }` (an optional trailing section of `Block::encode`; absent when empty, so blocks of chains without oracles encode as before). It is not part of the header hash: the beacon is unique and verified, and the observations are covered by the consensus block id.
- **Start of a block** (`oracle::begin_block`). The block must carry the beacon for its own height and epoch and it must verify under the group key of **its epoch** (`50 30 epoch`), or `execute_block` returns `BlockError::Oracle` and writes nothing. A block of an epoch that has no key yet (the DKG window of a chain that generates its first key on chain) must carry **no** beacon, and the seed record is deleted so `random_bytes` traps with `RandomnessUnavailable`. Before that check, the DKG hook opens the key generation whose window starts at this height. The seed is stored at `50 02` (`height || seed`) and the full beacon at `50 03`, so the state root commits to the last beacon and a verifier can check it from a state proof. Contracts reach the seed only through `random_bytes`.
- **Key generation** (`crates/btb-chain/src/dkg.rs`, `TxKind::Dkg`, tag 14: `Deal`, `Complain`, signed by a validator's consensus key). Sessions, deals and per-epoch keys live under `50 30`, `50 40`, `50 41`; at the end of the last block of a window the session is finalized, and the staking module uses the next validator set as locked at the window's start. Full protocol, failure behaviour and costs: [ORACLES.md](ORACLES.md#key-provisioning-and-the-dkg).
- **End of a block** (`oracle::end_block`, after the crons and the DEX, before staking). Observations are verified one by one and skipped, never an error, when badly signed, from a non-validator of the current epoch, for an unknown pair, from the future, older than `max_age_blocks`, or not newer than the validator's previous one for that pair. For every pair that got an accepted observation the chain takes the stake-weighted median of the current epoch's validators' fresh observations if at least two thirds of the stake reported, and stores it at `50 10 pair`. Expired observations and those of validators that left are deleted when a pair is recomputed.
- **State tag `0x50`**: `50 01` config, `50 02` seed, `50 03` last beacon, `50 10 pair` price (`price: u128 LE, height: u64 LE, decimals: u8, reporters: u32 LE`), `50 20 pair key` a validator's latest observation (`price: u128 LE, height: u64 LE`). None of it is money, so the supply invariant is untouched.
- **Gas.** `random_bytes` costs `300 + 2 * len`; `report_request` costs `5,000 + 10 * len`; `price` costs 200 in a query and 10,200 in a mutation or cron (200 plus the 10,000 per-read fee). A call to an entry point marked `randomness` is charged its **full gas limit** whatever happens (a cron: `cron_gas_limit`), so aborting or trapping after a draw is never cheaper.
- **Parallel execution.** The beacon and the prices are ordinary state written before and after the transactions, so Block-STM sees them as plain reads; sequential and parallel runs are byte-identical (`tests/oracle.rs::parallel_and_sequential_execution_agree_with_randomness_and_prices`).

## HTTPS actions and reports

HTTPS actions with validator-agreed results (`crates/btb-chain/src/actions.rs`; the design, the SSRF rules and the safety argument are in [ORACLES.md](ORACLES.md#3-custom-web-data-https-requests)) and queued reports for other chains (`crates/btb-chain/src/reports.rs`, [ORACLES.md](ORACLES.md#4-btb-as-an-oracle-for-other-chains)). Neither needs an oracle config; reports need the beacon committee to sign them.

- **Transaction** `TxKind::Action(ActionOp)`, wire tag 13. `Request { contract, function, args, gas_limit, max_fee }`: intrinsic gas `20,000 + 2 * (function + args bytes)`, then the action's first phase runs read-only with the rest of `gas_limit`. The sender must afford `gas_limit * gas_price + max_fee`; `max_fee` is **escrowed** (moved to the reserved `actions::escrow_pool()` account, which the supply invariant counts as a balance) and must be at least `(cron_gas_limit + 100,000 * validators of the epoch) * gas_price`. `SetHosts { contract, hosts }` (deployer only) replaces the contract's host allowlist. Failures (`Revert::Action(ActionError)`: no such contract, action or callback, bad spec, host not allowed, escrow too low, full queue, ...) change nothing and charge the gas used. The receipt output of a request is its id (the transaction id).
- **Block input.** `Block::actions: Vec<ActionVote>`, a second optional trailing section of `Block::encode` after the oracle input (absent when empty). Not part of the header hash; covered by the consensus block id of the payload it came in. At most 512 per block.
- **End of a block** (`run_actions`, after the crons, before the DEX). Votes are verified and recorded (bad ones skipped, never an error); each pending request is judged ([ORACLES.md](ORACLES.md#lifecycle)); every request with a verdict is delivered to `<function>_callback` with caller `actions::CALLER` and gas `cron_gas_limit`, as a system call whose receipt has `Origin::Action { contract, function, request }` and id `SHA256("btb.action.cb.v1" || request id)`, and its escrow is settled (callback gas to the validator pool, `100,000` gas to each validator that voted, the rest back to the requester). Deliveries count against `max_crons_per_block` and `cron_gas_block_cap`; later ones wait.
- **A transaction cannot call a callback**: `run_contract` refuses a mutation named `<action>_callback` unless the caller is `actions::CALLER` (`Revert::Action(CallbackIsSystemOnly)`).
- **State tag `0x51`**: `51 01 contract` allowlist, `51 02` pending count, `51 10 id` pending request, `51 20 id key` a validator's vote (outcome hash), `51 30 id outcome` the outcome (`status || data`). Everything of a request is deleted when it is delivered. The escrow is a balance of `actions::escrow_pool()`, so the supply invariant is untouched.
- **Reports**, state tag `0x52`: `52 00` next id, `52 01 id` `height:u64 LE || contract[32] || data`. Written by the `report_request` host call (`RuntimeHost`) inside the calling mutation or cron, so they commit or revert with it; `reports::end_block` drops records older than 100,000 blocks (64 per block at most). The committee's signature is not in state: it is a deterministic function of the record and the epoch key, held by the validators and served over RPC.
- **Parallel execution.** Requests are ordinary transactions (they conflict on the queue counter, so Block-STM re-executes them in order); votes and deliveries are in the sequential end-of-block part. Sequential and parallel runs are byte-identical (`tests/actions.rs::sequential_and_parallel_execution_agree`).

## Cross-chain keys and vault

Threshold FROST keys for Bitcoin Taproot and Solana, their signatures and the native vault ([CROSS_CHAIN.md](CROSS_CHAIN.md)). `TxKind::Cross(CrossOp)`, tag 21 (`cross.rs`): `Deal`/`Complain` (validator consensus key, gas 80,000 + 2,500 per item / 300,000), `Signature` (150,000, anyone), `Evidence` (400,000), `Attest` (30,000 + 20,000 per claim, validator), `Prepare` (60,000, validator), `Withdraw` (50,000). Refusals are `Revert::Cross(CrossError)` and change nothing.

State, tag `0x70` (`xdkg.rs`, `xsign.rs`, `vault.rs`): `01 scheme epoch` keys, `10`/`11` sessions and deals, `20`-`22` signing requests (next id, record, pending index), `30` offenders, `31` evidence markers, `50`-`55` vault (asset totals, claim votes, credited markers, withdrawals, prepare votes). Key generation sessions are opened and finalized by `cross::begin_block`/`end_block` right after the beacon DKG hooks. Finished requests are pruned after 100,000 blocks. Wrapped tokens are ordinary entries of the token ledger (`0x0A`, `0x0B`); BTB balances are never touched and the supply invariant holds every block.

## Site files

`TxKind::Site(SiteOp)` (wire tag 9, `crates/btb-chain/src/site.rs`) lets a contract's **deployer** publish static website files that hosting nodes serve ([HOSTING.md](HOSTING.md#websites)). `Publish { contract, path, content }` creates or replaces `0x32 | contract | path -> SHA-256(content) || content`; `Remove { contract, path }` deletes it. Paths are 1 to 128 bytes of `[A-Za-z0-9._~-]` segments separated by single `/`; files are at most 96 KiB and a contract has at most 256 (large files belong on storage nodes, which do not exist yet). A sender that is not the deployer, an unknown contract, a full contract or a missing file to remove is refused with `Revert::Site(..)` after the fee is charged; nothing changes. Malformed paths and oversize files are `Invalid::TooLarge` and never enter a block. The transaction touches no balance but the sender's fee, so in parallel mode it takes the generic (multi-version) path like every non-transfer. No BTB moves, so the supply invariant is unaffected.

## Genesis and the supply invariant

`GenesisConfig { config, timestamp, allocations }` lists every balance. The allocations must sum to **exactly** `TOTAL_SUPPLY` with no address twice, or `from_genesis` fails. This is the only place BTB is created. The final allocation (reserve pool, grants, vesting) is still open in TOKENOMICS.md; the config just takes whatever list is decided.

After every block the executor sums all balances (accounts, contracts, the fee pools and the slash vault), all storage deposits, all bonded stake, all unbonding stake and all BTB locked in payment channels, and requires the sum to equal `TOTAL_SUPPLY`. If not, `execute_block` returns `BlockError::SupplyInvariant` and writes nothing. Nothing in the crate mints: transfers, fees, deposits, staking, channels, rewards and slashing only move existing units. User tokens (`Token.create`, host calls 16 to 23) are separate and are not part of this sum. The check scans all balances, O(accounts) per block; `Executor::check_supply` can switch it off for benchmarks.

## Blocks

```
Header { height, parent_hash, timestamp, state_root, tx_root }
hash   = SHA256("btb.block.v1" || height || parent_hash || timestamp || state_root || tx_root)   (big-endian ints)
```

- `state_root` is the Merkle root **before** the block (what the previous block produced), so a proposer builds a block without executing it and each block commits to the exact state it extends. The root after the block is returned by `execute_block` and goes into the next header. Genesis is version 0; block `h` is version `h`.
- **The state root is certified by validators.** A header names a root, but a consensus certificate on a block covers its transactions, not what they produce. Consensus blocks therefore carry an execution claim and honest validators vote only when it equals their own execution: a quorum certificate on a block certifies the root after the newest block its ancestry commits (and, through a hash chain over the roots, every earlier root). The chain's execution defines the values (`state_root_h`, the receipts of block `h`); the digest chain and the receipts root are in [NODE.md](NODE.md#execution-attestation), the argument in [CONSENSUS.md](CONSENSUS.md#execution-attestation-validators-certify-state-roots). Nothing in the block format or the state changed for this.
- `timestamp` is chosen by the proposer (Unix milliseconds). Execution checks only that it is not earlier than the parent's. Contracts never see it, and crons run by height, so it cannot change what a block does.
- `tx_root` is a hash over the transaction ids.
- `Block::encode` / `Block::decode` give the wire form. `Block::oracle` carries the oracle input ([Oracles](#oracles)) and `Block::actions` the validators' votes on HTTPS actions ([HTTPS actions and reports](#https-actions-and-reports)).

`GenesisConfig` itself stays as it was; the epoch-0 validators go through `ChainState::from_genesis_with_validators(kv, &genesis, &[GenesisValidator { operator, consensus_key, network_addr, stake }])`. Each validator's stake is bonded out of its operator's allocation (which must hold it), so the allocations still sum to `TOTAL_SUPPLY`; the genesis hash commits to the validators (`GenesisConfig::hash_with_validators`; with none it equals `hash()`).

`execute_block` returns `Err` without changing state for: `Oracle` (missing, mismatched or invalid beacon on a chain with oracles), `BadHeight`, `BadParent`, `BadStateRoot`, `BadTxRoot`, `TimestampBeforeParent`, `TooManyTxs` (`max_block_txs`), `BlockGasLimit` (sum of `gas_limit`s above `block_gas_limit`), and `SupplyInvariant`.

## Execution order

1. Header checks.
2. The transactions, in block order, each seeing the effects of the earlier ones. `ExecMode::Sequential` runs them one at a time in one overlay (the reference); `ExecMode::Parallel` (the default) runs them speculatively on several threads with the same result, see [Parallel execution](#parallel-execution). One overlay holds the block's writes.
   Before the transactions: the beacon is verified and stored ([Oracles](#oracles)).
3. The due crons, as system transactions. Always sequential, after all transactions. Then the HTTPS action votes and deliveries, then the DEX clearing, then the price observations, then staking.
4. The new head, then the supply check, then the Merkle root, then one atomic commit.

Contract code runs in `btb-vm` with the call's gas limit. A reverted call's writes (storage, tables, deposit moves, token moves) are dropped as a whole.

## Parallel execution

`Executor::new()` runs the transactions of a block in parallel (Block-STM style); `Executor::sequential()` is the plain loop. **The two are byte-identical** in receipts (status, gas, fee, output), state root and every stored byte, for every block, whatever the thread count or timing. The sequential executor is the oracle: `tests/parallel.rs` runs random blocks on it and on 1, 2, 4 and 8 workers (and a second 4-worker run, and one without the fast path) and compares receipts, roots and finally the whole store. `Executor { mode: ExecMode::Parallel { threads }, fast_path, .. }` selects it (`threads = 0`: one per CPU). Blocks smaller than `parallel_min_txs` (default 16; thread start-up costs more than it saves), and stores that cannot be read from several threads (`KvStore::as_sync()` is `None`, e.g. an `Overlay`), run sequentially. `execute_block_stats` also reports what happened (`ParStats`).

**Order of a block, parallel.** 1. `begin_block` (sequential, its writes are index 0 of the multi-version memory). 2. The transactions, in parallel. 3. Pool credits (below). 4. **Crons, `end_block` (epochs, rewards, set rotation), the head, the supply check and the Merkle root stay sequential**, on the merged result: they are few, order-dependent and touch shared staking state.

**Engine** (`src/parallel.rs`):

- *Multi-version memory.* Every speculative write is stored under `(key, tx index, incarnation)`; a transaction reads the latest write of a lower index, else the committed state. Scans merge the same way.
- *Speculation.* Workers run the unchanged `apply_tx` against that view; each read is logged (point reads with the version seen, scans with their result).
- *Validation and re-execution.* A transaction is valid if every logged read still returns what it saw. An invalid one has its writes marked `ESTIMATE` and runs again with a new incarnation; a reader that meets an estimate waits for the (lower-index, already claimed) writer instead of reading a value about to change. Transactions above a failed one that read what it wrote are re-executed in the same wave. Waves repeat until a validation pass fails nothing; then every transaction has read exactly what the sequential order gives it, so by induction on the index its outputs are the sequential ones. The commit is the highest-index write per key, in block order, independent of thread timing.
- *Errors.* A state fault inside a speculative run is just a result that is validated like any other; the lowest-index genuine fault is returned as the block error, as the sequential loop would.

**Fee pools.** Every transaction would credit the validator pool (and its relayer) and so conflict with all others. Those credits commute, so in parallel mode each transaction only debits its sender, and the sums (computed from the receipts, same 80/20 split, per relayer) are credited once after the transactions. This is invisible unless something reads or writes a pool or relayer balance (a transfer to a pool address, a contract asking for its balance): if any final execution does, the block is re-run sequentially.

**Deploys.** Tables are registered per worker from the stored manifest of the contract being called; the shared registry is updated after the block from the successful deploys, in block order.

### Owner-only fast path (execution level)

A transaction's *account footprint* is known from the transaction alone: `Transfer` touches the sender and the recipient, `Deploy` the sender and the new contract address, `Stake`/`Unstake`/`RegisterValidator` the sender, `SubmitEvidence` the sender and the slash vault, `Channel` the sender (settlements also pay the payee and the payer, which is why channel transactions are never on the fast path), `Call` the sender and the contract. Transactions that share an account are joined into components. A component is **confined** (owner-only) when it contains only `Transfer`s and none of its accounts is a fee pool or the slash vault: everything it reads or writes is the nonce and balance of its own accounts. Confined components are independent of each other and of every other transaction, so each runs on one worker in block order, directly on the committed state, with no multi-version memory, no read tracking for scheduling and no validation. (Plain `Transfer` touches two accounts, so "owner-only" here means "closed over a known set of accounts", not "one account".)

Because a contract call can move tokens to or read the balance of any account, the footprint of a `Call` is only a lower bound. The fast path therefore checks itself after the run: each confined transaction must have read and written only its footprint keys, and no dynamic transaction may have read or written (or scanned over) a confined account. If either fails, or a pool is touched, the block runs sequentially. Fallback depends only on the block and the state, so every node takes the same path and the result is the same anyway.

### Relation to the consensus fast path

[ARCHITECTURE.md](ARCHITECTURE.md) describes a *consensus* fast path: transactions on one user's own data confirm with validator signatures but without global ordering. That is **not built** (future work). This execution-level path only schedules transactions inside an already ordered block; it does not change what consensus orders or when a transaction is final. The same footprint rule is what a consensus-level fast path would need to decide which transactions may skip ordering, and receiving a transfer (recipient is a shared account) would still need care there.

### Measured

`cargo test --release -p btb-chain --test parallel_bench -- --ignored --nocapture` (4 CPUs, MemKv, 4000 txs per block, signature checks included, tx/s):

| workload | sequential | 2 workers | 4 workers |
| --- | --- | --- | --- |
| disjoint transfers (low conflict, fast path) | 16.4k | 34.6k (2.1x) | 41.0k (2.5x) |
| disjoint transfers, fast path off | 16.4k | | 27.5k (1.7x) |
| 7/8 transfers + 1/8 counter calls (low conflict) | 13.3k | 14.2k (1.07x) | 20.1k (1.5x) |
| everyone pays one account (high conflict) | 16.2k | 1.0x | 1.0x (runs sequentially) |
| everyone calls one counter (high conflict) | 5.2k | 0.7x | 1.0x |

High-conflict blocks do not get faster (a dependency chain is sequential) but, thanks to waiting on estimates, do not get much slower either. The multi-version engine re-executes about twice per transaction in the worst case. These are single-machine numbers on a 4-core box, not a tuned result.

## Cron jobs

A deploy schedules each manifest cron for height `deploy_height + every`. At each block, after the transactions, the executor runs the crons that are due (due height at most the current height), ordered by (due height, contract address, function name):

- The cron runs with `caller() == contract_address()`, `cron_gas_limit` gas and the contract's balance as its payer. The fee goes 100% to the validator pool.
- The next run is scheduled for `current_height + every` (a late cron does not catch up in a burst).
- **Paused.** If the contract's balance is below `cron_gas_limit * gas_price` nothing runs and the receipt is `Status::Paused`; the schedule continues, so funding the contract resumes it.
- **Per-block cap.** A cron starts only if the cron gas already used in the block plus `cron_gas_limit` fits `cron_gas_block_cap`, and at most `max_crons_per_block` are considered. The rest stay due and run first in the next block. This is fair: the order is by due height first, a run reschedules to `height + every` (behind everything already waiting), and a paused cron also moves back, so with more due crons than the cap allows every one runs in turn (tested with three `every = 1` crons and room for one per block) and no cron can be starved by a busier contract.
- A failing cron reverts its changes and still pays for the gas it used.

Cron receipts follow the transaction receipts; their id is `SHA256("btb.cron.v1" || height || contract || function)`.

## Mempool

`Mempool` holds pending transactions. Admission rejects: wrong chain id, query, cron or action kinds, bad signatures, oversize transactions, duplicates, nonces already used, a balance that cannot cover the transaction, a full pool or a full sender slot. A second transaction for the same sender and nonce replaces the first only if it reserves a higher fee. `select` returns transactions for the next block: highest fee first (fee = `gas_limit * gas_price`, fixed for deploys and transfers), ties broken by transaction id, but each sender's transactions strictly in nonce order starting at its next nonce, within a transaction and gas budget. `remove` and `prune` clean up after a block.

## Queries

`Executor::query(state, contract, function, args, caller)` runs a `query` entry point read-only against the head state and returns the output and gas used. Nothing is written, no consensus is involved, and the result can be served by any node that has the state.

## Determinism

No floats, clocks, thread timing or hash-map iteration order reach state (parallel execution uses threads, but its result is fixed by the block order, see above): balances and keys are integers and ordered maps, crons are scheduled by height, and the contract VM is rv64im only. The tests run the same block sequence on two fresh states and require identical receipts, roots and stored bytes, and (with `--features rocksdb`) the same results on RocksDB, including after a reopen.

## Not done yet

- A consensus-level fast path (ordering-free confirmation of owner-only transactions), and parallel crons / end-of-block logic.
- A fee market (a per-gas bid), and a bond for registered hosts (see Relayers above).
- Channel parameters (challenge length) in the genesis config, and batched settlement of many channels.
- Delegation (stake behind someone else's validator), un-jailing, reward weighting by proposals and uptime, and slashing for downtime.
- A storage deposit for contract code and for user-token records.
- Oracles: the beacon committee counts validators, not stake (stake-weighted virtual shares, [ORACLES.md](ORACLES.md#status)); price feeds still take a mock observation source.
- HTTPS actions and reports: reports are signed by the BLS committee key only (a threshold-ECDSA variant for cheap EVM verification needs the cross-chain key); the allowlist is per contract and set by its deployer, there is no chain-wide governance of hosts; validator fetch policy (SSRF rules) is node configuration, not consensus.
- DEX: AMM liquidity in the batch, token-fee payout, record pruning (see [Native DEX](#native-dex)).
