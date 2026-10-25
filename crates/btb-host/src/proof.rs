//! Merkle proofs on answers: build them on a hosting node, verify them anywhere.
//!
//! A [`ProofBundle`] says: "at the state after block `height`, whose Merkle root is
//! `state_root`, these keys had these values (or were absent)". Each entry carries a Jellyfish
//! Merkle proof (`btb_runtime::merkle`). The bundle may also carry the **header of block
//! `height + 1`**, whose `state_root` field is the root after block `height`, and the
//! **consensus evidence** that block `height + 1` is committed: consecutive consensus blocks of
//! one epoch whose quorum certificates satisfy the 2-chain rule (the same evidence block sync
//! checks, `btb_node::sync::verify`).
//!
//! # What this proves, and what it does not
//!
//! * [`Verified::entries`]: each value is exactly what the tree under `state_root` holds.
//!   Nothing else is needed, and this part is cryptographic.
//! * With a header: `state_root` is the one the chain's header for block `height + 1` names, and
//!   the header hash covers its parent hash, timestamp and transaction root.
//! * With consensus evidence and the validator set of the epoch (a trust anchor the caller
//!   supplies): block `height + 1` **with these transactions** was committed by a quorum.
//!
//! What no validator signs today is the state root itself. A consensus certificate covers the
//! block payload (the transactions), and the header's `state_root` is what every node derived by
//! executing the chain before it. So a proof shows that the host's claimed root is consistent
//! with a committed block, not that a quorum attested the root. To rely on a root, compare it
//! with a validator's `btb_getBlock` (`state_root_after`) or another independent host, or
//! re-execute. Validator-signed state roots are a planned consensus change (`docs/HOSTING.md`).

use btb_chain::{Block, Header};
use btb_consensus::Context;
use btb_node::hex;
use btb_node::sync::{self, Expect};
use btb_node::wire::{Batch, SyncResponse};
use btb_runtime::merkle::{self, Proof};
use serde_json::{json, Value};

pub type Hash = [u8; 32];

/// One key of a bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The full state key (`01 contract key` for contract storage).
    pub key: Vec<u8>,
    /// `None`: the key is proven absent.
    pub value: Option<Vec<u8>>,
    pub proof: Proof,
}

/// Consensus evidence for the block after `height`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub epoch: u64,
    /// Consecutive consensus blocks of `epoch`, starting at `height + 1`.
    pub sync: SyncResponse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofBundle {
    /// State version: the state after block `height` (genesis is 0).
    pub height: u64,
    pub state_root: Hash,
    pub entries: Vec<Entry>,
    /// Header of block `height + 1`, if the host has executed it.
    pub header: Option<Header>,
    pub commit: Option<Commit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProofError {
    /// Malformed JSON bundle.
    Format(String),
    /// Entry `n` does not verify against `state_root`.
    BadEntry(usize),
    /// The header does not name `state_root` / the next height, or its hash is not what it claims.
    BadHeader,
    /// The consensus blocks do not match the header's block or are not proven committed.
    BadCommit(String),
    /// Consensus evidence without a header.
    CommitWithoutHeader,
}

/// What a bundle established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub height: u64,
    pub state_root: Hash,
    /// `(key, value)` pairs, value `None` for proven absence.
    pub entries: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    /// The root equals the `state_root` of block `height + 1`'s header (hash checked).
    pub header_bound: bool,
    /// Block `height + 1`, with the transactions its header commits to, was committed by a
    /// quorum of the validators in the context the caller supplied.
    pub quorum_committed: bool,
}

