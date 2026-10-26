//! Execution rules: signatures, nonces, fees, reverts, deploys, crons, supply.

mod support;
use btb_chain::*;
use btb_codec::Decode;
use btb_runtime::MemKv;
use btb_vm::FunctionKind;
use support::*;

type MemNet = Net<MemKv>;

fn net(wallets: &[&Wallet], each: u64) -> MemNet {
    Net::new(MemKv::new(), &genesis(wallets, each))
}

const RICH: u64 = 10 * UNITS_PER_BTB;

#[test]
fn supply_units_and_genesis() {
    assert_eq!(BTB_DECIMALS, 9);
    assert_eq!(TOTAL_SUPPLY, 1_000_000_000_000_000_000);
    let a = Wallet::new(1);
    let n = net(&[&a], RICH);
    assert_eq!(n.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));
    assert_eq!(n.bal(&a.addr()), RICH);
    assert_eq!(n.state.head().unwrap().height, 0);
}

#[test]
fn genesis_rejects_wrong_supply_and_duplicates() {
    let a = Wallet::new(1);
    let mut g = genesis(&[&a], RICH);
    g.allocations[1].1 -= 1;
    assert!(matches!(
        ChainState::from_genesis(MemKv::new(), &g),
        Err(GenesisError::WrongSupply { .. })
    ));
    let mut g = genesis(&[&a], RICH);
    g.allocations.push((a.addr(), 0));
    assert!(matches!(
        ChainState::from_genesis(MemKv::new(), &g),
        Err(GenesisError::DuplicateAddress(_))
    ));
    let mut g = genesis(&[&a], RICH);
    g.config.gas_price = 0;
    assert!(matches!(
        ChainState::from_genesis(MemKv::new(), &g),
        Err(GenesisError::Config(_))
    ));
    // Cannot create a second chain in a used store.
    let n = net(&[&a], RICH);
    let kv = n.state.into_store();
    assert!(matches!(
        ChainState::from_genesis(kv, &genesis(&[&a], RICH)),
        Err(GenesisError::NotEmpty)
    ));
}

#[test]
fn transfer_without_relayer_pays_validator_pool() {
    let (mut a, b) = (Wallet::new(1), Wallet::new(2));
    let mut n = net(&[&a, &b], RICH);
    let r = n.run(vec![a.transfer(b.addr(), 1_000_000)]);
    ok(&r[0]);
    let fee = TRANSFER_GAS; // gas_price 1
    assert_eq!(r[0].fee, fee);
    assert_eq!(n.bal(&a.addr()), RICH - 1_000_000 - fee);
    assert_eq!(n.bal(&b.addr()), RICH + 1_000_000);
    // No relayer in the body: the whole fee is the validator pool's (tests/relay.rs pays one).
    assert_eq!(n.state.validator_pool_balance().unwrap(), fee);
    assert_eq!(n.state.relay_pool_balance().unwrap(), 0);
    assert_eq!(n.state.nonce(&a.addr()).unwrap(), 1);
    assert_eq!(n.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));
}

#[test]
fn bad_signature_is_invalid_and_changes_nothing() {
    let (mut a, b) = (Wallet::new(1), Wallet::new(2));
    let mut n = net(&[&a, &b], RICH);
    let mut forged = a.transfer(b.addr(), 5);
    forged.signature[0] ^= 1;
    // Body altered after signing.
    let mut tampered = a.transfer(b.addr(), 5);
    if let TxKind::Transfer { amount, .. } = &mut tampered.body.kind {
        *amount = 6;
    }
    // Signed by someone else but claiming a's key.
    let mut stolen = Wallet::new(9).transfer(b.addr(), 5);
    stolen.public_key = a.key.verifying_key().to_bytes();
    // A public key that is not a curve point.
    let mut junk = a.transfer(b.addr(), 5);
    junk.public_key = [0xFF; 32];
    let before = (
        n.bal(&a.addr()),
        n.bal(&b.addr()),
        n.state.nonce(&a.addr()).unwrap(),
    );
    let r = n.run(vec![forged, tampered, stolen, junk]);
    for r in &r {
        assert_eq!(r.status, Status::Invalid(Invalid::BadSignature));
        assert_eq!((r.fee, r.gas_used), (0, 0));
    }
    let after = (
        n.bal(&a.addr()),
        n.bal(&b.addr()),
        n.state.nonce(&a.addr()).unwrap(),
    );
    assert_eq!(before, after);
    assert_eq!(n.state.validator_pool_balance().unwrap(), 0);
}

