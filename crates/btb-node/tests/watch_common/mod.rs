//! Local mock servers for the chain watchers: an Esplora-compatible REST API, a bitcoind
//! JSON-RPC endpoint and a Solana JSON-RPC endpoint. No external network is touched.
#![allow(dead_code)]

use bitcoin::absolute::LockTime;
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::hashes::Hash;
use bitcoin::{
    transaction::Version, Amount, Block, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut,
    Txid, Witness,
};
use btb_oracle::frost::addr;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

pub fn unb64(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    };
    let s: Vec<u8> = s.bytes().filter(|c| *c != b'=').collect();
    let mut out = Vec::new();
    for c in s.chunks(4) {
        let mut n = 0u32;
        for (i, x) in c.iter().enumerate() {
            n |= u32::from(val(*x)) << (18 - 6 * i);
        }
        for i in 0..c.len() - 1 {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    out
}

/// A running mock HTTP server; dropped: stops accepting.
pub struct Server {
    pub url: String,
    stop: Arc<AtomicBool>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::net::TcpStream::connect(self.url.trim_start_matches("http://"));
    }
}

/// `handler(method, path, body) -> (status, body)`.
pub fn serve(
    handler: impl Fn(&str, &str, &[u8]) -> (u16, Vec<u8>) + Send + Sync + 'static,
) -> Server {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let (st, h) = (stop.clone(), Arc::new(handler));
    std::thread::spawn(move || {
        for conn in l.incoming() {
            if st.load(Ordering::Relaxed) {
                break;
            }
            let Ok(mut c) = conn else { continue };
            let h = h.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let head_end = loop {
                    let n = c.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut lines = head.lines();
                let mut first = lines.next().unwrap_or("").split(' ');
                let (method, path) = (
                    first.next().unwrap_or("").to_string(),
                    first.next().unwrap_or("").to_string(),
                );
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap_or(0))
                    })
                    .unwrap_or(0);
                while buf.len() < head_end + len {
                    let n = c.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let (status, body) = h(&method, &path, &buf[head_end..]);
                let _ = write!(
                    c,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = c.write_all(&body);
            });
        }
    });
    Server { url, stop }
}

// ------------------------------------------------------------------ Bitcoin helpers

pub fn deposit_tx(
    vault_script: &[u8],
    sats: u64,
    memo: Option<&[u8; 32]>,
    seed: u8,
) -> Transaction {
    let mut output = vec![TxOut {
        value: Amount::from_sat(sats),
        script_pubkey: ScriptBuf::from_bytes(vault_script.to_vec()),
    }];
    if let Some(m) = memo {
        let mut s = vec![0x6a, 0x20];
        s.extend_from_slice(m);
        output.push(TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::from_bytes(s),
        });
    }
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::new(Txid::from_byte_array([seed; 32]), 0),
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output,
    }
}

#[derive(Default)]
pub struct BtcChain {
    /// Block hash (display hex) per height; index = height.
    pub hashes: Vec<String>,
    /// txid -> raw tx bytes
    pub raw: BTreeMap<String, Vec<u8>>,
    /// txid -> height of its block (absent: mempool)
    pub conf: BTreeMap<String, u64>,
    /// the address whose history lists these txids
    pub address_txs: Vec<String>,
    /// transactions accepted by `POST /tx` / `sendrawtransaction`
    pub broadcasts: Vec<Vec<u8>>,
    pub garbage: bool,
    /// serve this raw tx instead of the real one for these txids
    pub swap_raw: BTreeMap<String, Vec<u8>>,
    pub blocks: BTreeMap<u64, Block>,
    /// overrides the address given to `esplora` (set once the vault key exists)
    pub address: String,
}

