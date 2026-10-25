//! Verification of block sync responses.
//!
//! A node that fell more epochs behind than its peers keep consensus instances for asks them
//! for committed blocks by height. The answer is trusted for nothing: the node checks it against
//! the validator set it derived itself by executing the chain up to that point.
//!
//! What a response proves. `blocks` are consecutive consensus blocks of one epoch, each linked
//! to the previous one by its parent hash (the block id covers parent, justify QC and payload
//! hash). Block `i` is **committed** when block `i + 1` has the next round and a quorum
//! certificate exists for it: the 2-chain rule of `docs/CONSENSUS.md`. That certificate is the
//! `justify` of block `i + 2`, or `qc` for the last block of the response. A committed block
//! commits all its ancestors, so one verified quorum certificate (checked signature by
//! signature against the epoch's set, in the epoch's signature domain) proves every earlier block
//! of the response. The first block must extend what the node already executed.

use crate::attest::Claim;
use crate::wire::{SyncResponse, MAX_PAYLOAD, MAX_SYNC_CHAIN};
use btb_consensus::{Block, Context, Hash};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncError {
    /// No blocks (nothing to learn from this peer).
    Empty,
    TooLong,
    /// The response claims another epoch than the one the node is in.
    WrongEpoch,
    /// The first block is not the next height or does not extend the executed chain.
    BadStart,
    /// Block `n` does not extend block `n - 1` (hash, height, round or justify mismatch).
    BadLink(usize),
    /// Payload does not match its hash (a truncated or altered block), or is too big, or the
    /// block's execution attestation is malformed (not a claim about an earlier height).
    BadPayload(usize),
    /// The certificate is not a quorum of the epoch's validators (forged, short, or signed in
    /// another epoch's domain).
    BadCertificate,
    /// The response contains no proof that even its first block is committed.
    NoProof,
}

/// What the receiving node knows independently of the peer.
pub struct Expect<'a> {
    /// The epoch's context, rebuilt from the node's own executed state.
    pub ctx: &'a Context,
    pub epoch: u64,
    /// The next height the node has not executed.
    pub first_height: u64,
    /// Block id the first block must extend: the last executed block of this epoch, or the
    /// epoch's anchor when none was executed yet.
    pub parent: Hash,
    /// Height of the epoch's last block; blocks above it are only proofs.
    pub end_height: u64,
}

/// Check `resp`. On success returns how many leading blocks are proven committed and belong to
/// the epoch (at least 1).
pub fn verify(resp: &SyncResponse, e: &Expect) -> Result<usize, SyncError> {
    let chain = &resp.blocks;
    let m = chain.len();
    if m == 0 {
        return Err(SyncError::Empty);
    }
    if m > MAX_SYNC_CHAIN {
        return Err(SyncError::TooLong);
    }
    if resp.epoch != e.epoch {
        return Err(SyncError::WrongEpoch);
    }
    if e.first_height > e.end_height {
        return Err(SyncError::BadStart);
    }
    let first = &chain[0];
    if first.height != e.first_height
        || first.parent != e.parent
        || first.round == 0
        || first.justify.block_id != first.parent
    {
        return Err(SyncError::BadStart);
    }
    for (i, b) in chain.iter().enumerate() {
        if b.payload.len() > MAX_PAYLOAD || Block::hash_payload(&b.payload) != b.payload_hash {
            return Err(SyncError::BadPayload(i));
        }
        // The attestation is covered by the block id (so by the certificate); a block without
        // a well-formed one can never have been certified by honest validators.
        if !Claim::decode(&b.exec).is_some_and(|c| c.height < b.height) {
            return Err(SyncError::BadPayload(i));
        }
        if i == 0 {
            continue;
        }
        let p = &chain[i - 1];
        let pid = p.id();
        if b.height != p.height + 1
            || b.parent != pid
            || b.round <= p.round
            || b.justify.block_id != pid
            || b.justify.round != p.round
        {
            return Err(SyncError::BadLink(i));
        }
    }
    if let Some(qc) = &resp.qc {
        let last = &chain[m - 1];
        if qc.block_id != last.id() || qc.round != last.round {
            return Err(SyncError::BadCertificate);
        }
    }
    // The newest committed block: the last `i` whose child has the next round and a certificate.
    let proof = (0..m - 1).rev().find_map(|i| {
        if chain[i + 1].round != chain[i].round + 1 {
            return None;
        }
        let cert = if i + 2 < m {
            Some(&chain[i + 2].justify)
        } else {
            resp.qc.as_ref()
        };
        cert.map(|c| (i, c))
    });
    let Some((i, cert)) = proof else {
        return Err(SyncError::NoProof);
    };
    if !e.ctx.verify_qc(cert) {
        return Err(SyncError::BadCertificate);
    }
    let room = (e.end_height - e.first_height + 1) as usize;
    Ok((i + 1).min(room))
}