#[test]
fn wrong_chain_id_is_invalid() {
    let (mut a, b) = (Wallet::new(1), Wallet::new(2));
    let mut n = net(&[&a, &b], RICH);
    a.chain_id = CHAIN_ID + 1;
    let r = n.run(vec![a.transfer(b.addr(), 5)]);
    assert_eq!(
        r[0].status,
        Status::Invalid(Invalid::WrongChainId {
            expected: CHAIN_ID,
            got: CHAIN_ID + 1
        })
    );
    assert_eq!(n.bal(&b.addr()), RICH);
}

#[test]
fn replay_and_nonce_gaps_are_invalid() {
    let (mut a, b) = (Wallet::new(1), Wallet::new(2));
    let mut n = net(&[&a, &b], RICH);
    let tx = a.transfer(b.addr(), 100);
    // Same transaction twice in a block: the second is a replay.
    let r = n.run(vec![tx.clone(), tx.clone()]);
    ok(&r[0]);
    assert_eq!(
        r[1].status,
        Status::Invalid(Invalid::BadNonce {
            expected: 1,
            got: 0
        })
    );
    // And again in a later block.
    let r = n.run(vec![tx]);
    assert!(matches!(
        r[0].status,
        Status::Invalid(Invalid::BadNonce { .. })
    ));
    assert_eq!(n.bal(&b.addr()), RICH + 100);
    // A gap (nonce 5 while 1 is expected) is not executed either.
    a.nonce = 5;
    let r = n.run(vec![a.transfer(b.addr(), 1)]);
    assert_eq!(
        r[0].status,
        Status::Invalid(Invalid::BadNonce {
            expected: 1,
            got: 5
        })
    );
    // The same signed bytes cannot be replayed on another chain: chain id is signed.
    let mut other = Wallet::new(1);
    other.chain_id = 8;
    let tx8 = other.transfer(b.addr(), 1);
    let mut moved = tx8.clone();
    moved.body.chain_id = CHAIN_ID;
    moved.signature = tx8.signature;
    assert!(!moved.verify_signature());
}

#[test]
fn insufficient_balance_for_fee_is_invalid() {
    let (mut poor, b) = (Wallet::new(1), Wallet::new(2));
    let mut n = net(&[&poor, &b], TRANSFER_GAS - 1);
    let r = n.run(vec![poor.transfer(b.addr(), 0)]);
    assert_eq!(
        r[0].status,
        Status::Invalid(Invalid::InsufficientBalance {
            needed: TRANSFER_GAS,
            available: TRANSFER_GAS - 1
        })
    );
    assert_eq!(
        n.state.nonce(&poor.addr()).unwrap(),
        0,
        "nonce not consumed"
    );
    // Amount plus fee must be covered.
    let mut p2 = Wallet::new(3);
    let mut n = net(&[&p2, &b], 10_000);
    let r = n.run(vec![p2.transfer(b.addr(), 10_000)]);
    assert!(matches!(
        r[0].status,
        Status::Invalid(Invalid::InsufficientBalance { .. })
    ));
    // A call whose reserved gas exceeds the balance.
    let mut p3 = Wallet::new(4);
    let mut n = net(&[&p3], 1_000);
    let r = n.run(vec![p3.call([1; 32], "x", &())]);
    assert!(matches!(
        r[0].status,
        Status::Invalid(Invalid::InsufficientBalance { .. })
    ));
}