impl BtcChain {
    pub fn tip(&self) -> u64 {
        self.hashes.len() as u64 - 1
    }
    pub fn mine(&mut self, n: usize, salt: u8) {
        for _ in 0..n {
            let h = self.hashes.len() as u64;
            let d = Sha256::digest([&h.to_le_bytes()[..], &[salt]].concat());
            self.hashes.push(hex(&d));
        }
    }
    /// Put `tx` into a block at `height` (and list it for the address).
    pub fn confirm(&mut self, tx: &Transaction, height: u64) -> String {
        let id = tx.compute_txid().to_string();
        self.raw.insert(id.clone(), serialize(tx));
        self.conf.insert(id.clone(), height);
        if !self.address_txs.contains(&id) {
            self.address_txs.push(id.clone());
        }
        id
    }
    pub fn mempool(&mut self, tx: &Transaction) -> String {
        let id = tx.compute_txid().to_string();
        self.raw.insert(id.clone(), serialize(tx));
        self.address_txs.push(id.clone());
        id
    }
    /// Replace the blocks from `from` on and un-confirm what was in them.
    pub fn reorg(&mut self, from: u64, salt: u8) {
        let n = self.hashes.len() - from as usize;
        self.hashes.truncate(from as usize);
        self.mine(n, salt);
        self.conf.retain(|_, h| *h < from);
    }
}

pub fn esplora(chain: Arc<Mutex<BtcChain>>, address: String) -> Server {
    serve(move |method, path, body| {
        let c = &mut *chain.lock().unwrap();
        let ok = |s: String| (200, s.into_bytes());
        let address = if c.address.is_empty() {
            address.clone()
        } else {
            c.address.clone()
        };
        if method == "POST" && path == "/tx" {
            let raw = unhex(std::str::from_utf8(body).unwrap().trim());
            let Ok(tx) = deserialize::<Transaction>(&raw) else {
                return (400, b"bad tx".to_vec());
            };
            let id = tx.compute_txid().to_string();
            if c.broadcasts.contains(&raw) {
                return (400, b"txn-already-known".to_vec());
            }
            c.broadcasts.push(raw.clone());
            c.raw.insert(id.clone(), raw);
            return ok(id);
        }
        if path == "/blocks/tip/height" {
            return ok(c.tip().to_string());
        }
        if let Some(h) = path.strip_prefix("/block-height/") {
            return match c.hashes.get(h.parse::<usize>().unwrap()) {
                Some(x) => ok(x.clone()),
                None => (404, b"not found".to_vec()),
            };
        }
        if path.starts_with(&format!("/address/{address}/txs")) {
            if c.garbage {
                return (200, b"{not json".to_vec());
            }
            if path.contains("/chain/") {
                return ok("[]".into()); // single page
            }
            let v: Vec<Value> = c
                .address_txs
                .iter()
                .map(|id| match c.conf.get(id) {
                    Some(h) => json!({"txid": id, "status": {"confirmed": true, "block_height": h, "block_hash": c.hashes[*h as usize]}}),
                    None => json!({"txid": id, "status": {"confirmed": false}}),
                })
                .collect();
            return ok(Value::Array(v).to_string());
        }
        if let Some(id) = path
            .strip_prefix("/tx/")
            .and_then(|p| p.strip_suffix("/hex"))
        {
            if let Some(r) = c.swap_raw.get(id) {
                return ok(hex(r));
            }
            return match c.raw.get(id) {
                Some(r) => ok(hex(r)),
                None => (404, b"nf".to_vec()),
            };
        }
        if let Some(id) = path
            .strip_prefix("/tx/")
            .and_then(|p| p.strip_suffix("/status"))
        {
            if !c.raw.contains_key(id) {
                return (404, b"nf".to_vec());
            }
            return ok(match c.conf.get(id) {
                Some(h) => json!({"confirmed": true, "block_height": h, "block_hash": c.hashes[*h as usize]}),
                None => json!({"confirmed": false}),
            }
            .to_string());
        }
        if path == "/fee-estimates" {
            return ok(json!({"1": 31.2, "3": 12.4, "6": 8.0}).to_string());
        }
        (404, b"unknown".to_vec())
    })
}

