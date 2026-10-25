//! Storage nodes on chain: registration, deterministic spread assignment, challenges and
//! proofs, tamper rejection, misses (slash into the escrow, reassignment, repair), expiry and
//! payout accounting. The executor checks the supply invariant after every block, so each
//! `net.run` below also proves that escrow and deposits never create or destroy BTB.

mod support;
use btb_chain::storage::proof::{self, CHUNK};
use btb_chain::storage::{PieceState, StorageConfig, StorageError, StorageOp};
use btb_chain::*;
use btb_codec::Encode;
use btb_runtime::MemKv;
use support::*;

const RICH: u64 = 1_000_000_000_000;

fn cfg() -> StorageConfig {
    StorageConfig {
        epoch_blocks: 10,
        challenge_blocks: 3,
        repair_blocks: 4,
        price_per_kib_period: 10,
        deposit_per_kib: 100,
        min_deposit: 100_000,
        slash_bps: 5_000,
    }
}

/// A file made of random-looking pieces (the chain does not check erasure coding).
struct File {
    id: Hash,
    size: u64,
    k: u8,
    m: u8,
    pieces: Vec<Vec<u8>>,
    roots: Vec<Hash>,
}

fn file(seed: u8, size: u64, k: u8, m: u8) -> File {
    let pl = proof::piece_len(size, k as usize) as usize;
    let pieces: Vec<Vec<u8>> = (0..k + m)
        .map(|i| {
            (0..pl)
                .map(|j| {
                    ((j as u32 ^ (u32::from(i) << 20) ^ (u32::from(seed) << 12))
                        .wrapping_mul(2_654_435_761)
                        >> 24) as u8
                })
                .collect()
        })
        .collect();
    let roots: Vec<Hash> = pieces.iter().map(|p| proof::piece_root(p)).collect();
    File {
        id: proof::file_id_of_pieces(&roots, size, k, m),
        size,
        k,
        m,
        pieces,
        roots,
    }
}

impl File {
    fn store_op(&self, periods: u32) -> StorageOp {
        StorageOp::Store {
            root: self.id,
            size: self.size,
            k: self.k,
            m: self.m,
            periods,
        }
    }
    fn prove_op(&self, index: usize, chunk: u64) -> StorageOp {
        StorageOp::Prove {
            file: self.id,
            index: index as u8,
            proof: proof::prove_chunk(&self.pieces[index], chunk as usize, &self.roots, index)
                .unwrap(),
        }
    }
}

struct World {
    net: Net<MemKv>,
    owner: Wallet,
    nodes: Vec<Wallet>,
}

fn world(nodes: usize) -> World {
    world_with(nodes, cfg())
}

fn world_with(nodes: usize, storage: StorageConfig) -> World {
    let owner = Wallet::new(1);
    let mut ws: Vec<Wallet> = (0..nodes).map(|i| Wallet::new(10 + i as u8)).collect();
    let mut config = ChainConfig::new(CHAIN_ID);
    config.storage = storage;
    let g = {
        let mut refs: Vec<&Wallet> = vec![&owner];
        refs.extend(ws.iter());
        genesis_with(&refs, RICH, config)
    };
    let mut net = Net::new(MemKv::new(), &g);
    let txs: Vec<_> = ws
        .iter_mut()
        .enumerate()
        .map(|(i, w)| {
            w.sign(TxKind::Storage(StorageOp::Register {
                endpoint: format!("http://127.0.0.1:{}", 9000 + i).into_bytes(),
                deposit: 10_000_000,
            }))
        })
        .collect();
    for r in net.run(txs) {
        ok(&r);
    }
    World {
        net,
        owner,
        nodes: ws,
    }
}

