//! Block history for the API and for block sync.
//!
//! Every executed block is kept with its receipts, its consensus block (payload, justify QC) and,
//! for the last block of an epoch, the proof that finalizes it (`SyncResponse` with the tail
//! blocks and their certificate). Two backends share one key layout on a `KvStore`:
//!
//! * **Disk** (`--features rocksdb` with a `data_dir`): a separate RocksDB at `<data_dir>/blocks`,
//!   nothing is ever pruned, and it survives a restart.
//! * **Memory** (`MemKv`): bounded; once more than `keep` heights are stored the oldest are
//!   deleted together with their receipt index entries.
//!
//! Keys: `'B' || height:u64 BE` the block record; `'T' || tx id` the (height, index) of a receipt;
//! `'P' || epoch:u64 BE` the epoch's closing proof; `'I' || consensus block id` the height of an
//! executed block; `'U' || consensus block id` a block seen in a proposal that may not be
//! committed yet (a short FIFO). The last two let a node answer consensus `SyncRequest`s for
//! blocks the consensus replica itself no longer holds after a restart, so even a simultaneous
//! restart of every validator finds the certified-but-uncommitted tip again.
//!
//! The history is not part of consensus state. It is written after the block is executed, so a
//! crash in between can leave one height without a record; reads and block sync treat a missing
//! height as unavailable.

use crate::wire::SyncResponse;
use btb_chain::Header;
use btb_consensus::{Block as ConsensusBlock, Hash, Message};
use btb_runtime::{KvStore, MemKv, WriteBatch};
use std::collections::VecDeque;

/// Proposal blocks remembered besides the executed ones.
const KEEP_OBSERVED: usize = 512;

/// The parts of a receipt the API returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredReceipt {
    pub id: Hash,
    pub status: String,
    pub detail: String,
    pub gas_used: u64,
    pub fee: u64,
    pub output: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBlock {
    pub header: Header,
    pub hash: Hash,
    pub state_root_after: Hash,
    pub consensus_round: u64,
    pub tx_ids: Vec<Hash>,
    pub receipts: Vec<StoredReceipt>,
    /// The consensus block this chain block was derived from.
    pub consensus: ConsensusBlock,
}

struct W(Vec<u8>);
impl W {
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn hash(&mut self, h: &Hash) {
        self.0.extend_from_slice(h);
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u64(b.len() as u64);
        self.0.extend_from_slice(b);
    }
}

struct R<'a>(&'a [u8]);
impl R<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn hash(&mut self) -> Option<Hash> {
        self.take(32)?.try_into().ok()
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = usize::try_from(self.u64()?).ok()?;
        Some(self.take(n)?.to_vec())
    }
    /// A count that the remaining input could actually hold.
    fn count(&mut self, min_elem: usize) -> Option<usize> {
        let n = usize::try_from(self.u64()?).ok()?;
        (n.checked_mul(min_elem)? <= self.0.len()).then_some(n)
    }
}

const MAGIC: &[u8; 4] = b"BTR1";

impl StoredBlock {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(MAGIC.to_vec());
        let h = &self.header;
        w.u64(h.height);
        w.hash(&h.parent_hash);
        w.u64(h.timestamp);
        w.hash(&h.state_root);
        w.hash(&h.tx_root);
        w.hash(&self.hash);
        w.hash(&self.state_root_after);
        w.u64(self.consensus_round);
        w.u64(self.tx_ids.len() as u64);
        for id in &self.tx_ids {
            w.hash(id);
        }
        w.u64(self.receipts.len() as u64);
        for r in &self.receipts {
            w.hash(&r.id);
            w.bytes(r.status.as_bytes());
            w.bytes(r.detail.as_bytes());
            w.u64(r.gas_used);
            w.u64(r.fee);
            w.bytes(&r.output);
        }
        let c = Message::SyncResponse {
            blocks: vec![self.consensus.clone()],
        }
        .encode();
        w.bytes(&c);
        w.0
    }

    pub fn decode(data: &[u8]) -> Option<StoredBlock> {
        let mut r = R(data);
        if r.take(4)? != MAGIC {
            return None;
        }
        let header = Header {
            height: r.u64()?,
            parent_hash: r.hash()?,
            timestamp: r.u64()?,
            state_root: r.hash()?,
            tx_root: r.hash()?,
        };
        let hash = r.hash()?;
        let state_root_after = r.hash()?;
        let consensus_round = r.u64()?;
        let n = r.count(32)?;
        let tx_ids = (0..n).map(|_| r.hash()).collect::<Option<Vec<_>>>()?;
        let n = r.count(32 + 8 * 5)?;
        let mut receipts = Vec::with_capacity(n);
        for _ in 0..n {
            receipts.push(StoredReceipt {
                id: r.hash()?,
                status: String::from_utf8(r.bytes()?).ok()?,
                detail: String::from_utf8(r.bytes()?).ok()?,
                gas_used: r.u64()?,
                fee: r.u64()?,
                output: r.bytes()?,
            });
        }
        let c = r.bytes()?;
        if !r.0.is_empty() {
            return None;
        }
        let Message::SyncResponse { mut blocks } = Message::decode(&c)? else {
            return None;
        };
        if blocks.len() != 1 {
            return None;
        }
        Some(StoredBlock {
            header,
            hash,
            state_root_after,
            consensus_round,
            tx_ids,
            receipts,
            consensus: blocks.remove(0),
        })
    }
}