#[test]
fn queries_and_actions_are_not_transactions() {
    let mut a = Wallet::new(1);
    let mut n = net(&[&a], RICH);
    for kind in [
        FunctionKind::Query,
        FunctionKind::Action,
        FunctionKind::Cron,
    ] {
        let tx = a.call_with([1; 32], kind, "get", &(), GAS);
        // They even decode (so the error is explicit), then are rejected.
        assert_eq!(Transaction::decode(&tx.encode()).unwrap(), tx);
        let r = n.run(vec![tx]);
        assert_eq!(r[0].status, Status::Invalid(Invalid::KindNotAllowed(kind)));
    }
    assert_eq!(n.state.nonce(&a.addr()).unwrap(), 0);
}

#[test]
fn fee_cap_and_gas_limits() {
    let mut a = Wallet::new(1);
    let mut n = net(&[&a], RICH);
    let cap = a.sign(TxKind::Call {
        contract: [1; 32],
        kind: FunctionKind::Mutation,
        function: "f".into(),
        args: vec![],
        gas_limit: 100_000,
        max_fee: 99_999,
    });
    let low = a.call_with([1; 32], FunctionKind::Mutation, "f", &(), 10);
    let high = a.call_with([1; 32], FunctionKind::Mutation, "f", &(), 50_000_001);
    let r = n.run(vec![cap, low, high]);
    assert!(matches!(
        r[0].status,
        Status::Invalid(Invalid::FeeCapTooLow { .. })
    ));
    assert!(matches!(
        r[1].status,
        Status::Invalid(Invalid::GasLimitTooLow { .. })
    ));
    assert!(matches!(
        r[2].status,
        Status::Invalid(Invalid::GasLimitTooHigh { .. })
    ));
}

#[test]
fn deploy_and_call_counter_with_cron_and_deposit() {
    let mut a = Wallet::new(1);
    let mut n = net(&[&a], RICH);
    let mut m = manifest("counter");
    m.crons = vec![("tick".into(), 3)];
    let r = n.run(vec![a.deploy(elf("counter"), m)]);
    let c = deployed(&r[0]);
    assert_eq!(c, contract_address(&a.addr(), 0));
    assert!(n.state.contract(&c).unwrap().is_some());
    assert_eq!(
        r[0].fee,
        DEPLOY_BASE_GAS + DEPLOY_BYTE_GAS * elf("counter").len() as u64
    );

    // Unfunded contract cannot pay its storage deposit: the call reverts, fee is charged.
    let before = n.bal(&a.addr());
    let r = n.run(vec![a.call(c, "increment", &(5u64,))]);
    assert!(
        matches!(r[0].status, Status::Reverted(Revert::Fault(_))),
        "{:?}",
        r[0]
    );
    assert_eq!(before - n.bal(&a.addr()), r[0].fee);
    assert!(r[0].fee > 0);
    assert_eq!(n.state.nonce(&a.addr()).unwrap(), 2);

    // Fund the contract, then it works.
    let r = n.run(vec![
        a.transfer(c, 1_000_000_000),
        a.call(c, "increment", &(5u64,)),
    ]);
    ok(&r[0]);
    ok(&r[1]);
    assert_eq!(u64::decode(&r[1].output).unwrap(), 5);
    let who = a.addr();
    assert_eq!(u64::decode(&n.query(&c, "get", &(), &who)).unwrap(), 5);
    assert!(n.state.deposit(&c).unwrap() > 0, "storage deposit locked");
    assert_eq!(n.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));

    // Cron `tick` every 3 blocks: deployed at height 1, so it is due at 4, 7 and 10.
    assert_eq!(n.state.head().unwrap().height, 3);
    let pool_before = n.state.validator_pool_balance().unwrap();
    let crons = n.empty_blocks(9); // heights 4..=12
    assert_eq!(crons.len(), 3, "three runs in nine blocks");
    for r in &crons {
        ok(r);
        assert!(matches!(r.origin, Origin::Cron { .. }));
        assert!(r.fee > 0);
    }
    let paid: u64 = crons.iter().map(|c| c.fee).sum();
    assert_eq!(
        n.state.validator_pool_balance().unwrap(),
        pool_before + paid
    );
    assert_eq!(u64::decode(&n.query(&c, "get", &(), &who)).unwrap(), 5 + 3);
}

