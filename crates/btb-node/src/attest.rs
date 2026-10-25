//! Execution attestation: what validators sign about the result of executing the chain.
//!
//! A consensus block carries `exec` bytes, a [`Claim`]: "after executing height `k`, the chain's
//! state root chain ends in `digest` and the receipts of block `k` hash to `receipts_root`".
//! `k` is not chosen by the proposer; it is [`Replica::exec_height`](btb_consensus::Replica) of
//! the block's parent, a function of the block's ancestry (the newest block that ancestry commits
//! by the 2-chain rule, else the epoch's anchor). Replicas vote only when the claim equals their
//! own execution, so a quorum certificate on a block certifies the state root after block `k`
//! (and, through the hash chain, every earlier root). See `docs/CONSENSUS.md`.
//!
//! The digest chain: `d_0 = SHA256("BTB/exec/genesis/v1" || root_0)` and
//! `d_h = SHA256("BTB/exec/v1" || d_{h-1} || h LE || root_h)` where `root_h` is the state root
//! after block `h`. Given `d_{h-1}` and the roots `root_h .. root_k`, anyone recomputes `d_k`;
//! the claim therefore binds each of those roots (collision resistance).

use crate::blockstore::{BlockStore, ExecRecord, StoredReceipt};
use btb_consensus::{sha256, Block, ExecCheck, ExecProvider, ExecVerdict, Hash};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

/// Length of an encoded [`Claim`].
pub const CLAIM_LEN: usize = 72;
/// Entries kept for answering claims (a claim is for a height a few blocks behind the head).
pub const WINDOW: usize = 1024;

/// The execution result a block attests (`Block::exec`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claim {
    pub height: u64,
    /// `d_height`, the state root digest chain ending at `height`.
    pub digest: Hash,
    pub receipts_root: Hash,
}

impl Claim {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(CLAIM_LEN);
        v.extend_from_slice(&self.height.to_le_bytes());
        v.extend_from_slice(&self.digest);
        v.extend_from_slice(&self.receipts_root);
        v
    }

    pub fn decode(b: &[u8]) -> Option<Claim> {
        if b.len() != CLAIM_LEN {
            return None;
        }
        Some(Claim {
            height: u64::from_le_bytes(b[..8].try_into().ok()?),
            digest: b[8..40].try_into().ok()?,
            receipts_root: b[40..72].try_into().ok()?,
        })
    }
}

/// `d_0`, from the state root of the genesis state.
pub fn genesis_digest(root: &Hash) -> Hash {
    sha256(&[b"BTB/exec/genesis/v1", root])
}

/// `d_height` from `d_{height-1}` and the state root after block `height`.
pub fn next_digest(prev: &Hash, height: u64, state_root: &Hash) -> Hash {
    sha256(&[b"BTB/exec/v1", prev, &height.to_le_bytes(), state_root])
}

/// Root over the receipts of one block (status code, gas, fee and output of each, in order).
pub fn receipts_root(receipts: &[StoredReceipt]) -> Hash {
    let mut leaves: Vec<u8> = Vec::with_capacity(receipts.len() * 32);
    for r in receipts {
        let leaf = sha256(&[
            b"BTB/receipt/v1",
            &r.id,
            &(r.status.len() as u32).to_le_bytes(),
            r.status.as_bytes(),
            &r.gas_used.to_le_bytes(),
            &r.fee.to_le_bytes(),
            &(r.output.len() as u32).to_le_bytes(),
            &r.output,
        ]);
        leaves.extend_from_slice(&leaf);
    }
    sha256(&[
        b"BTB/receipts/v1",
        &(receipts.len() as u64).to_le_bytes(),
        &leaves,
    ])
}

/// One executed height as the window remembers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub digest: Hash,
    /// `None` when the block record is gone (the chain state survived but the block store did
    /// not, or a crash lost the record between state commit and record write): the digest is
    /// recomputed from the state roots, the receipts cannot be.
    pub receipts_root: Option<Hash>,
}

/// The node's memory of what it executed recently; the consensus hooks read it.
#[derive(Default)]
pub struct Window {
    entries: BTreeMap<u64, Entry>,
    /// Testing only: corrupt every digest recorded from this height on.
    fault_from: Option<u64>,
}

