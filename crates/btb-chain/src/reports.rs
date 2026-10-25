//! Reports for other chains: the chain side (`docs/ORACLES.md`, section 4).
//!
//! A mutation or cron calls the `report_request` host call; the runtime stores the report under
//! `52 01 id` (`btb_runtime::layout::report_key`) as `height:u64 LE || contract[32] || data`, with a
//! chain-wide counter under `52 00`. The beacon committee threshold-signs
//! `btb_oracle::report::report_message(chain_id, contract, id, data)` off-chain (the signature is
//! deterministic, so nothing about it needs consensus); validators serve it over RPC
//! (`btb_oracleReport`). Records are pruned [`RETENTION_BLOCKS`] after their block, which bounds
//! the state; a relayer has that long to fetch a report.

use crate::state::StateError;
use btb_runtime::kv::KvStore;
use btb_runtime::layout::{report_counter_key, report_key, TAG_REPORT};

/// Blocks a report stays in state.
pub const RETENTION_BLOCKS: u64 = 100_000;
const PRUNE_PER_BLOCK: usize = 64;

/// A queued report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub id: u64,
    /// Height of the block whose transaction asked for it.
    pub height: u64,
    /// The contract that asked; the signature binds it.
    pub contract: [u8; 32],
    pub data: Vec<u8>,
}

fn parse(id: u64, v: &[u8]) -> Option<Report> {
    Some(Report {
        id,
        height: u64::from_le_bytes(v.get(..8)?.try_into().ok()?),
        contract: v.get(8..40)?.try_into().ok()?,
        data: v.get(40..)?.to_vec(),
    })
}

/// The next report id (= how many were ever requested).
pub fn next_id(kv: &impl KvStore) -> Result<u64, StateError> {
    Ok(kv
        .get(&report_counter_key())?
        .and_then(|b| <[u8; 8]>::try_from(b).ok())
        .map_or(0, u64::from_le_bytes))
}

pub fn get(kv: &impl KvStore, id: u64) -> Result<Option<Report>, StateError> {
    Ok(kv.get(&report_key(id))?.and_then(|v| parse(id, &v)))
}

/// Up to `limit` reports with id >= `from`, ascending.
pub fn list_from(kv: &impl KvStore, from: u64, limit: usize) -> Result<Vec<Report>, StateError> {
    let end = [TAG_REPORT, 0x02];
    Ok(kv
        .scan(&report_key(from), Some(&end), false, limit)?
        .into_iter()
        .filter_map(|(k, v)| parse(u64::from_be_bytes(k.get(2..10)?.try_into().ok()?), &v))
        .collect())
}

/// End of block: drop reports older than the retention window (oldest first, bounded work).
pub(crate) fn end_block(kv: &mut impl KvStore, height: u64) -> Result<(), StateError> {
    if height <= RETENTION_BLOCKS {
        return Ok(());
    }
    let cutoff = height - RETENTION_BLOCKS;
    for r in list_from(kv, 0, PRUNE_PER_BLOCK)? {
        if r.height >= cutoff {
            break;
        }
        kv.delete(&report_key(r.id))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use btb_runtime::MemKv;

    fn put(kv: &mut MemKv, id: u64, height: u64, data: &[u8]) {
        let mut v = height.to_le_bytes().to_vec();
        v.extend_from_slice(&[id as u8; 32]);
        v.extend_from_slice(data);
        kv.put(&report_key(id), &v).unwrap();
        kv.put(&report_counter_key(), &(id + 1).to_le_bytes())
            .unwrap();
    }

    #[test]
    fn reports_list_in_order_and_old_ones_are_pruned() {
        let mut kv = MemKv::new();
        assert_eq!(next_id(&kv).unwrap(), 0);
        put(&mut kv, 0, 10, b"a");
        put(&mut kv, 1, 20, b"b");
        put(&mut kv, 2, RETENTION_BLOCKS + 500, b"c");
        assert_eq!(next_id(&kv).unwrap(), 3);
        let all = list_from(&kv, 0, 10).unwrap();
        assert_eq!(all.iter().map(|r| r.id).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(all[1].data, b"b");
        assert_eq!(all[1].contract, [1; 32]);
        assert_eq!(list_from(&kv, 1, 1).unwrap().len(), 1);
        assert_eq!(get(&kv, 2).unwrap().unwrap().height, RETENTION_BLOCKS + 500);
        assert!(get(&kv, 3).unwrap().is_none());
        // nothing is old enough yet
        end_block(&mut kv, RETENTION_BLOCKS).unwrap();
        assert_eq!(list_from(&kv, 0, 10).unwrap().len(), 3);
        // report 0 (height 10) falls out of the window first, then 1
        end_block(&mut kv, RETENTION_BLOCKS + 11).unwrap();
        assert_eq!(list_from(&kv, 0, 10).unwrap().len(), 2);
        end_block(&mut kv, RETENTION_BLOCKS + 21).unwrap();
        let left = list_from(&kv, 0, 10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, 2);
        // the counter is never reduced: ids are not reused
        assert_eq!(next_id(&kv).unwrap(), 3);
        // a damaged record is skipped, not fatal
        kv.put(&report_key(5), b"short").unwrap();
        assert_eq!(list_from(&kv, 0, 10).unwrap().len(), 1);
    }

    #[test]
    fn pruning_is_bounded_per_block() {
        let mut kv = MemKv::new();
        for id in 0..(PRUNE_PER_BLOCK as u64 + 10) {
            put(&mut kv, id, 1, b"x");
        }
        end_block(&mut kv, RETENTION_BLOCKS + 100).unwrap();
        assert_eq!(list_from(&kv, 0, 1000).unwrap().len(), 10);
        end_block(&mut kv, RETENTION_BLOCKS + 101).unwrap();
        assert!(list_from(&kv, 0, 1000).unwrap().is_empty());
    }
}