#[test]
fn cron_pauses_when_contract_is_empty_and_resumes_when_funded() {
    let mut a = Wallet::new(1);
    let mut n = net(&[&a], RICH);
    let mut m = manifest("counter");
    m.crons = vec![("tick".into(), 2)];
    let c = deployed(&n.run(vec![a.deploy(elf("counter"), m)])[0]);
    // Heights: deploy at 1, due at 3. Unfunded: paused.
    let r = n.empty_blocks(2);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].status, Status::Paused);
    assert_eq!((r[0].gas_used, r[0].fee), (0, 0));
    // Fund; the next due height (5) runs it.
    n.run(vec![a.transfer(c, 100_000_000)]); // height 4
    let r = n.empty_blocks(1); // height 5
    assert_eq!(r.len(), 1);
    ok(&r[0]);
}

#[test]
fn cron_gas_cap_defers_extra_runs_to_the_next_block() {
    let mut a = Wallet::new(1);
    let mut cfg = ChainConfig::new(CHAIN_ID);
    cfg.cron_gas_block_cap = cfg.cron_gas_limit; // room for one cron per block
    let mut n = Net::new(MemKv::new(), &genesis_with(&[&a], RICH, cfg));
    let mut m = manifest("counter");
    m.crons = vec![("tick".into(), 1)];
    let r = n.run(vec![
        a.deploy(elf("counter"), m.clone()),
        a.deploy(elf("counter"), m),
    ]);
    let (c1, c2) = (deployed(&r[0]), deployed(&r[1]));
    // Both crons are due at height 2 and funded in that same block; the cap lets one run.
    let r = n.run(vec![
        a.transfer(c1, 100_000_000),
        a.transfer(c2, 100_000_000),
    ]);
    assert_eq!(r.len(), 3);
    let first = r[2].origin.clone();
    ok(&r[2]);
    // Next block: the deferred one (due 2) goes before the first one's next run (due 3).
    let r = n.empty_blocks(1);
    assert_eq!(r.len(), 1);
    assert_ne!(r[0].origin, first);
}

#[test]
fn cron_cap_is_fair_every_deferred_cron_runs_in_turn() {
    let mut a = Wallet::new(1);
    let mut cfg = ChainConfig::new(CHAIN_ID);
    cfg.cron_gas_block_cap = cfg.cron_gas_limit; // one cron per block, three always due
    let mut n = Net::new(MemKv::new(), &genesis_with(&[&a], RICH, cfg));
    let mut m = manifest("counter");
    m.crons = vec![("tick".into(), 1)];
    let r = n.run(vec![
        a.deploy(elf("counter"), m.clone()),
        a.deploy(elf("counter"), m.clone()),
        a.deploy(elf("counter"), m),
    ]);
    let cs: Vec<_> = r.iter().map(deployed).collect();
    let mut fund: Vec<_> = cs.iter().map(|c| a.transfer(*c, 100_000_000)).collect();
    let mut seen = Vec::new();
    let mut first = n.run(std::mem::take(&mut fund));
    seen.extend(first.drain(3..));
    seen.extend(n.empty_blocks(8));
    // 9 blocks, one run each, and never the same contract twice before the others ran.
    assert_eq!(seen.len(), 9);
    let who: Vec<_> = seen
        .iter()
        .map(|r| match &r.origin {
            Origin::Cron { contract, .. } => *contract,
            o => panic!("{o:?}"),
        })
        .collect();
    for w in who.windows(3) {
        assert!(w[0] != w[1] && w[1] != w[2] && w[0] != w[2], "{who:?}");
    }
    // The same order repeats on a fresh run (deterministic).
    assert_eq!(who[..3], who[3..6]);
    assert_eq!(who[..3], who[6..9]);
}