fn block_key(h: u64) -> Vec<u8> {
    let mut k = vec![b'B'];
    k.extend_from_slice(&h.to_be_bytes());
    k
}

fn receipt_key(id: &Hash) -> Vec<u8> {
    let mut k = vec![b'T'];
    k.extend_from_slice(id);
    k
}

fn id_key(tag: u8, id: &Hash) -> Vec<u8> {
    let mut k = vec![tag];
    k.extend_from_slice(id);
    k
}

fn proof_key(epoch: u64) -> Vec<u8> {
    let mut k = vec![b'P'];
    k.extend_from_slice(&epoch.to_be_bytes());
    k
}

pub struct BlockStore {
    kv: Box<dyn KvStore + Send>,
    /// Heights kept (memory backend); `None` keeps everything (disk).
    keep: Option<u64>,
    first: u64,
    latest: u64,
    observed: VecDeque<Hash>,
}

impl BlockStore {
    /// A bounded in-memory store keeping the newest `keep` heights.
    pub fn memory(keep: u64) -> Self {
        Self::over(Box::new(MemKv::new()), Some(keep.max(1))).expect("empty store")
    }

    /// RocksDB at `path`: everything is kept.
    #[cfg(feature = "rocksdb")]
    pub fn disk(path: &std::path::Path) -> Result<Self, String> {
        let kv = btb_runtime::rocks::RocksKv::open(path)
            .map_err(|e| format!("open block store {}: {e}", path.display()))?;
        Self::over(Box::new(kv), None)
    }

    fn over(kv: Box<dyn KvStore + Send>, keep: Option<u64>) -> Result<Self, String> {
        let height_of = |e: Vec<(Vec<u8>, Vec<u8>)>| -> Option<u64> {
            let k = e.first()?.0.get(1..9)?;
            Some(u64::from_be_bytes(k.try_into().ok()?))
        };
        let scan = |rev: bool| {
            kv.scan(b"B", Some(b"C"), rev, 1)
                .map_err(|e| format!("scan block store: {e}"))
        };
        let first = height_of(scan(false)?).unwrap_or(0);
        let latest = height_of(scan(true)?).unwrap_or(0);
        let observed = kv
            .scan(b"U", Some(b"V"), false, KEEP_OBSERVED * 2)
            .map_err(|e| format!("scan block store: {e}"))?
            .into_iter()
            .filter_map(|(k, _)| k.get(1..33)?.try_into().ok())
            .collect();
        Ok(BlockStore {
            kv,
            keep,
            first,
            latest,
            observed,
        })
    }

    /// Newest stored height (0 when empty).
    pub fn latest(&self) -> u64 {
        self.latest
    }

    /// Oldest stored height (0 when empty).
    pub fn first(&self) -> u64 {
        self.first
    }

    pub fn bounded(&self) -> bool {
        self.keep.is_some()
    }

    pub fn get(&self, height: u64) -> Option<StoredBlock> {
        StoredBlock::decode(&self.kv.get(&block_key(height)).ok()??)
    }

