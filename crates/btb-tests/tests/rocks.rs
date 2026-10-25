//! Contracts over RocksDB: `examples/counter` through `RuntimeHost` gives the same results as
//! `MemKv`, and `btb-dev` with a RocksDB data dir keeps its todos across a restart.
#![cfg(feature = "rocksdb")]

use btb_codec::{Decode, Encode};
use btb_dev::{IndexSpec, Server};
use btb_runtime::rocks::RocksKv;
use btb_runtime::{layout, KvStore, MemKv, RuntimeHost, Tables};
use btb_vm::{Contract, FunctionKind};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

mod common;

const ME: [u8; 32] = [3; 32];
const CONTRACT: [u8; 32] = [8; 32];

struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("btb-tests-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        TempDir(p)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run<K: KvStore>(
    c: &Contract,
    kv: &mut K,
    kind: FunctionKind,
    name: &str,
    input: &impl Encode,
) -> Vec<u8> {
    let tables = Tables::new();
    let mut h = RuntimeHost::new(kv, &tables, CONTRACT, ME, 42);
    let out = c
        .call(
            kind,
            name,
            &input.encode(),
            &mut h,
            common::GAS,
            common::MEM,
        )
        .unwrap();
    let batch = h.finish().expect("no faults");
    kv.commit(batch).unwrap();
    out.output
}

fn counter_session<K: KvStore>(kv: &mut K, c: &Contract) -> Vec<Vec<u8>> {
    kv.put(&layout::balance_key(&CONTRACT), &1_000_000u64.to_le_bytes())
        .unwrap();
    let mut outs = Vec::new();
    for n in [5u64, 7, 1] {
        outs.push(run(c, kv, FunctionKind::Mutation, "increment", &(n,)));
    }
    outs.push(run(c, kv, FunctionKind::Query, "get", &()));
    outs
}

#[test]
fn counter_on_rocksdb_matches_memkv_and_persists() {
    let c = Contract::from_elf(&common::build_example("counter")).unwrap();
    let dir = TempDir::new("counter");
    let mem_outs;
    let mem_state;
    {
        let mut m = MemKv::new();
        mem_outs = counter_session(&mut m, &c);
        mem_state = m.scan(b"", None, false, usize::MAX).unwrap();
    }
    let mut r = RocksKv::open(&dir.0).unwrap();
    assert_eq!(counter_session(&mut r, &c), mem_outs);
    assert_eq!(u64::decode(mem_outs.last().unwrap()), Ok(13));
    assert_eq!(r.scan(b"", None, false, usize::MAX).unwrap(), mem_state);
    drop(r);
    let mut r = RocksKv::open(&dir.0).unwrap();
    let out = run(&c, &mut r, FunctionKind::Query, "get", &());
    assert_eq!(u64::decode(&out), Ok(13));
}

fn http(addr: SocketAddr, method: &str, target: &str, body: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    assert!(head.contains("200"), "{head} {body}");
    body.to_string()
}

fn start(elf: &[u8], dir: &std::path::Path) -> Server {
    // The previous server's threads may still hold the database lock for a moment.
    for _ in 0..50 {
        if let Ok(kv) = RocksKv::open(dir) {
            return Server::start(
                elf,
                ("127.0.0.1", 0),
                kv,
                vec![IndexSpec::parse("Todo:owner:addr@0").unwrap()],
            )
            .unwrap();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("could not open {dir:?}");
}

#[test]
fn btb_dev_on_rocksdb_keeps_todos_across_restart() {
    let elf = common::build_example("todo");
    let dir = TempDir::new("dev");
    let list = "/query/list_mine?format=json&caller=alice";

    let s = start(&elf, &dir.0);
    let addr = s.addr();
    assert_eq!(http(addr, "GET", list, ""), "[]");
    let id = http(
        addr,
        "POST",
        "/mutation/add?format=json&caller=alice",
        r#"["buy milk"]"#,
    );
    assert_eq!(id, "0");
    let before = http(addr, "GET", list, "");
    assert_eq!(before, r#"[{"id":0,"done":false,"text":"buy milk"}]"#);
    s.shutdown();

    let s = start(&elf, &dir.0);
    let addr = s.addr();
    assert_eq!(http(addr, "GET", list, ""), before);
    // The id counter survived too, and bob's list is still separate.
    assert_eq!(
        http(
            addr,
            "POST",
            "/mutation/add?format=json&caller=alice",
            r#"["walk dog"]"#
        ),
        "1"
    );
    assert_eq!(
        http(addr, "GET", "/query/list_mine?format=json&caller=bob", ""),
        "[]"
    );
    s.shutdown();
}
