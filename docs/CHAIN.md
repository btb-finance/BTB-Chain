# Chain execution layer

`crates/btb-chain` is the deterministic execution layer a consensus engine drives: transactions, genesis, blocks, fees, crons and a mempool, on top of `btb-vm` (contracts), `btb-runtime` (state, Merkle root) and `btb-codec` (encoding). It has no networking, no clock and no randomness. Consensus decides the order of blocks; this crate decides what a block does.

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

Three kinds, all encoded with `btb-codec` (little-endian, length-prefixed):

```
tx       = body || public_key[32] || signature[64]
body     = chain_id:u64 || nonce:u64 || tag:u8 || fields
Deploy   (0): code:bytes || manifest
Call     (1): contract[32] || kind:u8 || function:string || args:bytes || gas_limit:u64 || max_fee:u64
Transfer (2): to[32] || amount:u64
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
| Call | `CALL_BASE_GAS` 2,000 + `CALL_BYTE_GAS` 2 per byte of function name and arguments, plus the VM gas of the call, up to `gas_limit` |

Stored contract code is paid for by deploy gas, not by a storage deposit (a deposit for code is future work). State a contract writes locks the usual storage deposit from the **contract's** balance (fund a contract with a `Transfer` before calling it).

**Fee split.** Fees are not minted or burned; they move into two pool accounts at reserved addresses (`validator_pool()`, `relay_pool()`), which are ordinary balance entries:

| Source | Executor (validator) pool | Relay host pool |
| --- | --- | --- |
| User transaction (deploy, call, transfer) | 80% (the rounding remainder) | 20% (`RELAY_SHARE_BPS` = 2,000) |
| Cron run | 100% | none |

This implements the "80/20 proposed" mutation split of [TOKENOMICS.md](TOKENOMICS.md) as accounting only. Nothing pays out of the pools yet: who the executor and the relay host are, and how a pool is distributed (epochs, stake weights), belongs to the validator-set work of Phase 4 and 5. The pools just accumulate.

## Genesis and the supply invariant

`GenesisConfig { config, timestamp, allocations }` lists every balance. The allocations must sum to **exactly** `TOTAL_SUPPLY` with no address twice, or `from_genesis` fails. This is the only place BTB is created. The final allocation (reserve pool, grants, vesting) is still open in TOKENOMICS.md; the config just takes whatever list is decided.

After every block the executor sums all balances (accounts, contracts, both fee pools) and all storage deposits and requires `balances + deposits == TOTAL_SUPPLY`. If not, `execute_block` returns `BlockError::SupplyInvariant` and writes nothing. Nothing in the crate mints: transfers, fees and deposits only move existing units. User tokens (`Token.create`, host calls 16 to 23) are separate and are not part of this sum. The check scans all balances, O(accounts) per block; `Executor::check_supply` can switch it off for benchmarks.

## Blocks

```
Header { height, parent_hash, timestamp, state_root, tx_root }
hash   = SHA256("btb.block.v1" || height || parent_hash || timestamp || state_root || tx_root)   (big-endian ints)
```

- `state_root` is the Merkle root **before** the block (what the previous block produced), so a proposer builds a block without executing it and each block commits to the exact state it extends. The root after the block is returned by `execute_block` and goes into the next header. Genesis is version 0; block `h` is version `h`.
- `timestamp` is chosen by the proposer (Unix milliseconds). Execution checks only that it is not earlier than the parent's. Contracts never see it, and crons run by height, so it cannot change what a block does.
- `tx_root` is a hash over the transaction ids.
- `Block::encode` / `Block::decode` give the wire form.

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
- A fee market (a per-gas bid), payout of the fee pools, staking and slashing.
- A storage deposit for contract code and for user-token records.
- Randomness beacon, price feeds and other oracle inputs to execution.
- Conversion of the CLI deploy bundle into a `Deploy` transaction.