/// bitcoind JSON-RPC over the same chain data; blocks are built on demand from the confirmed
/// transactions (a coinbase-less block is fine: the watcher checks the header hash and the
/// merkle root, not proof of work).
pub fn bitcoind(chain: Arc<Mutex<BtcChain>>) -> Server {
    serve(move |_m, _p, body| {
        let req: Value = serde_json::from_slice(body).unwrap();
        let c = &mut *chain.lock().unwrap();
        let params = req["params"].as_array().cloned().unwrap_or_default();
        let result: Result<Value, (i64, &str)> = match req["method"].as_str().unwrap() {
            "getblockcount" => Ok(json!(c.tip())),
            "getblockhash" => {
                let h = params[0].as_u64().unwrap();
                if (h as usize) < c.hashes.len() {
                    Ok(json!(block_at(c, h).block_hash().to_string()))
                } else {
                    Err((-8, "out of range"))
                }
            }
            "getblock" => {
                let hash = params[0].as_str().unwrap().to_string();
                let h = (0..c.hashes.len() as u64)
                    .find(|h| block_at(c, *h).block_hash().to_string() == hash)
                    .unwrap();
                let mut block = block_at(c, h);
                if let Some(b) = c.blocks.get(&h) {
                    // a lying node: serves another block under this hash
                    block = b.clone();
                }
                Ok(json!(hex(&serialize(&block))))
            }
            "sendrawtransaction" => {
                let raw = unhex(params[0].as_str().unwrap());
                if c.broadcasts.contains(&raw) {
                    Err((-27, "Transaction already in block chain"))
                } else {
                    let tx: Transaction = deserialize(&raw).unwrap();
                    c.raw.insert(tx.compute_txid().to_string(), raw.clone());
                    c.broadcasts.push(raw);
                    Ok(json!(tx.compute_txid().to_string()))
                }
            }
            "getrawtransaction" => {
                let id = params[0].as_str().unwrap();
                if !c.raw.contains_key(id) {
                    Err((-5, "No such mempool or blockchain transaction"))
                } else {
                    Ok(match c.conf.get(id) {
                        Some(h) => json!({"confirmations": c.tip() - h + 1}),
                        None => json!({}),
                    })
                }
            }
            "estimatesmartfee" => Ok(json!({"feerate": 0.00012, "blocks": 3})),
            m => panic!("unexpected rpc {m}"),
        };
        let v = match result {
            Ok(r) => json!({"result": r, "error": null, "id": 1}),
            Err((code, msg)) => {
                json!({"result": null, "error": {"code": code, "message": msg}, "id": 1})
            }
        };
        (200, v.to_string().into_bytes())
    })
}

fn block_at(c: &BtcChain, h: u64) -> Block {
    let txs: Vec<Transaction> = c
        .conf
        .iter()
        .filter(|(_, ch)| **ch == h)
        .map(|(id, _)| deserialize(&c.raw[id]).unwrap())
        .collect();
    let mut filler = vec![deposit_tx(&[0x51], 1, None, h as u8)];
    filler.extend(txs);
    let mut b = mk_block(filler);
    b.header.nonce = u32::from_le_bytes(unhex(&c.hashes[h as usize])[..4].try_into().unwrap());
    b.header.time = h as u32;
    b
}

pub fn mk_block(txs: Vec<Transaction>) -> Block {
    let mut b = Block {
        header: bitcoin::block::Header {
            version: bitcoin::block::Version::from_consensus(0x2000_0000),
            prev_blockhash: bitcoin::BlockHash::all_zeros(),
            merkle_root: bitcoin::TxMerkleNode::all_zeros(),
            time: 1,
            bits: bitcoin::CompactTarget::from_consensus(0x207f_ffff),
            nonce: 0,
        },
        txdata: txs,
    };
    if let Some(r) = b.compute_merkle_root() {
        b.header.merkle_root = r;
    }
    b
}

// ------------------------------------------------------------------ Solana