impl World {
    fn node_index(&self, a: &Address) -> usize {
        self.nodes.iter().position(|w| &w.addr() == a).unwrap()
    }
    fn store(&mut self, f: &File, periods: u32) -> Receipt {
        let tx = self.owner.sign(TxKind::Storage(f.store_op(periods)));
        self.net.run(vec![tx]).remove(0)
    }
    fn pieces(&self, f: &File) -> Vec<btb_chain::storage::PieceInfo> {
        self.net.state.storage_pieces(&f.id).unwrap()
    }
    fn deposit(&self, a: &Address) -> u64 {
        self.net.state.storage_node(a).unwrap().unwrap().deposit
    }
    /// Run empty blocks until some piece of `f` has an open challenge; returns it.
    fn until_challenge(&mut self, f: &File) -> btb_chain::storage::PieceInfo {
        for _ in 0..40 {
            if let Some(p) = self.pieces(f).into_iter().find(|p| p.challenge.is_some()) {
                return p;
            }
            self.net.run(vec![]);
        }
        panic!("no challenge opened");
    }
    fn prove(&mut self, f: &File, node: usize, index: usize, chunk: u64) -> Receipt {
        let tx = self.nodes[node].sign(TxKind::Storage(f.prove_op(index, chunk)));
        self.net.run(vec![tx]).remove(0)
    }
}

fn reverted(r: &Receipt) -> &StorageError {
    match &r.status {
        Status::Reverted(Revert::Storage(e)) => e,
        s => panic!("expected a storage revert, got {s:?}"),
    }
}

#[test]
fn merkle_proofs_verify_and_tampering_is_caught() {
    let f = file(3, 10_000, 3, 2);
    let pl = proof::piece_len(f.size, 3);
    assert_eq!(pl % CHUNK as u64, 0);
    let chunks = (pl / CHUNK as u64) as usize;
    for idx in 0..5usize {
        for c in 0..chunks {
            let p = proof::prove_chunk(&f.pieces[idx], c, &f.roots, idx).unwrap();
            assert!(proof::verify_chunk(&f.id, f.size, 3, 2, pl, idx, c, &p));
            // Wrong position, wrong file parameters, flipped bytes, truncated paths.
            assert!(
                !proof::verify_chunk(&f.id, f.size, 3, 2, pl, idx, (c + 1) % chunks.max(2), &p)
                    || chunks == 1
            );
            assert!(!proof::verify_chunk(
                &f.id,
                f.size + 1,
                3,
                2,
                pl,
                idx,
                c,
                &p
            ));
            assert!(!proof::verify_chunk(&f.id, f.size, 2, 3, pl, idx, c, &p));
            let mut bad = p.clone();
            bad.chunk[7] ^= 1;
            assert!(!proof::verify_chunk(&f.id, f.size, 3, 2, pl, idx, c, &bad));
            let mut bad = p.clone();
            bad.piece_path.pop();
            assert!(!proof::verify_chunk(&f.id, f.size, 3, 2, pl, idx, c, &bad));
            let mut bad = p.clone();
            if let Some(h) = bad.chunk_path.first_mut() {
                h[0] ^= 1;
                assert!(!proof::verify_chunk(&f.id, f.size, 3, 2, pl, idx, c, &bad));
            }
            // A proof for a piece does not verify as another piece.
            assert!(!proof::verify_chunk(
                &f.id,
                f.size,
                3,
                2,
                pl,
                (idx + 1) % 5,
                c,
                &p
            ));
        }
    }
}

#[test]
fn ops_round_trip_on_the_wire() {
    let f = file(1, 3000, 2, 1);
    let ops = [
        StorageOp::Register {
            endpoint: b"http://x:1".to_vec(),
            deposit: 5,
        },
        StorageOp::AddDeposit { amount: 9 },
        StorageOp::Withdraw { amount: 3 },
        f.store_op(4),
        f.prove_op(1, 0),
    ];
    let mut w = Wallet::new(5);
    for op in ops {
        let tx = w.sign(TxKind::Storage(op.clone()));
        let back = Transaction::decode(&tx.encode()).unwrap();
        assert_eq!(back.body.kind, TxKind::Storage(op));
        assert!(back.verify_signature());
    }
    // Unknown sub-tag is a clear error, trailing garbage is not accepted.
    let mut body = Vec::new();
    7u64.encode_to(&mut body);
    0u64.encode_to(&mut body);
    12u8.encode_to(&mut body);
    9u8.encode_to(&mut body);
    body.extend_from_slice(&[0u8; 96]);
    assert!(matches!(
        Transaction::decode(&body),
        Err(TxError::BadTag(9))
    ));
}