#[cfg(test)]
mod tests {
    use super::*;
    use btb_consensus::{QuorumCert, Sig, Validator, ValidatorSet};
    use ed25519_dalek::{Signer, SigningKey};

    struct Net {
        keys: Vec<SigningKey>,
        ctx: Context,
        root: Hash,
    }

    fn net(epoch: u64) -> Net {
        let keys: Vec<SigningKey> = (1..=4u8)
            .map(|i| SigningKey::from_bytes(&[i; 32]))
            .collect();
        let set = ValidatorSet::new(
            keys.iter()
                .map(|k| Validator {
                    key: k.verifying_key().to_bytes(),
                    stake: 10,
                })
                .collect(),
        )
        .unwrap();
        let root = [9u8; 32];
        let ctx = Context::for_epoch(b"chain", epoch, set, Some(root));
        Net { keys, ctx, root }
    }

    fn qc_for(n: &Net, round: u64, id: Hash, signers: usize) -> QuorumCert {
        let msg = n.ctx.vote_bytes(round, &id);
        QuorumCert {
            round,
            block_id: id,
            votes: (0..signers)
                .map(|i| (i as u32, Sig(n.keys[i].sign(&msg).to_bytes())))
                .collect(),
        }
    }

    /// `count` blocks of consecutive rounds from height `first`, each justified by a real QC.
    fn chain(n: &Net, first: u64, count: usize) -> Vec<Block> {
        let mut out: Vec<Block> = Vec::new();
        for k in 0..count {
            let (parent, justify) = match out.last() {
                Some(p) => (p.id(), qc_for(n, p.round, p.id(), 3)),
                None => (n.root, n.ctx.genesis_qc()),
            };
            let payload = vec![k as u8; 5];
            out.push(Block {
                round: k as u64 + 1,
                height: first + k as u64,
                parent,
                justify,
                proposer: (k % 4) as u32,
                payload_hash: Block::hash_payload(&payload),
                payload,
                exec: test_claim(),
            });
        }
        out
    }

    fn test_claim() -> Vec<u8> {
        Claim {
            height: 0,
            digest: [1; 32],
            receipts_root: [2; 32],
        }
        .encode()
    }