#[test]
fn gas_exhaustion_charges_fee_and_reverts() {
    let mut a = Wallet::new(1);
    let mut n = net(&[&a], RICH);
    let c = deployed(&n.run(vec![a.deploy(elf("counter"), manifest("counter"))])[0]);
    n.run(vec![a.transfer(c, 1_000_000_000)]);
    let who = a.addr();
    let before = n.bal(&who);
    let limit = 3_000; // above intrinsic, far too little for the VM run
    let r = n.run(vec![a.call_with(
        c,
        FunctionKind::Mutation,
        "increment",
        &(1u64,),
        limit,
    )]);
    assert_eq!(r[0].status, Status::Reverted(Revert::OutOfGas));
    assert_eq!(r[0].gas_used, limit);
    assert_eq!(r[0].fee, limit);
    assert_eq!(before - n.bal(&who), limit);
    assert_eq!(
        u64::decode(&n.query(&c, "get", &(), &who)).unwrap(),
        0,
        "reverted"
    );
    assert_eq!(n.state.nonce(&who).unwrap(), 3, "nonce consumed");
    assert_eq!(n.state.deposit(&c).unwrap(), 0);
    assert!(n.state.validator_pool_balance().unwrap() >= limit);
    // Unused gas is refunded: a normal call costs far less than its limit.
    let r = n.run(vec![a.call(c, "increment", &(1u64,))]);
    ok(&r[0]);
    assert!(
        r[0].fee < GAS / 10,
        "fee {} is for gas used, not the limit",
        r[0].fee
    );
}

#[test]
fn abort_unknown_function_and_missing_contract_revert() {
    let mut a = Wallet::new(1);
    let mut n = net(&[&a], RICH);
    let c = deployed(&n.run(vec![a.deploy(elf("counter"), manifest("counter"))])[0]);
    n.run(vec![a.transfer(c, 1_000_000_000)]);
    let r = n.run(vec![
        a.call(c, "increment", &(u64::MAX,)),
        a.call(c, "increment", &(1u64,)), // overflows: aborts
        a.call(c, "nope", &()),
        a.call([9; 32], "increment", &(1u64,)),
        // `get` is a query: as a mutation it does not exist.
        a.call(c, "get", &()),
    ]);
    ok(&r[0]);
    assert!(matches!(&r[1].status, Status::Reverted(Revert::Abort(m)) if m.contains("overflow")));
    assert_eq!(r[2].status, Status::Reverted(Revert::NoSuchFunction));
    assert_eq!(r[3].status, Status::Reverted(Revert::NoSuchContract));
    assert_eq!(r[4].status, Status::Reverted(Revert::NoSuchFunction));
    for x in &r[1..] {
        assert!(x.fee > 0);
    }
    let who = a.addr();
    assert_eq!(
        u64::decode(&n.query(&c, "get", &(), &who)).unwrap(),
        u64::MAX
    );
}

#[test]
fn bad_deploys_revert_and_charge() {
    let mut a = Wallet::new(1);
    let mut n = net(&[&a], RICH);
    let mut bad_cron = manifest("x");
    bad_cron.crons = vec![("missing".into(), 1)];
    let mut zero = manifest("x");
    zero.crons = vec![("tick".into(), 0)];
    let mut idx = manifest("x");
    idx.indexes = vec!["nonsense".into()];
    let r = n.run(vec![
        a.deploy(b"not an elf", manifest("x")),
        a.deploy(elf("counter"), bad_cron),
        a.deploy(elf("counter"), zero),
        a.deploy(elf("counter"), idx),
    ]);
    assert!(matches!(
        r[0].status,
        Status::Reverted(Revert::BadContract(_))
    ));
    for x in &r[1..] {
        assert!(
            matches!(x.status, Status::Reverted(Revert::BadManifest(_))),
            "{x:?}"
        );
    }
    for x in &r {
        assert!(x.fee > 0);
    }
    assert_eq!(n.state.nonce(&a.addr()).unwrap(), 4);
}

