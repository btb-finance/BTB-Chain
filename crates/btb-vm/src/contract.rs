//! Contract entry-point convention.
//!
//! A contract is an ELF whose exported global function symbols are named
//! `btb_<kind>_<name>` with kind one of `query`, `mutation`, `cron`, `action` and name made of
//! `[a-z0-9_]`, 1..=64 bytes. Other exported symbols are ignored.
//!
//! Calling convention: `a0` = pointer to the input bytes, `a1` = input length, `ra` = a
//! trampoline holding `ebreak`, so a plain `ret` halts cleanly. Output is whatever the guest
//! passes to `return_data` (empty on plain return). Memory layout, from the top down: stack
//! (grows down from `sp`), then the input region (16-byte trampoline + input), both below
//! `mem_size & !15`. The call fails with `OutOfMemory` unless at least `MIN_STACK` bytes remain between the loaded image and the input region.
//!
//! Randomness: `random_bytes` (host call 40) is available only to mutation and cron entry points
//! marked `randomness`. The marker is an exported symbol `btb_randomness_<kind>_<name>` (kind
//! `mutation` or `cron`) that `#[btb::mutation(randomness)]` and
//! `#[btb::cron(randomness, every = "N blocks")]` emit; it is read from the ELF on every call, so
//! it cannot be bypassed by the manifest or by the caller. A marker on a query or action, or one
//! without a matching entry point, makes the contract invalid at deploy time. At run time a
//! `random_bytes` call anywhere else traps with `Trap::RandomnessForbidden`. There are no
//! cross-contract calls, so a marked function can never be reached from another contract's
//! code; if they are added, the callee must run with the flag off.

use crate::decode::Trap;
use crate::elf::{self, ElfError};
use crate::host::{run_with_host, Address, Host};
use crate::vm::{Exit, Vm};

const PREFIX: &str = "btb_";
const RANDOMNESS_PREFIX: &str = "btb_randomness_";
const MAX_NAME: usize = 64;
const EBREAK: u32 = 0x0010_0073;
const TRAMPOLINE: u64 = 16;
/// Minimum stack that must fit between the loaded image and the input region.
pub const MIN_STACK: u64 = 64 * 1024;

/// The four function kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FunctionKind {
    Query,
    Mutation,
    Cron,
    Action,
}

impl FunctionKind {
    pub const ALL: [FunctionKind; 4] = [Self::Query, Self::Mutation, Self::Cron, Self::Action];

    /// Symbol prefix, e.g. `btb_query_`.
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Query => "btb_query_",
            Self::Mutation => "btb_mutation_",
            Self::Cron => "btb_cron_",
            Self::Action => "btb_action_",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryPoint {
    pub kind: FunctionKind,
    pub name: String,
    pub addr: u64,
    /// Marked `randomness`: may call `random_bytes` (mutations and crons only).
    pub randomness: bool,
}

/// Why a contract was rejected by `from_elf`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    Elf(ElfError),
    /// Entry-point name is empty, longer than 64 bytes or has characters outside `[a-z0-9_]`.
    InvalidName(String),
    /// The same name is exported twice for one kind.
    DuplicateEntry {
        kind: FunctionKind,
        name: String,
    },
    NoEntryPoints,
    /// A `btb_randomness_*` marker that is malformed, names a query or action, or has no
    /// matching mutation or cron entry point.
    BadRandomnessMarker(String),
}

impl From<ElfError> for ContractError {
    fn from(e: ElfError) -> Self {
        Self::Elf(e)
    }
}