/// Check a bundle. `ctx` is the consensus context of the epoch of block `height + 1`, built by
/// the caller from a validator set it trusts (see [`context_from_epoch_json`]); without it the
/// consensus evidence is not checked and `quorum_committed` stays false.
pub fn verify_bundle(b: &ProofBundle, ctx: Option<&Context>) -> Result<Verified, ProofError> {
    for (i, e) in b.entries.iter().enumerate() {
        if !merkle::verify(&b.state_root, &e.key, e.value.as_deref(), &e.proof) {
            return Err(ProofError::BadEntry(i));
        }
    }
    let mut out = Verified {
        height: b.height,
        state_root: b.state_root,
        entries: b
            .entries
            .iter()
            .map(|e| (e.key.clone(), e.value.clone()))
            .collect(),
        header_bound: false,
        quorum_committed: false,
    };
    let Some(h) = &b.header else {
        return if b.commit.is_some() {
            Err(ProofError::CommitWithoutHeader)
        } else {
            Ok(out)
        };
    };
    if h.height != b.height + 1 || h.state_root != b.state_root {
        return Err(ProofError::BadHeader);
    }
    out.header_bound = true;
    let Some(c) = &b.commit else {
        return Ok(out);
    };
    let bad = |m: &str| ProofError::BadCommit(m.to_string());
    let first = c.sync.blocks.first().ok_or_else(|| bad("no blocks"))?;
    if first.height != h.height || c.sync.epoch != c.epoch {
        return Err(bad("evidence is for another block"));
    }
    // The header must be the one derived from the committed payload: same transactions (an
    // executor that rejects a block runs it empty, which the header then reflects).
    let batch = Batch::decode(&first.payload);
    let txs = batch.map(|b| b.txs).unwrap_or_default();
    let derives = |txs: Vec<btb_chain::Transaction>| {
        Block::new(h.height, h.parent_hash, h.timestamp, h.state_root, txs).header == *h
    };
    if !(derives(txs) || derives(Vec::new())) {
        return Err(bad("header does not match the committed payload"));
    }
    let Some(ctx) = ctx else {
        return Ok(out);
    };
    let expect = Expect {
        ctx,
        epoch: c.epoch,
        first_height: first.height,
        parent: first.parent,
        end_height: u64::MAX / 2,
    };
    match sync::verify(&c.sync, &expect) {
        Ok(n) if n >= 1 => out.quorum_committed = true,
        Ok(_) => return Err(bad("block not proven committed")),
        Err(e) => return Err(ProofError::BadCommit(format!("{e:?}"))),
    }
    Ok(out)
}

