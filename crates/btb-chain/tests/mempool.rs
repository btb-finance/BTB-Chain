mod support;
use btb_chain::*;
use btb_vm::FunctionKind;
use support::*;

fn pool() -> Mempool {
    Mempool::new(ChainConfig::new(CHAIN_ID), MempoolConfig::default())
}

const BAL: u64 = 1_000_000_000;

#[test]
fn rejects_bad_signature_wrong_chain_kind_and_duplicates() {
    let mut a = Wallet::new(1);
    let mut p = pool();
    let mut bad = a.transfer([2; 32], 1);
    bad.signature[5] ^= 0x10;
    assert_eq!(
        p.insert(bad, 0, BAL),
        Err(MempoolError::Invalid(Invalid::BadSignature))
    );
    let mut other = Wallet::new(1);
    other.chain_id = 99;
    assert!(matches!(
        p.insert(other.transfer([2; 32], 1), 0, BAL),
        Err(MempoolError::Invalid(Invalid::WrongChainId { .. }))
    ));
    for k in [
        FunctionKind::Query,
        FunctionKind::Action,
        FunctionKind::Cron,
    ] {
        let tx = Wallet::new(1).call_with([3; 32], k, "get", &(), GAS);
        assert_eq!(
            p.insert(tx, 0, BAL),
            Err(MempoolError::Invalid(Invalid::KindNotAllowed(k)))
        );
    }
    let good = a.transfer([2; 32], 1);
    let id = p.insert(good.clone(), 0, BAL).unwrap();
    assert_eq!(id, good.id());
    assert!(p.contains(&id));
    assert_eq!(p.insert(good, 0, BAL), Err(MempoolError::Duplicate));
    assert_eq!(p.len(), 1);
}

#[test]
fn rejects_used_nonces_and_unaffordable() {
    let mut a = Wallet::new(1);
    let mut p = pool();
    let tx = a.transfer([2; 32], 1);
    assert_eq!(
        p.insert(tx, 1, BAL),
        Err(MempoolError::NonceTooLow { next: 1, got: 0 })
    );
    let tx = a.transfer([2; 32], 1);
    assert!(matches!(
        p.insert(tx, 0, 10),
        Err(MempoolError::InsufficientBalance { .. })
    ));
}

#[test]
fn same_nonce_replaced_only_by_higher_fee() {
    let mut a = Wallet::new(1);
    let mut p = pool();
    let cheap = a.call_with([3; 32], FunctionKind::Mutation, "f", &(), 100_000);
    a.nonce = 0;
    let same = a.call_with([3; 32], FunctionKind::Mutation, "g", &(), 100_000);
    a.nonce = 0;
    let rich = a.call_with([3; 32], FunctionKind::Mutation, "f", &(), 200_000);
    p.insert(cheap.clone(), 0, BAL).unwrap();
    assert_eq!(p.insert(same, 0, BAL), Err(MempoolError::NonceTaken));
    p.insert(rich.clone(), 0, BAL).unwrap();
    assert_eq!(p.len(), 1);
    assert!(!p.contains(&cheap.id()));
    assert_eq!(p.select(|_| 0, 10, u64::MAX), vec![rich]);
}

#[test]
fn selection_orders_by_fee_but_respects_nonce_order() {
    let (mut a, mut b, mut c) = (Wallet::new(1), Wallet::new(2), Wallet::new(3));
    let mut p = pool();
    // a: nonce 0 cheap, nonce 1 very expensive (must not jump ahead of its nonce 0).
    let a0 = a.call_with([9; 32], FunctionKind::Mutation, "f", &(), 100_000);
    let a1 = a.call_with([9; 32], FunctionKind::Mutation, "f", &(), 900_000);
    let b0 = b.call_with([9; 32], FunctionKind::Mutation, "f", &(), 500_000);
    let c0 = c.call_with([9; 32], FunctionKind::Mutation, "f", &(), 300_000);
    // A nonce gap for c: nonce 2 without 1 is never selected.
    c.nonce = 2;
    let c2 = c.call_with([9; 32], FunctionKind::Mutation, "f", &(), 999_000);
    for t in [&a1, &c2, &b0, &a0, &c0] {
        p.insert((*t).clone(), t.body.nonce, BAL).unwrap();
    }
    let next = |_: &Address| 0;
    let got = p.select(next, 10, u64::MAX);
    let ids: Vec<_> = got.iter().map(|t| t.id()).collect();
    // Heads: b0 (500k), c0 (300k), a0 (100k); a0 unlocks a1 (900k), which comes out last
    // because it only became ready then.
    assert_eq!(ids, vec![b0.id(), c0.id(), a0.id(), a1.id()]);
    let pos = |t: &Transaction| ids.iter().position(|i| *i == t.id());
    assert_eq!(pos(&c2), None, "gap: not selectable");
    assert_eq!(got.len(), 4);
    // Limits.
    assert_eq!(p.select(next, 2, u64::MAX).len(), 2);
    assert_eq!(
        p.select(next, 10, 600_000).len(),
        2,
        "b0 and a0 fit; c0 and a1 do not"
    );
}

#[test]
fn size_limits_and_pruning() {
    let cfg = MempoolConfig {
        max_txs: 3,
        max_per_sender: 2,
        ..Default::default()
    };
    let mut p = Mempool::new(ChainConfig::new(CHAIN_ID), cfg);
    let mut a = Wallet::new(1);
    let mut b = Wallet::new(2);
    p.insert(a.transfer([1; 32], 1), 0, BAL).unwrap();
    p.insert(a.transfer([1; 32], 1), 0, BAL).unwrap();
    assert_eq!(
        p.insert(a.transfer([1; 32], 1), 0, BAL),
        Err(MempoolError::SenderFull)
    );
    p.insert(b.transfer([1; 32], 1), 0, BAL).unwrap();
    assert_eq!(
        p.insert(b.transfer([1; 32], 1), 0, BAL),
        Err(MempoolError::PoolFull)
    );
    let small = MempoolConfig {
        max_tx_bytes: 100,
        ..Default::default()
    };
    let mut p2 = Mempool::new(ChainConfig::new(CHAIN_ID), small);
    let big = Wallet::new(3).deploy(&[0u8; 500], manifest("x"));
    assert_eq!(p2.insert(big, 0, BAL), Err(MempoolError::TooLarge));

    // `a` used nonce 0 on chain: prune drops it.
    p.prune(|addr| if *addr == a.addr() { 1 } else { 0 });
    assert_eq!(p.len(), 2);
    let sel = p.select(|addr| if *addr == a.addr() { 1 } else { 0 }, 10, u64::MAX);
    assert_eq!(sel.len(), 2);
    p.remove(&sel);
    assert!(p.is_empty());
}

/// Mempool selection feeds block execution end to end.
#[test]
fn selected_transactions_execute_in_one_block() {
    let (mut a, mut b) = (Wallet::new(1), Wallet::new(2));
    let mut net = Net::new(btb_runtime::MemKv::new(), &genesis(&[&a, &b], BAL));
    let mut p = pool();
    for _ in 0..3 {
        p.insert(
            a.transfer(b.addr(), 10),
            net.state.nonce(&a.addr()).unwrap(),
            BAL,
        )
        .unwrap();
    }
    p.insert(b.transfer(a.addr(), 5), 0, BAL).unwrap();
    let sel = p.select(|x| net.state.nonce(x).unwrap(), 100, u64::MAX);
    assert_eq!(sel.len(), 4);
    let r = net.run(sel.clone());
    r.iter().for_each(ok);
    p.remove(&sel);
    assert!(p.is_empty());
}
