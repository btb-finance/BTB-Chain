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

Seven kinds, all encoded with `btb-codec` (little-endian, length-prefixed):

```
tx       = body || public_key[32] || signature[64]
body     = chain_id:u64 || nonce:u64 || tag:u8 || fields
Deploy   (0): code:bytes || manifest
Call     (1): contract[32] || kind:u8 || function:string || args:bytes || gas_limit:u64 || max_fee:u64
Transfer (2): to[32] || amount:u64
Stake    (3): validator_key[32] || amount:u64
Unstake  (4): amount:u64
RegisterValidator (5): consensus_key[32] || network_addr:bytes
SubmitEvidence    (6): epoch:u64 || evidence
Channel  (8): op:u8 || fields   (0 Open: payee[32] || deposit:u64 || expiry_blocks:u64 | 1 TopUp: channel[32] || amount:u64 | 2 Close: voucher[120] | 3 StartClose: channel[32] || has_voucher:u8 [|| voucher[120]] | 4 Finalize: channel[32])
Site     (9): op:u8 || fields   (0 Publish: contract[32] || path:string || content:bytes | 1 Remove: contract[32] || path:string)
evidence = 0 || voter:u32 || round:u64 || (id[32] || sig[64]) x2        (DoubleVote)
         | 1 || proposer:u32 || round:u64 || (id[32] || sig[64]) x2     (DoubleProposal)
manifest = name:string || crons:[(function:string, every_blocks:u64)] || indexes:[string]
```

- **Call kinds.** `kind` is 0 query, 1 mutation, 2 cron, 3 action. All four decode, but only **mutation** is valid in a transaction; query, cron and action are rejected as `Invalid::KindNotAllowed`. Queries never go through consensus (run them with `Executor::query` against the head state), actions are not transactions, and crons are scheduled-only: users cannot invoke them by hand, so a cron (for example a payout) runs only on its schedule, with `caller() == contract_address()`.
- **Signature.** Ed25519 (`ed25519-dalek`, strict verification) over `SHA256("btb.tx.sig.v1" || body)`. The body contains the chain id and nonce, so a signature is valid for one chain and one nonce only.
- **Transaction id.** `SHA256("btb.tx.id.v1" || wire bytes)`.
- **Manifest.** `crons` lists `(function, every_blocks)`: the `cron` entry point `function` runs every `every_blocks` blocks, `every = "N blocks"` (1 to 2^32; at most 16). `indexes` are table index declarations in the same temporary form as `btb dev --index` (`Table:index:addr@offset` or `Table:index:u64@offset`, see [DEV_SERVER.md](DEV_SERVER.md)); the chain registers them from state, so a restarted node rebuilds them. This is a binary struct, not the CLI's `manifest.json`; the CLI bundle's `manifest.json` already carries `crons` (`{function, every_blocks}`), taken from `#[btb::cron(every = "N blocks")]` ([LANGUAGE.md](LANGUAGE.md)); converting a bundle to a `Deploy` transaction is for the CLI to do when `btb deploy` targets a chain.
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
| Site (publish / remove) | `SITE_GAS` 20,000 + `SITE_BYTE_GAS` 100 per content byte (publish) / `SITE_GAS` (remove); charged in full also when refused |
| SubmitEvidence | `EVIDENCE_GAS` 100,000 (two signature checks; charged also when the evidence is refused) |
| Call | `CALL_BASE_GAS` 2,000 + `CALL_BYTE_GAS` 2 per byte of function name and arguments, plus the VM gas of the call, up to `gas_limit` |

Stored contract code is paid for by deploy gas, not by a storage deposit (a deposit for code is future work). State a contract writes locks the usual storage deposit from the **contract's** balance (fund a contract with a `Transfer` before calling it).

**Fee split.** Fees are not minted or burned; they move into two pool accounts at reserved addresses (`validator_pool()`, `relay_pool()`), which are ordinary balance entries:

| Source | Executor (validator) pool | Relay host pool |
| --- | --- | --- |
| User transaction (deploy, call, transfer) | 80% (the rounding remainder) | 20% (`RELAY_SHARE_BPS` = 2,000) |
| Cron run | 100% | none |

This implements the "80/20 proposed" mutation split of [TOKENOMICS.md](TOKENOMICS.md) as accounting only. The **validator pool is paid out at every epoch end** to the epoch's validators, pro rata by stake (see [Staking, epochs and slashing](#staking-epochs-and-slashing)). The relay pool still just accumulates (see the design note below).

**Design note: paying the relay pool (not implemented).** Nothing in a transaction says which hosting node relayed it, so the 20% cannot be attributed. Paying it needs (1) an optional `relayer: Address` field in the signed transaction body (the relay host that accepted it from the user and put it in the mempool; the sender signs it, so a proposer cannot steal the attribution), (2) at execution, crediting `fee * RELAY_SHARE_BPS / 10000` to that address instead of `relay_pool()` (falling back to the validator pool when absent or when the relayer is not a registered hosting node, so the fee is never burned), and (3) a hosting-node registry to check against, which belongs to the Phase 5 hosting-node item. Parallel mode stays valid only if relayer credits are applied after the block like the pool credits (they commute). This changes the transaction encoding and so needs a Design Proposal; until then `relay_pool()` is accounting only.

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
- `timestamp` is chosen by the proposer (Unix milliseconds). Execution checks only that it is not earlier than the parent's. Contracts never see it, and crons run by height, so it cannot change what a block does.
- `tx_root` is a hash over the transaction ids.
- `Block::encode` / `Block::decode` give the wire form.

`GenesisConfig` itself stays as it was; the epoch-0 validators go through `ChainState::from_genesis_with_validators(kv, &genesis, &[GenesisValidator { operator, consensus_key, network_addr, stake }])`. Each validator's stake is bonded out of its operator's allocation (which must hold it), so the allocations still sum to `TOTAL_SUPPLY`; the genesis hash commits to the validators (`GenesisConfig::hash_with_validators`; with none it equals `hash()`).

`execute_block` returns `Err` without changing state for: `BadHeight`, `BadParent`, `BadStateRoot`, `BadTxRoot`, `TimestampBeforeParent`, `TooManyTxs` (`max_block_txs`), `BlockGasLimit` (sum of `gas_limit`s above `block_gas_limit`), and `SupplyInvariant`.

## Execution order

1. Header checks.
2. The transactions, in block order, each seeing the effects of the earlier ones. `ExecMode::Sequential` runs them one at a time in one overlay (the reference); `ExecMode::Parallel` (the default) runs them speculatively on several threads with the same result, see [Parallel execution](#parallel-execution). One overlay holds the block's writes.
3. The due crons, as system transactions. Always sequential, after all transactions.
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

**Fee pools.** Every transaction would credit the validator and relay pool balances and so conflict with all others. Those credits commute, so in parallel mode each transaction only debits its sender, and the sums (computed from the receipts, same 80/20 split) are credited once after the transactions. This is invisible unless something reads or writes a pool balance (a transfer to a pool address, a contract asking for its balance): if any final execution does, the block is re-run sequentially.

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
- A fee market (a per-gas bid) and payout of the relay pool.
- Channel parameters (challenge length) in the genesis config, and batched settlement of many channels.
- Delegation (stake behind someone else's validator), un-jailing, reward weighting by proposals and uptime, and slashing for downtime.
- A storage deposit for contract code and for user-token records.
- Randomness beacon, price feeds and other oracle inputs to execution.
- Conversion of the CLI deploy bundle into a `Deploy` transaction.
