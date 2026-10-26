//! Files from contracts: `file_info` and `file_pin` (`examples/files`) on the chain. The executor
//! checks the supply invariant after every block, so each `net.run` below also proves that a
//! contract-funded escrow neither creates nor destroys BTB.

mod support;
use btb_chain::storage::proof;
use btb_chain::storage::{StorageConfig, StorageOp};
use btb_chain::*;
use btb_codec::{Address as CA, Decode};
use btb_runtime::MemKv;
use support::*;

const RICH: u64 = 1_000_000_000_000;
const FUND: u64 = 500_000_000;

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
                .map(|j| ((j as u32 ^ (u32::from(i) << 20) ^ (u32::from(seed) << 12)) >> 3) as u8)
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

struct World {
    net: Net<MemKv>,
    alice: Wallet,
    nodes: Vec<Wallet>,
    files: Address,
}

fn world(nodes: usize) -> World {
    let mut alice = Wallet::new(1);
    let mut ws: Vec<Wallet> = (0..nodes).map(|i| Wallet::new(10 + i as u8)).collect();
    let mut config = ChainConfig::new(CHAIN_ID);
    config.storage = cfg();
    let g = {
        let mut refs: Vec<&Wallet> = vec![&alice];
        refs.extend(ws.iter());
        genesis_with(&refs, RICH, config)
    };
    let mut net = Net::new(MemKv::new(), &g);
    let mut txs: Vec<_> = ws
        .iter_mut()
        .enumerate()
        .map(|(i, w)| {
            w.sign(TxKind::Storage(StorageOp::Register {
                endpoint: format!("http://127.0.0.1:{}", 9000 + i).into_bytes(),
                deposit: 10_000_000,
            }))
        })
        .collect();
    txs.push(alice.deploy(elf("files"), manifest("files")));
    let rec = net.run(txs);
    rec[..nodes].iter().for_each(ok);
    let files = deployed(&rec[nodes]);
    let t = alice.transfer(files, FUND);
    ok(&net.run(vec![t])[0]);
    World {
        net,
        alice,
        nodes: ws,
        files,
    }
}

impl World {
    fn call(&mut self, f: &str, args: &impl btb_codec::Encode) -> Receipt {
        let tx = self.alice.call(self.files, f, args);
        self.net.run(vec![tx]).remove(0)
    }
    fn info(&mut self, root: Hash) -> (u64, u8, u64) {
        let a = self.alice.addr();
        let out = self
            .net
            .query(&self.files.clone(), "info", &(CA(root),), &a);
        <(u64, u8, u64)>::decode(&out).unwrap()
    }
    fn store_by_client(&mut self, f: &File, periods: u32) {
        let tx = self.alice.sign(TxKind::Storage(StorageOp::Store {
            root: f.id,
            size: f.size,
            k: f.k,
            m: f.m,
            periods,
        }));
        ok(&self.net.run(vec![tx])[0]);
    }
}

fn status_of(r: &Receipt) -> u8 {
    ok(r);
    u8::decode(&r.output).unwrap()
}

#[test]
fn unknown_root_is_status_unknown_and_attach_refuses_it() {
    let mut w = world(4);
    let f = file(1, 3_000, 2, 1);
    assert_eq!(w.info(f.id), (0, 0, 0));
    let r = w.call("attach", &("a".to_string(), CA(f.id)));
    assert!(matches!(r.status, Status::Reverted(_)), "{r:?}");
    // attach_unchecked stores any root; the query still says it is not available.
    ok(&w.call("attach_unchecked", &("a".to_string(), CA(f.id))));
    let a = w.alice.addr();
    let out = w
        .net
        .query(&w.files.clone(), "is_available", &("a".to_string(),), &a);
    assert!(!bool::decode(&out).unwrap());
}

#[test]
fn client_paid_store_is_visible_to_the_contract() {
    let mut w = world(4);
    let f = file(2, 5_000, 2, 1);
    w.store_by_client(&f, 3);
    let (size, status, expiry) = w.info(f.id);
    assert_eq!((size, status), (5_000, 1));
    let head = w.net.state.head().unwrap().height;
    assert_eq!(expiry, head + 3 * cfg().epoch_blocks);
    let r = w.call("attach", &("doc".to_string(), CA(f.id)));
    ok(&r);
    assert_eq!(u64::decode(&r.output).unwrap(), 5_000);
    let a = w.alice.addr();
    let out = w
        .net
        .query(&w.files.clone(), "is_available", &("doc".to_string(),), &a);
    assert!(bool::decode(&out).unwrap());
    let out = w
        .net
        .query(&w.files.clone(), "root_of", &("doc".to_string(),), &a);
    assert_eq!(Option::<CA>::decode(&out).unwrap(), Some(CA(f.id)));
}