    /// A receipt by transaction id: (height, index in block, receipt).
    pub fn receipt(&self, id: &Hash) -> Option<(u64, usize, StoredReceipt)> {
        let loc = self.kv.get(&receipt_key(id)).ok()??;
        let height = u64::from_le_bytes(loc.get(..8)?.try_into().ok()?);
        let index = u32::from_le_bytes(loc.get(8..12)?.try_into().ok()?) as usize;
        let b = self.get(height)?;
        let r = b.receipts.get(index)?.clone();
        (r.id == *id).then_some((height, index, r))
    }

    /// Store an executed block (one atomic batch) and prune the memory backend.
    pub fn put(&mut self, b: &StoredBlock) -> Result<(), String> {
        let h = b.header.height;
        let mut batch = WriteBatch::new();
        batch.put(block_key(h), b.encode());
        for (i, r) in b.receipts.iter().enumerate() {
            let mut loc = h.to_le_bytes().to_vec();
            loc.extend_from_slice(&(i as u32).to_le_bytes());
            batch.put(receipt_key(&r.id), loc);
        }
        batch.put(id_key(b'I', &b.consensus.id()), h.to_be_bytes().to_vec());
        self.kv
            .commit(batch)
            .map_err(|e| format!("write block store: {e}"))?;
        if self.first == 0 {
            self.first = h;
        }
        self.latest = self.latest.max(h);
        if let Some(keep) = self.keep {
            while self.latest.saturating_sub(self.first) >= keep {
                self.drop_oldest()?;
            }
        }
        Ok(())
    }

    fn drop_oldest(&mut self) -> Result<(), String> {
        let h = self.first;
        let mut batch = WriteBatch::new();
        if let Some(old) = self.get(h) {
            for r in &old.receipts {
                // Only if the index still points at this height (a later cron may reuse ids).
                if self
                    .receipt(&r.id)
                    .is_some_and(|(height, _, _)| height == h)
                {
                    batch.delete(receipt_key(&r.id));
                }
            }
        }
        if let Some(old) = self.get(h) {
            batch.delete(id_key(b'I', &old.consensus.id()));
        }
        batch.delete(block_key(h));
        self.kv
            .commit(batch)
            .map_err(|e| format!("prune block store: {e}"))?;
        self.first += 1;
        Ok(())
    }

    /// A consensus block by id: an executed one, or one remembered from a proposal.
    pub fn block_by_id(&self, id: &Hash) -> Option<ConsensusBlock> {
        if let Some(raw) = self.kv.get(&id_key(b'U', id)).ok()? {
            if let Some(Message::SyncResponse { mut blocks }) = Message::decode(&raw) {
                if blocks.len() == 1 && blocks[0].id() == *id {
                    return Some(blocks.remove(0));
                }
            }
        }
        let h = u64::from_be_bytes(self.kv.get(&id_key(b'I', id)).ok()??.try_into().ok()?);
        let b = self.get(h)?.consensus;
        (b.id() == *id).then_some(b)
    }

    /// Remember a block seen in a proposal (the newest `KEEP_OBSERVED` are kept).
    pub fn put_observed(&mut self, b: &ConsensusBlock) -> Result<(), String> {
        let id = b.id();
        if self.observed.contains(&id) {
            return Ok(());
        }
        let raw = Message::SyncResponse {
            blocks: vec![b.clone()],
        }
        .encode();
        let mut batch = WriteBatch::new();
        batch.put(id_key(b'U', &id), raw);
        self.observed.push_back(id);
        if self.observed.len() > KEEP_OBSERVED {
            if let Some(old) = self.observed.pop_front() {
                batch.delete(id_key(b'U', &old));
            }
        }
        self.kv
            .commit(batch)
            .map_err(|e| format!("write block store: {e}"))
    }

    /// The closing proof of `epoch`: the blocks after its last block and a certificate for the
    /// newest of them.
    pub fn proof(&self, epoch: u64) -> Option<SyncResponse> {
        SyncResponse::decode(&self.kv.get(&proof_key(epoch)).ok()??)
    }