#[derive(Default)]
pub struct SolChain {
    pub slot: u64,
    pub skipped: BTreeSet<u64>,
    /// signature (base58) -> (slot, getTransaction result)
    pub txs: Vec<(String, u64, Value)>,
    pub sent: Vec<Vec<u8>>,
    /// signature (base58) -> confirmationStatus
    pub status: BTreeMap<String, String>,
    pub garbage: bool,
}

pub fn blockhash(slot: u64) -> [u8; 32] {
    Sha256::digest([b"mock-blockhash".as_slice(), &slot.to_le_bytes()].concat()).into()
}

pub fn sol_deposit(
    sig: &[u8; 64],
    slot: u64,
    vault: &[u8; 32],
    lamports: u64,
    memo: Option<&str>,
) -> (String, u64, Value) {
    let sig58 = addr::base58_encode(sig);
    let payer = [7u8; 32];
    let mut keys = vec![
        addr::solana_address(&payer),
        addr::solana_address(vault),
        "11111111111111111111111111111111".to_string(),
    ];
    let mut ixs = Vec::new();
    if let Some(m) = memo {
        keys.push("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr".into());
        ixs.push(
            json!({"programIdIndex": 3, "accounts": [], "data": addr::base58_encode(m.as_bytes())}),
        );
    }
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    ixs.push(json!({"programIdIndex": 2, "accounts": [0, 1], "data": addr::base58_encode(&data)}));
    let tx = json!({
        "slot": slot,
        "meta": {"err": null, "preBalances": [10_000_000_000u64, 0, 1, 1], "postBalances": [10_000_000_000u64 - lamports, lamports, 1, 1]},
        "transaction": {"signatures": [sig58], "message": {"accountKeys": keys, "instructions": ixs, "header": {"numRequiredSignatures": 1}}}
    });
    (sig58, slot, tx)
}

pub fn solana(chain: Arc<Mutex<SolChain>>) -> Server {
    serve(move |_m, _p, body| {
        let c = &mut *chain.lock().unwrap();
        if c.garbage {
            return (200, b"<html>".to_vec());
        }
        let req: Value = serde_json::from_slice(body).unwrap();
        let params = req["params"].as_array().cloned().unwrap_or_default();
        let result: Result<Value, i64> = match req["method"].as_str().unwrap() {
            "getSlot" => Ok(json!(c.slot)),
            "getSignaturesForAddress" => Ok(Value::Array(
                c.txs
                    .iter()
                    .rev()
                    .filter(|(_, s, _)| *s <= c.slot)
                    .map(|(sig, slot, _)| json!({"signature": sig, "slot": slot, "err": null}))
                    .collect(),
            )),
            "getTransaction" => {
                let sig = params[0].as_str().unwrap();
                Ok(c.txs
                    .iter()
                    .find(|(s, _, _)| s == sig)
                    .map_or(Value::Null, |(_, _, t)| t.clone()))
            }
            "getBlock" => {
                let s = params[0].as_u64().unwrap();
                if c.skipped.contains(&s) || s > c.slot {
                    Err(-32007)
                } else {
                    Ok(json!({"blockhash": addr::base58_encode(&blockhash(s)), "blockHeight": s}))
                }
            }
            "sendTransaction" => {
                let wire = unb64(params[0].as_str().unwrap());
                let sig = addr::base58_encode(&wire[1..65]);
                c.sent.push(wire);
                c.status
                    .entry(sig.clone())
                    .or_insert_with(|| "processed".into());
                Ok(json!(sig))
            }
            "getSignatureStatuses" => {
                let sig = params[0][0].as_str().unwrap();
                Ok(
                    json!({"context": {"slot": c.slot}, "value": [c.status.get(sig).map(|s| json!({"confirmationStatus": s, "err": null}))]}),
                )
            }
            m => panic!("unexpected rpc {m}"),
        };
        let v = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "result": r, "id": 1}),
            Err(code) => {
                json!({"jsonrpc": "2.0", "error": {"code": code, "message": "x"}, "id": 1})
            }
        };
        (200, v.to_string().into_bytes())
    })
}