impl Window {
    /// The window of a chain whose state at height 0 has root `genesis_root`.
    pub fn new(genesis_root: &Hash) -> Self {
        let mut w = Window::default();
        w.entries.insert(
            0,
            Entry {
                digest: genesis_digest(genesis_root),
                receipts_root: Some(receipts_root(&[])),
            },
        );
        w
    }

    /// Rebuild the newest `WINDOW` executed heights up to `done`. Block records give the
    /// receipts; every digest is recomputed from the state roots (`root_at(h)` is the root
    /// after block `h`; `store` supplies it where it can, the chain state otherwise), so a
    /// missing or partial block store only costs the receipts of those heights.
    pub fn load(
        store: &BlockStore,
        root_at: impl Fn(u64) -> Result<Hash, String>,
        done: u64,
    ) -> Result<Self, String> {
        let mut w = Window::new(&root_at(0)?);
        let first = done.saturating_sub(WINDOW as u64 - 1).max(1);
        // Seed from the record just below the window when there is one: avoids walking the
        // whole chain. Otherwise walk from genesis (a node whose block store was lost).
        let (mut from, mut d) = match first
            .checked_sub(1)
            .and_then(|h| store.get(h).map(|b| (h, b)))
        {
            Some((h, b)) if h > 0 => (h + 1, b.exec.digest),
            _ => (1, w.entries[&0].digest),
        };
        while from <= done {
            let rec = store.get(from);
            let root = match &rec {
                Some(b) => b.state_root_after,
                None => root_at(from)?,
            };
            d = next_digest(&d, from, &root);
            if from >= first {
                w.entries.insert(
                    from,
                    Entry {
                        digest: d,
                        receipts_root: rec.as_ref().map(|b| b.exec.receipts_root),
                    },
                );
            }
            from += 1;
        }
        Ok(w)
    }

    /// Testing only: make this node's executor "diverge" from height `h` on.
    pub fn inject_fault(&mut self, h: u64) {
        self.fault_from = Some(h);
    }

    pub fn get(&self, h: u64) -> Option<&Entry> {
        self.entries.get(&h)
    }

    /// The highest executed height.
    pub fn executed(&self) -> u64 {
        self.entries.keys().next_back().copied().unwrap_or(0)
    }

    /// Check the attestation of the committed block about to be executed. Voters already
    /// required the attested height to be the one the block's ancestry fixes; a committed
    /// block was certified, so here it is enough that the claim names an earlier height and
    /// equals what this node computed.
    pub fn check_executing(&self, b: &Block) -> Result<(), String> {
        let claim = Claim::decode(&b.exec).ok_or("attestation is malformed")?;
        if claim.height >= b.height {
            return Err(format!(
                "attests height {} (not an earlier one)",
                claim.height
            ));
        }
        let e = self
            .entries
            .get(&claim.height)
            .ok_or_else(|| format!("no execution record of height {}", claim.height))?;
        if e.digest != claim.digest || e.receipts_root.is_some_and(|r| r != claim.receipts_root) {
            return Err(format!(
                "the validators attest another execution result at height {} than this node computed",
                claim.height
            ));
        }
        Ok(())
    }

    /// Record the execution of block `height` whose state root is `state_root` and receipts
    /// `receipts`.
    pub fn record(
        &mut self,
        height: u64,
        state_root: &Hash,
        receipts: &[StoredReceipt],
    ) -> Result<ExecRecord, String> {
        let prev = self
            .entries
            .get(&(height - 1))
            .ok_or("no execution record of the parent")?;
        let mut digest = next_digest(&prev.digest, height, state_root);
        if self.fault_from.is_some_and(|f| height >= f) {
            digest[0] ^= 0xff;
        }
        let receipts_root = receipts_root(receipts);
        self.entries.insert(
            height,
            Entry {
                digest,
                receipts_root: Some(receipts_root),
            },
        );
        while self.entries.len() > WINDOW {
            self.entries.pop_first();
        }
        Ok(ExecRecord {
            digest,
            receipts_root,
        })
    }
}