    pub fn put_proof(&mut self, p: &SyncResponse) -> Result<(), String> {
        self.kv
            .put(&proof_key(p.epoch), &p.encode())
            .map_err(|e| format!("write block store: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use btb_consensus::{QuorumCert, Sig};

    fn block(h: u64) -> StoredBlock {
        let qc = QuorumCert {
            round: h,
            block_id: [h as u8; 32],
            votes: vec![(0, Sig([1; 64]))],
        };
        StoredBlock {
            header: Header {
                height: h,
                parent_hash: [1; 32],
                timestamp: 5 + h,
                state_root: [2; 32],
                tx_root: [3; 32],
            },
            hash: [h as u8; 32],
            state_root_after: [4; 32],
            consensus_round: h + 1,
            tx_ids: vec![[h as u8; 32]],
            receipts: vec![StoredReceipt {
                id: [h as u8; 32],
                status: "success".into(),
                detail: String::new(),
                gas_used: 7,
                fee: 8,
                output: vec![1, 2, 3],
            }],
            consensus: ConsensusBlock {
                round: h + 1,
                height: h,
                parent: [9; 32],
                justify: qc,
                proposer: 0,
                payload_hash: ConsensusBlock::hash_payload(&[5]),
                payload: vec![5],
            },
        }
    }

    #[test]
    fn record_roundtrip_and_corruption() {
        let b = block(3);
        let bytes = b.encode();
        assert_eq!(StoredBlock::decode(&bytes), Some(b));
        for cut in 0..bytes.len() {
            assert!(StoredBlock::decode(&bytes[..cut]).is_none(), "cut {cut}");
        }
        let mut extra = bytes;
        extra.push(0);
        assert!(StoredBlock::decode(&extra).is_none());
    }

    #[test]
    fn memory_store_is_bounded_and_drops_receipts_with_blocks() {
        let mut s = BlockStore::memory(5);
        for h in 1..=12 {
            s.put(&block(h)).unwrap();
        }
        assert_eq!((s.first(), s.latest()), (8, 12));
        assert!(s.get(7).is_none() && s.get(8).is_some());
        assert!(s.receipt(&[3; 32]).is_none());
        let (h, i, r) = s.receipt(&[10; 32]).unwrap();
        assert_eq!((h, i, r.gas_used), (10, 0, 7));
    }

    #[test]
    fn blocks_are_found_by_id_executed_or_observed() {
        let mut s = BlockStore::memory(3);
        for h in 1..=5 {
            s.put(&block(h)).unwrap();
        }
        let old = block(1).consensus;
        assert!(s.block_by_id(&old.id()).is_none(), "pruned with its height");
        let kept = block(5).consensus;
        assert_eq!(s.block_by_id(&kept.id()), Some(kept));
        let mut seen = block(9).consensus;
        seen.payload = vec![1];
        seen.payload_hash = ConsensusBlock::hash_payload(&seen.payload);
        s.put_observed(&seen).unwrap();
        s.put_observed(&seen).unwrap();
        assert_eq!(s.block_by_id(&seen.id()), Some(seen.clone()));
        for k in 0..KEEP_OBSERVED as u64 {
            let mut b = seen.clone();
            b.round = 100 + k;
            s.put_observed(&b).unwrap();
        }
        assert!(
            s.block_by_id(&seen.id()).is_none(),
            "oldest observed evicted"
        );
    }

    #[test]
    fn proofs_are_kept_per_epoch() {
        let mut s = BlockStore::memory(5);
        assert!(s.proof(2).is_none());
        let p = SyncResponse {
            epoch: 2,
            blocks: vec![block(4).consensus],
            qc: None,
        };
        s.put_proof(&p).unwrap();
        assert_eq!(s.proof(2), Some(p));
        assert!(s.proof(3).is_none());
    }

    #[cfg(feature = "rocksdb")]
    #[test]
    fn disk_store_keeps_everything_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut s = BlockStore::disk(dir.path()).unwrap();
            for h in 1..=50 {
                s.put(&block(h)).unwrap();
            }
            assert_eq!((s.first(), s.latest()), (1, 50));
        }
        let s = BlockStore::disk(dir.path()).unwrap();
        assert_eq!((s.first(), s.latest()), (1, 50));
        assert_eq!(s.get(1), Some(block(1)));
        assert!(s.receipt(&[1; 32]).is_some());
    }
}
