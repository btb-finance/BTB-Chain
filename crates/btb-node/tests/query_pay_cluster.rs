//! Paid `btb_query` end to end (proposal 0004, phase B0): four real validators, node 0 charges
//! for queries over its HTTP RPC. A client uses its free quota, is told to pay, opens a payment
//! channel to node 0's account, pays with vouchers (charged by gas used), is refused a replayed
//! voucher, and node 0 settles the channel on chain by itself once it earned enough.

#[path = "../../btb-tests/tests/common/mod.rs"]
mod common;
mod lite;

use btb_chain::channel::{channel_id, ChannelOp};
use btb_chain::{account_address, TxKind};
use btb_node::hex;
use btb_node::query_pay::{QueryPayConfig, PAYMENT_REQUIRED};
use btb_voucher::Spender;
use lite::Lite;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};

const DEPOSIT: u64 = 10_000_000;

fn cfg() -> QueryPayConfig {
    QueryPayConfig {
        free_queries: 2,
        window_ms: 3_600_000,
        price_per_mgas: 1_000_000, // 1 base unit per gas
        min_charge: 100,
        redeem_min: 1_000_000,
    }
}

/// POST one JSON-RPC call to `addr` with extra headers; returns the JSON body.
fn post(addr: SocketAddr, method: &str, params: Value, headers: &[(&str, String)]) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let mut s = TcpStream::connect(addr).unwrap();
    let mut req = format!(
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(&body);
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let json = out.split("\r\n\r\n").nth(1).unwrap();
    serde_json::from_str(json).unwrap()
}

#[test]
fn queries_are_paid_with_vouchers_and_the_node_settles_on_chain() {
    let _g = lite::serial();
    let elf = common::build_example("counter");
    let mut c = Lite::with_params(4, 4, 1_000, None, Default::default(), |i, p| {
        if i == 0 {
            p.query_pay = Some(cfg());
        }
    });
    let counter = c.deploy(&elf);
    let rpc = c.node(0).rpc_addr().unwrap();
    let other = c.node(1).rpc_addr().unwrap();
    let q = || json!([hex::encode(&counter), "get"]);
    let client = |ip: &str| vec![("X-Real-IP", ip.to_string())];

    // Free quota: two queries, then payment required.
    for _ in 0..2 {
        let r = post(rpc, "btb_query", q(), &client("1.2.3.4"));
        assert_eq!(r["result"]["charged"], "0", "{r}");
    }
    let r = post(rpc, "btb_query", q(), &client("1.2.3.4"));
    assert_eq!(r["error"]["code"], PAYMENT_REQUIRED, "{r}");
    assert!(r["error"]["message"]
        .as_str()
        .unwrap()
        .contains("free quota"));
    // Another client still has its own quota; a node without query_pay stays free.
    assert_eq!(
        post(rpc, "btb_query", q(), &client("5.6.7.8"))["result"]["charged"],
        "0"
    );
    assert!(post(other, "btb_query", q(), &client("1.2.3.4"))["result"]["charged"].is_null());

    // The client opens a channel to node 0's account (the account of its consensus key).
    let node0 = account_address(&lite::key(10).verifying_key().to_bytes());
    let info = post(rpc, "btb_queryPay", json!([]), &[]);
    assert_eq!(info["result"]["payee"], hex::encode(&node0));
    let open = c.faucet.sign(TxKind::Channel(ChannelOp::Open {
        payee: node0,
        deposit: DEPOSIT,
        expiry_blocks: 10_000,
    }));
    let ch = channel_id(&c.faucet.addr(), open.body.nonce);
    c.run(&open);
    let mut spender = Spender::new(lite::CHAIN_ID, ch, DEPOSIT);

    // A paid query: charged by gas used (1 unit per gas, at least 100).
    let v1 = spender.pay(50_000, &c.faucet.key).unwrap();
    let pay = |v: &btb_voucher::SignedVoucher| {
        let mut h = client("1.2.3.4");
        h.push(("X-BTB-Voucher", hex::encode(&v.to_bytes())));
        h
    };
    let r = post(rpc, "btb_query", q(), &pay(&v1));
    let gas = r["result"]["gas_used"].as_u64().expect("query ran");
    let charged: u64 = r["result"]["charged"].as_str().unwrap().parse().unwrap();
    assert_eq!(charged, gas.max(100), "{r}");

    // A replayed voucher is refused (and does not fall back to the quota).
    let r = post(rpc, "btb_query", q(), &pay(&v1));
    assert_eq!(r["error"]["code"], PAYMENT_REQUIRED, "{r}");
    assert!(r["error"]["message"]
        .as_str()
        .unwrap()
        .contains("voucher refused"));
    // A voucher signed by someone else is refused.
    let mut forger = Spender::new(lite::CHAIN_ID, ch, DEPOSIT);
    let forged = forger.pay(60_000, &lite::key(77)).unwrap();
    assert_eq!(
        post(rpc, "btb_query", q(), &pay(&forged))["error"]["code"],
        PAYMENT_REQUIRED
    );

    // Pay past redeem_min: the node closes the channel on chain by itself.
    let before = c
        .node(0)
        .call("btb_getBalance", json!([hex::encode(&node0)]))
        .unwrap();
    let before: u64 = before["balance"].as_str().map_or_else(
        || before["balance"].as_u64().unwrap(),
        |s| s.parse().unwrap(),
    );
    let v2 = spender.pay(1_950_000, &c.faucet.key).unwrap(); // cumulative 2,000,000
    let r = post(rpc, "btb_query", q(), &pay(&v2));
    assert!(r["result"]["output"].is_string(), "{r}");
    c.wait_until("the node to settle the channel", 30, || {
        c.node(0)
            .call("btb_channel", json!([hex::encode(&ch)]))
            .unwrap()
            .is_null()
    });
    let after = c
        .node(0)
        .call("btb_getBalance", json!([hex::encode(&node0)]))
        .unwrap();
    let after: u64 = after["balance"].as_str().map_or_else(
        || after["balance"].as_u64().unwrap(),
        |s| s.parse().unwrap(),
    );
    // Node 0 received the 2,000,000 the client signed for, minus the gas of its Close (and
    // plus whatever validator rewards landed meanwhile, which only add).
    assert!(
        after + 100_000 > before + 2_000_000,
        "before {before} after {after}"
    );
    let st = post(rpc, "btb_queryPay", json!([]), &[])["result"].clone();
    assert_eq!(st["redeemed_total"], "2000000", "{st}");
    assert_eq!(st["paid_served"], 2, "{st}");
    // The channel is gone: further vouchers for it are refused.
    let v3 = spender.pay(1_000, &c.faucet.key).unwrap();
    assert_eq!(
        post(rpc, "btb_query", q(), &pay(&v3))["error"]["code"],
        PAYMENT_REQUIRED
    );
}
