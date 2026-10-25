//! Talking to a node: `btb keygen`, `btb deploy --rpc`, `btb call --rpc`, `btb query --rpc`.
//! Plain HTTP/1.1 JSON-RPC over `std::net` (the node's RPC has no TLS, see docs/NODE.md).
use btb_chain::{
    account_address, contract_address, Manifest, SigningKey, Transaction, TxBody, TxKind,
};
use btb_codec::Encode;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::{Duration, Instant};

pub const DEFAULT_CALL_GAS: u64 = 2_000_000;

pub fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return Err(format!("bad hex {s:?}"));
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|_| format!("bad hex {s:?}")))
        .collect()
}

fn hex32(s: &str) -> Result<[u8; 32], String> {
    hex_decode(s)?
        .try_into()
        .map_err(|_| format!("{s:?} is not 32 bytes of hex"))
}

/// Read a key file in the format `btb-node init-testnet` writes (`{"secret": hex, ...}`).
pub fn load_key(path: &Path) -> Result<SigningKey, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read key {}: {e}", path.display()))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("key json: {e}"))?;
    let secret = v
        .get("secret")
        .and_then(Value::as_str)
        .ok_or("key file needs a `secret` field")?;
    Ok(SigningKey::from_bytes(&hex32(secret)?))
}

/// Write a fresh key (mode 0600) to `path`; refuses to overwrite. Returns the account address.
pub fn keygen(path: &Path) -> Result<[u8; 32], String> {
    use std::io::Write as _;
    let mut seed = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut seed))
        .map_err(|e| format!("random bytes: {e}"))?;
    let key = SigningKey::from_bytes(&seed);
    let public = key.verifying_key().to_bytes();
    let text = serde_json::to_string_pretty(
        &json!({"secret": hex_encode(&key.to_bytes()), "public": hex_encode(&public)}),
    )
    .expect("json");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(account_address(&public))
}

/// A node's JSON-RPC endpoint (`host:port` or `http://host:port`).
pub struct Rpc(String);

impl Rpc {
    pub fn new(url: &str) -> Rpc {
        let u = url.strip_prefix("http://").unwrap_or(url);
        Rpc(u.trim_end_matches('/').to_string())
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let body =
            json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
        let addr = self
            .0
            .to_socket_addrs()
            .map_err(|e| format!("rpc address {}: {e}", self.0))?
            .next()
            .ok_or("rpc address does not resolve")?;
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
            .map_err(|e| format!("connect {}: {e}", self.0))?;
        s.set_read_timeout(Some(Duration::from_secs(30))).ok();
        let req = format!(
            "POST / HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.0,
            body.len()
        );
        s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).map_err(|e| e.to_string())?;
        let at = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or("bad http response")?;
        let v: Value = serde_json::from_slice(&raw[at + 4..]).map_err(|e| format!("rpc: {e}"))?;
        if let Some(e) = v.get("error") {
            let m = e.get("message").and_then(Value::as_str).unwrap_or("error");
            return Err(format!("{method}: {m}"));
        }
        v.get("result").cloned().ok_or("rpc: no result".into())
    }
}

/// Everything needed to sign for one account against one node.
pub struct Session {
    rpc: Rpc,
    key: SigningKey,
    chain_id: u64,
}

impl Session {
    pub fn open(rpc: &str, key_file: &Path) -> Result<Session, String> {
        let rpc = Rpc::new(rpc);
        let st = rpc.call("btb_status", json!([]))?;
        let chain_id = st["chain_id"].as_u64().ok_or("status has no chain_id")?;
        Ok(Session {
            rpc,
            key: load_key(key_file)?,
            chain_id,
        })
    }

    pub fn sender(&self) -> [u8; 32] {
        account_address(&self.key.verifying_key().to_bytes())
    }

    fn nonce(&self) -> Result<u64, String> {
        let r = self
            .rpc
            .call("btb_getBalance", json!([hex_encode(&self.sender())]))?;
        r["nonce"].as_u64().ok_or("balance has no nonce".into())
    }