#[test]
fn token_contract_end_to_end() {
    let (mut a, mut b) = (Wallet::new(1), Wallet::new(2));
    let mut n = net(&[&a, &b], RICH);
    let t = deployed(&n.run(vec![a.deploy(elf("token"), manifest("token"))])[0]);
    n.run(vec![a.transfer(t, 1_000_000_000)]);
    let ba = btb_codec::Address(b.addr());
    let aa = btb_codec::Address(a.addr());
    let r = n.run(vec![
        a.call(t, "init", &(1_000u128,)),
        a.call(t, "transfer", &(ba, 250u128)),
        b.call(t, "transfer", &(aa, 50u128)),
    ]);
    ok(&r[0]);
    ok(&r[1]);
    assert_eq!(bool::decode(&r[1].output), Ok(true));
    let who = a.addr();
    assert_eq!(
        u128::decode(&n.query(&t, "balance_of", &(ba,), &who)).unwrap(),
        200
    );
    assert_eq!(
        u128::decode(&n.query(&t, "total_supply", &(), &who)).unwrap(),
        1_000
    );
    assert_eq!(n.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));
}

#[test]
fn todo_contract_declares_its_indexes() {
    // No `manifest.indexes`: the chain reads the indexes from the schema in the ELF.
    let (mut a, mut b) = (Wallet::new(1), Wallet::new(2));
    let mut n = net(&[&a, &b], RICH);
    let m = manifest("todo");
    assert!(m.indexes.is_empty());
    let t = deployed(&n.run(vec![a.deploy(elf("todo"), m)])[0]);
    n.run(vec![a.transfer(t, 1_000_000_000)]);
    let r = n.run(vec![
        a.call(t, "add", &("buy milk".to_string(),)),
        b.call(t, "add", &("write docs".to_string(),)),
        a.call(t, "add", &("ship it".to_string(),)),
    ]);
    r.iter().for_each(ok);
    type Item = (u64, bool, String);
    let list = |n: &mut MemNet, who: &Address| {
        Vec::<Item>::decode(&n.query(&t, "list_mine", &(), who)).unwrap()
    };
    let (wa, wb) = (a.addr(), b.addr());
    assert_eq!(list(&mut n, &wa).len(), 2);
    assert_eq!(list(&mut n, &wb).len(), 1);
    // The index registry survives a restart of the node (rebuilt from state).
    let kv = n.state.into_store();
    let mut n = Net {
        state: ChainState::open(kv).unwrap(),
        ex: Executor::new(),
    };
    assert_eq!(list(&mut n, &wa).len(), 2);
    let r = n.run(vec![b.call(t, "add", &("again".to_string(),))]);
    ok(&r[0]);
    assert_eq!(list(&mut n, &wb).len(), 2);
}

#[test]
fn block_level_errors_leave_state_untouched() {
    let (mut a, b) = (Wallet::new(1), Wallet::new(2));
    let mut n = net(&[&a, &b], RICH);
    let tx = a.transfer(b.addr(), 1);
    let good = n.block_for(vec![tx.clone()]);
    let root = n.state.root().unwrap();
    let head = n.state.head().unwrap();

    let mut cases: Vec<(Block, BlockError)> = Vec::new();
    let mut x = good.clone();
    x.header.height = 5;
    cases.push((
        x,
        BlockError::BadHeight {
            expected: 1,
            got: 5,
        },
    ));
    let mut x = good.clone();
    x.header.parent_hash = [1; 32];
    cases.push((x, BlockError::BadParent));
    let mut x = good.clone();
    x.header.state_root = [1; 32];
    cases.push((x, BlockError::BadStateRoot));
    let mut x = good.clone();
    x.header.timestamp = head.timestamp - 1;
    cases.push((x, BlockError::TimestampBeforeParent));
    let mut x = good.clone();
    x.txs.push(tx.clone());
    cases.push((x, BlockError::BadTxRoot));
    let mut cfg = ChainConfig::new(CHAIN_ID);
    cfg.max_block_txs = 1;
    cfg.block_gas_limit = TRANSFER_GAS;
    let mut small = Net::new(MemKv::new(), &genesis_with(&[&a, &b], RICH, cfg));
    let two = vec![tx.clone(), Wallet::new(2).transfer(a.addr(), 1)];
    let blk = small.block_for(two);
    assert_eq!(
        small.ex.execute_block(&mut small.state, &blk).unwrap_err(),
        BlockError::TooManyTxs
    );
    cfg.max_block_txs = 10;
    let mut small = Net::new(MemKv::new(), &genesis_with(&[&a, &b], RICH, cfg));
    let two = vec![tx.clone(), Wallet::new(2).transfer(a.addr(), 1)];
    let blk = small.block_for(two);
    assert_eq!(
        small.ex.execute_block(&mut small.state, &blk).unwrap_err(),
        BlockError::BlockGasLimit
    );

    for (blk, err) in cases {
        assert_eq!(n.ex.execute_block(&mut n.state, &blk).unwrap_err(), err);
        assert_eq!(n.state.root().unwrap(), root);
        assert_eq!(n.state.head().unwrap(), head);
        assert_eq!(n.bal(&b.addr()), RICH);
    }
    // The good block still applies afterwards.
    n.ex.execute_block(&mut n.state, &good).unwrap();
    assert_eq!(n.state.head().unwrap().height, 1);
}

