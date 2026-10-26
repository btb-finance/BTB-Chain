//! Phase 2 gate: "query a table by index and watch a UI update live", automated.
//!
//! Starts `btb-dev` on an ephemeral port with the real `examples/todo` contract, opens a
//! Server-Sent Events stream on `list_mine` (what the demo page does with `EventSource`),
//! posts mutations over HTTP and asserts the stream pushes the new state.

use btb_dev::Server;
use btb_runtime::MemKv;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

mod common;

fn start() -> Server {
    Server::start(
        &common::build_example("todo"),
        ("127.0.0.1", 0),
        MemKv::new(),
        vec![],
    )
    .unwrap()
}

/// One HTTP request; returns (status line, body).
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

fn post(addr: SocketAddr, who: &str, f: &str, args: &str) -> String {
    let (status, body) = http(
        addr,
        "POST",
        &format!("/mutation/{f}?format=json&caller={who}"),
        args,
    );
    assert!(status.contains("200"), "{status} {body}");
    body
}

/// A Server-Sent Events stream.
struct Sse(BufReader<TcpStream>);

impl Sse {
    fn open(addr: SocketAddr, target: &str) -> Sse {
        let mut s = TcpStream::connect(addr).unwrap();
        write!(
            s,
            "GET {target} HTTP/1.1\r\nHost: x\r\nAccept: text/event-stream\r\n\r\n"
        )
        .unwrap();
        let mut r = BufReader::new(s);
        let mut status = String::new();
        r.read_line(&mut status).unwrap();
        assert!(status.contains("200"), "{status}");
        loop {
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            if l.trim().is_empty() {
                break;
            }
        }
        Sse(r)
    }

    /// The next `data:` payload, or None if nothing arrives within `wait`.
    fn next(&mut self, wait: Duration) -> Option<String> {
        self.0.get_ref().set_read_timeout(Some(wait)).unwrap();
        loop {
            let mut l = String::new();
            match self.0.read_line(&mut l) {
                Ok(0) => panic!("stream closed"),
                Ok(_) => {
                    if let Some(d) = l.trim_end().strip_prefix("data: ") {
                        return Some(d.to_string());
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return None
                }
                Err(e) => panic!("{e}"),
            }
        }
    }
}

const LONG: Duration = Duration::from_secs(10);
const SHORT: Duration = Duration::from_millis(600);

#[test]
fn gate_index_query_updates_live_over_sse() {
    let server = start();
    let addr = server.addr();
    let list = "/subscribe/list_mine?format=json&caller=alice";
    let mut alice = Sse::open(addr, list);
    let mut bob = Sse::open(addr, "/subscribe/list_mine?format=json&caller=bob");

    // The first event is the current (empty) result.
    assert_eq!(alice.next(LONG).unwrap(), "[]");
    assert_eq!(bob.next(LONG).unwrap(), "[]");

    // Add: the stream pushes the new todo, found through the owner index.
    assert_eq!(post(addr, "alice", "add", r#"["buy milk"]"#), "0");
    assert_eq!(
        alice.next(LONG).unwrap(),
        r#"[{"id":0,"done":false,"text":"buy milk"}]"#
    );
    // Bob's stream does not fire for Alice's row (his index range is untouched).
    assert_eq!(bob.next(SHORT), None);

    // Another user's row does not disturb Alice's stream either.
    assert_eq!(post(addr, "bob", "add", r#"["bob's thing"]"#), "1");
    assert_eq!(
        bob.next(LONG).unwrap(),
        r#"[{"id":1,"done":false,"text":"bob's thing"}]"#
    );
    assert_eq!(alice.next(SHORT), None);

    // Toggle: the row changes, the stream pushes the new value.
    assert_eq!(post(addr, "alice", "toggle", "[0]"), "true");
    assert_eq!(
        alice.next(LONG).unwrap(),
        r#"[{"id":0,"done":true,"text":"buy milk"}]"#
    );

    // A second add arrives as a second event holding both items.
    post(addr, "alice", "add", r#"["walk dog"]"#);
    assert_eq!(
        alice.next(LONG).unwrap(),
        r#"[{"id":0,"done":true,"text":"buy milk"},{"id":2,"done":false,"text":"walk dog"}]"#
    );

    // A plain query sees the same state; a new subscriber starts from it.
    let (st, body) = http(addr, "GET", "/query/list_mine?format=json&caller=alice", "");
    assert!(st.contains("200"));
    assert!(body.contains("walk dog"));
    let mut late = Sse::open(addr, list);
    assert!(late.next(LONG).unwrap().contains("walk dog"));
    drop((alice, bob, late));
    server.shutdown();
}

#[test]
fn errors_are_reported_and_change_nothing() {
    let server = start();
    let addr = server.addr();
    let mut alice = Sse::open(addr, "/subscribe/list_mine?format=json&caller=alice");
    assert_eq!(alice.next(LONG).unwrap(), "[]");

    let (st, body) = http(addr, "POST", "/mutation/toggle?caller=alice", "[5]");
    assert!(
        st.contains("400") && body.contains("no such todo"),
        "{st} {body}"
    );
    let (st, _) = http(addr, "POST", "/mutation/nope?caller=alice", "[]");
    assert!(st.contains("404"), "{st}");
    let (st, _) = http(addr, "GET", "/mutation/add", "");
    assert!(st.contains("405"), "{st}");
    let (st, _) = http(addr, "POST", "/mutation/add?caller=alice", "[1,2");
    assert!(st.contains("400"), "{st}");
    // A query cannot be run as a mutation.
    let (st, _) = http(addr, "POST", "/mutation/list_mine?caller=alice", "[]");
    assert!(st.contains("404"), "{st}");
    assert_eq!(alice.next(SHORT), None, "failed calls must not push events");

    // Hex args work too: add("hi") = len 2 LE u32 + "hi".
    let (st, body) = http(
        addr,
        "POST",
        "/mutation/add?caller=alice&args=0200000068690a",
        "",
    );
    assert!(st.contains("400"), "trailing bytes rejected: {st} {body}");
    let (st, body) = http(
        addr,
        "POST",
        "/mutation/add?caller=alice&args=020000006869",
        "",
    );
    assert!(
        st.contains("200") && body == "0000000000000000",
        "{st} {body}"
    );
    assert!(alice.next(LONG).unwrap().contains("\"hi\""));
    server.shutdown();
}
