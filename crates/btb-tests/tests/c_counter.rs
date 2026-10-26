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
        ("BTB_TABLE_INDEX_SCAN", abi::TABLE_INDEX_SCAN),
        ("BTB_TOKEN_CREATE", abi::TOKEN_CREATE),
        ("BTB_TOKEN_TRANSFER_OF", abi::TOKEN_TRANSFER_OF),
        ("BTB_TOKEN_BALANCE_OF", abi::TOKEN_BALANCE_OF),
        ("BTB_TOKEN_INFO", abi::TOKEN_INFO),
        ("BTB_TOKEN_HOLDERS", abi::TOKEN_HOLDERS),
        ("BTB_TOKEN_MINT", abi::TOKEN_MINT),
        ("BTB_TOKEN_BURN", abi::TOKEN_BURN),
        ("BTB_CONTRACT_ADDRESS", abi::CONTRACT_ADDRESS),
        ("BTB_CALLER_VIA_SESSION", abi::CALLER_VIA_SESSION),
        ("BTB_RANDOM_BYTES", abi::RANDOM_BYTES),
        ("BTB_PRICE", abi::PRICE),
        ("BTB_REPORT_REQUEST", abi::REPORT_REQUEST),
        ("BTB_PRICE_TWAP", abi::PRICE_TWAP),
        ("BTB_CALL_CONTRACT", abi::CALL_CONTRACT),
        ("BTB_FILE_INFO", abi::FILE_INFO),
        ("BTB_FILE_PIN", abi::FILE_PIN),
        ("BTB_FILE_UNKNOWN", u64::from(abi::FILE_UNKNOWN)),
        ("BTB_FILE_STORED", u64::from(abi::FILE_STORED)),
        ("BTB_FILE_EXPIRED", u64::from(abi::FILE_EXPIRED)),
        ("BTB_FILE_PIN_OK", abi::FILE_PIN_OK),
        ("BTB_FILE_PIN_EXISTS", abi::FILE_PIN_EXISTS),
        ("BTB_FILE_PIN_INSUFFICIENT", abi::FILE_PIN_INSUFFICIENT),
        ("BTB_FILE_PIN_NO_NODES", abi::FILE_PIN_NO_NODES),
        ("BTB_FILE_PIN_INVALID", abi::FILE_PIN_INVALID),
        ("BTB_FILE_PIN_UNSUPPORTED", abi::FILE_PIN_UNSUPPORTED),
        ("BTB_CALL_QUERY", 0),
        ("BTB_CALL_MUTATION", 1),
        ("BTB_PRICE_PAUSED", abi::PRICE_PAUSED),
        ("BTB_PRICE_BAD_WINDOW", abi::PRICE_BAD_WINDOW),
        ("BTB_TOKEN_OK", abi::TOKEN_OK),
        ("BTB_TOKEN_INVALID", abi::TOKEN_INVALID),
        ("BTB_TOKEN_INSUFFICIENT", abi::TOKEN_INSUFFICIENT),
        ("BTB_TOKEN_NO_TOKEN", abi::TOKEN_NO_TOKEN),
        ("BTB_TOKEN_UNAUTHORIZED", abi::TOKEN_UNAUTHORIZED),
        ("BTB_TOKEN_OVERFLOW", abi::TOKEN_OVERFLOW),
        ("BTB_TOKEN_UNSUPPORTED", abi::TOKEN_UNSUPPORTED),
        ("BTB_TRANSFER_OK", abi::TRANSFER_OK),
        ("BTB_TRANSFER_INSUFFICIENT", abi::TRANSFER_INSUFFICIENT),
        ("BTB_TABLE_OK", abi::TABLE_OK),
        ("BTB_TABLE_MISSING", abi::TABLE_MISSING),
    ];
    let defines: Vec<(&str, &str)> = h
        .lines()
        .filter_map(|l| l.strip_prefix("#define "))
        .filter_map(|l| l.split_once(' '))
        // Drop trailing `/* ... */` comments so only the value is parsed.
        .map(|(n, v)| (n, v.split("/*").next().unwrap_or(v)))
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

/// The header's `btb_call_request` / `btb_call_contract` build the request the VM parses, and the
/// status codes decode: scripted host, C caller.
#[test]
fn c_contract_can_call_another_contract() {
    use btb_vm::host::parse_call;
    use btb_vm::testing::MemHost;
    use btb_vm::{CallEnd, CallReply};
    let Some(elf) = elf() else { return };
    let c = Contract::from_elf(elf).expect("valid contract");
    let mut input = vec![0x42u8; 32];
    input.push(1);
    input.extend_from_slice(b"transfer");

    let mut h = MemHost::default();
    h.call_replies.push_back(CallReply {
        end: CallEnd::Ok(vec![5, 6, 7]),
        gas_used: 100,
    });
    let (out, gas) = run(&c, FunctionKind::Mutation, "relay", &input, &mut h);
    assert_eq!(out, [5, 6, 7]);
    assert!(gas > 3_000 + 100);
    let (target, kind, f, args, forwarded, read_only, max) = h.calls_seen[0].clone();
    assert_eq!(
        (target, kind, f.as_str()),
        ([0x42; 32], FunctionKind::Mutation, "transfer")
    );
    assert!(args.is_empty() && !read_only && max == 64);
    // GAS_ALL was capped at what the C caller had left
    assert!(forwarded > 9_000_000 && forwarded < GAS, "{forwarded}");
    // the request the header built is the one the VM would parse
    let mut req = input[..32].to_vec();
    req.push(1);
    req.extend_from_slice(&u64::MAX.to_le_bytes());
    req.extend_from_slice(&8u32.to_le_bytes());
    req.extend_from_slice(b"transfer");
    assert_eq!(parse_call(&req).unwrap().3, "transfer");

    // an error status aborts the C contract with the code
    let mut h = MemHost::default();
    h.call_replies
        .push_back(CallReply::refused(CallEnd::Reentrant));
    let e = c
        .call(FunctionKind::Mutation, "relay", &input, &mut h, GAS, MEM)
        .unwrap_err();
    match e {
        btb_vm::CallError::Trap {
            trap: btb_vm::Trap::Abort(m),
            ..
        } => assert_eq!(m.as_bytes(), [6], "UINT64_MAX - CALL_REENTRANT"),
        other => panic!("{other:?}"),
    }
    // a host that cannot run other contracts
    let e = c
        .call(
            FunctionKind::Mutation,
            "relay",
            &input,
            &mut MemHost::default(),
            GAS,
            MEM,
        )
        .unwrap_err();
    assert!(matches!(e, btb_vm::CallError::Trap { .. }));
}
