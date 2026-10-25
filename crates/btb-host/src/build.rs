//! Building proof bundles on the node (runs on the driver thread, between two blocks).

use crate::proof::{Attestation, Commit, Entry, ProofBundle, MAX_ROOTS};
use crate::state::HostState;
use btb_consensus::{Block as ConsensusBlock, Context};
use btb_node::attest::Claim;
use btb_node::blockstore::StoredBlock;
use btb_node::sync::{self, Expect};
use btb_node::wire::{SyncResponse, MAX_SYNC_CHAIN};
use btb_runtime::merkle;
use std::collections::VecDeque;

/// Most keys one bundle proves.
pub const MAX_KEYS: usize = 64;
/// How far behind the head [`newest_anchored`] looks for a version whose next block is
/// provably committed.
const ANCHOR_SEARCH: u64 = 16;

fn block_at(log: &VecDeque<StoredBlock>, height: u64) -> Option<&StoredBlock> {
    let first = log.front()?.header.height;
    log.get(usize::try_from(height.checked_sub(first)?).ok()?)
        .filter(|b| b.header.height == height)
}

/// Proofs for `keys` at state version `version`.
pub fn entries(st: &HostState, version: u64, keys: &[Vec<u8>]) -> Result<Vec<Entry>, String> {
    keys.iter()
        .map(|key| {
            let (value, proof) = merkle::get_with_proof(st.store(), version, key)
                .map_err(|e| format!("version {version}: {e}"))?;
            Ok(Entry {
                key: key.clone(),
                value,
                proof,
            })
        })
        .collect()
}

/// Consensus evidence that block `version + 1` is committed: the shortest run of consecutive
/// consensus blocks from it that `sync::verify` accepts against the epoch's validator set.
fn commit_for(st: &HostState, log: &VecDeque<StoredBlock>, version: u64) -> Option<Commit> {
    let h = version + 1;
    let cfg = st.config().staking;
    let epoch = cfg.epoch_of_block(h);
    let end = cfg.epoch_end(epoch);
    let rec = st.epoch_record(epoch).ok()??;
    let ctx = Context::for_epoch(
        &st.consensus_chain_id(),
        epoch,
        rec.consensus_set().ok()?,
        Some(rec.anchor),
    );
    let blocks: Vec<ConsensusBlock> = (h..=end.min(h + MAX_SYNC_CHAIN as u64 - 1))
        .map_while(|x| block_at(log, x))
        .map(|b| b.consensus.clone())
        .collect();
    let first = blocks.first()?;
    let expect = Expect {
        ctx: &ctx,
        epoch,
        first_height: h,
        parent: first.parent,
        end_height: end,
    };
    (2..=blocks.len()).find_map(|m| {
        let sync = SyncResponse {
            epoch,
            blocks: blocks[..m].to_vec(),
            qc: None,
        };
        matches!(sync::verify(&sync, &expect), Ok(n) if n >= 1).then_some(Commit { epoch, sync })
    })
}

/// The quorum attestation of the state root after block `version`: the first consensus block
/// whose execution claim covers `version` and whose own certificate is in the log (the
/// `justify` of the next block), with the state roots from `version` up to the claimed height.
fn attestation_for(
    st: &HostState,
    log: &VecDeque<StoredBlock>,
    version: u64,
) -> Option<Attestation> {
    let head = log.back()?.header.height;
    let cfg = st.config().staking;
    let prev_digest = match version {
        0 => None,
        v => Some(block_at(log, v - 1)?.exec.digest),
    };
    // A block attesting `version` has height at least `version + 2`; its certificate is the
    // justify of the next block, so it must not be the head.
    for x in (version + 2)..head.min(version + 2 + MAX_ROOTS as u64) {
        let b = block_at(log, x)?;
        let Some(claim) = Claim::decode(&b.consensus.exec) else {
            continue;
        };
        if claim.height < version {
            continue;
        }
        let qc = &block_at(log, x + 1)?.consensus.justify;
        let id = b.consensus.id();
        if qc.block_id != id || qc.round != b.consensus.round {
            continue; // the next block belongs to another epoch (its justify is not this QC)
        }
        let roots: Option<Vec<_>> = (version..=claim.height)
            .map(|j| block_at(log, j).map(|s| s.state_root_after))
            .collect();
        let mut block = b.consensus.clone();
        block.payload = Vec::new(); // the id covers `payload_hash`; the payload is not needed
        return Some(Attestation {
            epoch: cfg.epoch_of_block(x),
            block,
            qc: qc.clone(),
            prev_digest,
            roots: roots.filter(|r| r.len() <= MAX_ROOTS).or_else(|| {
                // Height 0 has no stored block; its root is the version-0 root.
                (version == 0).then(|| {
                    let mut r = vec![merkle::root(st.store(), 0).ok().flatten()?];
                    r.extend(
                        (1..=claim.height)
                            .map(|j| block_at(log, j).map(|s| s.state_root_after))
                            .collect::<Option<Vec<_>>>()?,
                    );
                    Some(r)
                })?
            })?,
        });
    }
    None
}

/// A bundle for `keys` at `version`. With `log`, it carries the next block's header and, when
/// enough blocks followed, the consensus evidence for it.
pub fn bundle(
    st: &HostState,
    log: Option<&VecDeque<StoredBlock>>,
    version: u64,
    keys: &[Vec<u8>],
) -> Result<ProofBundle, String> {
    if keys.len() > MAX_KEYS {
        return Err(format!("at most {MAX_KEYS} keys per proof"));
    }
    let state_root = merkle::root(st.store(), version)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no state at height {version}"))?;
    let entries = entries(st, version, keys)?;
    let header = log
        .and_then(|l| block_at(l, version + 1))
        .map(|b| b.header)
        .filter(|h| h.state_root == state_root);
    let commit = match (log, &header) {
        (Some(l), Some(_)) => commit_for(st, l, version),
        _ => None,
    };
    let attest = log.and_then(|l| attestation_for(st, l, version));
    Ok(ProofBundle {
        height: version,
        state_root,
        entries,
        header,
        commit,
        attest,
    })
}

/// The newest bundle whose next block is provably committed; failing that, one at the head
/// without header or evidence (the head's root is named by no header yet).
pub fn newest_anchored(
    st: &HostState,
    log: &VecDeque<StoredBlock>,
    keys: &[Vec<u8>],
) -> Result<ProofBundle, String> {
    let head = st.head().map_err(|e| e.0)?.height;
    for v in (head.saturating_sub(ANCHOR_SEARCH)..head).rev() {
        if let Ok(b) = bundle(st, Some(log), v, keys) {
            if b.commit.is_some() && b.attest.is_some() {
                return Ok(b);
            }
        }
    }
    for v in (head.saturating_sub(ANCHOR_SEARCH)..head).rev() {
        if let Ok(b) = bundle(st, Some(log), v, keys) {
            if b.commit.is_some() {
                return Ok(b);
            }
        }
    }
    bundle(st, Some(log), head, keys)
}