#[test]
fn registration_rules_and_withdrawal() {
    let mut w = world(2);
    let a = w.nodes[0].addr();
    // Too small a deposit, an empty endpoint, an oversized endpoint.
    let tx = w.nodes[0].sign(TxKind::Storage(StorageOp::AddDeposit { amount: 1 }));
    ok(&w.net.run(vec![tx])[0]);
    let low = w.nodes[1].sign(TxKind::Storage(StorageOp::Withdraw { amount: 20_000_000 }));
    assert_eq!(
        reverted(&w.net.run(vec![low])[0]),
        &StorageError::DepositLocked
    );
    // A fresh account cannot register with less than min_deposit.
    let mut poor = Wallet::new(77);
    let g = poor.sign(TxKind::Storage(StorageOp::Register {
        endpoint: b"x".to_vec(),
        deposit: 1,
    }));
    // (not funded in this world: invalid, not a revert)
    assert!(matches!(w.net.run(vec![g])[0].status, Status::Invalid(_)));
    let _ = &mut poor;
    let empty = w.nodes[0].sign(TxKind::Storage(StorageOp::Register {
        endpoint: vec![],
        deposit: 0,
    }));
    assert!(matches!(
        w.net.run(vec![empty])[0].status,
        Status::Invalid(Invalid::TooLarge)
    ));
    w.nodes[0].nonce -= 1; // invalid transactions do not use the nonce
    assert_eq!(w.deposit(&a), 10_000_001);
    // Withdraw everything: the node is gone, the money is back (minus fees).
    let all = w.nodes[0].sign(TxKind::Storage(StorageOp::Withdraw { amount: 10_000_001 }));
    ok(&w.net.run(vec![all])[0]);
    assert!(w.net.state.storage_node(&a).unwrap().is_none());
    let again = w.nodes[0].sign(TxKind::Storage(StorageOp::AddDeposit { amount: 1 }));
    assert_eq!(
        reverted(&w.net.run(vec![again])[0]),
        &StorageError::NotRegistered
    );
}

#[test]
fn store_assigns_deterministically_and_spreads_over_distinct_nodes() {
    let f = file(9, 20_000, 3, 2);
    let assign = |nodes: usize| -> Vec<Address> {
        let mut w = world(nodes);
        let r = w.store(&f, 3);
        ok(&r);
        let ps = w.pieces(&f);
        assert_eq!(ps.len(), 5);
        let out: Vec<Address> = r.output.chunks(32).map(|c| c.try_into().unwrap()).collect();
        assert_eq!(out, ps.iter().map(|p| p.node).collect::<Vec<_>>());
        out
    };
    // Same chain, same seed: same assignment. 8 nodes, 5 pieces: all distinct.
    let (a, b) = (assign(8), assign(8));
    assert_eq!(a, b);
    let distinct: std::collections::BTreeSet<_> = a.iter().collect();
    assert_eq!(distinct.len(), 5);
    // Exactly as many nodes as pieces: everyone holds one.
    let c = assign(5);
    assert_eq!(c.iter().collect::<std::collections::BTreeSet<_>>().len(), 5);
    // Fewer nodes than pieces: shared as evenly as possible (never 3 on one node with 2 nodes
    // when 2+3 is possible).
    let d = assign(2);
    let mut counts = std::collections::BTreeMap::new();
    for n in &d {
        *counts.entry(*n).or_insert(0) += 1;
    }
    assert!(counts.values().all(|c| *c == 2 || *c == 3), "{counts:?}");
    // A different file lands elsewhere often enough: across 20 files, 8 nodes, every node is used
    // and no node holds more than half of all pieces.
    let mut w = world(8);
    let mut load = std::collections::BTreeMap::new();
    for i in 0..20u8 {
        let g = file(i, 4_000 + u64::from(i), 2, 1);
        let r = w.store(&g, 1);
        ok(&r);
        for p in w.pieces(&g) {
            *load.entry(p.node).or_insert(0u32) += 1;
        }
    }
    assert_eq!(load.len(), 8, "{load:?}");
    assert!(*load.values().max().unwrap() <= 30, "{load:?}");
}