/// Build the consensus context of an epoch from the JSON a validator returns for
/// `btb_getEpoch` (`validators: [{key, stake}]`, `anchor`).
pub fn context_from_epoch_json(chain_id: u64, epoch: u64, ep: &Value) -> Result<Context, String> {
    use btb_consensus::{Validator, ValidatorSet};
    let vals = ep["validators"]
        .as_array()
        .ok_or("no validators in epoch record")?
        .iter()
        .map(|v| {
            Ok(Validator {
                key: hex::decode32(v["key"].as_str().ok_or("validator key")?)?,
                stake: v["stake"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| v["stake"].as_u64().map(|n| n.to_string()))
                    .ok_or("validator stake")?
                    .parse()
                    .map_err(|_| "validator stake")?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let set = ValidatorSet::new(vals).map_err(|e| format!("{e}"))?;
    let anchor = hex::decode32(ep["anchor"].as_str().ok_or("no anchor")?)?;
    Ok(Context::for_epoch(
        &btb_chain::staking::consensus_chain_id(chain_id),
        epoch,
        set,
        Some(anchor),
    ))
}

// ------------------------------------------------------------------ JSON

fn proof_json(p: &Proof) -> Value {
    json!({
        "siblings": p.siblings.iter()
            .map(|s| s.iter().map(|h| hex::encode(h)).collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        "leaf": p.leaf.as_ref().map(|(k, v)| json!([hex::encode(k), hex::encode(v)])),
    })
}

fn proof_from_json(v: &Value) -> Result<Proof, String> {
    let siblings = v["siblings"]
        .as_array()
        .ok_or("siblings")?
        .iter()
        .map(|level| {
            level
                .as_array()
                .ok_or("sibling level")?
                .iter()
                .map(|h| hex::decode32(h.as_str().ok_or("sibling hash")?))
                .collect::<Result<Vec<_>, String>>()
        })
        .collect::<Result<Vec<_>, String>>()?;
    let leaf = match &v["leaf"] {
        Value::Null => None,
        l => Some((
            hex::decode32(l[0].as_str().ok_or("leaf key hash")?)?,
            hex::decode32(l[1].as_str().ok_or("leaf value hash")?)?,
        )),
    };
    Ok(Proof { siblings, leaf })
}

fn header_json(h: &Header) -> Value {
    json!({
        "height": h.height,
        "parent_hash": hex::encode(&h.parent_hash),
        "timestamp": h.timestamp,
        "state_root": hex::encode(&h.state_root),
        "tx_root": hex::encode(&h.tx_root),
        "hash": hex::encode(&h.hash()),
    })
}

fn header_from_json(v: &Value) -> Result<Header, String> {
    let h = Header {
        height: v["height"].as_u64().ok_or("header height")?,
        parent_hash: hex::decode32(v["parent_hash"].as_str().ok_or("parent_hash")?)?,
        timestamp: v["timestamp"].as_u64().ok_or("header timestamp")?,
        state_root: hex::decode32(v["state_root"].as_str().ok_or("state_root")?)?,
        tx_root: hex::decode32(v["tx_root"].as_str().ok_or("tx_root")?)?,
    };
    // The claimed hash must be the real one: a forged field cannot hide behind it.
    if hex::decode32(v["hash"].as_str().ok_or("header hash")?)? != h.hash() {
        return Err("header hash does not match its fields".into());
    }
    Ok(h)
}

impl ProofBundle {
    pub fn to_json(&self) -> Value {
        json!({
            "version": 1,
            "height": self.height,
            "state_root": hex::encode(&self.state_root),
            "entries": self.entries.iter().map(|e| json!({
                "key": hex::encode(&e.key),
                "value": e.value.as_ref().map(|v| hex::encode(v)),
                "proof": proof_json(&e.proof),
            })).collect::<Vec<_>>(),
            "header": self.header.as_ref().map(header_json),
            "commit": self.commit.as_ref().map(|c| json!({
                "epoch": c.epoch,
                "sync": hex::encode(&c.sync.encode()),
            })),
        })
    }

    pub fn from_json(v: &Value) -> Result<ProofBundle, ProofError> {
        let f = ProofError::Format;
        let entries = v["entries"]
            .as_array()
            .ok_or_else(|| f("entries".into()))?
            .iter()
            .map(|e| {
                Ok(Entry {
                    key: hex::decode(e["key"].as_str().ok_or("entry key")?)?,
                    value: match &e["value"] {
                        Value::Null => None,
                        s => Some(hex::decode(s.as_str().ok_or("entry value")?)?),
                    },
                    proof: proof_from_json(&e["proof"])?,
                })
            })
            .collect::<Result<Vec<_>, String>>()
            .map_err(f)?;
        let header = match &v["header"] {
            Value::Null => None,
            h => Some(header_from_json(h).map_err(ProofError::Format)?),
        };
        let commit = match &v["commit"] {
            Value::Null => None,
            c => {
                let raw =
                    hex::decode(c["sync"].as_str().unwrap_or("")).map_err(ProofError::Format)?;
                Some(Commit {
                    epoch: c["epoch"]
                        .as_u64()
                        .ok_or_else(|| ProofError::Format("commit epoch".into()))?,
                    sync: SyncResponse::decode(&raw)
                        .ok_or_else(|| ProofError::Format("commit blocks".into()))?,
                })
            }
        };
        Ok(ProofBundle {
            height: v["height"]
                .as_u64()
                .ok_or_else(|| ProofError::Format("height".into()))?,
            state_root: hex::decode32(
                v["state_root"]
                    .as_str()
                    .ok_or_else(|| ProofError::Format("state_root".into()))?,
            )
            .map_err(ProofError::Format)?,
            entries,
            header,
            commit,
        })
    }
}
