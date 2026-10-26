//! `btb dev` reload computes the same schema diff as a chain upgrade: new indexes are backfilled
//! for rows written by the previous build, migrations run, incompatible changes are refused.

use btb_dev::Server;
use btb_runtime::MemKv;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

mod common;

const ME: &str = "1111111111111111111111111111111111111111111111111111111111111111";

fn http(addr: SocketAddr, method: &str, target: &str, body: &str) -> (String, String) {
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
    (head.lines().next().unwrap().to_string(), body.to_string())
}

fn query(addr: SocketAddr, f: &str, args: &str) -> String {
    let (status, body) = http(
        addr,
        "GET",
        &format!("/query/{f}?format=json&caller={ME}&args={args}"),
        "",
    );
    assert!(status.contains("200"), "{f}: {status} {body}");
    body
}

#[test]
fn reload_backfills_new_indexes_and_migrates_rows() {
    let v1 = common::build_example_with("evolve", "");
    let compat = common::build_example_with("evolve", "compat");
    let bad = common::build_example_with("evolve", "bad");
    let migrate = common::build_example_with("evolve", "migrate");
    let s = Server::start(&v1, ("127.0.0.1", 0), MemKv::new(), vec![]).unwrap();
    let addr = s.addr();
    for t in ["a", "b", "c"] {
        let (st, b) = http(
            addr,
            "POST",
            &format!("/mutation/add?format=json&caller={ME}"),
            &format!("[\"{t}\"]"),
        );
        assert!(st.contains("200"), "{st} {b}");
    }
    let owner = ME.to_string();
    assert!(query(addr, "count", &owner).contains('3'));

    // An incompatible change without a migration is refused; the old build keeps running.
    let e = s.reload(&bad).unwrap_err();
    assert!(e.contains("`done`") && e.contains("version"), "{e}");
    assert!(query(addr, "count", &owner).contains('3'));

    // A compatible change: the new index on `done` finds the three old rows at once.
    s.reload(&compat).unwrap();
    let n = query(addr, "count_done", "00");
    assert!(n.contains('3'), "count_done(false) = {n}");

    // Back to a fresh server to try the migration (compat -> migrate is also incompatible: `note`
    // would be dropped), v1 -> migrate runs the migration over the stored rows.
    s.shutdown();
    let s = Server::start(&v1, ("127.0.0.1", 0), MemKv::new(), vec![]).unwrap();
    let addr = s.addr();
    for t in ["x", "y"] {
        http(
            addr,
            "POST",
            &format!("/mutation/add?format=json&caller={ME}"),
            &format!("[\"{t}\"]"),
        );
    }
    s.reload(&migrate).unwrap();
    let got = query(addr, "with_status", "01");
    assert!(got.contains("0200000001000000780100000079"), "{got}");
    s.shutdown();
}
