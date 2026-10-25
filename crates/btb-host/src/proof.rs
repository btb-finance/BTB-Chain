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
//! The **state root attestation** ([`Attestation`]) anchors the root to a quorum. Every
//! consensus block carries a claim about the execution of one of its ancestors (the newest
//! block its ancestry commits, `docs/CONSENSUS.md`) and honest validators vote only when the
//! claim equals their own execution. A bundle therefore carries that block, the quorum
//! certificate for it and the state roots from `height` up to the claimed height; the verifier
//! checks the certificate against the epoch's validator set (a trust anchor the caller
//! supplies) and recomputes the claim's digest chain from the roots. With
//! [`Verified::root_attested`] set, `state_root` is the root a quorum of that epoch's validators
//! executed, not just a root consistent with committed transactions. Without a validator set
//! (or without an attestation in the bundle) compare the root with a validator's `btb_getBlock`
//! (`state_root_after`) or another independent host, or re-execute.

use btb_chain::{Block, Header};
use btb_consensus::{Block as ConsensusBlock, Context, QuorumCert, Sig};
use btb_node::attest::{genesis_digest, next_digest, Claim};
use btb_node::hex;
use btb_node::sync::{self, Expect};
use btb_node::wire::{Batch, SyncResponse};
use btb_runtime::merkle::{self, Proof};
use serde_json::{json, Value};
use std::collections::BTreeMap;

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

/// Proof that a quorum of an epoch's validators attested the state root of the bundle's height.
///
/// `block` is a consensus block (without its payload; the id covers `payload_hash`) whose
/// `exec` claim names height `k >= height` and the digest chain value `d_k`; `qc` is the
/// quorum certificate for that block. `roots` are the state roots after blocks `height ..= k`
/// (the first is the bundle's `state_root`) and `prev_digest` is `d_{height-1}` (`None` for
/// height 0). Folding `roots` into the chain starting at `prev_digest` must give `d_k`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attestation {
    pub epoch: u64,
    pub block: ConsensusBlock,
    pub qc: QuorumCert,
    pub prev_digest: Option<Hash>,
    pub roots: Vec<Hash>,
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
    /// Quorum attestation of `state_root`, if the host could build one.
    pub attest: Option<Attestation>,
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
    /// The state root attestation is malformed, does not match the root, or its certificate
    /// is not a quorum of the epoch's validators.
    BadAttestation(String),
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
    /// A quorum of the validators in the supplied context of the attestation's epoch attested
    /// `state_root` (their votes cover an execution claim that includes it).
    pub root_attested: bool,
    /// The epoch whose validators attested the root, when `root_attested`.
    pub attested_epoch: Option<u64>,
}

/// Check a bundle. `ctx` is the consensus context of the epoch of block `height + 1`, built by
/// the caller from a validator set it trusts (see [`context_from_epoch_json`]); without it the
/// consensus evidence is not checked and `quorum_committed` stays false.
pub fn verify_bundle(b: &ProofBundle, ctx: Option<&Context>) -> Result<Verified, ProofError> {
    // The one context is for the epoch of the commit evidence; the attestation may come from
    // another epoch (the next one for the last blocks of an epoch), so it is checked only if
    // it is from the same epoch. Use [`verify_bundle_with`] to supply several epochs.
    let mut trust = BTreeMap::new();
    if let (Some(ctx), Some(c)) = (ctx, &b.commit) {
        trust.insert(c.epoch, ctx.clone());
    }
    verify_with(b, ctx, &trust)
}

/// As [`verify_bundle`], with the trusted contexts of several epochs: the commit evidence is
/// checked against the context of its epoch and the state root attestation against that of
/// its epoch. A missing epoch leaves the corresponding claim unchecked (false in [`Verified`]).
pub fn verify_bundle_with(
    b: &ProofBundle,
    trust: &BTreeMap<u64, Context>,
) -> Result<Verified, ProofError> {
    let ctx = b.commit.as_ref().and_then(|c| trust.get(&c.epoch));
    verify_with(b, ctx, trust)
}

fn verify_with(
    b: &ProofBundle,
    ctx: Option<&Context>,
    trust: &BTreeMap<u64, Context>,
) -> Result<Verified, ProofError> {
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
        root_attested: false,
        attested_epoch: None,
    };
    check_header_and_commit(b, ctx, &mut out)?;
    if let Some(a) = &b.attest {
        check_attestation(b, a)?;
        if let Some(ctx) = trust.get(&a.epoch) {
            if !ctx.verify_qc(&a.qc) {
                return Err(ProofError::BadAttestation(
                    "the certificate is not a quorum of the epoch's validators".into(),
                ));
            }
            out.root_attested = true;
            out.attested_epoch = Some(a.epoch);
        }
    }
    Ok(out)
}

