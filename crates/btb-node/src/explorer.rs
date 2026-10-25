//! Read-side helpers for the block explorer: JSON views of transactions and the embedded page.
//!
//! The page is served by the node's HTTP API at `/explorer` (see `rpc`). It has no external
//! assets and talks to the same node through the JSON-RPC methods `btb_getBlocks`,
//! `btb_getBlock`, `btb_getTransaction`, `btb_getAccount`, `btb_getAccountTxs`, `btb_getEpoch`
//! and `btb_status`.

use crate::hex;
use btb_chain::{Transaction, TxKind};
use serde_json::{json, Value};

/// The explorer page (single file, no external requests).
pub const PAGE: &str = include_str!("../web/explorer.html");

/// The DEX page (`examples/native-dex/web`, single file, no external requests).
pub const DEX_PAGE: &str = include_str!("../../../examples/native-dex/web/index.html");

/// Short name of a transaction kind.
pub fn kind_name(k: &TxKind) -> &'static str {
    match k {
        TxKind::Deploy { .. } => "deploy",
        TxKind::Call { .. } => "call",
        TxKind::Transfer { .. } => "transfer",
        TxKind::Stake { .. } => "stake",
        TxKind::Unstake { .. } => "unstake",
        TxKind::RegisterValidator { .. } => "register_validator",
        TxKind::SubmitEvidence { .. } => "submit_evidence",
        TxKind::Channel(_) => "channel",
        TxKind::Site(_) => "site",
        TxKind::Dex(_) => "dex",
        TxKind::Storage(_) => "storage",
    }
}

/// A JSON view of a transaction (amounts as strings, bytes as hex).
pub fn tx_json(tx: &Transaction) -> Value {
    let mut v = json!({
        "id": hex::encode(&tx.id()),
        "from": hex::encode(&tx.sender()),
        "public_key": hex::encode(&tx.public_key),
        "chain_id": tx.body.chain_id,
        "nonce": tx.body.nonce,
        "kind": kind_name(&tx.body.kind),
        "gas_limit": tx.gas_limit(),
    });
    let o = v.as_object_mut().expect("object");
    match &tx.body.kind {
        TxKind::Transfer { to, amount } => {
            o.insert("to".into(), json!(hex::encode(to)));
            o.insert("amount".into(), json!(amount.to_string()));
        }
        TxKind::Call {
            contract,
            function,
            args,
            max_fee,
            ..
        } => {
            o.insert("to".into(), json!(hex::encode(contract)));
            o.insert("function".into(), json!(function));
            o.insert("args".into(), json!(hex::encode(args)));
            o.insert("max_fee".into(), json!(max_fee.to_string()));
        }
        TxKind::Deploy { code, manifest } => {
            o.insert("name".into(), json!(manifest.name));
            o.insert("code_bytes".into(), json!(code.len()));
        }
        TxKind::Stake { validator, amount } => {
            o.insert("validator".into(), json!(hex::encode(validator)));
            o.insert("amount".into(), json!(amount.to_string()));
        }
        TxKind::Unstake { amount } => {
            o.insert("amount".into(), json!(amount.to_string()));
        }
        TxKind::RegisterValidator { consensus_key, .. } => {
            o.insert("consensus_key".into(), json!(hex::encode(consensus_key)));
        }
        TxKind::SubmitEvidence { epoch, .. } => {
            o.insert("epoch".into(), json!(epoch));
        }
        TxKind::Channel(op) => {
            o.insert(
                "op".into(),
                json!(format!("{op:?}").split([' ', '(', '{']).next()),
            );
        }
        TxKind::Site(op) => {
            let (what, contract, path) = match op {
                btb_chain::site::SiteOp::Publish { contract, path, .. } => {
                    ("publish", contract, path)
                }
                btb_chain::site::SiteOp::Remove { contract, path } => ("remove", contract, path),
            };
            o.insert("op".into(), json!(what));
            o.insert("contract".into(), json!(hex::encode(contract)));
            o.insert("path".into(), json!(path));
        }
        TxKind::Dex(op) => {
            o.insert(
                "op".into(),
                json!(format!("{op:?}").split([' ', '(', '{']).next()),
            );
        }
        TxKind::Storage(op) => {
            o.insert(
                "op".into(),
                json!(format!("{op:?}").split([' ', '(', '{']).next()),
            );
        }
    }
    v
}

/// Addresses a transaction involves besides its sender (recipient or contract).
pub fn counterparty(tx: &Transaction) -> Option<[u8; 32]> {
    match &tx.body.kind {
        TxKind::Transfer { to, .. } => Some(*to),
        TxKind::Call { contract, .. } => Some(*contract),
        TxKind::Site(btb_chain::site::SiteOp::Publish { contract, .. })
        | TxKind::Site(btb_chain::site::SiteOp::Remove { contract, .. }) => Some(*contract),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use btb_chain::{SigningKey, TxBody};

    #[test]
    fn transfer_json_has_fields() {
        let k = SigningKey::from_bytes(&[3; 32]);
        let tx = Transaction::sign(
            TxBody {
                chain_id: 7,
                nonce: 2,
                kind: TxKind::Transfer {
                    to: [9; 32],
                    amount: 5,
                },
            },
            &k,
        );
        let j = tx_json(&tx);
        assert_eq!(j["kind"], "transfer");
        assert_eq!(j["amount"], "5");
        assert_eq!(j["nonce"], 2);
        assert_eq!(j["to"], hex::encode(&[9u8; 32]));
        assert_eq!(counterparty(&tx), Some([9; 32]));
    }

    #[test]
    fn page_is_self_contained() {
        assert!(!PAGE.contains("http://") && !PAGE.contains("https://"));
        assert!(!PAGE.contains("<script src") && !PAGE.contains("<link"));
    }
}