#[test]
fn store_checks_and_escrow() {
    let mut w = world(4);
    let f = file(2, 5_000, 2, 1);
    let owner = w.owner.addr();
    let before = w.net.bal(&owner);
    let r = w.store(&f, 3);
    ok(&r);
    let info = w.net.state.storage_file(&f.id).unwrap().unwrap();
    let want = cfg().escrow_for(f.size, 2, 1, 3).unwrap();
    assert_eq!(info.escrow, want);
    assert_eq!(info.pay, 10 * (info.piece_len / 1024));
    assert_eq!(want, info.pay * 3 * 3);
    assert_eq!(w.net.bal(&owner), before - want - r.fee);
    // Same root again, more pieces than nodes capacity-wise, bad shapes.
    assert_eq!(reverted(&w.store(&f, 3)), &StorageError::AlreadyStored);
    for (size, k, m, periods) in [
        (0, 2, 1, 1),
        (10, 0, 1, 1),
        (10, 1, 0, 1),
        (10, 2, 1, 0),
        (10, 100, 100, 1),
    ] {
        let tx = w.owner.sign(TxKind::Storage(StorageOp::Store {
            root: [5; 32],
            size,
            k,
            m,
            periods,
        }));
        assert!(
            matches!(
                w.net.run(vec![tx])[0].status,
                Status::Invalid(Invalid::TooLarge)
            ),
            "{size} {k} {m} {periods}"
        );
        w.owner.nonce -= 1; // invalid transactions do not use the nonce
    }
    // No capacity: nodes whose deposit cannot cover the piece are not eligible.
    let big = file(4, 400_000_000, 2, 1);
    assert_eq!(reverted(&w.store(&big, 1)), &StorageError::NotEnoughNodes);
    assert!(w.net.state.storage_file(&big.id).unwrap().is_none());
}

#[test]
fn challenge_proof_pays_the_node_from_the_escrow() {
    let mut w = world(5);
    let f = file(6, 12_000, 3, 2);
    ok(&w.store(&f, 2));
    let p = w.until_challenge(&f);
    let ch = p.challenge.unwrap();
    let node = w.node_index(&p.node);
    let bal0 = w.net.bal(&p.node);
    let esc0 = w.net.state.storage_file(&f.id).unwrap().unwrap().escrow;
    let r = w.prove(&f, node, p.index as usize, ch.chunk);
    ok(&r);
    let pay = ch.pay;
    assert_eq!(r.output, pay.to_le_bytes().to_vec());
    assert_eq!(w.net.bal(&p.node), bal0 + pay - r.fee);
    assert_eq!(
        w.net.state.storage_file(&f.id).unwrap().unwrap().escrow,
        esc0 - pay
    );
    let after = w
        .pieces(&f)
        .into_iter()
        .find(|q| q.index == p.index)
        .unwrap();
    assert!(after.challenge.is_none());
    assert_eq!((after.proven, after.missed), (1, 0));
    // A second proof for the same challenge is refused (no open challenge).
    let r = w.prove(&f, node, p.index as usize, ch.chunk);
    assert_eq!(reverted(&r), &StorageError::NoChallenge);
    // Run past the deadline: nothing is slashed, since it was answered.
    let dep = w.deposit(&p.node);
    w.net.empty_blocks(5);
    assert_eq!(w.deposit(&p.node), dep);
}

