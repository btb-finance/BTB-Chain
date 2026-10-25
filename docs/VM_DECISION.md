# VM Decision: PolkaVM, CKB-VM, or our own `btb-vm`

Status: **Recommendation, pending review** (closes the Phase 1 ROADMAP item "Decide: build on an existing RISC-V VM or write our own").

## Question

BTB contracts run on a metered RISC-V VM. Do we build on an existing VM (PolkaVM, CKB-VM) or keep writing our own?

## What we need

From [AGENTS.md](../AGENTS.md) and [ARCHITECTURE.md](ARCHITECTURE.md):

- **Determinism:** integer-only, no floats, no system time, bit-identical results on every machine.
- **Exact gas:** every instruction and host call costs gas; gas is charged before execution; the same gas on every machine (Phase 1 gate).
- **Permissive license** (MIT / Apache-2.0 / BSD).
- **Host-call ABI we control:** tables, storage, tokens, context, with our own gas costs.
- **Speed later:** a recompiler or JIT for near-native performance (Phase 4+), without changing gas results.
- **Small, auditable core:** the VM is the most security-sensitive component.

## Options

### A. Own interpreter (`crates/btb-vm`, today)

RV64IM only (no C, no CSRs, no floats). About 2.4k lines including tests and ELF loader, about 1k in decode and `vm.rs`, no dependencies. Gas is charged before each instruction from a fixed table (base 1, load/store 2, mul 3, div/rem 5), host-call gas is documented per call, ELF loading and the entry-point convention are done, and queries are forced read-only. Plain interpreter: slow compared with native, but simple to audit.

### B. PolkaVM

Parity's RISC-V-derived VM. It does **not** run raw RISC-V ELF: a linker (`polkatool`) converts RV32/RV64 ELF into its own PolkaVM program blob with its own bytecode and import/export tables. Has a Linux x86-64 recompiler (single-pass, linear time) plus an interpreter fallback. Gas metering exists in two modes: synchronous (exact, slower) and asynchronous (checked periodically, faster; the exact interruption point is not deterministic, though the out-of-gas outcome is). Origin of the JAM PVM spec. Reported as MIT / Apache-2.0 (verify before adoption).

### C. CKB-VM

Nervos's VM, MIT licensed, many releases over several years, used in production on CKB mainnet. Executes standard RV64IMC plus the B extension (Zba, Zbb, Zbc, Zbs). Two modes: a Rust interpreter (development) and a hand-written assembly interpreter (production, x86-64/aarch64). Per-instruction cycle costs follow real hardware, with macro-op fusion in newer VM versions. Syscalls are the host interface. No JIT/recompiler. `no_std` support not confirmed by this research.

## Comparison

| Criterion | Own `btb-vm` | PolkaVM | CKB-VM |
| --- | --- | --- | --- |
| License | ours (workspace license) | MIT / Apache-2.0 (verify) | MIT |
| ISA | rv64im | own RISC-V-derived bytecode (from rv32/rv64 ELF via linker) | rv64imc + B |
| Standard rustc/LLVM ELF runs directly | yes | no, needs `polkatool` link step | yes |
| Gas model | per instruction, before execution, exact | sync (exact) or async (outcome-deterministic, trap point not) | cycles per instruction, model fixed per VM version |
| Gas fully under our control | yes | partly (basic-block costs defined by the VM) | partly (cycle table is theirs) |
| Recompiler / JIT | none yet | yes (x86-64 Linux), interpreter fallback | none (asm interpreter instead) |
| Speed | slowest | near native | fast interpreter |
| Determinism review burden | ours, small | large, recompiler and its sandbox | medium, asm interpreter |
| Dependencies | none | several, plus platform-specific code | several |
| `no_std` | n/a (std only so far) | partial, verify | unconfirmed |
| Maturity | new, tested only by us | newer, tied to the Polkadot/JAM roadmap; earlier READMEs called it not production-ready | most battle-tested of the two |
| Maintenance risk | ours | Parity-driven; bytecode and API have changed between versions | Nervos-driven; VM versions are consensus-critical there |
| Host-call fit | exactly our ABI | imports, adaptable | syscalls, adaptable |
| Fit with "any language to RISC-V" | direct | indirect (link step) | direct |

