//! Multi-contract `btb-dev` and file storage in the dev server, with the real `examples/caller`
//! (a router), `examples/token` and `examples/files` contracts.

use btb_codec::{Address as CA, Decode, Encode};
use btb_dev::{dev_address, ContractSpec, Server};
use btb_runtime::MemKv;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

mod common;

fn spec(name: &str, example: &str) -> ContractSpec {
    ContractSpec {
        name: name.into(),
        elf: common::build_example(example),
    }
}

fn start(specs: &[ContractSpec]) -> Server {
    Server::start_multi(specs, ("127.0.0.1", 0), MemKv::new(), vec![], None).unwrap()
}

fn raw(addr: SocketAddr, method: &str, target: &str, body: &[u8]) -> (String, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    s.write_all(body).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let at = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&buf[..at]).into_owned();
    (
        head.lines().next().unwrap().to_string(),
        buf[at + 4..].to_vec(),
    )
}

fn text(addr: SocketAddr, method: &str, target: &str) -> (String, String) {
    let (s, b) = raw(addr, method, target, b"");
    (s, String::from_utf8_lossy(&b).into_owned())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

/// Run a mutation of `contract` as `who` (a name hashed by the server, or 64 hex).
fn mutate<T: Decode>(a: SocketAddr, contract: &str, who: &str, f: &str, args: &impl Encode) -> T {
    let (st, body) = text(
        a,
        "POST",
        &format!(
            "/c/{contract}/mutation/{f}?caller={who}&args={}",
            hex(&args.encode())
        ),
    );
    assert!(st.contains("200"), "{f}: {st} {body}");
    T::decode(&unhex(&body)).unwrap()
}

fn mutate_fails(a: SocketAddr, contract: &str, f: &str, args: &impl Encode) -> String {
    let (st, body) = text(
        a,
        "POST",
        &format!(
            "/c/{contract}/mutation/{f}?caller=x&args={}",
            hex(&args.encode())
        ),
    );
    assert!(st.contains("400"), "{f}: {st} {body}");
    body
}

fn query<T: Decode>(a: SocketAddr, contract: &str, f: &str, args: &impl Encode) -> T {
    let (st, body) = text(
        a,
        "GET",
        &format!("/c/{contract}/query/{f}?args={}", hex(&args.encode())),
    );
    assert!(st.contains("200"), "{f}: {st} {body}");
    T::decode(&unhex(&body)).unwrap()
}

fn ca(a: [u8; 32]) -> CA {
    CA(a)
}

struct Sse(BufReader<TcpStream>);

impl Sse {
    fn open(addr: SocketAddr, target: &str) -> Sse {
        let mut s = TcpStream::connect(addr).unwrap();
        write!(s, "GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut r = BufReader::new(s);
        loop {
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            if l.trim().is_empty() {
                break;
            }
        }
        Sse(r)
    }
    fn next(&mut self, wait: Duration) -> Option<String> {
        self.0.get_ref().set_read_timeout(Some(wait)).unwrap();
        loop {
            let mut l = String::new();
            match self.0.read_line(&mut l) {
                Ok(0) => panic!("closed"),
                Ok(_) => {
                    if let Some(d) = l.trim_end().strip_prefix("data: ") {
                        return Some(d.into());
                    }
                }
                Err(_) => return None,
            }
        }
    }
}

fn two() -> (Server, [u8; 32], [u8; 32]) {
    let s = start(&[spec("router", "caller"), spec("token", "token")]);
    let (router, token) = (dev_address("router"), dev_address("token"));
    // The router holds the tokens: it is the caller of `init`.
    let _: () = mutate(s.addr(), "token", &hex(&router), "init", &(1_000u128,));
    (s, router, token)
}

#[test]
fn router_calls_token_through_the_dev_resolver() {
    let (s, router, token) = two();
    let a = s.addr();
    assert_eq!(
        s.contracts(),
        vec![("router".to_string(), router), ("token".to_string(), token)]
    );
    assert_ne!(router, token);
    let bob = [0xB0; 32];
    // A query of the router asks the token's query.
    let b: u128 = query(a, "router", "balance_via", &(ca(token), ca(router)));
    assert_eq!(b, 1_000);
    // A mutation of the router calls the token's mutation; the token sees the router.
    let ok: bool = mutate(
        a,
        "router",
        "anyone",
        "forward_transfer",
        &(ca(token), ca(bob), 250u128),
    );
    assert!(ok);
    assert_eq!(query::<u128>(a, "token", "balance_of", &(ca(bob),)), 250);
    assert_eq!(query::<u128>(a, "token", "balance_of", &(ca(router),)), 750);
    assert_eq!(
        query::<u128>(a, "router", "balance_via", &(ca(token), ca(bob))),
        250
    );
    // The token aborts on more than the router holds; the router sees Aborted (3).
    let code: u8 = mutate(
        a,
        "router",
        "anyone",
        "try_transfer",
        &(ca(token), ca(bob), 9_999u128),
    );
    assert_eq!(code, 3, "the token aborts: CallError::Aborted");
    // Calling a contract that is not there: the router reports NoContract (1).
    let code: u8 = mutate(
        a,
        "router",
        "anyone",
        "try_transfer",
        &(ca([7; 32]), ca(bob), 1u128),
    );
    assert_eq!(code, 1);
    // Unknown contract names are 404.
    let (st, _) = text(a, "GET", "/c/nope/query/balance_of");
    assert!(st.contains("404"), "{st}");
    // Listing: names, addresses, entry points.
    let (_, list) = text(a, "GET", "/contracts");
    assert!(
        list.contains(&hex(&router)) && list.contains(&hex(&token)),
        "{list}"
    );
    assert!(list.contains("\"name\":\"router\"") && list.contains("forward_transfer"));
    // Dashboard endpoints take the contract.
    let (st, o) = text(a, "GET", "/admin/overview?contract=token");
    assert!(
        st.contains("200") && o.contains(&hex(&token)) && o.contains("\"name\":\"token\""),
        "{o}"
    );
    let (_, o) = text(a, "GET", "/admin/overview");
    assert!(
        o.contains(&hex(&router)),
        "primary is the first contract: {o}"
    );
    s.shutdown();
}

#[test]
fn reloading_one_contract_keeps_the_others_state_and_calls_still_work() {
    let (s, router, token) = two();
    let a = s.addr();
    let bob = [0xB0; 32];
    let _: bool = mutate(
        a,
        "router",
        "x",
        "forward_transfer",
        &(ca(token), ca(bob), 100u128),
    );
    let _: u8 = mutate(
        a,
        "router",
        "x",
        "try_transfer",
        &(ca(token), ca(bob), 1u128),
    );
    assert_eq!(query::<u64>(a, "router", "attempts", &()), 1);

    let mut live = Sse::open(
        a,
        &format!(
            "/c/router/subscribe/balance_via?args={}",
            hex(&(ca(token), ca(bob)).encode())
        ),
    );
    let first = live.next(Duration::from_secs(10)).unwrap();
    assert_eq!(u128::decode(&unhex(&first)).unwrap(), 101);

    // Reload only the token (same build): the router's storage and the token's rows survive.
    let token_elf = common::build_example("token");
    s.reload_contract("token", &token_elf).unwrap();
    assert_eq!(query::<u64>(a, "router", "attempts", &()), 1);
    assert_eq!(query::<u128>(a, "token", "balance_of", &(ca(bob),)), 101);
    // Reload only the router.
    let router_elf = common::build_example("caller");
    s.reload_contract("router", &router_elf).unwrap();
    assert_eq!(query::<u128>(a, "token", "balance_of", &(ca(router),)), 899);
    assert_eq!(query::<u64>(a, "router", "attempts", &()), 1);
    assert!(s.reload_contract("missing", &router_elf).is_err());
    // Addresses are stable across reloads, and cross-contract calls still resolve.
    assert_eq!(s.contracts()[0].1, router);
    let ok: bool = mutate(
        a,
        "router",
        "x",
        "forward_transfer",
        &(ca(token), ca(bob), 9u128),
    );
    assert!(ok);
    // The live subscription of the router (reading the token) saw the new balance.
    let mut last = first;
    while let Some(n) = live.next(Duration::from_secs(2)) {
        last = n;
    }
    assert_eq!(u128::decode(&unhex(&last)).unwrap(), 110);
    s.shutdown();
}

fn files_world() -> (Server, [u8; 32]) {
    let s = start(&[spec("files", "files")]);
    (s, dev_address("files"))
}

fn upload(a: SocketAddr, data: &[u8], q: &str) -> (String, u64) {
    let (st, body) = raw(a, "POST", &format!("/upload{q}"), data);
    assert!(
        st.contains("200"),
        "{st} {}",
        String::from_utf8_lossy(&body)
    );
    let j = String::from_utf8(body).unwrap();
    let root = j
        .split("\"root\":\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let size: u64 = j
        .split("\"size\":")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    (root.to_string(), size)
}

fn info(a: SocketAddr, root: &str) -> (u64, u8, u64) {
    let r: [u8; 32] = unhex(root).try_into().unwrap();
    query(a, "files", "info", &(ca(r),))
}

#[test]
fn upload_attach_query_and_fetch_by_url() {
    let (s, _) = files_world();
    let a = s.addr();
    let data: Vec<u8> = (0..70_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let (root, size) = upload(a, &data, "");
    assert_eq!(size, 70_000);
    // The root is exactly what `btb store` would print.
    let want = btb_storage::erasure::encode(&data, 4, 2).unwrap().id;
    assert_eq!(root, hex(&want));
    // Stored and paid: file_info says so; the mutation accepts the root.
    let (sz, status, expiry) = info(a, &root);
    assert_eq!((sz, status), (70_000, 1));
    assert!(expiry > 1000);
    let r: [u8; 32] = want;
    let got: u64 = mutate(a, "files", "alice", "attach", &("photo".to_string(), ca(r)));
    assert_eq!(got, 70_000);
    assert!(query::<bool>(
        a,
        "files",
        "is_available",
        &("photo".to_string(),)
    ));
    let stored: Option<CA> = query(a, "files", "root_of", &("photo".to_string(),));
    assert_eq!(stored, Some(ca(r)));
    // Fetch the bytes by URL.
    let (st, bytes) = raw(a, "GET", &format!("/file/{root}"), b"");
    assert!(st.contains("200"));
    assert_eq!(bytes, data);
    // A file nobody uploaded is 404; a bad root is 400.
    let (st, _) = raw(a, "GET", &format!("/file/{}", "00".repeat(32)), b"");
    assert!(st.contains("404"), "{st}");
    let (st, _) = raw(a, "GET", "/file/zz", b"");
    assert!(st.contains("400"), "{st}");
    // The dashboard lists it.
    let (_, f) = text(a, "GET", "/admin/files");
    assert!(
        f.contains(&root) && f.contains("\"status\":\"stored\""),
        "{f}"
    );
    s.shutdown();
}

#[test]
fn unknown_unpaid_and_late_paid_files() {
    let (s, _) = files_world();
    let a = s.addr();
    let unknown = [9u8; 32];
    assert_eq!(info(a, &hex(&unknown)), (0, 0, 0));
    assert!(
        mutate_fails(a, "files", "attach", &("n".to_string(), ca(unknown))).contains("not stored")
    );
    // Uploaded but not paid for: the bytes are served, the contract refuses the root.
    let data = vec![42u8; 5_000];
    let (root, _) = upload(a, &data, "?store=0");
    assert_eq!(info(a, &root).1, 0);
    let r: [u8; 32] = unhex(&root).try_into().unwrap();
    assert!(mutate_fails(a, "files", "attach", &("n".to_string(), ca(r))).contains("not stored"));
    // The client pays (a free StoreFile in dev); now it is accepted.
    let (st, _) = text(a, "POST", &format!("/file/{root}/store?periods=5"));
    assert!(st.contains("200"), "{st}");
    assert_eq!(info(a, &root).1, 1);
    let sz: u64 = mutate(a, "files", "alice", "attach", &("n".to_string(), ca(r)));
    assert_eq!(sz, 5_000);
    s.shutdown();
}

fn balance_of_contract(a: SocketAddr, name: &str) -> u64 {
    let (_, o) = text(a, "GET", &format!("/admin/overview?contract={name}"));
    let v = o.split("\"balance\":").nth(1).unwrap();
    v.split(',').next().unwrap().parse().unwrap()
}

#[test]
fn contract_funded_pin_pays_escrow_and_expires() {
    let (s, _) = files_world();
    let a = s.addr();
    let data = vec![7u8; 20_000];
    let enc = btb_storage::erasure::encode(&data, 4, 2).unwrap();
    let escrow = btb_dev::files_config()
        .escrow_for(enc.size, 4, 2, 1)
        .unwrap();
    let before = balance_of_contract(a, "files");
    // The root was never uploaded: pinning only needs the shape.
    let code: u8 = mutate(
        a,
        "files",
        "alice",
        "pin_and_attach",
        &("doc".to_string(), ca(enc.id), enc.size, 4u8, 2u8, 1u32),
    );
    assert_eq!(code, 0);
    let after = balance_of_contract(a, "files");
    assert!(
        before - after >= escrow && before - after < escrow + 10_000,
        "{before} {after} {escrow}"
    );
    assert_eq!(info(a, &hex(&enc.id)).1, 1);
    // The same root again costs nothing more.
    let _: u8 = mutate(
        a,
        "files",
        "alice",
        "pin_and_attach",
        &("doc2".to_string(), ca(enc.id), enc.size, 4u8, 2u8, 1u32),
    );
    assert!(after - balance_of_contract(a, "files") < 10_000);
    // A pin the balance cannot cover is a status, not a failure.
    let big = btb_storage::erasure::encode(&vec![1u8; 100_000], 4, 2).unwrap();
    let code: u8 = mutate(
        a,
        "files",
        "alice",
        "pin_and_attach",
        &(
            "big".to_string(),
            ca(big.id),
            1u64 << 30,
            4u8,
            2u8,
            1_000_000u32,
        ),
    );
    assert_eq!(code, 2);
    // One period is ten dev blocks: after that the file is expired.
    for i in 0..12 {
        let _: () = mutate(
            a,
            "files",
            "alice",
            "attach_unchecked",
            &(format!("n{i}"), ca([i as u8; 32])),
        );
    }
    assert_eq!(info(a, &hex(&enc.id)).1, 2);
    s.shutdown();
}