#[test]
fn tx_and_block_wire_roundtrip_and_rejects_garbage() {
    let mut a = Wallet::new(1);
    let mut m = manifest("c");
    m.crons = vec![("tick".into(), 4)];
    m.indexes = vec!["T:i:u64@8".into()];
    let txs = vec![
        a.deploy(b"code", m),
        a.call([3; 32], "f", &(1u64, "x".to_string())),
        a.transfer([4; 32], 9),
    ];
    for t in &txs {
        let wire = t.encode();
        assert_eq!(&Transaction::decode(&wire).unwrap(), t);
        assert!(t.verify_signature());
        let mut extra = wire.clone();
        extra.push(0);
        assert!(Transaction::decode(&extra).is_err());
        assert!(Transaction::decode(&wire[..wire.len() - 1]).is_err());
    }
    let mut bad_tag = txs[2].encode();
    bad_tag[16] = 100;
    assert!(matches!(
        Transaction::decode(&bad_tag),
        Err(TxError::BadTag(100))
    ));
    let blk = Block::new(3, [1; 32], 5, [2; 32], txs);
    assert_eq!(Block::decode(&blk.encode()).unwrap(), blk);
    assert_ne!(
        blk.hash(),
        Block::new(4, [1; 32], 5, [2; 32], vec![]).hash()
    );
}

#[test]
fn addresses_are_deterministic_and_distinct() {
    let (a, b) = (Wallet::new(1), Wallet::new(2));
    assert_eq!(
        contract_address(&a.addr(), 0),
        contract_address(&a.addr(), 0)
    );
    assert_ne!(
        contract_address(&a.addr(), 0),
        contract_address(&a.addr(), 1)
    );
    assert_ne!(
        contract_address(&a.addr(), 0),
        contract_address(&b.addr(), 0)
    );
    assert_ne!(a.addr(), b.addr());
    assert_ne!(validator_pool(), relay_pool());
}

/// Tiny deterministic generator (no external randomness anywhere).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

