//! JSON-RPC view of payment channels (`btb_chain::channel`).
//!
//! | Method | Parameters | Result |
//! | --- | --- | --- |
//! | `btb_channel` | `[channel_id_hex]` | `{payer, payer_key, payee, deposit, expiry_height, closing, height}`, or `null` |
//!
//! Hosting and edge nodes use it to check a voucher's channel before serving against it.
//! `deposit` is a decimal string; `closing` is `null` or `{deadline, offered}`.

use crate::driver::{RpcError, RpcResult};
use crate::hex;
use btb_chain::ChainState;
use btb_runtime::KvStore;
use serde_json::{json, Value};

/// Handle one `btb_channel*` method against the head state.
pub fn handle<S: KvStore>(st: &ChainState<S>, method: &str, p: &[Value]) -> RpcResult {
    match method {
        "btb_channel" => {
            let id: [u8; 32] = hex::decode(
                p.first()
                    .and_then(Value::as_str)
                    .ok_or_else(|| RpcError::invalid_params("expected a channel id"))?,
            )
            .map_err(RpcError::invalid_params)?
            .try_into()
            .map_err(|_| RpcError::invalid_params("expected 32 bytes"))?;
            let height = st.head().map_err(|e| RpcError::app(e.to_string()))?.height;
            let c = st.channel(&id).map_err(|e| RpcError::app(e.to_string()))?;
            Ok(match c {
                None => Value::Null,
                Some(c) => json!({
                    "payer": hex::encode(&c.payer),
                    "payer_key": hex::encode(&c.payer_key),
                    "payee": hex::encode(&c.payee),
                    "deposit": c.deposit.to_string(),
                    "expiry_height": c.expiry_height,
                    "closing": c.closing.map(|x| json!({"deadline": x.deadline, "offered": x.offered.to_string()})),
                    "height": height,
                }),
            })
        }
        _ => Err(RpcError {
            code: -32601,
            message: format!("method not found: {method}"),
        }),
    }
}
