//! Files from contracts, end to end on a real cluster: a 4-validator chain, 6 `btb storage-node`
//! daemons, a hosting node with the upload gateway, and the `examples/files` contract.
//!
//! The client uploads the bytes to the host and gets the root; a mutation passes the root and the
//! contract pays for the storage from its own balance (`file_pin`); the host then pushes the
//! pieces to the assigned nodes; `file_info` reports the file stored; the host serves it by
//! root, verified against the root; an unknown root is status unknown. The executor checks the
//! supply invariant after every block, so the contract-funded escrow cannot create or destroy BTB.

#[path = "../../btb-tests/tests/common/mod.rs"]
mod common;
#[path = "../../btb-node/tests/lite/mod.rs"]
mod lite;

use btb_chain::{account_address, StorageConfig};
use btb_codec::{Address as CA, Decode};
use btb_host::{Host, HostConfig};
use btb_node::hex;
use btb_node::net::FollowerPolicy;
use lite::*;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Daemons(Vec<Child>);
impl Drop for Daemons {
    fn drop(&mut self) {
        for d in &mut self.0 {
            let _ = d.kill();
            let _ = d.wait();
        }
    }
}

struct Tmp(PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn start_daemon(rpc: &str, dir: &Path, seed: u8) -> Child {
    let key_file = dir.join(format!("node{seed}.key"));
    btb_node::config::save_key(&key_file, &key(seed)).unwrap();
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
    rx.recv_timeout(Duration::from_secs(90))
        .expect("storage node did not start");
    child
}

fn http(addr: SocketAddr, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .unwrap();
    s.write_all(body).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let at = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..at]).into_owned();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, raw[at + 4..].to_vec())
}

fn query(c: &Lite, contract: &[u8; 32], f: &str, args: &impl btb_codec::Encode) -> Vec<u8> {
    let r = c
        .node(0)
        .call(
            "btb_query",
            json!([hex::encode(contract), f, hex::encode(&args.encode())]),
        )
        .unwrap();
    hex::decode(r["output"].as_str().unwrap()).unwrap()
}

#[test]
fn upload_pin_from_contract_push_and_serve() {
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
    let mut c = Lite::with_storage(4, 4, 600, Some(FollowerPolicy::default()), storage);
    let tmp = Tmp(std::env::temp_dir().join(format!("btb-files-e2e-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&tmp.0);
    std::fs::create_dir_all(&tmp.0).unwrap();
    let rpc = format!("http://{}", c.node(0).rpc_addr().unwrap());

    for seed in 50..56u8 {
        let addr = account_address(&key(seed).verifying_key().to_bytes());
        let tx = c.faucet.transfer(addr, 20_000_000_000);
        c.run(&tx);
    }
    let mut daemons = Daemons(Vec::new());
    for seed in 50..56u8 {
        daemons.0.push(start_daemon(&rpc, &tmp.0, seed));
    }

    let files = c.deploy(&common::build_example("files"));
    let mut cfg = HostConfig::new(
        c.genesis.clone(),
        c.validator_addrs(),
        "127.0.0.1:0".parse().unwrap(),
    );
    cfg.upload_dir = Some(tmp.0.join("uploads"));
    let host = Host::start(cfg).unwrap();
    let target = c.height(0).saturating_sub(3);
    c.wait_until("the host to catch up", 60, || host.height() >= target);
    let h = host.http_addr();

    // 1. Upload to the gateway: the root comes back, nothing is on chain yet.
    let data: Vec<u8> = (0..(200usize << 10))
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8 ^ (i >> 10) as u8)
        .collect();
    let (st, body) = http(h, "POST", "/upload", &data);
    assert_eq!(st, 200, "{}", String::from_utf8_lossy(&body));
    let up: Value = serde_json::from_slice(&body).unwrap();
    let root = up["root"].as_str().unwrap().to_string();
    let rootb: [u8; 32] = hex::decode(&root).unwrap().try_into().unwrap();
    assert_eq!(up["size"], data.len());
    assert_eq!(up["stored"], false);
    let (k, m) = (
        up["k"].as_u64().unwrap() as u8,
        up["m"].as_u64().unwrap() as u8,
    );
    // Pushing before the chain knows the file is refused.
    let (st, _) = http(h, "POST", &format!("/upload/{root}/push"), b"");
    assert_eq!(st, 409);

    // 2. Unknown root: status unknown (0), zeros.
    let out = query(&c, &files, "info", &(CA(rootb),));
    assert_eq!(<(u64, u8, u64)>::decode(&out).unwrap(), (0, 0, 0));

    // 3. A mutation passes the root; the contract pays the escrow from its own balance.
    let tx = c.faucet.call(
        files,
        "pin_and_attach",
        &(
            "avatar".to_string(),
            CA(rootb),
            data.len() as u64,
            k,
            m,
            3u32,
        ),
    );
    let receipt = c.run(&tx);
    assert_eq!(receipt["output"], "00", "pin status ok: {receipt}");
    let view = c.node(0).call("btb_storageFile", json!([root])).unwrap();
    assert_eq!(view["owner"].as_str().unwrap(), hex::encode(&files));
    assert_eq!(view["pieces"].as_array().unwrap().len(), (k + m) as usize);
    let escrow: u64 = view["escrow"].as_str().unwrap().parse().unwrap();
    assert!(escrow > 0, "{view}");

    // 4. file_info now says stored, with the size; attach accepts the root.
    let out = query(&c, &files, "info", &(CA(rootb),));
    let (size, status, expiry) = <(u64, u8, u64)>::decode(&out).unwrap();
    assert_eq!((size, status), (data.len() as u64, 1));
    assert!(expiry > c.height(0));
    let tx = c
        .faucet
        .call(files, "attach", &("again".to_string(), CA(rootb)));
    c.run(&tx);

    // 5. The gateway pushes the pieces to the assigned nodes; the host serves the verified file.
    let mut pushed = Value::Null;
    c.wait_until("the host to see the stored file", 60, || {
        let (st, body) = http(h, "POST", &format!("/upload/{root}/push"), b"");
        if st == 200 {
            pushed = serde_json::from_slice(&body).unwrap();
            true
        } else {
            false
        }
    });
    assert_eq!(pushed["pushed"], (k + m) as u64, "{pushed}");
    assert_eq!(pushed["failed"].as_array().unwrap().len(), 0);
    let (st, bytes) = http(h, "GET", &format!("/file/{root}"), b"");
    assert_eq!(st, 200);
    assert_eq!(
        bytes, data,
        "served by root, verified against the storage commitment"
    );

    // 6. A root nobody stored stays unknown, and the same pin twice costs nothing more.
    let out = query(&c, &files, "info", &(CA([9; 32]),));
    assert_eq!(<(u64, u8, u64)>::decode(&out).unwrap().1, 0);
    let tx = c.faucet.call(
        files,
        "pin_and_attach",
        &(
            "third".to_string(),
            CA(rootb),
            data.len() as u64,
            k,
            m,
            3u32,
        ),
    );
    assert_eq!(c.run(&tx)["output"], "00");
    let view2 = c.node(0).call("btb_storageFile", json!([root])).unwrap();
    assert_eq!(view2["escrow"], view["escrow"]);
    drop(host);
    drop(daemons);
}