#[test]
fn contract_funded_pin_pays_the_escrow_from_the_contract_balance() {
    let mut w = world(4);
    let f = file(3, 4_000, 2, 1);
    let escrow = cfg().escrow_for(f.size, 2, 1, 2).unwrap();
    let before = w.net.bal(&w.files);
    let args = ("doc".to_string(), CA(f.id), f.size, f.k, f.m, 2u32);
    let r = w.call("pin_and_attach", &args);
    assert_eq!(status_of(&r), 0);
    let fi = w.net.state.storage_file(&f.id).unwrap().unwrap();
    assert_eq!((fi.owner, fi.escrow), (w.files, escrow));
    // The balance dropped by the escrow plus the storage deposit of the one key it wrote.
    let after = w.net.bal(&w.files);
    assert!(before - after >= escrow, "{before} -> {after}");
    assert!(before - after < escrow + 10_000, "{before} -> {after}");
    assert_eq!(w.info(f.id).1, 1);
    // Pinning the same root again pays nothing and still attaches.
    let again = w.net.bal(&w.files);
    let r = w.call(
        "pin_and_attach",
        &("copy".to_string(), CA(f.id), f.size, f.k, f.m, 2u32),
    );
    assert_eq!(status_of(&r), 0);
    assert!(again - w.net.bal(&w.files) < 10_000);
    assert_eq!(
        w.net.state.storage_file(&f.id).unwrap().unwrap().escrow,
        escrow
    );
}

#[test]
fn pin_failures_are_statuses_not_reverts() {
    let mut w = world(4);
    // Balance too low: a file whose escrow is more than the contract holds.
    let big = file(4, 8 << 20, 4, 2);
    let r = w.call(
        "pin_and_attach",
        &(
            "big".to_string(),
            CA(big.id),
            big.size,
            big.k,
            big.m,
            100_000u32,
        ),
    );
    assert_eq!(status_of(&r), 2);
    // Bad shape.
    let f = file(5, 1_000, 1, 1);
    let r = w.call(
        "pin_and_attach",
        &("x".to_string(), CA(f.id), 0u64, 1u8, 1u8, 1u32),
    );
    assert_eq!(status_of(&r), 4);
    let r = w.call(
        "pin_and_attach",
        &("x".to_string(), CA(f.id), f.size, 0u8, 1u8, 1u32),
    );
    assert_eq!(status_of(&r), 4);
    // Nothing was stored.
    assert!(w.net.state.storage_file(&f.id).unwrap().is_none());
    assert_eq!(w.info(f.id), (0, 0, 0));
    // A pin from a query traps.
    let a = w.alice.addr();
    let err = w.net.ex.query(
        &mut w.net.state,
        &w.files.clone(),
        "bad_pin_in_query",
        &btb_codec::Encode::encode(&(CA(f.id),)),
        &a,
    );
    assert!(err.is_err());
}

#[test]
fn no_storage_nodes_is_a_status() {
    let mut w = world(0);
    let f = file(6, 2_000, 1, 1);
    let r = w.call(
        "pin_and_attach",
        &("n".to_string(), CA(f.id), f.size, f.k, f.m, 1u32),
    );
    assert_eq!(status_of(&r), 3);
    assert_eq!(w.info(f.id), (0, 0, 0));
}

#[test]
fn a_file_that_ran_its_course_is_expired_and_the_contract_gets_the_rest_back() {
    let mut w = world(4);
    let f = file(7, 4_000, 2, 1);
    let r = w.call(
        "pin_and_attach",
        &("d".to_string(), CA(f.id), f.size, f.k, f.m, 2u32),
    );
    assert_eq!(status_of(&r), 0);
    assert_eq!(w.info(f.id).1, 1);
    // Nodes answer every challenge for the file's whole life.
    for _ in 0..120 {
        let ps = w.net.state.storage_pieces(&f.id).unwrap();
        if ps.is_empty() {
            break;
        }
        let mut txs = vec![];
        for q in ps.iter().filter(|q| q.challenge.is_some()) {
            let n = w.nodes.iter().position(|x| x.addr() == q.node).unwrap();
            let c = q.challenge.unwrap();
            let idx = q.index as usize;
            let p = proof::prove_chunk(&f.pieces[idx], c.chunk as usize, &f.roots, idx).unwrap();
            txs.push(w.nodes[n].sign(TxKind::Storage(StorageOp::Prove {
                file: f.id,
                index: q.index,
                proof: p,
            })));
        }
        for r in w.net.run(txs) {
            ok(&r);
        }
    }
    let (size, status, expiry) = w.info(f.id);
    assert_eq!((size, status), (4_000, 2));
    assert!(expiry > 0);
    // Expired is not available: attach refuses.
    let r = w.call("attach", &("e".to_string(), CA(f.id)));
    assert!(matches!(r.status, Status::Reverted(_)));
    // Storing it again clears the expired marker.
    w.store_by_client(&f, 1);
    assert_eq!(w.info(f.id).1, 1);
}