/// Why a call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    Elf(ElfError),
    NoSuchEntry {
        kind: FunctionKind,
        name: String,
    },
    /// Input plus trampoline does not fit in VM memory.
    InputTooLarge,
    /// Image, minimum stack (`MIN_STACK`) and input region do not fit in memory together.
    OutOfMemory,
    /// The guest trapped (including `OutOfGas` and `WriteInQuery`); gas used up to the trap.
    Trap {
        trap: Trap,
        gas_used: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallOutcome {
    /// Bytes passed to `return_data`; empty on plain return.
    pub output: Vec<u8>,
    pub gas_used: u64,
}

/// A validated contract binary and its entry points.
#[derive(Debug, Clone)]
pub struct Contract {
    elf: Vec<u8>,
    entries: Vec<EntryPoint>,
}

fn valid_name(n: &str) -> bool {
    (1..=MAX_NAME).contains(&n.len())
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl Contract {
    /// Validate the ELF headers and symbols and collect entry points (sorted by kind, name).
    /// Segment placement is checked against the memory size at call time.
    pub fn from_elf(bytes: &[u8]) -> Result<Self, ContractError> {
        let mut entries: Vec<EntryPoint> = Vec::new();
        let mut markers: Vec<(FunctionKind, String)> = Vec::new();
        for (sym, addr) in elf::export_list(bytes)? {
            if !sym.starts_with(PREFIX) {
                continue;
            }
            if let Some(rest) = sym.strip_prefix(RANDOMNESS_PREFIX) {
                let bad = || ContractError::BadRandomnessMarker(sym.clone());
                let (kind, name) = [FunctionKind::Mutation, FunctionKind::Cron]
                    .iter()
                    .find_map(|k| {
                        rest.strip_prefix(&k.prefix()["btb_".len()..])
                            .map(|n| (*k, n))
                    })
                    .ok_or_else(bad)?;
                if !valid_name(name) || markers.iter().any(|(k, n)| *k == kind && n == name) {
                    return Err(bad());
                }
                markers.push((kind, name.to_string()));
                continue;
            }
            let Some((kind, name)) = FunctionKind::ALL
                .iter()
                .find_map(|k| sym.strip_prefix(k.prefix()).map(|n| (*k, n)))
            else {
                continue;
            };
            if !valid_name(name) {
                return Err(ContractError::InvalidName(name.to_string()));
            }
            if entries.iter().any(|e| e.kind == kind && e.name == name) {
                return Err(ContractError::DuplicateEntry {
                    kind,
                    name: name.to_string(),
                });
            }
            entries.push(EntryPoint {
                kind,
                name: name.to_string(),
                addr,
                randomness: false,
            });
        }
        for (kind, name) in markers {
            match entries
                .iter_mut()
                .find(|e| e.kind == kind && e.name == name)
            {
                Some(e) => e.randomness = true,
                None => {
                    return Err(ContractError::BadRandomnessMarker(format!(
                        "{RANDOMNESS_PREFIX}{}{name}",
                        &kind.prefix()["btb_".len()..]
                    )))
                }
            }
        }
        if entries.is_empty() {
            return Err(ContractError::NoEntryPoints);
        }
        entries.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
        Ok(Contract {
            elf: bytes.to_vec(),
            entries,
        })
    }

    pub fn entry_points(&self) -> &[EntryPoint] {
        &self.entries
    }

    /// Whether the entry point `(kind, name)` exists and is marked `randomness`.
    pub fn is_randomness(&self, kind: FunctionKind, name: &str) -> bool {
        self.entries
            .iter()
            .any(|e| e.kind == kind && e.name == name && e.randomness)
    }

    /// Run one entry point in a fresh VM. Queries always run read-only, whatever `host` says.
    pub fn call(
        &self,
        kind: FunctionKind,
        name: &str,
        input: &[u8],
        host: &mut impl Host,
        gas_limit: u64,
        mem_size: usize,
    ) -> Result<CallOutcome, CallError> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.kind == kind && e.name == name)
            .ok_or_else(|| CallError::NoSuchEntry {
                kind,
                name: name.to_string(),
            })?;
        let mut vm = Vm::new(mem_size, gas_limit);
        let loaded = elf::load(&mut vm, &self.elf).map_err(CallError::Elf)?;

        // [base .. base+16) trampoline, [base+16 .. ) input, then the stack below `top`.
        let top = (mem_size as u64) & !15;
        let input_len = input.len() as u64;
        let region = TRAMPOLINE
            .checked_add(input_len.checked_add(15).ok_or(CallError::InputTooLarge)? & !15)
            .ok_or(CallError::InputTooLarge)?;
        let base = top.checked_sub(region).ok_or(CallError::InputTooLarge)?;
        if base < loaded.image_end.saturating_add(MIN_STACK) {
            return Err(CallError::OutOfMemory);
        }
        let input_ptr = base + TRAMPOLINE;
        let too_large = |_| CallError::InputTooLarge;
        vm.write_mem(base, &EBREAK.to_le_bytes())
            .map_err(too_large)?;
        vm.write_mem(input_ptr, input).map_err(too_large)?;
        vm.set_reg(2, base);
        vm.set_reg(1, base);
        vm.set_reg(10, input_ptr);
        vm.set_reg(11, input_len);
        vm.set_pc(entry.addr);
        // Only a marked mutation or cron may use the beacon; the host is read-only for queries
        // anyway, but the flag is the single gate and is off for everything else.
        vm.set_randomness_allowed(
            entry.randomness && matches!(kind, FunctionKind::Mutation | FunctionKind::Cron),
        );

        let result = if kind == FunctionKind::Query {
            run_with_host(&mut vm, &mut ReadOnly(host))
        } else {
            run_with_host(&mut vm, host)
        };
        let gas_used = vm.gas_used();
        match result {
            Ok(Exit::Return(output)) => Ok(CallOutcome { output, gas_used }),
            Ok(_) => Ok(CallOutcome {
                output: Vec::new(),
                gas_used,
            }),
            Err(trap) => Err(CallError::Trap { trap, gas_used }),
        }
    }
}

