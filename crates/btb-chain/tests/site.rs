//! Site files: only the deployer publishes, content is addressed by SHA-256, limits hold, and
//! every refusal still charges the fee and changes nothing.

mod support;
use btb_chain::site::{self, SiteError, SiteOp, MAX_FILES, MAX_FILE_BYTES};
use btb_chain::*;
use btb_runtime::{merkle, MemKv};
use support::*;

fn setup() -> (Net<MemKv>, Wallet, Wallet, Address) {
    let mut owner = Wallet::new(1);
    let mallory = Wallet::new(2);
    let g = genesis(&[&owner, &mallory], 1_000_000_000_000);
    let mut net = Net::new(MemKv::new(), &g);
    let tx = owner.deploy(elf("counter"), manifest("site-host"));
    let c = deployed(&net.run(vec![tx])[0]);
    (net, owner, mallory, c)
}

fn publish(w: &mut Wallet, contract: Address, path: &str, content: &[u8]) -> Transaction {
    w.sign(TxKind::Site(SiteOp::Publish {
        contract,
        path: path.into(),
        content: content.to_vec(),
    }))
}

fn reverted(r: &Receipt) -> &Revert {
    match &r.status {
        Status::Reverted(e) => e,
        s => panic!("expected a revert, got {s:?}"),
    }
}

#[test]
fn deployer_publishes_and_the_file_is_content_addressed_and_provable() {
    let (mut net, mut owner, _, c) = setup();
    let html = b"<!doctype html><h1>hello</h1>";
    let tx = publish(&mut owner, c, "index.html", html);
    assert_eq!(tx.intrinsic_gas(), site::SITE_GAS + 100 * html.len() as u64);
    let r = net.run(vec![tx]);
    ok(&r[0]);
    let want = site::content_hash(html);
    assert_eq!(r[0].output, want.to_vec());
    assert_eq!(r[0].gas_used, r[0].fee, "gas price is 1");

    let f = site::get(net.state.store(), &c, "index.html")
        .unwrap()
        .unwrap();
    assert_eq!(f.content, html);
    assert_eq!(f.hash, want);
    assert_eq!(
        site::list(net.state.store(), &c).unwrap(),
        vec![("index.html".to_string(), want)]
    );

    // The stored entry is ordinary state: it has a Merkle proof against the block's root.
    let head = net.state.head().unwrap();
    let root = merkle::root(net.state.store(), head.height)
        .unwrap()
        .unwrap();
    let key = site::site_key(&c, "index.html");
    let (v, proof) = merkle::get_with_proof(net.state.store(), head.height, &key).unwrap();
    assert_eq!(v.as_deref(), Some(f.to_value().as_slice()));
    assert!(merkle::verify(&root, &key, v.as_deref(), &proof));

    // Replace, then remove.
    ok(&net.run(vec![publish(&mut owner, c, "index.html", b"v2")])[0]);
    assert_eq!(
        site::get(net.state.store(), &c, "index.html")
            .unwrap()
            .unwrap()
            .content,
        b"v2"
    );
    let rm = owner.sign(TxKind::Site(SiteOp::Remove {
        contract: c,
        path: "index.html".into(),
    }));
    ok(&net.run(vec![rm])[0]);
    assert!(site::get(net.state.store(), &c, "index.html")
        .unwrap()
        .is_none());
    let rm = owner.sign(TxKind::Site(SiteOp::Remove {
        contract: c,
        path: "index.html".into(),
    }));
    assert_eq!(
        reverted(&net.run(vec![rm])[0]),
        &Revert::Site(SiteError::NoSuchFile)
    );
}

