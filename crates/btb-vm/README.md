# btb-vm

Deterministic, gas-metered RISC-V **RV64IM** interpreter for BTB Chain. No dependencies, no floats, no compressed instructions, no CSRs.

- `decode`: `decode(u32) -> Result<Instr, Trap>`
- `vm`: `Vm::new(mem_size, gas_limit)`, `step()`, `run() -> Result<Exit, Trap>`
- `gas`: per-instruction cost table, charged before execution

`Exit::Ecall` returns control to the host (call number in a7); call `run()` again to resume. `EBREAK` returns `Exit::Halt`. Traps: `IllegalInstruction`, `MisalignedFetch`, `MemoryOutOfBounds { addr }`, `OutOfGas`.

Gas: base 1, branch/jump 1, load 2, store 2, mul 3, div/rem 5.

```
cargo test -p btb-vm
```
