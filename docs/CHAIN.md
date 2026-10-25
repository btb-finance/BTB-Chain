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
evidence = 0 || voter:u32 || round:u64 || (id[32] || sig[64]) x2        (DoubleVote)
         | 1 || proposer:u32 || round:u64 || (id[32] || sig[64]) x2     (DoubleProposal)
manifest = name:string || crons:[(function:string, every_blocks:u64)] || indexes:[string]
```

- **Call kinds.** `kind` is 0 query, 1 mutation, 2 cron, 3 action. All four decode, but only **mutation** is valid in a transaction; query, cron and action are rejected as `Invalid::KindNotAllowed`. Queries never go through consensus (run them with `Executor::query` against the head state), actions are not transactions, and crons are scheduled-only: users cannot invoke them by hand, so a cron (for example a payout) runs only on its schedule, with `caller() == contract_address()`.
- **Signature.** Ed25519 (`ed25519-dalek`, strict verification) over `SHA256("btb.tx.sig.v1" || body)`. The body contains the chain id and nonce, so a signature is valid for one chain and one nonce only.
- **Transaction id.** `SHA256("btb.tx.id.v1" || wire bytes)`.
- **Manifest.** `crons` lists `(function, every_blocks)`: the `cron` entry point `function` runs every `every_blocks` blocks, `every = "N blocks"` (1 to 2^32; at most 16). `indexes` are table index declarations in the same temporary form as `btb dev --index` (`Table:index:addr@offset` or `Table:index:u64@offset`, see [DEV_SERVER.md](DEV_SERVER.md)); the chain registers them from state, so a restarted node rebuilds them. This is a binary struct, not the CLI's `manifest.json`; converting a deploy bundle to a `Deploy` transaction (and adding cron schedules to the bundle) is for the CLI to do when `btb deploy` targets a chain.
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
| SubmitEvidence | `EVIDENCE_GAS` 100,000 (two signature checks; charged also when the evidence is refused) |
| Call | `CALL_BASE_GAS` 2,000 + `CALL_BYTE_GAS` 2 per byte of function name and arguments, plus the VM gas of the call, up to `gas_limit` |

Stored contract code is paid for by deploy gas, not by a storage deposit (a deposit for code is future work). State a contract writes locks the usual storage deposit from the **contract's** balance (fund a contract with a `Transfer` before calling it).

**Fee split.** Fees are not minted or burned; they move into two pool accounts at reserved addresses (`validator_pool()`, `relay_pool()`), which are ordinary balance entries:

| Source | Executor (validator) pool | Relay host pool |
| --- | --- | --- |
| User transaction (deploy, call, transfer) | 80% (the rounding remainder) | 20% (`RELAY_SHARE_BPS` = 2,000) |
| Cron run | 100% | none |

This implements the "80/20 proposed" mutation split of [TOKENOMICS.md](TOKENOMICS.md) as accounting only. The **validator pool is paid out at every epoch end** to the epoch's validators, pro rata by stake (see [Staking, epochs and slashing](#staking-epochs-and-slashing)). The relay pool still just accumulates: who the relay hosts are belongs to Phase 5.

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

## Genesis and the supply invariant

`GenesisConfig { config, timestamp, allocations }` lists every balance. The allocations must sum to **exactly** `TOTAL_SUPPLY` with no address twice, or `from_genesis` fails. This is the only place BTB is created. The final allocation (reserve pool, grants, vesting) is still open in TOKENOMICS.md; the config just takes whatever list is decided.

After every block the executor sums all balances (accounts, contracts, the fee pools and the slash vault), all storage deposits, all bonded stake and all unbonding stake, and requires the sum to equal `TOTAL_SUPPLY`. If not, `execute_block` returns `BlockError::SupplyInvariant` and writes nothing. Nothing in the crate mints: transfers, fees, deposits, staking, rewards and slashing only move existing units. User tokens (`Token.create`, host calls 16 to 23) are separate and are not part of this sum. The check scans all balances, O(accounts) per block; `Executor::check_supply` can switch it off for benchmarks.

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
2. The transactions, one at a time, in block order, each seeing the effects of the earlier ones (parallel execution is a later step). One overlay holds the block's writes.
3. The due crons, as system transactions.
4. The new head, then the supply check, then the Merkle root, then one atomic commit.

Contract code runs in `btb-vm` with the call's gas limit. A reverted call's writes (storage, tables, deposit moves, token moves) are dropped as a whole.

## Cron jobs

A deploy schedules each manifest cron for height `deploy_height + every`. At each block, after the transactions, the executor runs the crons that are due (due height at most the current height), ordered by (due height, contract address, function name):

- The cron runs with `caller() == contract_address()`, `cron_gas_limit` gas and the contract's balance as its payer. The fee goes 100% to the validator pool.
- The next run is scheduled for `current_height + every` (a late cron does not catch up in a burst).
- **Paused.** If the contract's balance is below `cron_gas_limit * gas_price` nothing runs and the receipt is `Status::Paused`; the schedule continues, so funding the contract resumes it.
- **Per-block cap.** A cron starts only if the cron gas already used in the block plus `cron_gas_limit` fits `cron_gas_block_cap`, and at most `max_crons_per_block` are considered. The rest stay due and run first in the next block.
- A failing cron reverts its changes and still pays for the gas it used.

Cron receipts follow the transaction receipts; their id is `SHA256("btb.cron.v1" || height || contract || function)`.

## Mempool

`Mempool` holds pending transactions. Admission rejects: wrong chain id, query, cron or action kinds, bad signatures, oversize transactions, duplicates, nonces already used, a balance that cannot cover the transaction, a full pool or a full sender slot. A second transaction for the same sender and nonce replaces the first only if it reserves a higher fee. `select` returns transactions for the next block: highest fee first (fee = `gas_limit * gas_price`, fixed for deploys and transfers), ties broken by transaction id, but each sender's transactions strictly in nonce order starting at its next nonce, within a transaction and gas budget. `remove` and `prune` clean up after a block.

## Queries

`Executor::query(state, contract, function, args, caller)` runs a `query` entry point read-only against the head state and returns the output and gas used. Nothing is written, no consensus is involved, and the result can be served by any node that has the state.

## Determinism

No floats, clocks, threads or hash-map iteration order reach state: balances and keys are integers and ordered maps, crons are scheduled by height, and the contract VM is rv64im only. The tests run the same block sequence on two fresh states and require identical receipts, roots and stored bytes, and (with `--features rocksdb`) the same results on RocksDB, including after a reopen.

## Not done yet

- Parallel (Block-STM style) execution and the fast path.
- A fee market (a per-gas bid) and payout of the relay pool.
- Delegation (stake behind someone else's validator), un-jailing, reward weighting by proposals and uptime, and slashing for downtime.
- A storage deposit for contract code and for user-token records.
- Randomness beacon, price feeds and other oracle inputs to execution.
- Conversion of the CLI deploy bundle into a `Deploy` transaction.