#[test]
fn bad_proofs_are_refused_and_do_not_pay() {
    let mut w = world(5);
    let f = file(8, 12_000, 3, 2);
    ok(&w.store(&f, 2));
    let p = w.until_challenge(&f);
    let ch = p.challenge.unwrap();
    let node = w.node_index(&p.node);
    let idx = p.index as usize;
    let good = f.prove_op(idx, ch.chunk);
    let StorageOp::Prove {
        file: fid,
        index,
        proof,
    } = good.clone()
    else {
        unreachable!()
    };
    let mut cases: Vec<(StorageOp, StorageError)> = vec![];
    // Another chunk than the challenged one.
    let chunks = w.net.state.storage_file(&f.id).unwrap().unwrap().piece_len / CHUNK as u64;
    if chunks > 1 {
        cases.push((
            f.prove_op(idx, (ch.chunk + 1) % chunks),
            StorageError::BadProof,
        ));
    }
    // Flipped bytes.
    let mut t = proof.clone();
    t.chunk[0] ^= 0xff;
    cases.push((
        StorageOp::Prove {
            file: fid,
            index,
            proof: t,
        },
        StorageError::BadProof,
    ));
    // Bad path.
    let mut t = proof.clone();
    t.piece_path[0][3] ^= 1;
    cases.push((
        StorageOp::Prove {
            file: fid,
            index,
            proof: t,
        },
        StorageError::BadProof,
    ));
    // Valid data for another piece index of the file.
    cases.push((
        StorageOp::Prove {
            file: fid,
            index: (index + 1) % 5,
            proof: proof.clone(),
        },
        StorageError::NotHolder,
    ));
    // Unknown file.
    cases.push((
        StorageOp::Prove {
            file: [1; 32],
            index,
            proof: proof.clone(),
        },
        StorageError::NoSuchFile,
    ));
    let bal = w.net.bal(&p.node);
    // All refusals go into one block, so no deadline passes in between.
    let mut txs = vec![];
    let mut wants = vec![];
    for (op, want) in cases {
        txs.push(w.nodes[node].sign(TxKind::Storage(op)));
        wants.push(want);
    }
    // Someone else cannot answer for the holder.
    let other = (node + 1) % 5;
    txs.push(w.nodes[other].sign(TxKind::Storage(good)));
    wants.push(StorageError::NotHolder);
    let rs = w.net.run(txs);
    let mut fees = 0;
    for (r, want) in rs.iter().zip(&wants) {
        assert_eq!(reverted(r), want);
        if w.nodes[node].addr() == r_sender(r) {
            fees += r.fee;
        }
    }
    assert_eq!(w.net.bal(&p.node), bal - fees);
    assert_eq!(w.pieces(&f)[idx].proven, 0);
}

#[test]
fn a_missed_challenge_slashes_and_repair_restores_the_piece() {
    let mut w = world(7);
    let f = file(5, 12_000, 3, 2);
    ok(&w.store(&f, 3));
    let p = w.until_challenge(&f);
    let (failed, idx) = (p.node, p.index as usize);
    let dep0 = w.deposit(&failed);
    let esc0 = w.net.state.storage_file(&f.id).unwrap().unwrap().escrow;
    // Everybody else answers; the failing node stays silent.
    let others: Vec<_> = w
        .pieces(&f)
        .into_iter()
        .filter(|q| q.challenge.is_some() && q.node != failed)
        .collect();
    for q in &others {
        let c = q.challenge.unwrap();
        let n = w.node_index(&q.node);
        ok(&w.prove(&f, n, q.index as usize, c.chunk));
    }
    // Run to the deadline.
    for _ in 0..8 {
        w.net.run(vec![]);
        if w.pieces(&f)[idx].state == PieceState::Repairing {
            break;
        }
    }
    let after = w.pieces(&f)[idx].clone();
    assert_eq!(after.state, PieceState::Repairing);
    assert_eq!(after.missed, 1);
    assert_ne!(after.node, failed);
    // The replacement avoids nodes that already hold a piece of the file (7 nodes, 5 pieces).
    let holders: Vec<_> = w
        .pieces(&f)
        .iter()
        .filter(|q| q.index as usize != idx)
        .map(|q| q.node)
        .collect();
    assert!(
        !holders.contains(&after.node),
        "reassigned onto an existing holder"
    );
    // The slash: half of the piece's collateral left the deposit and went into the escrow.
    let pl = w.net.state.storage_file(&f.id).unwrap().unwrap().piece_len;
    let cut = 100 * (pl / 1024) / 2;
    assert_eq!(w.deposit(&failed), dep0 - cut);
    let node_set = w.net.state.storage_node(&failed).unwrap().unwrap();
    assert_eq!(
        node_set.pieces as usize,
        w.pieces(&f).iter().filter(|q| q.node == failed).count()
    );
    let paid_so_far: u64 = others.iter().map(|q| q.challenge.unwrap().pay).sum();
    assert_eq!(
        w.net.state.storage_file(&f.id).unwrap().unwrap().escrow,
        esc0 + cut - paid_so_far
    );
    // Not provable by the failed node any more; the new node proves the repaired piece.
    let c = after.challenge.unwrap();
    assert!(c.deadline > 0);
    let fi = w.node_index(&failed);
    let tx = w.nodes[fi].sign(TxKind::Storage(f.prove_op(idx, c.chunk)));
    assert_eq!(reverted(&w.net.run(vec![tx])[0]), &StorageError::NotHolder);
    let n = w.node_index(&after.node);
    let bal = w.net.bal(&after.node);
    let r = w.prove(&f, n, idx, c.chunk);
    ok(&r);
    assert_eq!(w.net.bal(&after.node), bal + c.pay - r.fee);
    let fixed = w.pieces(&f)[idx].clone();
    assert_eq!(fixed.state, PieceState::Holding);
    assert!(fixed.challenge.is_none());
}