    /// Sign `kind` with the account's next nonce, submit it and wait for the receipt.
    /// Returns `(nonce, receipt)`; a failed transaction is an error.
    pub fn send(&self, kind: TxKind) -> Result<(u64, Value), String> {
        let nonce = self.nonce()?;
        let tx = Transaction::sign(
            TxBody {
                chain_id: self.chain_id,
                nonce,
                kind,
            },
            &self.key,
        );
        let r = self
            .rpc
            .call("btb_sendTransaction", json!([hex_encode(&tx.encode())]))?;
        let id = r["id"].as_str().ok_or("no transaction id")?.to_string();
        let end = Instant::now() + Duration::from_secs(60);
        while Instant::now() < end {
            let rc = self.rpc.call("btb_getReceipt", json!([id]))?;
            if !rc.is_null() {
                if rc["status"] != "success" {
                    return Err(format!("transaction {id} failed: {rc}"));
                }
                return Ok((nonce, rc));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(format!("no receipt for {id} after 60s"))
    }

    /// Deploy a bundle directory written by `btb deploy`. Returns the contract address.
    pub fn deploy_bundle(&self, dir: &Path, gas_limit: Option<u64>) -> Result<[u8; 32], String> {
        let (code, manifest) = read_bundle(dir)?;
        let kind = TxKind::Deploy { code, manifest };
        if let Some(g) = gas_limit {
            let need = Transaction::sign(
                TxBody {
                    chain_id: self.chain_id,
                    nonce: 0,
                    kind: kind.clone(),
                },
                &self.key,
            )
            .intrinsic_gas();
            if g < need {
                return Err(format!("--gas-limit {g} is below the deploy cost {need}"));
            }
        }
        let (nonce, _) = self.send(kind)?;
        Ok(contract_address(&self.sender(), nonce))
    }

    pub fn call(
        &self,
        contract: [u8; 32],
        function: &str,
        args: Vec<u8>,
        gas_limit: u64,
    ) -> Result<Value, String> {
        let (_, rc) = self.send(TxKind::Call {
            contract,
            kind: btb_vm::FunctionKind::Mutation,
            function: function.into(),
            args,
            gas_limit,
            max_fee: u64::MAX,
        })?;
        Ok(rc)
    }

    pub fn query(&self, contract: [u8; 32], function: &str, args: &[u8]) -> Result<Value, String> {
        self.rpc.call(
            "btb_query",
            json!([hex_encode(&contract), function, hex_encode(args)]),
        )
    }
}

/// `(elf, manifest)` from a bundle directory (`manifest.json` plus the ELF it names).
pub fn read_bundle(dir: &Path) -> Result<(Vec<u8>, Manifest), String> {
    let text = std::fs::read_to_string(dir.join("manifest.json"))
        .map_err(|e| format!("read manifest.json: {e}"))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("manifest.json: {e}"))?;
    let name = v["name"].as_str().ok_or("manifest.json: name")?.to_string();
    let elf_name = v["elf"].as_str().ok_or("manifest.json: elf")?;
    let code = std::fs::read(dir.join(elf_name)).map_err(|e| format!("read {elf_name}: {e}"))?;
    let crons = v["crons"]
        .as_array()
        .ok_or("manifest.json: crons")?
        .iter()
        .map(|c| {
            Ok((
                c["function"].as_str().ok_or("cron function")?.to_string(),
                c["every_blocks"].as_u64().ok_or("cron every_blocks")?,
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let indexes = v["indexes"]
        .as_array()
        .ok_or("manifest.json: indexes")?
        .iter()
        .map(|i| i.as_str().map(String::from).ok_or("index spec".to_string()))
        .collect::<Result<Vec<_>, String>>()?;
    let manifest = Manifest {
        name,
        crons,
        indexes,
    };
    manifest.validate()?;
    Ok((code, manifest))
}

/// Contract arguments: a JSON array (`[5, "alice", true, "0x<64 hex>"]`: integers as u64 LE,
/// strings length-prefixed, 32-byte hex strings as raw addresses, bools as one byte) or
/// canonical-encoding bytes as hex. Empty means no arguments.
pub fn parse_args(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if !s.starts_with('[') {
        return hex_decode(s);
    }
    let v: Vec<Value> = serde_json::from_str(s).map_err(|e| format!("args json: {e}"))?;
    let mut out = Vec::new();
    for x in &v {
        match x {
            Value::Bool(b) => b.encode_to(&mut out),
            Value::Number(n) => {
                if let Some(u) = n.as_u64() {
                    u.encode_to(&mut out)
                } else if let Some(i) = n.as_i64() {
                    i.encode_to(&mut out)
                } else {
                    return Err(format!("unsupported number {n} (floats are not allowed)"));
                }
            }
            Value::String(t) => match t.strip_prefix("0x").map(hex32) {
                Some(Ok(a)) => out.extend_from_slice(&a),
                _ => t.encode_to(&mut out),
            },
            other => return Err(format!("unsupported argument {other}; use hex bytes")),
        }
    }
    Ok(out)
}

pub fn parse_address(s: &str) -> Result<[u8; 32], String> {
    hex32(s)
}
