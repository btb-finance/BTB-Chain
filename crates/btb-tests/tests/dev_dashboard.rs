//! The dev dashboard: `/dashboard` page and read-only `/admin/...` JSON endpoints, checked
//! against the real `examples/todo` contract.

use btb_dev::{parse_fields, IndexSpec, Server};
use btb_runtime::MemKv;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

mod common;

fn start() -> Server {
    let s = Server::start(
        &common::build_example("todo"),
        ("127.0.0.1", 0),
        MemKv::new(),
        vec![IndexSpec::parse("Todo:owner:addr@0").unwrap()],
    )
    .unwrap();
    s.set_fields(vec![parse_fields("Todo:addr,bool,string").unwrap()]);
    s
}

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

fn add(addr: SocketAddr, who: &str, text: &str) {
    let (st, body) = http(
        addr,
        "POST",
        &format!("/mutation/add?caller={who}"),
        &format!("[\"{text}\"]"),
    );
    assert!(st.contains("200"), "{st} {body}");
}

fn get(addr: SocketAddr, target: &str) -> String {
    let (st, body) = http(addr, "GET", target, "");
    assert!(st.contains("200"), "{target}: {st} {body}");
    body
}

fn alice_hex() -> String {
    "aa".repeat(32)
}

#[test]
fn dashboard_page_and_overview_list_tables_with_counts() {
    let server = start();
    let a = server.addr();
    let page = get(a, "/dashboard");
    assert!(page.contains("<title>BTB Dashboard</title>"));
    assert!(!page.contains("http://") && !page.contains("https://"));

    // Empty contract: no tables yet.
    assert!(get(a, "/admin/overview").contains("\"tables\":[]"));

    for t in ["one", "two", "three"] {
        add(a, &alice_hex(), t);
    }
    add(a, "bob", "b1");
    let o = get(a, "/admin/overview");
    assert!(
        o.contains(
            r#"{"name":"Todo","rows":4,"indexes":["owner"],"fields":["addr","bool","string"]}"#
        ),
        "{o}"
    );
    assert!(o.contains("\"height\":5"), "{o}");
    assert!(o.contains("\"merkle\":{\"tracked\":false"));
    // Storage is metered: rows and index entries cost a deposit.
    assert!(!o.contains("\"storage_deposit\":0"), "{o}");
    assert_eq!(get(a, "/admin/tables"), o);
    server.shutdown();
}

#[test]
fn table_rows_are_paged_and_decoded() {
    let server = start();
    let a = server.addr();
    for i in 0..5 {
        add(a, &alice_hex(), &format!("item {i}"));
    }
    let p1 = get(a, "/admin/table/Todo?limit=2");
    assert!(p1.contains("\"total\":5"));
    assert!(p1.contains(&format!(
        "\"decoded\":[\"{}\",false,\"item 0\"]",
        alice_hex()
    )));
    assert!(p1.contains("\"id\":0") && p1.contains("\"id\":1") && !p1.contains("\"id\":2"));
    let cur = p1
        .split("\"next_cursor\":\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let p2 = get(a, &format!("/admin/table/Todo?limit=2&cursor={cur}"));
    assert!(p2.contains("item 2") && p2.contains("item 3") && !p2.contains("item 1"));
    let cur = p2
        .split("\"next_cursor\":\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let p3 = get(a, &format!("/admin/table/Todo?limit=2&cursor={cur}"));
    assert!(p3.contains("item 4") && p3.ends_with("\"next_cursor\":null}"));
    // Descending order, single row, unknown table.
    assert!(get(a, "/admin/table/Todo?limit=1&order=desc").contains("item 4"));
    assert!(get(a, "/admin/table/Todo/3").contains("item 3"));
    let (st, _) = http(a, "GET", "/admin/table/Nope", "");
    assert!(st.contains("404"));
    server.shutdown();
}

#[test]
fn raw_hex_without_hint_and_decode_error_on_mismatch() {
    let server = start();
    let a = server.addr();
    add(a, &alice_hex(), "hi");
    server.set_fields(vec![]);
    let r = get(a, "/admin/table/Todo");
    assert!(r.contains("\"hex\":\"") && !r.contains("decoded"));
    server.set_fields(vec![parse_fields("Todo:u64").unwrap()]);
    assert!(get(a, "/admin/table/Todo").contains("decode_error"));
    assert!(parse_fields("Todo:float").is_err());
    server.shutdown();
}

#[test]
fn browse_by_index_value_and_list_entries() {
    let server = start();
    let a = server.addr();
    add(a, &alice_hex(), "a1");
    add(a, "bob", "b1");
    add(a, &alice_hex(), "a2");
    let r = get(a, &format!("/admin/index/Todo/owner?value={}", alice_hex()));
    assert!(
        r.contains("a1") && r.contains("a2") && !r.contains("b1"),
        "{r}"
    );
    let r = get(
        a,
        &format!("/admin/index/Todo/owner?value={}&limit=1", alice_hex()),
    );
    assert!(r.contains("a1") && !r.contains("a2") && !r.contains("\"next_cursor\":null"));
    let e = get(a, "/admin/index/Todo/owner");
    assert_eq!(e.matches("\"id\":").count(), 3, "{e}");
    let (st, _) = http(a, "GET", "/admin/index/Todo/nope", "");
    assert!(st.contains("404"));
    let (st, _) = http(a, "GET", "/admin/index/Todo/owner?value=zz", "");
    assert!(st.contains("400"));
    server.shutdown();
}

#[test]
fn storage_tokens_and_read_only() {
    let server = start();
    let a = server.addr();
    assert_eq!(
        get(a, "/admin/storage"),
        "{\"keys\":[],\"next_cursor\":null}"
    );
    assert_eq!(get(a, "/admin/tokens"), "{\"tokens\":[]}");
    let (st, _) = http(a, "POST", "/admin/overview", "");
    assert!(st.contains("405"));
    let (st, _) = http(a, "GET", "/admin/other", "");
    assert!(st.contains("404"));
    server.shutdown();
}

#[test]
fn events_stream_reports_touched_tables_after_commit() {
    let server = start();
    let a = server.addr();
    let mut s = TcpStream::connect(a).unwrap();
    write!(s, "GET /admin/events HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let mut r = BufReader::new(s);
    let mut line = String::new();
    loop {
        line.clear();
        r.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
    }
    r.get_ref()
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let next = |r: &mut BufReader<TcpStream>| loop {
        let mut l = String::new();
        r.read_line(&mut l).unwrap();
        if let Some(d) = l.strip_prefix("data: ") {
            return d.trim().to_string();
        }
    };
    assert!(next(&mut r).contains("\"tables\":[]"));
    add(a, &alice_hex(), "live");
    let ev = next(&mut r);
    assert!(
        ev.contains("\"tables\":[\"Todo\"]") && ev.contains("\"height\":2"),
        "{ev}"
    );
    server.shutdown();
}
