//! `btb store <file>`, `btb fetch <root>` and `btb storage-node run`: the client and the daemon
//! of `btb-storage` over a node's JSON-RPC. See `docs/STORAGE.md`.

use crate::chain::{hex_decode, hex_encode, load_key, Rpc, Session};
use btb_chain::storage::StorageOp;
use btb_chain::{account_address, TxKind};
use btb_storage::chain::Chain;
use btb_storage::client;
use btb_storage::daemon::{DaemonConfig, StorageNode};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

/// A signing session as a [`Chain`].
pub struct SessionChain(pub Session);

impl Chain for SessionChain {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.0.rpc_call(method, params)
    }
    fn send(&self, op: StorageOp) -> Result<Value, String> {
        self.0.send(TxKind::Storage(op)).map(|(_, rc)| rc)
    }
    fn address(&self) -> [u8; 32] {
        self.0.sender()
    }
}

/// Reads only (`btb fetch` without `--key`).
pub struct ReadChain(pub Rpc);

impl Chain for ReadChain {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.0.call(method, params)
    }
    fn send(&self, _: StorageOp) -> Result<Value, String> {
        Err("this operation needs --key".into())
    }
    fn address(&self) -> [u8; 32] {
        [0; 32]
    }
}

#[derive(Default)]
struct Opts {
    rpc: Option<String>,
    key: Option<String>,
    k: Option<u8>,
    m: Option<u8>,
    periods: Option<u32>,
    out: Option<String>,
    dir: Option<String>,
    listen: Option<String>,
    endpoint: Option<String>,
    deposit: Option<u64>,
    positional: Vec<String>,
}

fn parse(args: Vec<String>) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut it = args.into_iter();
    fn num<T: std::str::FromStr>(
        it: &mut impl Iterator<Item = String>,
        f: &str,
    ) -> Result<T, String> {
        it.next()
            .and_then(|v| v.parse().ok())
            .ok_or(format!("{f} needs a number"))
    }
    while let Some(a) = it.next() {
        let mut val = |f: &str| it.next().ok_or(format!("{f} needs a value"));
        match a.as_str() {
            "--rpc" => o.rpc = Some(val("--rpc")?),
            "--key" => o.key = Some(val("--key")?),
            "--out" => o.out = Some(val("--out")?),
            "--dir" => o.dir = Some(val("--dir")?),
            "--listen" => o.listen = Some(val("--listen")?),
            "--endpoint" => o.endpoint = Some(val("--endpoint")?),
            "--k" => o.k = Some(num(&mut it, "--k")?),
            "--m" => o.m = Some(num(&mut it, "--m")?),
            "--periods" => o.periods = Some(num(&mut it, "--periods")?),
            "--deposit" => o.deposit = Some(num(&mut it, "--deposit")?),
            _ if a.starts_with("--") => return Err(format!("unknown flag {a}")),
            _ => o.positional.push(a),
        }
    }
    Ok(o)
}

fn session(o: &Opts) -> Result<SessionChain, String> {
    let rpc = o.rpc.as_deref().ok_or("--rpc is required")?;
    let key = o.key.as_deref().ok_or("--key is required")?;
    Ok(SessionChain(Session::open(rpc, &PathBuf::from(key))?))
}

/// Run `store`, `fetch` or `storage-node` with its arguments.
pub fn run(cmd: &str, rest: Vec<String>) -> Result<(), String> {
    match cmd {
        "store" => {
            let mut rest = rest;
            let o = parse(std::mem::take(&mut rest))?;
            let [path] = o.positional.as_slice() else {
                return Err(
                    "usage: btb store <file> --rpc URL --key FILE [--k N --m N --periods N]".into(),
                );
            };
            let data = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
            let chain = session(&o)?;
            let s = client::store(
                &chain,
                &data,
                o.k.unwrap_or(4),
                o.m.unwrap_or(2),
                o.periods.unwrap_or(12),
            )?;
            println!("root: {}", hex_encode(&s.id));
            println!("size: {}", s.size);
            println!("pieces: {} data + {} parity", s.k, s.m);
            for (i, (node, ep)) in s.assignments.iter().enumerate() {
                println!("piece {i}: {} {ep}", hex_encode(node));
            }
            if s.failed.is_empty() {
                Ok(())
            } else {
                for (i, e) in &s.failed {
                    eprintln!("piece {i} was not uploaded: {e}");
                }
                Err(format!(
                    "{} of {} pieces were not uploaded (the file is registered on chain)",
                    s.failed.len(),
                    s.assignments.len()
                ))
            }
        }
        "fetch" => {
            let o = parse(rest)?;
            let [root] = o.positional.as_slice() else {
                return Err("usage: btb fetch <root> --rpc URL [--out FILE]".into());
            };
            let id: [u8; 32] = hex_decode(root)?
                .try_into()
                .map_err(|_| "root must be 32 bytes of hex".to_string())?;
            let rpc = o.rpc.as_deref().ok_or("--rpc is required")?;
            let data = client::fetch(&ReadChain(Rpc::new(rpc)), &id)?;
            let out = o
                .out
                .clone()
                .unwrap_or_else(|| format!("{}.bin", &root[..root.len().min(16)]));
            std::fs::write(&out, &data).map_err(|e| format!("write {out}: {e}"))?;
            println!("fetched {} bytes to {out}", data.len());
            Ok(())
        }
        "storage-node" => {
            let mut rest = rest;
            if rest.first().map(String::as_str) != Some("run") {
                return Err("usage: btb storage-node run --rpc URL --key FILE --dir DIR [--listen ADDR] [--endpoint URL] [--deposit UNITS]".into());
            }
            rest.remove(0);
            let o = parse(rest)?;
            let dir = PathBuf::from(o.dir.clone().ok_or("--dir is required")?);
            let listen = o.listen.clone().unwrap_or_else(|| "127.0.0.1:0".into());
            let chain: Arc<SessionChain> = Arc::new(session(&o)?);
            let me = account_address(
                &load_key(&PathBuf::from(o.key.as_ref().expect("checked")))?
                    .verifying_key()
                    .to_bytes(),
            );
            let node = StorageNode::start(
                chain.clone(),
                DaemonConfig::new(dir, listen.parse().map_err(|e| format!("--listen: {e}"))?),
            )?;
            let endpoint = o
                .endpoint
                .clone()
                .unwrap_or_else(|| format!("http://{}", node.addr()));
            let listed = chain
                .call("btb_storageNodes", serde_json::json!([]))?
                .as_array()
                .cloned()
                .unwrap_or_default();
            let mine = listed
                .iter()
                .find(|n| n["address"].as_str() == Some(&hex_encode(&me)));
            let needs = match mine {
                None => o.deposit.is_some(),
                Some(n) => n["endpoint"].as_str() != Some(endpoint.as_str()),
            };
            if needs {
                chain.send(StorageOp::Register {
                    endpoint: endpoint.clone().into_bytes(),
                    deposit: o.deposit.unwrap_or(0),
                })?;
            } else if mine.is_none() {
                eprintln!("warning: not registered; pass --deposit UNITS to register");
            }
            println!("storage node: {endpoint} account {}", hex_encode(&me));
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
        other => Err(format!("unknown command {other}")),
    }
}
