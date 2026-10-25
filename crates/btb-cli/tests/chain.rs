//! `btb deploy/call/query --rpc` against an in-process 4-validator cluster: deploy the counter
//! example with its cron, call a mutation, query, and see the cron run.

#[path = "../../btb-node/tests/lite/mod.rs"]
mod lite;

use lite::*;
use std::path::Path;
use std::process::{Command, Output};

fn btb(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_btb"))
        .args(args)
        .current_dir(dir)
        .env_remove("RUSTFLAGS")
        .output()
        .unwrap()
}

fn ok(o: Output) -> String {
    assert!(
        o.status.success(),
        "btb failed: {}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

fn field<'a>(out: &'a str, name: &str) -> &'a str {
    out.lines()
        .find_map(|l| l.strip_prefix(name).map(str::trim))
        .unwrap_or_else(|| panic!("no {name:?} in {out}"))
}

/// The counter example, used in place (its cron runs every 10 blocks).
fn counter_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/counter")
}

#[test]
fn deploy_call_query_over_rpc() {
    let _g = serial();
    let mut c = Lite::new(4, 600, None);
    let tmp = std::env::temp_dir().join(format!("btb-cli-chain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let counter = counter_dir();
    let key = tmp.join("faucet.key");
    btb_node::config::save_key(&key, &c.faucet.key).unwrap();
    let rpc = format!("http://{}", c.node(0).rpc_addr().unwrap());
    let (key_s, rpc_s) = (key.to_str().unwrap(), rpc.as_str());

    // Without --rpc the old local-bundle behaviour stays; key flags alone are refused.
    assert!(!btb(&counter, &["deploy", "--key", key_s]).status.success());

    let out = ok(btb(&counter, &["deploy", "--rpc", rpc_s, "--key", key_s]));
    let addr = field(&out, "deployed: contract ").to_string();
    assert_eq!(addr.len(), 64);
    c.faucet.nonce = 1;
    let fund = c.faucet.transfer(
        btb_node::hex::decode32(&addr).unwrap(),
        10 * btb_chain::UNITS_PER_BTB,
    );
    // The CLI took nonce 0; the wallet continues at 1 for the funding transfer.
    c.run(&fund);

    let m = std::fs::read_to_string(counter.join("target/btb/deploy/manifest.json")).unwrap();
    assert!(m.contains("\"every_blocks\": 10"), "{m}");

    // A mutation through the CLI: increment(5) returns the new total (u64 LE).
    let out = ok(btb(
        &counter,
        &[
            "call",
            "--rpc",
            rpc_s,
            "--key",
            key_s,
            &addr,
            "increment",
            "[5]",
        ],
    ));
    assert_eq!(field(&out, "output ").len(), 16, "{out}");

    let q = |a: &str| ok(btb(&counter, &["query", "--rpc", rpc_s, &addr, a]));
    let get = || {
        let out = q("get");
        let b = btb_node::hex::decode(field(&out, "output ")).unwrap();
        u64::from_le_bytes(b[..8].try_into().unwrap())
    };
    let after_call = get();
    assert!(after_call >= 5, "{after_call}");

    // The cron adds one every `every` blocks; wait for it to move the counter.
    c.wait_until("the cron to run", 60, || get() > after_call);

    // Errors are reported: unknown function.
    let o = btb(
        &counter,
        &["call", "--rpc", rpc_s, "--key", key_s, &addr, "nope"],
    );
    assert!(!o.status.success());
    let _ = std::fs::remove_dir_all(&tmp);
}
