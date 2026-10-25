//! End to end: build `examples/c-counter` with clang (proving the host-call ABI works from C),
//! then run it in `btb-vm`. Skips with a message if clang or ld.lld is missing, unless
//! `BTB_REQUIRE_C=1` is set (as in CI), in which case a missing toolchain fails the tests.

use btb_vm::decode::decode;
use btb_vm::host::abi;
use btb_vm::testing::MemHost;
use btb_vm::{Contract, FunctionKind};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

const MEM: usize = 1 << 20;
const GAS: u64 = 10_000_000;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build once per test binary; None (with a message) if the C toolchain is unavailable.
fn elf() -> Option<&'static [u8]> {
    static ELF: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    ELF.get_or_init(|| {
        let clang = std::env::var("CLANG").unwrap_or_else(|_| "/usr/bin/clang".into());
        if !have(&clang) || !have("ld.lld") {
            if std::env::var("BTB_REQUIRE_C").is_ok_and(|v| !v.is_empty() && v != "0") {
                panic!("BTB_REQUIRE_C is set but clang ({clang}) and/or ld.lld not found");
            }
            eprintln!("SKIPPED c_counter: clang ({clang}) and/or ld.lld not found");
            return None;
        }
        let dir = root().join("examples/c-counter");
        let status = Command::new("sh")
            .arg("build.sh")
            .current_dir(&dir)
            .env("CLANG", &clang)
            .status()
            .expect("run build.sh");
        assert!(status.success(), "examples/c-counter/build.sh failed");
        Some(std::fs::read(dir.join("counter.elf")).expect("read counter.elf"))
    })
    .as_deref()
}

fn run(
    c: &Contract,
    kind: FunctionKind,
    name: &str,
    input: &[u8],
    h: &mut MemHost,
) -> (Vec<u8>, u64) {
    let o = c.call(kind, name, input, h, GAS, MEM).expect("call");
    (o.output, o.gas_used)
}

fn session(c: &Contract) -> (Vec<Vec<u8>>, Vec<u64>) {
    let mut h = MemHost::default();
    let mut outs = Vec::new();
    let mut gas = Vec::new();
    for (kind, name, input) in [
        (
            FunctionKind::Mutation,
            "increment",
            5u64.to_le_bytes().to_vec(),
        ),
        (
            FunctionKind::Mutation,
            "increment",
            7u64.to_le_bytes().to_vec(),
        ),
        (FunctionKind::Query, "get", vec![]),
    ] {
        let (o, g) = run(c, kind, name, &input, &mut h);
        outs.push(o);
        gas.push(g);
    }
    (outs, gas)
}

#[test]
fn increment_twice_then_get() {
    let Some(elf) = elf() else { return };
    let c = Contract::from_elf(elf).expect("valid contract");
    let (outs, _) = session(&c);
    assert_eq!(outs[0], 5u64.to_le_bytes());
    assert_eq!(outs[1], 12u64.to_le_bytes());
    assert_eq!(outs[2], 12u64.to_le_bytes());
    // Bad argument length aborts.
    let mut h = MemHost::default();
    assert!(c
        .call(
            FunctionKind::Mutation,
            "increment",
            &[1, 2, 3],
            &mut h,
            GAS,
            MEM
        )
        .is_err());
}

#[test]
fn gas_is_deterministic() {
    let Some(elf) = elf() else { return };
    let c = Contract::from_elf(elf).expect("valid contract");
    let a = session(&c);
    let b = session(&c);
    assert_eq!(a, b);
    assert!(a.1.iter().all(|&g| g > 0));
}

#[test]
fn only_rv64im_instructions_are_emitted() {
    let Some(b) = elf() else { return };
    let u = |o: usize, n: usize| -> u64 {
        let mut a = [0u8; 8];
        a[..n].copy_from_slice(&b[o..o + n]);
        u64::from_le_bytes(a)
    };
    assert_eq!(u(16, 2), 2, "ET_EXEC");
    assert_eq!(u(18, 2), 243, "EM_RISCV");
    let (phoff, phentsize, phnum) = (u(32, 8) as usize, u(54, 2) as usize, u(56, 2) as usize);
    let mut words = 0;
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        let (typ, flags, off, filesz) = (u(ph, 4), u(ph + 4, 4), u(ph + 8, 8), u(ph + 32, 8));
        if typ == 1 && flags & 1 != 0 {
            assert_eq!(filesz % 4, 0);
            for w in b[off as usize..(off + filesz) as usize].chunks_exact(4) {
                let word = u32::from_le_bytes(w.try_into().unwrap());
                decode(word).unwrap_or_else(|e| panic!("word {word:#010x}: {e:?}"));
                words += 1;
            }
        }
    }
    assert!(words > 20, "suspiciously little code: {words} words");
}

/// Every `#define BTB_X <n>` call number in the header must equal `btb_vm::host::abi`.
#[test]
fn header_call_numbers_match_abi() {
    let h = std::fs::read_to_string(root().join("include/btb.h")).expect("read btb.h");
    let want: &[(&str, u64)] = &[
        ("BTB_STORAGE_READ", abi::STORAGE_READ),
        ("BTB_STORAGE_WRITE", abi::STORAGE_WRITE),
        ("BTB_STORAGE_REMOVE", abi::STORAGE_REMOVE),
        ("BTB_CALLER", abi::CALLER),
        ("BTB_BLOCK_HEIGHT", abi::BLOCK_HEIGHT),
        ("BTB_TOKEN_BALANCE", abi::TOKEN_BALANCE),
        ("BTB_TOKEN_TRANSFER", abi::TOKEN_TRANSFER),
        ("BTB_RETURN_DATA", abi::RETURN_DATA),
        ("BTB_ABORT", abi::ABORT),
        ("BTB_TABLE_INSERT", abi::TABLE_INSERT),
        ("BTB_TABLE_GET", abi::TABLE_GET),
        ("BTB_TABLE_UPDATE", abi::TABLE_UPDATE),
        ("BTB_TABLE_DELETE", abi::TABLE_DELETE),
        ("BTB_TABLE_SCAN", abi::TABLE_SCAN),
        ("BTB_TRANSFER_OK", abi::TRANSFER_OK),
        ("BTB_TRANSFER_INSUFFICIENT", abi::TRANSFER_INSUFFICIENT),
        ("BTB_TABLE_OK", abi::TABLE_OK),
        ("BTB_TABLE_MISSING", abi::TABLE_MISSING),
    ];
    let defines: Vec<(&str, &str)> = h
        .lines()
        .filter_map(|l| l.strip_prefix("#define "))
        .filter_map(|l| l.split_once(' '))
        .collect();
    for (name, num) in want {
        let (_, v) = defines
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("{name} missing from btb.h"));
        assert_eq!(v.trim().parse::<u64>().unwrap(), *num, "{name}");
    }
    // Any other numeric BTB_ call define must be accounted for (catches additions to the ABI).
    let numeric = defines
        .iter()
        .filter(|(n, v)| n.starts_with("BTB_") && v.trim().parse::<u64>().is_ok())
        .count();
    assert_eq!(
        numeric,
        want.len(),
        "header has defines the test does not cover"
    );
    assert!(h.contains("#define BTB_NOT_FOUND UINT64_MAX") && abi::NOT_FOUND == u64::MAX);
}
