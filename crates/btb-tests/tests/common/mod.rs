//! Helpers shared by the end-to-end tests: build an example contract for riscv64 and check it.
#![allow(dead_code)]

use btb_codec::Encode;
use btb_vm::decode::decode;
use btb_vm::testing::MemHost;
use btb_vm::{CallError, Contract, FunctionKind};
use std::path::PathBuf;
use std::process::Command;

pub const MEM: usize = 1 << 20;
pub const GAS: u64 = 10_000_000;

/// Build `examples/<name>` with the real toolchain and return the ELF bytes.
pub fn build_example(name: &str) -> Vec<u8> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name);
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "--release", "--quiet"])
        .current_dir(&dir)
        // Env flags would override .cargo/config.toml (the rv64im feature flags live there).
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("CARGO_TARGET_DIR")
        .env("RUSTC_BOOTSTRAP", "1")
        .status()
        .expect("run cargo");
    assert!(
        status.success(),
        "building examples/{name} failed (is the riscv64imac-unknown-none-elf target installed?)"
    );
    std::fs::read(
        dir.join("target/riscv64imac-unknown-none-elf/release")
            .join(name),
    )
    .expect("read contract ELF")
}

/// Call a function with encoded `input`; returns `(output, gas_used)`.
pub fn call(
    c: &Contract,
    kind: FunctionKind,
    name: &str,
    input: &impl Encode,
    h: &mut MemHost,
) -> Result<(Vec<u8>, u64), CallError> {
    c.call(kind, name, &input.encode(), h, GAS, MEM)
        .map(|o| (o.output, o.gas_used))
}

/// The message of an `Abort` trap; panics on any other error.
pub fn abort_msg(e: CallError) -> String {
    match e {
        CallError::Trap {
            trap: btb_vm::decode::Trap::Abort(m),
            ..
        } => m,
        other => panic!("expected abort, got {other:?}"),
    }
}

/// Linear sweep over every executable segment: each 4-byte word must decode as rv64im. This
/// rejects compressed (C), atomic (A) and floating-point instructions anywhere in the binary,
/// including precompiled library code that is never executed in the other tests.
pub fn assert_only_rv64im(b: &[u8]) {
    let u = |o: usize, n: usize| -> u64 {
        let mut a = [0u8; 8];
        a[..n].copy_from_slice(&b[o..o + n]);
        u64::from_le_bytes(a)
    };
    assert_eq!(u(16, 2), 2, "ET_EXEC (static, non-PIE)");
    assert_eq!(u(18, 2), 243, "EM_RISCV");
    let (phoff, phentsize, phnum) = (u(32, 8) as usize, u(54, 2) as usize, u(56, 2) as usize);
    let mut words = 0;
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        let (typ, flags, off, filesz) = (u(ph, 4), u(ph + 4, 4), u(ph + 8, 8), u(ph + 32, 8));
        assert_ne!(typ, 2, "no PT_DYNAMIC: statically linked");
        if typ == 1 && flags & 1 != 0 {
            assert_eq!(filesz % 4, 0);
            for w in b[off as usize..(off + filesz) as usize].chunks_exact(4) {
                let word = u32::from_le_bytes(w.try_into().unwrap());
                decode(word).unwrap_or_else(|e| panic!("word {word:#010x}: {e:?}"));
                words += 1;
            }
        }
    }
    assert!(words > 100, "suspiciously little code: {words} words");
}

/// Tables with the indexes the contract declares in its ELF (`#[btb::table]`), registered for
/// `contract` exactly as `btb dev` and the chain do.
pub fn declared_tables(contract: [u8; 32], elf: &[u8]) -> btb_runtime::Tables {
    let schema = btb_runtime::schema::Schema::from_elf(elf).expect("valid schema");
    let mut tables = btb_runtime::Tables::new();
    for (name, defs) in schema.index_defs().expect("index definitions") {
        tables
            .register(contract, name.as_bytes(), defs)
            .expect("register");
    }
    tables
}
