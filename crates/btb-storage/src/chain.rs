//! The chain as the storage code sees it: a JSON-RPC caller that can also sign and send
//! storage transactions as one account. `btb-cli` implements [`Chain`] over a node's RPC;
//! tests use fakes.
//!
//! RPC methods used (see `btb-node/src/storage_rpc.rs`): `btb_storageFile` and
//! `btb_storageDuties`. Amounts are decimal strings.

use crate::{unhex32, Address, Hash};
use btb_chain::storage::{Challenge, PieceState, StorageOp};
use serde_json::{json, Value};

pub trait Chain: Send + Sync {
    /// A JSON-RPC call.
    fn call(&self, method: &str, params: Value) -> Result<Value, String>;
    /// Sign `op` with this account's key, submit it and wait for a successful receipt.
    fn send(&self, op: StorageOp) -> Result<Value, String>;
    /// This account's address.
    fn address(&self) -> Address;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieceView {
    pub index: u8,
    pub node: Address,
    pub endpoint: String,
    pub state: PieceState,
    pub challenge: Option<Challenge>,
    pub proven: u32,
    pub missed: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileView {
    pub id: Hash,
    pub owner: Address,
    pub size: u64,
    pub k: u8,
    pub m: u8,
    pub piece_len: u64,
    pub escrow: u64,
    pub live: u32,
    pub pieces: Vec<PieceView>,
}

/// A piece the account has to keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Duty {
    pub file: Hash,
    pub size: u64,
    pub k: u8,
    pub m: u8,
    pub piece_len: u64,
    pub index: u8,
    pub state: PieceState,
    pub challenge: Option<Challenge>,
}

fn s<'a>(v: &'a Value, k: &str) -> Result<&'a str, String> {
    v[k].as_str().ok_or_else(|| format!("missing field {k}"))
}
fn n(v: &Value, k: &str) -> Result<u64, String> {
    match &v[k] {
        Value::Number(x) => x.as_u64().ok_or_else(|| format!("bad number {k}")),
        Value::String(x) => x.parse().map_err(|_| format!("bad number {k}")),
        _ => Err(format!("missing field {k}")),
    }
}

fn state_of(v: &Value) -> Result<PieceState, String> {
    Ok(match s(v, "state")? {
        "holding" => PieceState::Holding,
        "repairing" => PieceState::Repairing,
        "vacant" => PieceState::Vacant,
        o => return Err(format!("unknown piece state {o}")),
    })
}

fn challenge_of(v: &Value) -> Result<Option<Challenge>, String> {
    let c = &v["challenge"];
    if c.is_null() {
        return Ok(None);
    }
    Ok(Some(Challenge {
        chunk: n(c, "chunk")?,
        deadline: n(c, "deadline")?,
        pay: n(c, "pay")?,
    }))
}

/// The file with its pieces, or `None` if the chain does not know it (never stored, or ended).
pub fn file(chain: &dyn Chain, id: &Hash) -> Result<Option<FileView>, String> {
    let v = chain.call("btb_storageFile", json!([crate::hex(id)]))?;
    if v.is_null() {
        return Ok(None);
    }
    let pieces = v["pieces"]
        .as_array()
        .ok_or("missing pieces")?
        .iter()
        .map(|p| {
            Ok(PieceView {
                index: n(p, "index")? as u8,
                node: unhex32(s(p, "node")?)?,
                endpoint: s(p, "endpoint")?.to_string(),
                state: state_of(p)?,
                challenge: challenge_of(p)?,
                proven: n(p, "proven")? as u32,
                missed: n(p, "missed")? as u32,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Some(FileView {
        id: *id,
        owner: unhex32(s(&v, "owner")?)?,
        size: n(&v, "size")?,
        k: n(&v, "k")? as u8,
        m: n(&v, "m")? as u8,
        piece_len: n(&v, "piece_len")?,
        escrow: n(&v, "escrow")?,
        live: n(&v, "live")? as u32,
        pieces,
    }))
}

/// The pieces assigned to `node`.
pub fn duties(chain: &dyn Chain, node: &Address) -> Result<Vec<Duty>, String> {
    let v = chain.call("btb_storageDuties", json!([crate::hex(node)]))?;
    v.as_array()
        .ok_or("duties is not a list")?
        .iter()
        .map(|d| {
            Ok(Duty {
                file: unhex32(s(d, "file")?)?,
                size: n(d, "size")?,
                k: n(d, "k")? as u8,
                m: n(d, "m")? as u8,
                piece_len: n(d, "piece_len")?,
                index: n(d, "index")? as u8,
                state: state_of(d)?,
                challenge: challenge_of(d)?,
            })
        })
        .collect()
}
