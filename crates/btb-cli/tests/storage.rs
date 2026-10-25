//! End to end: a 4-validator cluster, 6 `btb storage-node` processes, `btb store` of a 1 MiB
//! file, two storage nodes killed, `btb fetch` still verifies the file, and the chain's
//! challenges notice the dead nodes, slash them and reassign their pieces to live nodes that
//! rebuild them from the others.

#[path = "../../btb-node/tests/lite/mod.rs"]
mod lite;

use btb_chain::{account_address, StorageConfig};
use btb_storage::{erasure, http, unhex32};
use lite::*;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn btb(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_btb"))
        .args(args)
        .env_remove("RUSTFLAGS")
        .output()
        .unwrap()
}

fn ok(o: std::process::Output) -> String {
    assert!(
        o.status.success(),
        "btb failed: {}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

struct Daemon {
    child: Child,
    endpoint: String,
    addr: [u8; 32],
}

struct Tmp(PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Daemons(Vec<Daemon>);
impl Drop for Daemons {
    fn drop(&mut self) {
        for d in &mut self.0 {
            let _ = d.child.kill();
            let _ = d.child.wait();
        }
    }
}

fn start_daemon(rpc: &str, dir: &Path, seed: u8) -> Daemon {
    let key = key(seed);
    let key_file = dir.join(format!("node{seed}.key"));
    btb_node::config::save_key(&key_file, &key).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_btb"))
        .args([
            "storage-node",
            "run",
            "--rpc",
            rpc,
            "--key",
            key_file.to_str().unwrap(),
            "--dir",
            dir.join(format!("pieces{seed}")).to_str().unwrap(),
            "--deposit",
            "100000000",
        ])
        .env_remove("RUSTFLAGS")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(out).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx
        .recv_timeout(Duration::from_secs(90))
        .expect("storage node did not start");
    let endpoint = line
        .strip_prefix("storage node: ")
        .and_then(|l| l.split(' ').next())
        .unwrap_or_else(|| panic!("unexpected output {line:?}"))
        .to_string();
    Daemon {
        child,
        endpoint,
        addr: account_address(&key.verifying_key().to_bytes()),
    }
}

fn file_view(c: &Lite, root: &str) -> Value {
    c.node(0).call("btb_storageFile", json!([root])).unwrap()
}

#[test]
fn store_kill_two_nodes_fetch_and_repair() {
    let _g = serial();
    let storage = StorageConfig {
        epoch_blocks: 60,
        challenge_blocks: 30,
        repair_blocks: 45,
        price_per_kib_period: 1_000,
        deposit_per_kib: 100,
        min_deposit: 1_000_000,
        slash_bps: 5_000,
    };
    let mut c = Lite::with_storage(4, 4, 600, None, storage);
    let tmp = Tmp(std::env::temp_dir().join(format!("btb-cli-storage-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&tmp.0);
    std::fs::create_dir_all(&tmp.0).unwrap();
    let dir = tmp.0.as_path();
    let rpc = format!("http://{}", c.node(0).rpc_addr().unwrap());

    // Fund 6 storage-node accounts and start their daemons (they register themselves).
    for seed in 50..56u8 {
        let addr = account_address(&key(seed).verifying_key().to_bytes());
        let tx = c.faucet.transfer(addr, 20_000_000_000);
        c.run(&tx);
    }
    let mut daemons = Daemons(Vec::new());
    for seed in 50..56u8 {
        daemons.0.push(start_daemon(&rpc, dir, seed));
    }
    let nodes = c.node(0).call("btb_storageNodes", json!([])).unwrap();
    assert_eq!(nodes.as_array().unwrap().len(), 6, "{nodes}");

    // A 1 MiB file with structure that makes any mix-up visible.
    let data: Vec<u8> = (0..(1usize << 20))
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8 ^ (i >> 10) as u8)
        .collect();
    let src = dir.join("big.bin");
    std::fs::write(&src, &data).unwrap();
    let key_file = dir.join("faucet.key");
    btb_node::config::save_key(&key_file, &c.faucet.key).unwrap();
    let out = ok(btb(&[
        "store",
        src.to_str().unwrap(),
        "--rpc",
        &rpc,
        "--key",
        key_file.to_str().unwrap(),
        "--k",
        "4",
        "--m",
        "2",
        "--periods",
        "60",
    ]));
    let root = out
        .lines()
        .find_map(|l| l.strip_prefix("root: "))
        .expect("root line")
        .trim()
        .to_string();
    assert_eq!(root.len(), 64);
    let view = file_view(&c, &root);
    assert_eq!(view["k"], 4);
    assert_eq!(view["m"], 2);
    let pieces = view["pieces"].as_array().unwrap();
    assert_eq!(pieces.len(), 6);
    // Spread: 6 pieces on 6 distinct nodes.
    let holders: std::collections::BTreeSet<&str> =
        pieces.iter().map(|p| p["node"].as_str().unwrap()).collect();
    assert_eq!(holders.len(), 6);
    // Only hashes and assignments are on chain: the file record is a few hundred bytes of fields.
    assert!(view.to_string().len() < 4_000);

    // Fetch with everyone alive.
    let fetched = dir.join("back.bin");
    ok(btb(&[
        "fetch",
        &root,
        "--rpc",
        &rpc,
        "--out",
        fetched.to_str().unwrap(),
    ]));
    assert_eq!(std::fs::read(&fetched).unwrap(), data);

    // Kill m = 2 nodes (the holders of piece 0 and piece 5).
    let victims: Vec<[u8; 32]> = [0usize, 5]
        .iter()
        .map(|i| unhex32(pieces[*i]["node"].as_str().unwrap()).unwrap())
        .collect();
    let mut dead_eps = Vec::new();
    for d in daemons.0.iter_mut() {
        if victims.contains(&d.addr) {
            d.child.kill().unwrap();
            let _ = d.child.wait();
            dead_eps.push(d.endpoint.clone());
        }
    }
    assert_eq!(dead_eps.len(), 2);
    let fetched2 = dir.join("back2.bin");
    ok(btb(&[
        "fetch",
        &root,
        "--rpc",
        &rpc,
        "--out",
        fetched2.to_str().unwrap(),
    ]));
    assert_eq!(
        std::fs::read(&fetched2).unwrap(),
        data,
        "fetch with m nodes down"
    );

    // The chain notices (missed challenges), slashes, and the live nodes rebuild the pieces:
    // every piece ends up Holding on a live node, and the two lost pieces changed hands.
    let live_eps: Vec<String> = daemons
        .0
        .iter()
        .filter(|d| !victims.contains(&d.addr))
        .map(|d| d.endpoint.clone())
        .collect();
    let start = Instant::now();
    let healed = loop {
        let v = file_view(&c, &root);
        let ps = v["pieces"].as_array().unwrap();
        let repaired = |i: usize| {
            let p = &ps[i];
            p["missed"].as_u64().unwrap() >= 1
                && p["state"] == "holding"
                && live_eps.contains(&p["endpoint"].as_str().unwrap().to_string())
                && p["proven"].as_u64().unwrap() >= 1
        };
        if ps
            .iter()
            .all(|p| live_eps.contains(&p["endpoint"].as_str().unwrap().to_string()))
            && repaired(0)
            && repaired(5)
        {
            break v;
        }
        assert!(
            start.elapsed() < Duration::from_secs(240),
            "pieces were not repaired in time: {v}"
        );
        std::thread::sleep(Duration::from_millis(250));
    };

    // The repaired pieces are really served and verify against the file id.
    let id = unhex32(&root).unwrap();
    let (size, k, m, pl) = (
        healed["size"].as_u64().unwrap(),
        healed["k"].as_u64().unwrap() as u8,
        healed["m"].as_u64().unwrap() as u8,
        healed["piece_len"].as_u64().unwrap(),
    );
    let enc = erasure::encode(&data, k, m).unwrap();
    assert_eq!(enc.id, id);
    for i in [0usize, 5] {
        let ep = healed["pieces"][i]["endpoint"].as_str().unwrap();
        let (bytes, path) = http::get_piece(ep, &id, i as u8).unwrap();
        assert!(erasure::verify_piece(&id, size, k, m, pl, i, &bytes, &path));
        assert_eq!(
            bytes, enc.pieces[i],
            "repaired piece {i} differs from the original"
        );
    }
    // Slashing moved deposit into the file's escrow and payments started flowing.
    let escrow: u64 = healed["escrow"].as_str().unwrap().parse().unwrap();
    assert!(escrow > 0);
    // And a plain fetch works again from live nodes only.
    ok(btb(&[
        "fetch",
        &root,
        "--rpc",
        &rpc,
        "--out",
        fetched.to_str().unwrap(),
    ]));
    assert_eq!(std::fs::read(&fetched).unwrap(), data);
}