/// The consensus hooks over a shared window.
pub fn hooks(w: &Rc<RefCell<Window>>) -> (ExecProvider, ExecCheck) {
    let a = w.clone();
    let b = w.clone();
    (
        Box::new(move |h| {
            let w = a.borrow();
            let e = w.get(h)?;
            Some(
                Claim {
                    height: h,
                    digest: e.digest,
                    receipts_root: e.receipts_root?,
                }
                .encode(),
            )
        }),
        Box::new(move |blk, h| {
            let Some(claim) = Claim::decode(&blk.exec) else {
                return ExecVerdict::Reject;
            };
            if claim.height != h {
                return ExecVerdict::Reject;
            }
            let w = b.borrow();
            match w.get(h) {
                Some(e) => match e.receipts_root {
                    Some(r) if e.digest == claim.digest && r == claim.receipts_root => {
                        ExecVerdict::Accept
                    }
                    // The receipts of this one height were lost in a crash: cannot judge.
                    None if e.digest == claim.digest => ExecVerdict::Defer,
                    _ => ExecVerdict::Reject,
                },
                None if h > w.executed() => ExecVerdict::Defer,
                None => ExecVerdict::Reject,
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: u8, gas: u64) -> StoredReceipt {
        StoredReceipt {
            id: [id; 32],
            status: "success".into(),
            detail: String::new(),
            gas_used: gas,
            fee: 1,
            output: vec![id],
        }
    }

    #[test]
    fn claim_roundtrip_and_length() {
        let c = Claim {
            height: 9,
            digest: [1; 32],
            receipts_root: [2; 32],
        };
        assert_eq!(Claim::decode(&c.encode()), Some(c));
        assert_eq!(Claim::decode(&c.encode()[..71]), None);
        assert_eq!(Claim::decode(&[c.encode(), vec![0]].concat()), None);
    }

    #[test]
    fn receipts_root_binds_content_and_order() {
        let a = receipts_root(&[rec(1, 5), rec(2, 6)]);
        assert_ne!(a, receipts_root(&[rec(2, 6), rec(1, 5)]));
        assert_ne!(a, receipts_root(&[rec(1, 5), rec(2, 7)]));
        assert_ne!(a, receipts_root(&[rec(1, 5)]));
        assert_eq!(a, receipts_root(&[rec(1, 5), rec(2, 6)]));
    }

    #[test]
    fn record_chains_digests_and_check_compares_them() {
        let mut w = Window::new(&[7; 32]);
        let r1 = w.record(1, &[1; 32], &[rec(1, 3)]).unwrap();
        assert_eq!(
            r1.digest,
            next_digest(&genesis_digest(&[7; 32]), 1, &[1; 32])
        );
        let mut b = Block {
            round: 2,
            height: 2,
            parent: [0; 32],
            justify: btb_consensus::QuorumCert {
                round: 1,
                block_id: [0; 32],
                votes: vec![],
            },
            proposer: 0,
            payload_hash: [0; 32],
            payload: vec![],
            exec: Claim {
                height: 1,
                digest: r1.digest,
                receipts_root: r1.receipts_root,
            }
            .encode(),
        };
        assert_eq!(w.check_executing(&b), Ok(()));
        // A flipped digest bit, a malformed claim, a wrong receipts root, a claim for a
        // height that is not earlier, and one for a height we never executed.
        b.exec[10] ^= 1;
        assert!(w.check_executing(&b).is_err());
        b.exec = vec![1, 2];
        assert!(w.check_executing(&b).is_err());
        b.exec = Claim {
            height: 1,
            digest: r1.digest,
            receipts_root: [9; 32],
        }
        .encode();
        assert!(w.check_executing(&b).is_err());
        b.exec = Claim {
            height: 2,
            digest: r1.digest,
            receipts_root: r1.receipts_root,
        }
        .encode();
        assert!(w.check_executing(&b).is_err());
        b.exec = Claim {
            height: 0,
            digest: r1.digest,
            receipts_root: r1.receipts_root,
        }
        .encode();
        assert!(w.check_executing(&b).is_err());
        // Without receipts (a lost record) only the digest is compared.
        w.entries.get_mut(&1).unwrap().receipts_root = None;
        b.exec = Claim {
            height: 1,
            digest: r1.digest,
            receipts_root: [9; 32],
        }
        .encode();
        assert_eq!(w.check_executing(&b), Ok(()));
    }
}