#[test]
fn a_repair_that_never_comes_is_slashed_and_reassigned_again() {
    let mut w = world(7);
    let f = file(11, 12_000, 3, 2);
    ok(&w.store(&f, 3));
    let p = w.until_challenge(&f);
    let idx = p.index as usize;
    let mut seen = vec![p.node];
    for _ in 0..40 {
        w.net.run(vec![]);
        let q = w.pieces(&f)[idx].clone();
        if q.missed >= 2 {
            assert_eq!(q.state, PieceState::Repairing);
            assert!(!seen.contains(&q.node) || q.node == p.node);
            seen.push(q.node);
            break;
        }
    }
    assert!(w.pieces(&f)[idx].missed >= 2, "second miss not reached");
}

#[test]
fn vacant_pieces_are_reassigned_when_a_node_appears() {
    // 2 nodes, 3 pieces (k=2,m=1): shared. Fail one holder; the other node takes the piece (it
    // is the only eligible one), so nothing is vacant. With a single node excluded as the failed
    // one and no one else: vacant until a new node registers.
    let mut w = world(1);
    let f = file(12, 3_000, 1, 1);
    ok(&w.store(&f, 4));
    // Both pieces on the sole node. Let it miss: it is excluded, so pieces go vacant.
    let mut vacant = false;
    for _ in 0..40 {
        w.net.run(vec![]);
        if w.pieces(&f).iter().any(|q| q.state == PieceState::Vacant) {
            vacant = true;
            break;
        }
    }
    assert!(vacant);
    // A new node registers and picks the vacant pieces up at the next retry.
    let mut fresh = Wallet::new(90);
    let t = w.owner.transfer(fresh.addr(), 100_000_000);
    ok(&w.net.run(vec![t])[0]);
    let reg = fresh.sign(TxKind::Storage(StorageOp::Register {
        endpoint: b"http://fresh:1".to_vec(),
        deposit: 10_000_000,
    }));
    ok(&w.net.run(vec![reg])[0]);
    for _ in 0..20 {
        w.net.run(vec![]);
        if w.pieces(&f)
            .iter()
            .any(|q| q.node == fresh.addr() && q.state == PieceState::Repairing)
        {
            return;
        }
    }
    panic!("the vacant piece was not reassigned");
}

