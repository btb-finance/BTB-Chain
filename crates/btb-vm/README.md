# btb-vm

Deterministic, gas-metered RISC-V **RV64IM** interpreter for BTB Chain. No dependencies, no floats, no compressed instructions, no CSRs.

- `decode`: `decode(u32) -> Result<Instr, Trap>`
- `vm`: `Vm::new(mem_size, gas_limit)`, `step()`, `run() -> Result<Exit, Trap>`
- `elf`: `elf::load(&mut vm, bytes)` loads an ELF64 RISC-V ET_EXEC (PT_LOAD segments, zero-filled bss, pc = entry, sp = top of memory & !15) and returns exported global `STT_FUNC` symbols (name -> address) as entry points; typed `ElfError`, no panics, VM untouched on error
- `contract`: entry-point convention. `Contract::from_elf(bytes)` validates the ELF and lists entry points from exported symbols `btb_query_<name>`, `btb_mutation_<name>`, `btb_cron_<name>`, `btb_action_<name>` (name `[a-z0-9_]`, 1..=64; duplicates per kind, bad names and zero entry points are rejected). `Contract::call(kind, name, input, host, gas_limit, mem_size)` runs one entry in a fresh VM: `a0` = input ptr, `a1` = input len (input sits above the stack, below the top of memory), `ra` = an `ebreak` trampoline so `ret` halts cleanly. Queries are forced read-only (`WriteInQuery`). Fails with `OutOfMemory` if less than 64 KiB of stack fits between the loaded image and the input. Returns the `return_data` bytes (empty on plain return) and gas used; traps carry gas used. Randomness is not implemented yet
- `gas`: per-instruction cost table, charged before execution; host-call costs documented there too
- `host`: host-call ABI. `Host` trait (implemented by the embedder), `abi` call numbers, and `run_with_host(&mut Vm, &mut impl Host)`, which serves ECALLs until halt, return or trap
- `testing`: `MemHost`, an in-memory `Host` (BTreeMap storage, tables, balances), and `build_elf` (tiny ELF builder) for tests

`Exit::Ecall` returns control to the host (call number in a7); call `run()` again to resume. `EBREAK` returns `Exit::Halt`. Traps: `IllegalInstruction`, `MisalignedFetch`, `MemoryOutOfBounds { addr }`, `OutOfGas`, `UnknownHostCall(n)`, `WriteInQuery`, `Abort(msg)`.

Gas: base 1, branch/jump 1, load 2, store 2, mul 3, div/rem 5.

## Host calls

Number in a7, args in a0..a5, result in a0. Gas (on top of the 1-gas ECALL) is charged before the call; a failed charge means no effect.

| # | Call | Gas |
| - | ---- | --- |
| 1 | `storage_read(key_ptr, key_len, out_ptr, out_cap) -> len \| u64::MAX` | 200 + 1/key byte + 1/byte copied |
| 2 | `storage_write(key_ptr, key_len, val_ptr, val_len)` | 500 + 4/(key+val) byte |
| 3 | `storage_remove(key_ptr, key_len)` | 300 + 1/key byte |
| 4 | `caller(out_ptr)` (32 bytes) | 10 |
| 5 | `block_height() -> u64` | 5 |
| 6 | `token_balance(addr_ptr) -> u64` | 50 |
| 7 | `token_transfer(to_ptr, amount) -> 0 ok \| 1 insufficient` | 300 |
| 8 | `return_data(ptr, len)` ends with `Exit::Return` | 10 + 1/byte |
| 9 | `abort(ptr, len)` traps `Abort(msg)` | 10 + 1/byte |
| 10 | `table_insert(table_ptr, table_len, row_ptr, row_len) -> row id` (monotonic per table, never reused) | 600 + 4/(table+row) byte |
| 11 | `table_get(table_ptr, table_len, id, out_ptr, out_cap) -> len \| u64::MAX` | 250 + 1/table byte + 1/byte copied |
| 12 | `table_update(table_ptr, table_len, id, row_ptr, row_len) -> 0 ok \| 1 missing` | 500 + 4/(table+row) byte |
| 13 | `table_delete(table_ptr, table_len, id) -> 0 ok \| 1 missing` | 300 + 1/table byte |
| 14 | `table_scan(table_ptr, table_len, start_id, out_ptr, out_cap) -> count` (up to out_cap/8 ascending u64 LE ids >= start_id) | 250 + 1/table byte + 10/id returned |
| 15 | `table_index_scan(req_ptr, req_len, out_ptr, out_cap) -> total_len` (read-only; allowed in queries; request and result layout in `abi::TABLE_INDEX_SCAN`) | 400 + 1/request byte + 40/id returned + 1/cursor byte returned |
| 16 | `token_create(req_ptr, req_len, out_ptr) -> status` (user tokens; mutations only; `req` is the `btb-codec` tuple `(name, symbol, decimals: u8, initial_supply: u128, mint_authority: Option<Address>)`; writes the 32-byte token id) | 2000 + 1/request byte |
| 17 | `token_transfer_of(id_ptr, to_ptr, amount_ptr) -> status` (mutations only; `amount_ptr` = 16 bytes `u128` LE; moves from the running contract) | 400 |
| 18 | `token_balance_of(id_ptr, holder_ptr, out_ptr) -> status` (read-only; writes `u128` LE) | 100 |
| 19 | `token_info(id_ptr, out_ptr, out_cap) -> len \| u64::MAX` (read-only; `btb-codec` tuple `(name, symbol, decimals, supply, mint_authority, creator)`) | 150 + 1/byte copied |
| 20 | `token_holders(req_ptr, req_len, out_ptr, out_cap) -> total_len \| u64::MAX` (read-only; request `id[32] \|\| limit:u32 LE \|\| cursor`, holders ascending by address, limit capped at 256; layout in `abi::TOKEN_HOLDERS`) | 300 + 1/request byte + 30/holder returned + 1/cursor byte returned |
| 21 | `token_mint(id_ptr, to_ptr, amount_ptr) -> status` (mutations only; mint authority only) | 500 |
| 22 | `token_burn(id_ptr, amount_ptr) -> status` (mutations only; mint authority only; burns the contract's own balance) | 400 |
| 23 | `contract_address(out_ptr)` (32 bytes) | 10 |
| 40 | `random_bytes(out_ptr, len) -> 0` (at most 1024 bytes from the beacon; only in a mutation or cron marked `randomness`, else `Trap::RandomnessForbidden`; `Trap::RandomnessUnavailable` without a beacon) | 300 + 2/byte requested, charged first |
| 41 | `price(pair_id, out_ptr) -> 0 \| u64::MAX` (32 bytes: `price: u128 LE`, `height: u64 LE`, `decimals: u8`, 7 zero bytes; read-only) | 200 in a query; 10,200 in a mutation or cron (200 + a 10,000 per-read fee) |

`storage_read` copies at most `out_cap` bytes but returns the full length. Calls 2, 3, 7, 16, 17, 21 and 22 trap with `WriteInQuery` when `Host::read_only()` is true (queries). Calls 10, 12 and 13 also trap with `WriteInQuery`. `Vm::set_randomness_allowed` is the single gate of call 40: `Contract::call` turns it on only for a mutation or cron whose ELF exports `btb_randomness_<kind>_<name>`; see `tests/oracle.rs`. Rows are opaque bytes (encoding comes later); `table_get` has `storage_read` copy semantics. Paginate `table_scan` by passing the last id + 1. Call 15 reads secondary indexes; hosts without index storage (`MemHost`) return no results for it (`MemHost` has no schemas), and `btb-runtime` implements it.

```
cargo test -p btb-vm
```
