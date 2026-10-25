//! Serving block sync to non-validators is opt-in, limited to sync requests, and rate limited.

#[path = "lite/mod.rs"]
mod lite;

use btb_node::net::{self, FollowerPolicy};
use btb_node::wire::{consensus_frame, sync_request_frame, tx_frame, Inbound, SyncResponse};
use lite::*;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A node that is not a validator: it dials the validators and sends raw frames.
struct Outsider {
    net: net::NetHandle,
    rx: Receiver<([u8; 32], Inbound)>,
    _rt: tokio::runtime::Runtime,
}

impl Outsider {
    fn start(c: &Lite, seed: u8) -> Outsider {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = channel();
        let tx = std::sync::Mutex::new(tx);
        let sink: net::Sink = Arc::new(move |from, msg| {
            let _ = tx.lock().unwrap().send((from, msg));
        });
        let keys: Vec<[u8; 32]> = c
            .keys
            .iter()
            .map(|k| k.verifying_key().to_bytes())
            .collect();
        let peers = (0..keys.len())
            .map(|i| (keys[i], c.addr_of(i).parse().unwrap()))
            .collect();
        let net = net::start(
            rt.handle(),
            net::NetParams {
                secret: [seed; 32],
                listen: "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
                validators: keys,
                peers,
                rate_per_sec: 1e9,
                rate_burst: 1e9,
                delay: Duration::ZERO,
                followers: None,
            },
            sink,
        )
        .unwrap();
        Outsider { net, rx, _rt: rt }
    }

    fn sync_response(&self, secs: u64) -> Option<SyncResponse> {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if let Ok((_, Inbound::SyncResp(r))) = self.rx.recv_timeout(Duration::from_millis(50)) {
                return Some(*r);
            }
        }
        None
    }
}

fn quick(c: &Lite) {
    c.wait_until("a few blocks", 30, || c.height(0) >= 5);
}

#[test]
fn validators_serve_nobody_else_by_default() {
    let _g = serial();
    let c = Lite::new(4, 600, None);
    quick(&c);
    let o = Outsider::start(&c, 200);
    let v0 = c.keys[0].verifying_key().to_bytes();
    // Give the connection time to form, then ask repeatedly.
    for _ in 0..5 {
        o.net.send(v0, sync_request_frame(1));
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(o.sync_response(1).is_none(), "an unlisted peer was served");
    assert_eq!(c.status(0)["sync"]["served"], 0);
    assert_eq!(c.status(0)["net"]["followers"], 0);
}

#[test]
fn opted_in_validators_serve_sync_and_only_sync() {
    let _g = serial();
    let mut c = Lite::new(4, 600, Some(FollowerPolicy::default()));
    quick(&c);
    let o = Outsider::start(&c, 201);
    let v0 = c.keys[0].verifying_key().to_bytes();
    let mut resp = None;
    for _ in 0..50 {
        o.net.send(v0, sync_request_frame(1));
        if let Some(r) = o.sync_response(1) {
            resp = Some(r);
            break;
        }
    }
    let r = resp.expect("an opted-in validator answers a follower");
    assert!(!r.blocks.is_empty() && r.blocks[0].height == 1);
    assert_eq!(c.status(0)["net"]["followers"], 1);

    // Anything but a sync request is refused and counted; the connection is not used for
    // consensus or transactions.
    let tx = c.faucet.transfer([9; 32], 5);
    for _ in 0..3 {
        o.net.send(v0, tx_frame(&tx));
    }
    let vote_like = consensus_frame(
        0,
        &btb_consensus::Message::SyncRequest {
            from: 0,
            block_id: [0; 32],
        },
    );
    o.net.send(v0, vote_like);
    c.wait_until("follower frames to be dropped", 10, || {
        c.status(0)["net"]["dropped_follower"].as_u64().unwrap() >= 4
    });
    assert_eq!(c.status(0)["consensus"]["evidence_found"], 0);
}

#[test]
fn follower_slots_and_request_rate_are_limited() {
    let _g = serial();
    let policy = FollowerPolicy {
        max_peers: 1,
        rate_per_sec: 0.5,
        rate_burst: 3.0,
    };
    let c = Lite::new(4, 600, Some(policy));
    quick(&c);
    let v0 = c.keys[0].verifying_key().to_bytes();
    let a = Outsider::start(&c, 202);
    c.wait_until("the first follower to be admitted", 20, || {
        a.net.send(v0, sync_request_frame(1));
        c.status(0)["net"]["followers"] == 1
    });
    // A second follower finds the slot taken.
    let b = Outsider::start(&c, 203);
    c.wait_until("the second follower to be refused", 20, || {
        b.net.send(v0, sync_request_frame(1));
        c.status(0)["net"]["dropped_follower"].as_u64().unwrap() >= 1
    });
    assert_eq!(c.status(0)["net"]["followers"], 1);

    // A flood from the admitted one is cut to the burst plus the refill.
    let served_before = c.status(0)["sync"]["served"].as_u64().unwrap();
    for _ in 0..60 {
        a.net.send(v0, sync_request_frame(1));
    }
    std::thread::sleep(Duration::from_secs(2));
    let served = c.status(0)["sync"]["served"].as_u64().unwrap() - served_before;
    assert!(served <= 6, "served {served} of 60 flooded requests");
    assert!(c.status(0)["net"]["dropped_rate"].as_u64().unwrap() >= 40);
}