    fn expect<'a>(n: &'a Net, end: u64) -> Expect<'a> {
        Expect {
            ctx: &n.ctx,
            epoch: 0,
            first_height: 1,
            parent: n.root,
            end_height: end,
        }
    }

    fn resp(blocks: Vec<Block>, qc: Option<QuorumCert>) -> SyncResponse {
        SyncResponse {
            epoch: 0,
            blocks,
            qc,
        }
    }

    #[test]
    fn valid_chain_commits_all_but_the_unproven_tail() {
        let n = net(0);
        let c = chain(&n, 1, 6);
        // Six blocks, no final certificate: committing index i needs a certificate for i + 1
        // (the justify of i + 2), so index 3 is the newest proven one.
        assert_eq!(verify(&resp(c.clone(), None), &expect(&n, 100)), Ok(4));
        // With a certificate for the last block, index 4 is committed too.
        let q = qc_for(&n, c[5].round, c[5].id(), 3);
        assert_eq!(verify(&resp(c.clone(), Some(q)), &expect(&n, 100)), Ok(5));
        // Capped at the epoch's last height (the rest are proofs).
        let q = qc_for(&n, c[5].round, c[5].id(), 3);
        assert_eq!(verify(&resp(c, Some(q)), &expect(&n, 3)), Ok(3));
    }

    #[test]
    fn two_blocks_and_a_certificate_prove_the_first() {
        let n = net(0);
        let c = chain(&n, 1, 2);
        let q = qc_for(&n, c[1].round, c[1].id(), 3);
        assert_eq!(verify(&resp(c, Some(q)), &expect(&n, 10)), Ok(1));
    }

    #[test]
    fn blocks_without_a_wellformed_attestation_are_rejected() {
        let n = net(0);
        let mut c = chain(&n, 1, 3);
        let q = qc_for(&n, c[2].round, c[2].id(), 3);
        // Re-link the chain after changing block 1's attestation (the id covers it).
        let mut bad = c.clone();
        for variant in [
            vec![],
            vec![7; 71],
            Claim {
                height: 2,
                digest: [0; 32],
                receipts_root: [0; 32],
            }
            .encode(),
        ] {
            bad[1].exec = variant;
            bad[2].parent = bad[1].id();
            bad[2].justify = qc_for(&n, bad[1].round, bad[1].id(), 3);
            let q2 = qc_for(&n, bad[2].round, bad[2].id(), 3);
            assert_eq!(
                verify(&resp(bad.clone(), Some(q2)), &expect(&n, 10)),
                Err(SyncError::BadPayload(1))
            );
        }
        c[1].exec = test_claim();
        assert!(verify(&resp(c, Some(q)), &expect(&n, 10)).is_ok());
    }

    #[test]
    fn missing_proof_is_reported() {
        let n = net(0);
        let c = chain(&n, 1, 2);
        assert_eq!(
            verify(&resp(c.clone(), None), &expect(&n, 10)),
            Err(SyncError::NoProof)
        );
        assert_eq!(
            verify(&resp(c[..1].to_vec(), None), &expect(&n, 10)),
            Err(SyncError::NoProof)
        );
        assert_eq!(
            verify(&resp(vec![], None), &expect(&n, 10)),
            Err(SyncError::Empty)
        );
    }

    #[test]
    fn forged_or_short_certificates_are_rejected() {
        let n = net(0);
        let c = chain(&n, 1, 3);
        // Too few signers (2 of 4 equal stakes is not a quorum).
        let weak = qc_for(&n, c[2].round, c[2].id(), 2);
        assert_eq!(
            verify(&resp(c.clone(), Some(weak)), &expect(&n, 10)),
            Err(SyncError::BadCertificate)
        );
        // A flipped signature bit.
        let mut forged = qc_for(&n, c[2].round, c[2].id(), 3);
        forged.votes[1].1 .0[0] ^= 1;
        assert_eq!(
            verify(&resp(c.clone(), Some(forged)), &expect(&n, 10)),
            Err(SyncError::BadCertificate)
        );
        // A forged justify inside the chain is the certificate for block 1 and fails.
        let mut bad = c.clone();
        bad[2].justify.votes[0].1 .0[3] ^= 1;
        assert_eq!(
            verify(&resp(bad, None), &expect(&n, 10)),
            Err(SyncError::BadCertificate)
        );
        // A certificate for some other block.
        let other = qc_for(&n, c[2].round, [1; 32], 3);
        assert_eq!(
            verify(&resp(c.clone(), Some(other)), &expect(&n, 10)),
            Err(SyncError::BadCertificate)
        );
        // Signatures by keys outside the validator set.
        let outsiders: Vec<SigningKey> = (50..54u8)
            .map(|i| SigningKey::from_bytes(&[i; 32]))
            .collect();
        let msg = n.ctx.vote_bytes(c[2].round, &c[2].id());
        let fake = QuorumCert {
            round: c[2].round,
            block_id: c[2].id(),
            votes: (0..3)
                .map(|i| (i as u32, Sig(outsiders[i].sign(&msg).to_bytes())))
                .collect(),
        };
        assert_eq!(
            verify(&resp(c, Some(fake)), &expect(&n, 10)),
            Err(SyncError::BadCertificate)
        );
    }

    #[test]
    fn certificates_of_another_epoch_are_rejected() {
        // The same keys sign a perfectly good chain, but in epoch 1's signature domain.
        let other = net(1);
        let c = chain(&other, 1, 4);
        let q = qc_for(&other, c[3].round, c[3].id(), 4);
        let mine = net(0);
        // Claimed as epoch 0 (so the epoch field matches) but signed for epoch 1.
        let r = resp(c.clone(), Some(q.clone()));
        assert_eq!(
            verify(&r, &expect(&mine, 10)),
            Err(SyncError::BadCertificate)
        );
        // Claimed as epoch 1 to a node in epoch 0.
        let mut r1 = r;
        r1.epoch = 1;
        assert_eq!(verify(&r1, &expect(&mine, 10)), Err(SyncError::WrongEpoch));
    }

    #[test]
    fn truncated_or_altered_blocks_are_rejected() {
        let n = net(0);
        let c = chain(&n, 1, 4);
        let q = qc_for(&n, c[3].round, c[3].id(), 3);
        // Payload cut short: its hash no longer matches.
        let mut cut = c.clone();
        cut[1].payload.truncate(2);
        assert_eq!(
            verify(&resp(cut, Some(q.clone())), &expect(&n, 10)),
            Err(SyncError::BadPayload(1))
        );
        // Payload and its hash both swapped: the block id changes, the next block's parent
        // no longer matches.
        let mut swap = c.clone();
        swap[1].payload = vec![0xee; 5];
        swap[1].payload_hash = Block::hash_payload(&swap[1].payload);
        assert_eq!(
            verify(&resp(swap, Some(q.clone())), &expect(&n, 10)),
            Err(SyncError::BadLink(2))
        );
        // A block dropped from the middle.
        let mut gap = c.clone();
        gap.remove(1);
        assert_eq!(
            verify(&resp(gap, Some(q.clone())), &expect(&n, 10)),
            Err(SyncError::BadLink(1))
        );
        // Oversize payload.
        let mut big = c.clone();
        big[0].payload = vec![0; MAX_PAYLOAD + 1];
        big[0].payload_hash = Block::hash_payload(&big[0].payload);
        assert_eq!(
            verify(&resp(big, Some(q)), &expect(&n, 10)),
            Err(SyncError::BadPayload(0))
        );
    }

    #[test]
    fn start_must_extend_what_the_node_executed() {
        let n = net(0);
        let c = chain(&n, 1, 3);
        let q = qc_for(&n, c[2].round, c[2].id(), 3);
        let r = resp(c, Some(q));
        let mut e = expect(&n, 10);
        e.first_height = 2;
        assert_eq!(verify(&r, &e), Err(SyncError::BadStart));
        let mut e = expect(&n, 10);
        e.parent = [1; 32];
        assert_eq!(verify(&r, &e), Err(SyncError::BadStart));
        let e = expect(&n, 0);
        assert_eq!(verify(&r, &e), Err(SyncError::BadStart));
    }

    #[test]
    fn rounds_with_gaps_need_a_later_consecutive_pair() {
        let n = net(0);
        // Rounds 1, 3, 4, 5: block 2 -> 3 is not consecutive, but 3 -> 4 and 4 -> 5 are.
        let mut out: Vec<Block> = Vec::new();
        for (k, round) in [1u64, 3, 4, 5].iter().enumerate() {
            let (parent, justify) = match out.last() {
                Some(p) => (p.id(), qc_for(&n, p.round, p.id(), 3)),
                None => (n.root, n.ctx.genesis_qc()),
            };
            out.push(Block {
                round: *round,
                height: 1 + k as u64,
                parent,
                justify,
                proposer: 0,
                exec: test_claim(),
                payload_hash: Block::hash_payload(&[]),
                payload: vec![],
            });
        }
        let q = qc_for(&n, 5, out[3].id(), 3);
        // Newest committed index is 2 (child 3 has round 5 = 4 + 1 and a certificate).
        assert_eq!(verify(&resp(out.clone(), Some(q)), &expect(&n, 10)), Ok(3));
        // Without the final certificate only the pair (3 -> 4) proven by out[3]'s justify counts:
        // index 1 (round 3) is committed, which also commits index 0.
        assert_eq!(verify(&resp(out, None), &expect(&n, 10)), Ok(2));
    }
}
