# Quickstart: run the chain, deploy a token, schedule a transfer

This page walks through running BTB Chain on your own machine: start a 4-validator network, get test BTB from the faucet, deploy the example token, call it, and add a cron that moves tokens on its own. Every command here was run end to end on a fresh checkout.

> This is a **local** network. It runs on `127.0.0.1`, keeps state in memory and is gone when you stop it. No public testnet is running yet; see [TESTNET.md](TESTNET.md) for the plan.

## 1. Requirements

- Linux or macOS with Git and `curl`
- Rust via [rustup](https://rustup.rs). The repository pins its toolchain in `rust-toolchain.toml`; `rustup` installs it, the `riscv64imac-unknown-none-elf` target and `rust-src` on first use.
- About 4 GB of free disk for the build

```bash
git clone https://github.com/btb-finance/BTB-Chain.git
cd BTB-Chain
```

## 2. Start the network

```bash
scripts/testnet-local.sh --release
```

This builds `btb-node` and `btb-faucet`, writes a genesis with 4 validator keys and a funded faucet key, and starts everything. When it prints `BTB local testnet is running`, open:

| What | Where |
| --- | --- |
| Explorer (blocks, transactions, accounts, validators) | `http://127.0.0.1:8545/explorer` |
| Faucet page | `http://127.0.0.1:8080/` |
| Node status (JSON) | `http://127.0.0.1:8545/status` |

Leave it running in its own terminal; Ctrl-C stops the chain. Useful options: `--validators N`, `--dir DIR` (where keys and logs go; default under `/tmp`), `--epoch-len N`, `--no-build`. Without `--release` it builds in debug mode, which is slower to run but faster to compile.

Check that blocks are being made (the height goes up about 4 times a second when idle):

```bash
curl -s http://127.0.0.1:8545/status | grep -o '"height":[0-9]*'
```

## 3. Get test BTB

```bash
curl -s http://127.0.0.1:8080/info                      # the faucet's address and balance
curl -s -X POST http://127.0.0.1:8080/drip -d '{"address":"<64 hex characters>"}'
```

A drip sends 100 BTB (limits: one per address per day, 10 per IP per day). The faucet's own key is in the network directory as `faucet.key`; the steps below use it as the developer account because it already holds the test supply.

## 4. Deploy the example token

Build the `btb` CLI, then deploy from inside the contract's directory:

```bash
cargo build -p btb-cli
export BTB=$PWD/target/debug/btb
export KEY=<network dir>/faucet.key      # printed by testnet-local.sh as "data and keys"
export RPC=http://127.0.0.1:8545
cd examples/token
$BTB deploy --rpc $RPC --key $KEY
```

It prints `deployed: contract <address>`. Keep that address:

```bash
export C=<contract address>
```

**Fund the contract.** A contract pays a deposit for the storage it uses, from its own balance. Send it some BTB first or the first write fails with `InsufficientDeposit`:

```bash
curl -s -X POST http://127.0.0.1:8080/drip -d "{\"address\":\"$C\"}"
```

## 5. Call it

The token's amounts are `u128`. The CLI's JSON arguments only cover 64-bit numbers, so pass `u128` values as hex of the canonical encoding: 16 bytes, little endian ([ENCODING.md](ENCODING.md)). This helper prints one:

```bash
u128() { python3 -c "print(($1).to_bytes(16,'little').hex())"; }
```

```bash
$BTB call  --rpc $RPC --key $KEY $C init $(u128 1000000)      # mint 1,000,000 to yourself (once)
$BTB query --rpc $RPC $C total_supply                           # 40420f00000000000000000000000000 = 1,000,000

BOB=$(printf '11%.0s' {1..32})                                  # any 32-byte address as 64 hex
$BTB call  --rpc $RPC --key $KEY $C transfer "$BOB$(u128 250000)"   # args: address then amount
$BTB query --rpc $RPC $C balance_of $BOB                        # 90d00300000000000000000000000000 = 250,000
```

- `call` sends a **mutation**: signed, ordered by consensus, costs gas (a transfer here used about 10,000 gas). It prints `output`, `gas_used` and the block `height`.
- `query` runs a read-only **query**: no signature, no fee, no transaction.
- A failing mutation is refused and still pays its gas: sending more than you hold gives `Abort("insufficient balance")`, calling `init` twice gives `Abort("already initialised")`.
- Outputs are hex of the canonical encoding (decode `u128` the same way: little endian).

The CLI looks up your nonce and the chain id for you; you never pass them.

## 6. Schedule a transfer with a cron

Crons are mutations the chain runs on its own on a schedule. Schedules are in **blocks**, not seconds, so every validator runs them at exactly the same point (`"5s"` is a compile error). At about 4 blocks per second on a local network, `every = "20 blocks"` is roughly every 5 seconds; on a real network a block takes longer.

Add this to `examples/token/src/main.rs` (or a copy of it):

```rust
const PLAN: &[u8] = b"plan";

/// Every 20 blocks, move `amount` from the caller to `to`. `amount = 0` cancels.
#[btb::mutation]
pub fn schedule(to: Address, amount: u128) {
    if amount == 0 {
        host::storage_remove(PLAN);
        return;
    }
    let mut v = [0u8; 80];
    v[..32].copy_from_slice(&host::caller().0);
    v[32..64].copy_from_slice(&to.0);
    v[64..].copy_from_slice(&amount.to_le_bytes());
    host::storage_set(PLAN, &v);
}

/// Run by the chain every 20 blocks; skips the round when the payer is short.
#[btb::cron(every = "20 blocks")]
pub fn auto_pay() {
    let Some(p) = host::storage_get(PLAN) else { return };
    let (mut a, mut n) = ([0u8; 32], [0u8; 16]);
    a.copy_from_slice(&p[..32]);
    let from = Address(a);
    a.copy_from_slice(&p[32..64]);
    let to = Address(a);
    n.copy_from_slice(&p[64..80]);
    let amount = u128::from_le_bytes(n);
    let Some(rest) = balance(&from).checked_sub(amount) else { return };
    let Some(new_to) = balance(&to).checked_add(amount) else { return };
    store(&balance_key(&from), rest);
    store(&balance_key(&to), new_to);
}
```

Deploy it again (a new address), fund it, `init`, then schedule 1,000 tokens to Bob and watch his balance grow without any more transactions:

```bash
$BTB call --rpc $RPC --key $KEY $C schedule "$BOB$(u128 1000)"
watch -n 5 "$BTB query --rpc $RPC $C balance_of $BOB"
```

Measured on a local network: Bob's balance went 0, 1,000, 2,000, 3,000, 4,000 at 5-second intervals. The cron's gas is paid from the contract's balance, which is another reason to fund it. This example keeps one plan per contract; a real one would keep one per user in a `#[btb::table]` ([LANGUAGE.md](LANGUAGE.md)).

## 7. What else to try

- **Hot reload without a chain:** `btb dev` in a contract directory runs a single-node dev server with live queries ([DEV_SERVER.md](DEV_SERVER.md)); `btb init my-app` scaffolds a project ([CLI.md](CLI.md)).
- **A web front end:** `examples/todo` and `@btb/client` ([TS_SDK.md](TS_SDK.md)).
- **The built-in DEX page:** `http://127.0.0.1:8545/dex`.
- **Serving reads to the public:** run `btb-host` nodes instead of exposing a validator's RPC ([HOSTING.md](HOSTING.md)). The validator RPC binds to loopback only and has no authentication.

## Throughput on one machine

Flooding the local release-mode network (4 validators and the load generator sharing 4 CPU cores) with 36,000 signed transfers from 600 accounts: all were included, about 2,500 transfers per second end to end, with validators near 10% CPU, so the load generator was the limit. Blocks arrived about 3.7 per second under load. A block carries up to 25 MiB of payload and at most 10,000 transactions (`max_block_txs`); a plain transfer is about 160 bytes. A network spread over real distances will make blocks more slowly.

## Troubleshooting

| Symptom | Cause |
| --- | --- |
| `InsufficientDeposit { needed, available: 0 }` | the contract has no balance for its storage deposit: send it BTB (step 4) |
| `btb: bad hex` | an argument has an odd number of hex digits; a `u128` is exactly 32 hex digits |
| JSON number argument gives a wrong result | JSON integers are encoded as `u64`; use hex for `u128` |
| `Abort("...")` in a failed receipt | the contract refused the call; the message is the contract's |
| Port already in use | an earlier network is still running: `pkill -f btb-node; pkill -f btb-faucet` |
| The chain is empty after a restart | state is in memory; build `btb-node` with `--features rocksdb` to keep it ([NODE.md](NODE.md#crash-safety)) |
