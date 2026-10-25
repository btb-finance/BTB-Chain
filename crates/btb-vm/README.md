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

`storage_read` copies at most `out_cap` bytes but returns the full length. Calls 2, 3 and 7 trap with `WriteInQuery` when `Host::read_only()` is true (queries). Calls 10, 12 and 13 also trap with `WriteInQuery`. Rows are opaque bytes (encoding comes later); `table_get` has `storage_read` copy semantics. Paginate `table_scan` by passing the last id + 1. Indexes are out of scope for now.

```
cargo test -p btb-vm
```
