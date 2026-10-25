//! Building proof bundles on the node (runs on the driver thread, between two blocks).

use crate::proof::{Commit, Entry, ProofBundle};
use crate::state::HostState;
use btb_consensus::{Block as ConsensusBlock, Context};
use btb_node::blockstore::StoredBlock;
use btb_node::sync::{self, Expect};
use btb_node::wire::{SyncResponse, MAX_SYNC_CHAIN};
use btb_runtime::merkle;
use std::collections::VecDeque;

/// Most keys one bundle proves.
pub const MAX_KEYS: usize = 64;
/// How far behind the head [`newest_anchored`] looks for a version whose next block is
/// provably committed.
const ANCHOR_SEARCH: u64 = 8;

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
    Ok(ProofBundle {
        height: version,
        state_root,
        entries,
        header,
        commit,
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
            if b.commit.is_some() {
                return Ok(b);
            }
        }
    }
    bundle(st, Some(log), head, keys)
}