/// Forces `read_only()` to true; everything else delegates.
struct ReadOnly<'a, H>(&'a mut H);

impl<H: Host> Host for ReadOnly<'_, H> {
    fn read_only(&self) -> bool {
        true
    }
    fn storage_read(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        self.0.storage_read(key)
    }
    fn storage_write(&mut self, key: &[u8], value: &[u8]) {
        self.0.storage_write(key, value)
    }
    fn storage_remove(&mut self, key: &[u8]) {
        self.0.storage_remove(key)
    }
    fn caller(&self) -> Address {
        self.0.caller()
    }
    fn contract_address(&self) -> Address {
        self.0.contract_address()
    }
    fn block_height(&self) -> u64 {
        self.0.block_height()
    }
    fn token_balance(&self, addr: &Address) -> u64 {
        self.0.token_balance(addr)
    }
    fn token_transfer(&mut self, to: &Address, amount: u64) -> bool {
        self.0.token_transfer(to, amount)
    }
    fn table_insert(&mut self, table: &[u8], row: &[u8]) -> u64 {
        self.0.table_insert(table, row)
    }
    fn table_get(&mut self, table: &[u8], id: u64) -> Option<Vec<u8>> {
        self.0.table_get(table, id)
    }
    fn table_update(&mut self, table: &[u8], id: u64, row: &[u8]) -> bool {
        self.0.table_update(table, id, row)
    }
    fn table_delete(&mut self, table: &[u8], id: u64) -> bool {
        self.0.table_delete(table, id)
    }
    fn table_scan(&mut self, table: &[u8], start_id: u64, max: u64) -> Vec<u64> {
        self.0.table_scan(table, start_id, max)
    }
    fn table_index_scan(&mut self, scan: &crate::host::IndexScan) -> crate::host::IndexScanPage {
        self.0.table_index_scan(scan)
    }
    fn tokens(&mut self) -> Option<&mut dyn crate::token::TokenStore> {
        self.0.tokens()
    }
    fn random_bytes(&mut self, len: usize) -> Option<Vec<u8>> {
        self.0.random_bytes(len)
    }
    fn price(&mut self, pair: u32) -> Option<crate::host::PriceRecord> {
        self.0.price(pair)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{build_elf, MemHost};

    const MEM: usize = 1 << 18;
    const RET: u32 = 0x0000_8067;
    const ECALL: u32 = 0x0000_0073;
    const SPIN: u32 = 0x0000_006f;

    fn addi(rd: u32, rs1: u32, imm: u32) -> u32 {
        (imm << 20) | (rs1 << 15) | (rd << 7) | 0x13
    }

    /// echo @0, ret @8, storage_write(input[0] -> input[0]) @12, spin @36.
    fn code() -> Vec<u8> {
        [
            addi(17, 0, 8),
            ECALL,
            RET,
            addi(13, 0, 1),
            addi(11, 0, 1),
            addi(12, 10, 0),
            addi(17, 0, 2),
            ECALL,
            RET,
            SPIN,
        ]
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect()
    }

    fn contract(syms: &[(&str, u64)]) -> Result<Contract, ContractError> {
        Contract::from_elf(&build_elf(&code(), syms))
    }

    fn all() -> Contract {
        contract(&[
            ("btb_query_echo", 0),
            ("btb_mutation_noop", 8),
            ("btb_mutation_put", 12),
            ("btb_query_put", 12),
            ("btb_cron_tick", 8),
            ("btb_action_fetch", 0),
            ("btb_query_spin", 36),
            ("helper", 0),
        ])
        .unwrap()
    }

    #[test]
    fn lists_entry_points() {
        let c = all();
        assert_eq!(c.entry_points().len(), 7);
        assert!(c.entry_points().iter().all(|e| e.name != "helper"));
        assert_eq!(c.entry_points()[0].kind, FunctionKind::Query);
    }

    #[test]
    fn calls_each_kind_and_passes_input() {
        let c = all();
        let mut h = MemHost::default();
        let input = b"hello world, 17 bytes";
        for (k, n) in [
            (FunctionKind::Query, "echo"),
            (FunctionKind::Action, "fetch"),
        ] {
            let o = c.call(k, n, input, &mut h, 10_000, MEM).unwrap();
            assert_eq!(o.output, input);
            assert!(o.gas_used > 0);
        }
        let o = c
            .call(FunctionKind::Mutation, "noop", input, &mut h, 10_000, MEM)
            .unwrap();
        assert!(o.output.is_empty());
        let o = c
            .call(FunctionKind::Cron, "tick", &[], &mut h, 10_000, MEM)
            .unwrap();
        assert!(o.output.is_empty());
        c.call(FunctionKind::Mutation, "put", b"k", &mut h, 10_000, MEM)
            .unwrap();
        assert_eq!(h.storage.get(b"k".as_slice()), Some(&b"k".to_vec()));
    }

    #[test]
    fn query_cannot_write_even_with_writable_host() {
        let c = all();
        let mut h = MemHost::default();
        assert!(!h.read_only);
        let e = c
            .call(FunctionKind::Query, "put", b"k", &mut h, 10_000, MEM)
            .unwrap_err();
        assert!(matches!(
            e,
            CallError::Trap {
                trap: Trap::WriteInQuery,
                ..
            }
        ));
        assert!(h.storage.is_empty());
    }

    #[test]
    fn missing_entry_and_wrong_kind() {
        let c = all();
        let mut h = MemHost::default();
        for (k, n) in [(FunctionKind::Query, "nope"), (FunctionKind::Cron, "echo")] {
            assert_eq!(
                c.call(k, n, &[], &mut h, 100, MEM),
                Err(CallError::NoSuchEntry {
                    kind: k,
                    name: n.to_string()
                })
            );
        }
    }

    #[test]
    fn out_of_gas() {
        let c = all();
        let mut h = MemHost::default();
        let e = c
            .call(FunctionKind::Query, "spin", &[], &mut h, 500, MEM)
            .unwrap_err();
        assert!(matches!(
            e,
            CallError::Trap {
                trap: Trap::OutOfGas,
                ..
            }
        ));
    }

    #[test]
    fn input_too_large() {
        let c = all();
        let mut h = MemHost::default();
        let big = vec![0u8; MEM];
        assert_eq!(
            c.call(FunctionKind::Query, "echo", &big, &mut h, 100, MEM),
            Err(CallError::InputTooLarge)
        );
    }

    #[test]
    fn input_overlapping_image_or_stack_is_rejected() {
        let c = all();
        let mut h = MemHost::default();
        // Image ends near 0x1028; 70 KiB of memory leaves < 64 KiB of stack.
        assert_eq!(
            c.call(FunctionKind::Query, "echo", &[], &mut h, 100, 0x1028 + 1000),
            Err(CallError::OutOfMemory)
        );
        // Input that fits in memory but eats the stack reserve.
        let big = vec![0u8; MEM - 0x1028 - MIN_STACK as usize];
        assert_eq!(
            c.call(FunctionKind::Query, "echo", &big, &mut h, 100, MEM),
            Err(CallError::OutOfMemory)
        );
        // Just fits.
        let ok = vec![1u8; MEM - 0x1028 - MIN_STACK as usize - 64];
        assert!(c
            .call(FunctionKind::Query, "echo", &ok, &mut h, 10_000_000, MEM)
            .is_ok());
    }

    #[test]
    fn bad_names() {
        for bad in [
            "btb_query_".to_string(),
            "btb_query_Bad".to_string(),
            "btb_query_a-b".to_string(),
            format!("btb_cron_{}", "a".repeat(65)),
        ] {
            assert!(matches!(
                contract(&[(&bad, 0)]),
                Err(ContractError::InvalidName(_))
            ));
        }
        assert!(contract(&[(&format!("btb_cron_{}", "a".repeat(64)), 0)]).is_ok());
    }

    #[test]
    fn duplicates_and_empty() {
        assert!(matches!(
            contract(&[("btb_query_a", 0), ("btb_query_a", 8)]),
            Err(ContractError::DuplicateEntry {
                kind: FunctionKind::Query,
                ..
            })
        ));
        assert!(contract(&[("btb_query_a", 0), ("btb_mutation_a", 8)]).is_ok());
        assert_eq!(
            contract(&[("main", 0), ("btb_other_x", 0)]).unwrap_err(),
            ContractError::NoEntryPoints
        );
        assert!(matches!(
            Contract::from_elf(b"not an elf"),
            Err(ContractError::Elf(_))
        ));
    }
}