#[test]
fn only_the_deployer_may_publish_or_remove() {
    let (mut net, mut owner, mut mallory, c) = setup();
    ok(&net.run(vec![publish(&mut owner, c, "index.html", b"mine")])[0]);
    let before = mallory_balance(&net, &mallory);
    let r = net.run(vec![publish(&mut mallory, c, "index.html", b"defaced")]);
    assert_eq!(reverted(&r[0]), &Revert::Site(SiteError::NotDeployer));
    assert!(r[0].fee > 0 && mallory_balance(&net, &mallory) == before - r[0].fee);
    let rm = mallory.sign(TxKind::Site(SiteOp::Remove {
        contract: c,
        path: "index.html".into(),
    }));
    assert_eq!(
        reverted(&net.run(vec![rm])[0]),
        &Revert::Site(SiteError::NotDeployer)
    );
    assert_eq!(
        site::get(net.state.store(), &c, "index.html")
            .unwrap()
            .unwrap()
            .content,
        b"mine"
    );
    // A new file by a stranger is refused too, and a contract that does not exist.
    let r = net.run(vec![publish(&mut mallory, c, "new.html", b"x")]);
    assert_eq!(reverted(&r[0]), &Revert::Site(SiteError::NotDeployer));
    assert!(site::get(net.state.store(), &c, "new.html")
        .unwrap()
        .is_none());
    let r = net.run(vec![publish(&mut owner, [9; 32], "a", b"x")]);
    assert_eq!(reverted(&r[0]), &Revert::Site(SiteError::NoSuchContract));
}

fn mallory_balance(net: &Net<MemKv>, w: &Wallet) -> u64 {
    net.bal(&w.addr())
}

#[test]
fn limits_and_bad_paths_are_refused_before_they_reach_a_block() {
    let (net, mut owner, _, c) = setup();
    let cfg = *net.state.config();
    let big = publish(&mut owner, c, "big.bin", &vec![0; MAX_FILE_BYTES + 1]);
    assert_eq!(
        btb_chain::exec::check_static(&big, &cfg),
        Err(Invalid::TooLarge)
    );
    for bad in ["../x", "/abs", "a//b", "", "a b"] {
        let tx = publish(&mut owner, c, bad, b"x");
        assert_eq!(
            btb_chain::exec::check_static(&tx, &cfg),
            Err(Invalid::TooLarge),
            "{bad:?}"
        );
    }
    let max = publish(&mut owner, c, "ok.bin", &vec![7; MAX_FILE_BYTES]);
    assert_eq!(btb_chain::exec::check_static(&max, &cfg), Ok(()));
    assert!(max.intrinsic_gas() <= cfg.max_tx_gas);
}

#[test]
fn file_count_is_capped_per_contract() {
    let (mut net, mut owner, _, c) = setup();
    for chunk in (0..MAX_FILES).collect::<Vec<_>>().chunks(64) {
        let txs: Vec<_> = chunk
            .iter()
            .map(|i| publish(&mut owner, c, &format!("f{i}"), b"x"))
            .collect();
        for r in net.run(txs) {
            ok(&r);
        }
    }
    assert_eq!(site::list(net.state.store(), &c).unwrap().len(), MAX_FILES);
    let r = net.run(vec![publish(&mut owner, c, "one-too-many", b"x")]);
    assert_eq!(reverted(&r[0]), &Revert::Site(SiteError::TooManyFiles));
    // Replacing an existing file is still fine.
    ok(&net.run(vec![publish(&mut owner, c, "f0", b"y")])[0]);
}

#[test]
fn site_files_do_not_leak_between_contracts_and_the_wire_form_round_trips() {
    let (mut net, mut owner, _, c) = setup();
    let tx = owner.deploy(elf("counter"), manifest("second"));
    let c2 = deployed(&net.run(vec![tx])[0]);
    ok(&net.run(vec![publish(&mut owner, c, "a", b"one")])[0]);
    ok(&net.run(vec![publish(&mut owner, c2, "a", b"two")])[0]);
    assert_eq!(
        site::get(net.state.store(), &c, "a")
            .unwrap()
            .unwrap()
            .content,
        b"one"
    );
    assert_eq!(
        site::get(net.state.store(), &c2, "a")
            .unwrap()
            .unwrap()
            .content,
        b"two"
    );
    assert_eq!(site::list(net.state.store(), &c).unwrap().len(), 1);

    let tx = publish(&mut owner, c, "w.txt", b"wire");
    assert_eq!(Transaction::decode(&tx.encode()).unwrap(), tx);
    // An unknown sub-tag is a decode error, not a panic.
    let mut bytes = tx.encode();
    bytes[8 + 8 + 1] = 77;
    assert!(Transaction::decode(&bytes).is_err());
}