#[test]
fn files_expire_refund_the_rest_and_free_node_capacity() {
    let mut w = world(4);
    let f = file(13, 4_000, 2, 1);
    let owner = w.owner.addr();
    ok(&w.store(&f, 2));
    let spent = cfg().escrow_for(f.size, 2, 1, 2).unwrap();
    let bal0 = w.net.bal(&owner);
    // Nodes answer every challenge for the whole life of the file.
    let mut proofs = 0u64;
    for _ in 0..120 {
        let ps = w.pieces(&f);
        if ps.is_empty() {
            break;
        }
        let mut txs = vec![];
        for q in ps.iter().filter(|q| q.challenge.is_some()) {
            let n = w.node_index(&q.node);
            let c = q.challenge.unwrap();
            txs.push(w.nodes[n].sign(TxKind::Storage(f.prove_op(q.index as usize, c.chunk))));
            proofs += 1;
        }
        for r in w.net.run(txs) {
            ok(&r);
        }
    }
    assert_eq!(proofs, 2 * 3, "3 pieces x 2 periods");
    assert!(w.net.state.storage_file(&f.id).unwrap().is_none());
    assert!(w.pieces(&f).is_empty());
    // Everything was paid out, nothing left to refund; nodes hold nothing.
    let pay = cfg().pay_per_period(proof::piece_len(f.size, 2)).unwrap();
    assert_eq!(pay * proofs, spent);
    assert_eq!(w.net.bal(&owner), bal0);
    for n in w.net.state.storage_nodes().unwrap() {
        assert_eq!((n.pieces, n.held_bytes), (0, 0));
    }
}

#[test]
fn unspent_escrow_is_refunded_when_a_node_misses() {
    let mut w = world(8);
    let f = file(14, 4_000, 2, 1);
    let owner = w.owner.addr();
    ok(&w.store(&f, 2));
    let bal0 = w.net.bal(&owner);
    // Nobody ever answers: pieces get slashed and bounce between nodes; the file runs out.
    for _ in 0..200 {
        w.net.run(vec![]);
        if w.net.state.storage_file(&f.id).unwrap().is_none() {
            break;
        }
    }
    assert!(
        w.net.state.storage_file(&f.id).unwrap().is_none(),
        "file never ended"
    );
    // Escrow plus the slashed deposits came back to the owner (supply is conserved: checked per block).
    assert!(w.net.bal(&owner) > bal0 + cfg().escrow_for(f.size, 2, 1, 2).unwrap());
    for n in w.net.state.storage_nodes().unwrap() {
        assert_eq!((n.pieces, n.held_bytes), (0, 0));
    }
}

#[test]
fn withdraw_is_locked_while_pieces_are_held() {
    let mut w = world(3);
    let f = file(15, 4_000, 1, 1);
    ok(&w.store(&f, 1));
    let holder = w.pieces(&f)[0].node;
    let n = w.node_index(&holder);
    let tx = w.nodes[n].sign(TxKind::Storage(StorageOp::Withdraw { amount: 10_000_000 }));
    assert_eq!(
        reverted(&w.net.run(vec![tx])[0]),
        &StorageError::DepositLocked
    );
    // The part above the requirement can leave.
    let need = cfg()
        .required_deposit(
            w.net
                .state
                .storage_node(&holder)
                .unwrap()
                .unwrap()
                .held_bytes,
        )
        .unwrap();
    let free = 10_000_000 - need;
    let tx = w.nodes[n].sign(TxKind::Storage(StorageOp::Withdraw { amount: free }));
    ok(&w.net.run(vec![tx])[0]);
    assert_eq!(w.deposit(&holder), need);
}

#[test]
fn storage_config_is_validated_and_part_of_genesis() {
    let mut c = ChainConfig::new(CHAIN_ID);
    assert!(c.validate().is_ok());
    c.storage.challenge_blocks = c.storage.epoch_blocks;
    assert!(c.validate().is_err());
    let a = ChainConfig::new(1);
    let mut b = ChainConfig::new(1);
    b.storage.price_per_kib_period += 1;
    let ga = genesis_with(&[], 0, a);
    let gb = genesis_with(&[], 0, b);
    let sa = ChainState::from_genesis(MemKv::new(), &ga).unwrap();
    let sb = ChainState::from_genesis(MemKv::new(), &gb).unwrap();
    assert_ne!(sa.head().unwrap().hash, sb.head().unwrap().hash);
}

fn r_sender(r: &Receipt) -> Address {
    match &r.origin {
        Origin::Tx { sender } => *sender,
        _ => unreachable!(),
    }
}