fn check_header_and_commit(
    b: &ProofBundle,
    ctx: Option<&Context>,
    out: &mut Verified,
) -> Result<(), ProofError> {
    let Some(h) = &b.header else {
        return if b.commit.is_some() {
            Err(ProofError::CommitWithoutHeader)
        } else {
            Ok(())
        };
    };
    if h.height != b.height + 1 || h.state_root != b.state_root {
        return Err(ProofError::BadHeader);
    }
    out.header_bound = true;
    let Some(c) = &b.commit else {
        return Ok(());
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
        return Ok(());
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
    Ok(())
}

/// Everything about an attestation that needs no trust anchor: the certificate is for the
/// block, the block's claim covers the bundle's height, and the roots fold into the claim.
fn check_attestation(b: &ProofBundle, a: &Attestation) -> Result<(), ProofError> {
    let bad = |m: &str| ProofError::BadAttestation(m.to_string());
    let claim = Claim::decode(&a.block.exec).ok_or_else(|| bad("malformed claim"))?;
    if a.qc.block_id != a.block.id() || a.qc.round != a.block.round {
        return Err(bad("the certificate is for another block"));
    }
    if claim.height < b.height || claim.height >= a.block.height {
        return Err(bad("the claim does not cover this height"));
    }
    if a.roots.len() as u64 != claim.height - b.height + 1 || a.roots.len() > MAX_ROOTS {
        return Err(bad("wrong number of roots"));
    }
    if a.roots[0] != b.state_root {
        return Err(bad("the attested root is not the bundle's root"));
    }
    let mut d = match (b.height, a.prev_digest) {
        (0, _) => genesis_digest(&a.roots[0]),
        (h, Some(prev)) => next_digest(&prev, h, &a.roots[0]),
        (_, None) => return Err(bad("missing previous digest")),
    };
    for (i, root) in a.roots.iter().enumerate().skip(1) {
        d = next_digest(&d, b.height + i as u64, root);
    }
    if d != claim.digest {
        return Err(bad("the roots do not fold into the attested digest"));
    }
    Ok(())
}

/// Most state roots one attestation carries.
pub const MAX_ROOTS: usize = 256;

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

fn qc_json(q: &QuorumCert) -> Value {
    json!({
        "round": q.round,
        "block_id": hex::encode(&q.block_id),
        "votes": q.votes.iter().map(|(i, s)| json!([i, hex::encode(&s.0)])).collect::<Vec<_>>(),
    })
}

fn qc_from_json(v: &Value) -> Result<QuorumCert, String> {
    let votes = v["votes"]
        .as_array()
        .ok_or("votes")?
        .iter()
        .map(|p| {
            let i = u32::try_from(p[0].as_u64().ok_or("vote index")?).map_err(|_| "vote index")?;
            let raw = hex::decode(p[1].as_str().ok_or("vote signature")?)?;
            let sig: [u8; 64] = raw.try_into().map_err(|_| "signature length")?;
            Ok((i, Sig(sig)))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(QuorumCert {
        round: v["round"].as_u64().ok_or("qc round")?,
        block_id: hex::decode32(v["block_id"].as_str().ok_or("qc block_id")?)?,
        votes,
    })
}

fn attest_json(a: &Attestation) -> Value {
    let b = &a.block;
    json!({
        "epoch": a.epoch,
        "block": {
            "round": b.round,
            "height": b.height,
            "parent": hex::encode(&b.parent),
            "justify": qc_json(&b.justify),
            "proposer": b.proposer,
            "payload_hash": hex::encode(&b.payload_hash),
            "exec": hex::encode(&b.exec),
            "id": hex::encode(&b.id()),
        },
        "qc": qc_json(&a.qc),
        "prev_digest": a.prev_digest.as_ref().map(|d| hex::encode(d)),
        "roots": a.roots.iter().map(|r| hex::encode(r)).collect::<Vec<_>>(),
    })
}

fn attest_from_json(v: &Value) -> Result<Attestation, String> {
    let b = &v["block"];
    let block = ConsensusBlock {
        round: b["round"].as_u64().ok_or("block round")?,
        height: b["height"].as_u64().ok_or("block height")?,
        parent: hex::decode32(b["parent"].as_str().ok_or("block parent")?)?,
        justify: qc_from_json(&b["justify"])?,
        proposer: u32::try_from(b["proposer"].as_u64().ok_or("proposer")?)
            .map_err(|_| "proposer")?,
        payload_hash: hex::decode32(b["payload_hash"].as_str().ok_or("payload_hash")?)?,
        payload: Vec::new(),
        exec: hex::decode(b["exec"].as_str().ok_or("exec")?)?,
    };
    // The claimed id must be the real one: a forged field cannot hide behind it.
    if hex::decode32(b["id"].as_str().ok_or("block id")?)? != block.id() {
        return Err("block id does not match its fields".into());
    }
    let roots = v["roots"]
        .as_array()
        .ok_or("roots")?
        .iter()
        .map(|r| hex::decode32(r.as_str().ok_or("root")?))
        .collect::<Result<Vec<_>, String>>()?;
    if roots.is_empty() || roots.len() > MAX_ROOTS {
        return Err("roots".into());
    }
    Ok(Attestation {
        epoch: v["epoch"].as_u64().ok_or("attest epoch")?,
        block,
        qc: qc_from_json(&v["qc"])?,
        prev_digest: match &v["prev_digest"] {
            Value::Null => None,
            d => Some(hex::decode32(d.as_str().ok_or("prev_digest")?)?),
        },
        roots,
    })
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
            "attest": self.attest.as_ref().map(attest_json),
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
            attest: match &v["attest"] {
                Value::Null => None,
                a => Some(attest_from_json(a).map_err(ProofError::Format)?),
            },
        })
    }
}