## Risks

**Adopting an external VM**

- Gas semantics become theirs. Our "exact, repeatable gas" gate depends on a cost model we did not design and that can change between releases (CKB versions its VM; PolkaVM changed its bytecode and metering).
- Chain consensus would depend on upstream bug fixes and release timing. A consensus-critical divergence in a dependency is a chain split.
- PolkaVM's recompiler is a large, platform-specific attack surface (JIT-style codegen, sandboxing). Strong for speed, hard to audit, and a risk for validators running mixed hardware.
- PolkaVM's own bytecode adds a build step and a second artifact format; CKB-VM's C extension and B instructions widen what our determinism review must cover.
- License and dependency tree must be rechecked per release (AGENTS.md: permissive licenses only).

**Staying on our own VM**

- Interpreter speed will not be enough for heavy contracts; we owe a recompiler.
- We carry all security review and fuzzing ourselves.
- We risk reimplementing known pitfalls (memory model, trap ordering, metering granularity under a recompiler).

## Recommendation

**Keep our own interpreter for Phase 1 and the single-node phases.** Gas-before-execute with a table we own is exactly the Phase 1 gate; the code is small, dependency-free, deterministic by construction, and already carries the host-call ABI, entry-point convention and read-only enforcement. Neither external VM removes meaningful Phase 1 work, and both would put gas semantics and consensus risk in someone else's hands. Treat the recompiler as a Phase 4 decision, taken with benchmark data, and design it as an optimization that must reproduce the interpreter's gas and traps exactly (the interpreter stays the reference and the differential-test oracle).

PolkaVM and CKB-VM remain valuable as references: CKB-VM for its cycle-cost calibration and macro-op fusion; PolkaVM for single-pass recompilation and basic-block gas metering.

### What would change this

- Benchmarks show the interpreter, even optimized, cannot meet Phase 3 devnet latency and we lack capacity to build a recompiler: reconsider PolkaVM's recompiler (or CKB-VM's asm interpreter).
- An upstream VM offers a **pinned, versioned, documented** gas model we can adopt as ours, with a permissive license verified and `no_std`/determinism claims confirmed.
- Security audit capacity is too thin to own the VM: favor the more battle-tested option (CKB-VM).
- Compressed instructions or B extension become important for contract size or ZK proving: revisit ISA scope (the choice is ours either way).

## Next steps

1. Record the decision: tick the ROADMAP item (done with this document).
2. Add a benchmark harness in `crates/btb-vm` (instructions per second on hash loop, sort, memory-heavy and host-call-heavy workloads), and record the baseline.
3. Cheap interpreter wins before any JIT: pre-decoded instruction cache, fused dispatch loop, per-basic-block gas charging that preserves exact totals and out-of-gas trap points.
4. Add differential and fuzz tests (random programs; gas and trap equality across machines and, later, across execution engines). Add failure-case tests per AGENTS.md security rules.
5. Spike (time-boxed, not merged): run the same contracts under CKB-VM and PolkaVM to compare speed and gas shape, and verify the unconfirmed items: PolkaVM license and `no_std` status, CKB-VM `no_std`, current maintenance activity.
6. Phase 4 design proposal for a recompiler: basic-block gas, identical traps, W^X-safe codegen, per-platform fallback to the interpreter, interpreter as oracle.
7. Calibrate the gas table against measured cost per instruction class, then freeze it as versioned (VM version number in the chain spec).

## Sources

- PolkaVM design goals and discussion: https://github.com/koute/polkavm, https://forum.polkadot.network/t/announcing-polkavm-a-new-risc-v-based-vm-for-smart-contracts-and-possibly-more/3811/33, https://jam-docs.onrender.com/basics/pvm
- CKB-VM: https://docs.nervos.org/docs/script/vm-version, https://docs.nervos.org/docs/script/vm-cycle-limits, https://docs.rs/crate/ckb-vm/latest, https://github.com/nervosnetwork/rfcs/blob/master/rfcs/0003-ckb-vm/0003-ckb-vm.md

Search coverage was limited; claims marked "verify" or "unconfirmed" were not checked against the upstream repositories.
