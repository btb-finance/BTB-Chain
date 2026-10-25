//! JSON-RPC views of storage nodes and stored files (`btb_chain::storage`). Amounts are decimal
//! strings, ids and addresses hex.
//!
//! | Method | Parameters | Result |
//! | --- | --- | --- |
//! | `btb_storageConfig` | `[]` | challenge period, deadlines, price, deposit parameters, `height` |
//! | `btb_storageNodes` | `[]` | registered storage nodes with endpoint, deposit, load |
//! | `btb_storageFile` | `[file_id_hex]` | the file, its escrow and its pieces with holders and endpoints, or `null` |
//! | `btb_storageFiles` | `[limit?]` | live files (without pieces) |
//! | `btb_storageDuties` | `[address_hex]` | the pieces assigned to a node, with open challenges |

use crate::driver::{RpcError, RpcResult};
use crate::hex;
use btb_chain::storage::{FileInfo, PieceInfo, PieceState};
use btb_chain::ChainState;
use btb_runtime::KvStore;
use serde_json::{json, Value};

fn app(e: impl ToString) -> RpcError {
    RpcError::app(e.to_string())
}

fn h32(v: &Value) -> Result<[u8; 32], RpcError> {
    hex::decode(
        v.as_str()
            .ok_or_else(|| RpcError::invalid_params("expected a hex string"))?,
    )
    .map_err(RpcError::invalid_params)?
    .try_into()
    .map_err(|_| RpcError::invalid_params("expected 32 bytes"))
}

fn state_name(s: PieceState) -> &'static str {
    match s {
        PieceState::Holding => "holding",
        PieceState::Repairing => "repairing",
        PieceState::Vacant => "vacant",
    }
}

fn file_json(f: &FileInfo) -> Value {
    json!({
        "id": hex::encode(&f.id),
        "owner": hex::encode(&f.owner),
        "size": f.size,
        "k": f.k,
        "m": f.m,
        "piece_len": f.piece_len,
        "periods": f.periods,
        "pay": f.pay.to_string(),
        "created": f.created,
        "live": f.live,
        "escrow": f.escrow.to_string(),
    })
}

fn challenge_json(p: &PieceInfo) -> Value {
    match &p.challenge {
        None => Value::Null,
        Some(c) => json!({"chunk": c.chunk, "deadline": c.deadline, "pay": c.pay.to_string()}),
    }
}

/// Handle one `btb_storage*` method against the head state.
pub fn handle<S: KvStore>(st: &ChainState<S>, method: &str, p: &[Value]) -> RpcResult {
    let arg = |i: usize| {
        p.get(i)
            .ok_or_else(|| RpcError::invalid_params(format!("missing parameter {i}")))
    };
    let height = st.head().map_err(app)?.height;
    match method {
        "btb_storageConfig" => {
            let c = st.storage_config();
            Ok(json!({
                "height": height,
                "epoch_blocks": c.epoch_blocks,
                "challenge_blocks": c.challenge_blocks,
                "repair_blocks": c.repair_blocks,
                "price_per_kib_period": c.price_per_kib_period.to_string(),
                "deposit_per_kib": c.deposit_per_kib.to_string(),
                "min_deposit": c.min_deposit.to_string(),
                "slash_bps": c.slash_bps,
            }))
        }
        "btb_storageNodes" => {
            let nodes: Vec<Value> = st
                .storage_nodes()
                .map_err(app)?
                .iter()
                .map(|n| {
                    json!({
                        "address": hex::encode(&n.address),
                        "endpoint": String::from_utf8_lossy(&n.endpoint),
                        "deposit": n.deposit.to_string(),
                        "held_bytes": n.held_bytes,
                        "pieces": n.pieces,
                        "registered": n.registered,
                    })
                })
                .collect();
            Ok(json!(nodes))
        }
        "btb_storageFile" => {
            let id = h32(arg(0)?)?;
            let Some(f) = st.storage_file(&id).map_err(app)? else {
                return Ok(Value::Null);
            };
            let mut pieces = Vec::new();
            for q in st.storage_pieces(&id).map_err(app)? {
                let endpoint = st
                    .storage_node(&q.node)
                    .map_err(app)?
                    .map(|n| String::from_utf8_lossy(&n.endpoint).into_owned())
                    .unwrap_or_default();
                pieces.push(json!({
                    "index": q.index,
                    "node": hex::encode(&q.node),
                    "endpoint": endpoint,
                    "state": state_name(q.state),
                    "periods_left": q.periods_left,
                    "next_due": q.next_due,
                    "challenge": challenge_json(&q),
                    "proven": q.proven,
                    "missed": q.missed,
                }));
            }
            let mut v = file_json(&f);
            v["pieces"] = json!(pieces);
            v["height"] = json!(height);
            Ok(v)
        }
        "btb_storageFiles" => {
            let limit = p.first().and_then(Value::as_u64).unwrap_or(100).min(1000) as usize;
            let files: Vec<Value> = st
                .storage_files(limit)
                .map_err(app)?
                .iter()
                .map(file_json)
                .collect();
            Ok(json!(files))
        }
        "btb_storageDuties" => {
            let node = h32(arg(0)?)?;
            let mut out = Vec::new();
            for q in st.storage_duties(&node).map_err(app)? {
                let Some(f) = st.storage_file(&q.file).map_err(app)? else {
                    continue;
                };
                out.push(json!({
                    "file": hex::encode(&q.file),
                    "index": q.index,
                    "size": f.size,
                    "k": f.k,
                    "m": f.m,
                    "piece_len": f.piece_len,
                    "state": state_name(q.state),
                    "challenge": challenge_json(&q),
                }));
            }
            Ok(json!(out))
        }
        _ => Err(RpcError {
            code: -32601,
            message: format!("unknown method {method}"),
        }),
    }
}