/// Random mixes of valid, invalid, failing and gas-starved transactions: balances + deposits
/// (+ pools) always equal the fixed supply, and the executor's own check never trips.
#[test]
fn supply_invariant_over_random_sequences() {
    let mut w: Vec<Wallet> = (1..=5).map(Wallet::new).collect();
    let refs: Vec<&Wallet> = w.iter().collect();
    let mut n = net(&refs, RICH);
    let mut m = manifest("counter");
    m.crons = vec![("tick".into(), 2)];
    let c = deployed(&n.run(vec![w[0].deploy(elf("counter"), m)])[0]);
    n.run(vec![w[0].transfer(c, 50_000_000)]);
    let mut rng = Lcg(42);
    let (mut good, mut reverted, mut invalid) = (0, 0, 0);
    for _ in 0..40 {
        let mut txs = Vec::new();
        for _ in 0..(rng.next() % 6) {
            let i = (rng.next() % 5) as usize;
            let j = (rng.next() % 5) as usize;
            let to = if rng.next().is_multiple_of(7) {
                c
            } else {
                w[j].addr()
            };
            let tx = match rng.next() % 8 {
                0 | 1 => w[i].transfer(to, rng.next() % 1_000_000_000),
                2 => w[i].transfer(to, u64::MAX / 2), // cannot afford: invalid
                3 => w[i].call(c, "increment", &(rng.next() % 100,)),
                4 => w[i].call(c, "increment", &(u64::MAX,)), // aborts
                5 => w[i].call_with(c, FunctionKind::Mutation, "increment", &(1u64,), 2_100),
                6 => {
                    let mut t = w[i].transfer(to, 1);
                    t.signature[3] ^= 1; // forged; the nonce is not consumed
                    t
                }
                _ => w[i].deploy(elf("counter"), manifest("again")),
            };
            txs.push(tx);
        }
        let r = n.run(txs);
        // Invalid transactions did not consume a nonce: resync wallets from state.
        for wallet in w.iter_mut() {
            wallet.nonce = n.state.nonce(&wallet.addr()).unwrap();
        }
        for x in &r {
            match x.status {
                Status::Success => good += 1,
                Status::Reverted(_) => reverted += 1,
                Status::Invalid(_) => invalid += 1,
                Status::Paused => {}
            }
        }
        let known: u128 = w.iter().map(|x| u128::from(n.bal(&x.addr()))).sum::<u128>()
            + u128::from(n.bal(&treasury()))
            + u128::from(n.bal(&c))
            + u128::from(n.state.deposit(&c).unwrap())
            + u128::from(n.state.validator_pool_balance().unwrap())
            + u128::from(n.state.relay_pool_balance().unwrap());
        // Deployed copies are unfunded and own no deposits, so the named accounts hold it all.
        assert_eq!(known, u128::from(TOTAL_SUPPLY));
        assert_eq!(n.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));
    }
    assert!(
        good > 20 && reverted > 5 && invalid > 5,
        "{good} {reverted} {invalid}"
    );
}

#[test]
fn same_blocks_on_two_fresh_states_give_identical_roots_and_receipts() {
    fn session() -> (Vec<Vec<Receipt>>, Vec<[u8; 32]>, MemKv) {
        let (mut a, mut b) = (Wallet::new(1), Wallet::new(2));
        let mut n = net(&[&a, &b], RICH);
        let mut m = manifest("counter");
        m.crons = vec![("tick".into(), 2)];
        let mut all = Vec::new();
        let mut roots = Vec::new();
        let mut step = |n: &mut MemNet, txs: Vec<Transaction>| {
            all.push(n.run(txs));
            roots.push(n.state.root().unwrap());
        };
        step(&mut n, vec![a.deploy(elf("counter"), m)]);
        let c = contract_address(&a.addr(), 0);
        step(
            &mut n,
            vec![a.transfer(c, 100_000_000), b.transfer(a.addr(), 77)],
        );
        step(
            &mut n,
            vec![
                a.call(c, "increment", &(3u64,)),
                b.call(c, "increment", &(4u64,)),
            ],
        );
        step(
            &mut n,
            vec![a.call_with(c, FunctionKind::Mutation, "increment", &(1u64,), 2_100)],
        );
        for _ in 0..6 {
            step(&mut n, vec![]);
        }
        (all, roots, n.state.into_store())
    }
    let (r1, root1, kv1) = session();
    let (r2, root2, kv2) = session();
    assert_eq!(r1, r2);
    assert_eq!(root1, root2);
    assert_eq!(
        kv1.entries().collect::<Vec<_>>(),
        kv2.entries().collect::<Vec<_>>()
    );
    // Roots actually move.
    assert!(root1.windows(2).all(|p| p[0] != p[1]));
}
