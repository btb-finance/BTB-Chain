//! The edge's view of the chain: node JSON-RPC (channels, storage files), the storage client,
//! and the transaction that redeems a voucher.

use crate::wire;
use btb_chain::{account_address, ChannelOp, SigningKey, Transaction, TxBody, TxKind};
use btb_storage::chain::Chain;
use btb_storage::{client, hex, Address, Hash};
use btb_voucher::SignedVoucher;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A node's JSON-RPC.
pub trait Rpc: Send + Sync {
    fn call(&self, method: &str, params: Value) -> Result<Value, String>;
}

/// JSON-RPC over HTTP to several nodes; the first that answers wins.
pub struct HttpRpc {
    urls: Vec<String>,
    timeout: Duration,
}

impl HttpRpc {
    pub fn new(urls: Vec<String>, timeout: Duration) -> Self {
        HttpRpc { urls, timeout }
    }
}

impl Rpc for HttpRpc {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let body =
            json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
        let mut errors = Vec::new();
        for u in &self.urls {
            let r = wire::request(
                u,
                "POST",
                "/",
                &[("Content-Type".into(), "application/json".into())],
                body.as_bytes(),
                self.timeout,
                16 << 20,
            );
            match r {
                Ok(r) => {
                    let v: Value =
                        serde_json::from_slice(&r.body).map_err(|e| format!("rpc: {e}"))?;
                    if let Some(e) = v.get("error") {
                        // The node answered; another node would say the same.
                        let m = e.get("message").and_then(Value::as_str).unwrap_or("error");
                        return Err(format!("{method}: {m}"));
                    }
                    return v.get("result").cloned().ok_or("rpc: no result".into());
                }
                Err(e) => errors.push(e),
            }
        }
        Err(format!("no rpc node answered: {}", errors.join("; ")))
    }
}

/// Read-only [`Chain`] for the storage client.
pub struct RpcChain(pub Arc<dyn Rpc>);

impl Chain for RpcChain {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.0.call(method, params)
    }
    fn send(&self, _: btb_chain::storage::StorageOp) -> Result<Value, String> {
        Err("the edge does not send storage transactions".into())
    }
    fn address(&self) -> Address {
        [0; 32]
    }
}

/// Where large files come from, by storage root. The result must already be verified against
/// the root.
pub trait StorageSource: Send + Sync {
    fn fetch(&self, root: &Hash) -> Result<Vec<u8>, String>;
}

/// `btb-storage`'s `fetch`: asks the chain where the pieces are, downloads any `k` verified
/// pieces, decodes, and checks the result against the root.
pub struct ChainStorage(pub Arc<dyn Chain>);

impl StorageSource for ChainStorage {
    fn fetch(&self, root: &Hash) -> Result<Vec<u8>, String> {
        client::fetch(self.0.as_ref(), root)
    }
}

/// Settles a voucher on chain.
pub trait Submitter: Send + Sync {
    fn close(&self, voucher: &SignedVoucher) -> Result<(), String>;
}

/// Signs `ChannelOp::Close` with the edge's account key and sends it through node RPC.
pub struct RpcSubmitter {
    pub rpc: Arc<dyn Rpc>,
    pub key: SigningKey,
}

impl RpcSubmitter {
    pub fn address(&self) -> Address {
        account_address(&self.key.verifying_key().to_bytes())
    }
}

impl Submitter for RpcSubmitter {
    fn close(&self, voucher: &SignedVoucher) -> Result<(), String> {
        let chain_id = self.rpc.call("btb_status", json!([]))?["chain_id"]
            .as_u64()
            .ok_or("status has no chain_id")?;
        let nonce = self
            .rpc
            .call("btb_getBalance", json!([hex(&self.address())]))?["nonce"]
            .as_u64()
            .ok_or("balance has no nonce")?;
        let tx = Transaction::sign(
            TxBody {
                relayer: None,
                chain_id,
                nonce,
                kind: TxKind::Channel(ChannelOp::Close { voucher: *voucher }),
            },
            &self.key,
        );
        let r = self
            .rpc
            .call("btb_sendTransaction", json!([hex(&tx.encode())]))?;
        let id = r["id"].as_str().ok_or("no transaction id")?.to_string();
        let end = Instant::now() + Duration::from_secs(60);
        while Instant::now() < end {
            let rc = self.rpc.call("btb_getReceipt", json!([id]))?;
            if !rc.is_null() {
                return if rc["status"] == "success" {
                    Ok(())
                } else {
                    Err(format!("close transaction {id} failed: {rc}"))
                };
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(format!("no receipt for {id} after 60s"))
    }
}
