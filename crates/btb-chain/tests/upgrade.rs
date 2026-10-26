//! Contract upgrades and schema migrations (`TxKind::Upgrade`, docs/CHAIN.md "Upgrades").
//! The contract is `examples/evolve`, built in several versions by Cargo feature.

mod support;
use btb_chain::upgrade::{
    BACKFILL_ROW_GAS, INDEX_ENTRY_GAS, MIGRATE_ROW_GAS, ROWS_PER_BLOCK, ROW_BYTE_GAS,
};
use btb_chain::*;
use btb_codec::{Decode, Encode};
use btb_runtime::{KvStore, MemKv};
use support::*;

const RICH: u64 = 10 * UNITS_PER_BTB;

fn evolve(features: &'static str) -> &'static [u8] {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<&'static str, &'static [u8]>>> = OnceLock::new();
    let mut g = CACHE.get_or_init(Default::default).lock().unwrap();
    g.entry(features).or_insert_with(|| {
        Box::leak(common::build_example_with("evolve", features).into_boxed_slice())
    })
}

fn net(wallets: &[&Wallet]) -> Net<MemKv> {
    Net::new(MemKv::new(), &genesis(wallets, RICH))
}

fn upgrade(w: &mut Wallet, c: Address, code: &[u8], budget_gas: u64) -> Transaction {
    w.sign(TxKind::Upgrade {
        contract: c,
        code: code.to_vec(),
        manifest: manifest("evolve"),
        budget_gas,
    })
}

fn texts(n: &mut Net<MemKv>, c: &Address, owner: Address) -> Vec<String> {
    Vec::<String>::decode(&n.query(c, "texts", &(owner,), &owner)).unwrap()
}

/// Deploy v1, fund it, add `rows` todos for `w` (50 per block). Returns the contract.
fn setup(n: &mut Net<MemKv>, w: &mut Wallet, rows: usize, mut m: Manifest) -> Address {
    m.name = "evolve".into();
    let c = deployed(&n.run(vec![w.deploy(evolve(""), m)])[0]);
    ok(&n.run(vec![w.transfer(c, UNITS_PER_BTB)])[0]);
    for chunk in (0..rows).collect::<Vec<_>>().chunks(50) {
        let txs = chunk
            .iter()
            .map(|i| w.call(c, "add", &(format!("todo {i}"),)))
            .collect();
        for r in n.run(txs) {
            ok(&r);
        }
    }
    c
}

fn revert(r: &Receipt) -> &Revert {
    match &r.status {
        Status::Reverted(x) => x,
        s => panic!("expected a revert, got {s:?}"),
    }
}

#[test]
fn compatible_upgrade_keeps_data_and_the_new_index_finds_old_rows() {
    let mut a = Wallet::new(1);
    let mut n = net(&[&a]);
    let c = setup(&mut n, &mut a, 4, manifest("evolve"));
    let me = a.addr();
    let bal = n.bal(&me);
    let r = n.run(vec![upgrade(&mut a, c, evolve("compat"), 1_000_000)]);
    ok(&r[0]);
    // The job ran in the block of the upgrade (4 rows <= ROWS_PER_BLOCK) and is finished.
    let st = n.state.upgrade_status(&c).unwrap().unwrap();
    assert_eq!(st.phase, "done", "{st:?}");
    assert_eq!(st.tasks.len(), 1);
    assert_eq!(
        (st.tasks[0].kind.as_str(), st.tasks[0].done),
        ("backfill", 4)
    );
    // Exact gas: each row costs the backfill base plus one new index entry.
    assert_eq!(st.charged_gas, 4 * (BACKFILL_ROW_GAS + INDEX_ENTRY_GAS));
    // Fee for the transaction + the charged gas; the rest of the prepaid budget came back.
    assert_eq!(n.bal(&me), bal - r[0].fee - st.charged_gas);
    assert_eq!(n.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));

    // Data kept; the new index on `done` finds rows written by the old code.
    assert_eq!(texts(&mut n, &c, me).len(), 4);
    assert_eq!(
        u64::decode(&n.query(&c, "count_done", &(false,), &me)).unwrap(),
        4
    );
    // The appended option reads as None on an old row, and can be set.
    assert_eq!(
        Option::<String>::decode(&n.query(&c, "note_of", &(0u64,), &me)).unwrap(),
        None
    );
    ok(&n.run(vec![
        a.call(c, "set_note", &(0u64, "hi".to_string())),
        a.call(c, "finish", &(1u64,)),
    ])[0]);
    assert_eq!(
        Option::<String>::decode(&n.query(&c, "note_of", &(0u64,), &me)).unwrap(),
        Some("hi".to_string())
    );
    assert_eq!(
        u64::decode(&n.query(&c, "count_done", &(true,), &me)).unwrap(),
        1
    );
    assert_eq!(
        u64::decode(&n.query(&c, "count_done", &(false,), &me)).unwrap(),
        3
    );
}

#[test]
fn incompatible_change_without_a_migration_is_rejected() {
    let mut a = Wallet::new(1);
    let mut n = net(&[&a]);
    let c = setup(&mut n, &mut a, 3, manifest("evolve"));
    let me = a.addr();
    let old_code = n.state.contract(&c).unwrap().unwrap().code;
    let bal = n.bal(&me);
    let r = n.run(vec![upgrade(&mut a, c, evolve("bad"), 1_000_000)]);
    let Revert::Upgrade(UpgradeError::Incompatible(m)) = revert(&r[0]) else {
        panic!("{:?}", r[0]);
    };
    assert!(m.contains("`done`") && m.contains("version"), "{m}");
    // Nothing changed but the nonce and the fee; no budget was taken.
    assert_eq!(n.state.contract(&c).unwrap().unwrap().code, old_code);
    assert_eq!(n.bal(&me), bal - r[0].fee);
    assert!(n.state.upgrade_status(&c).unwrap().is_none());
    assert_eq!(texts(&mut n, &c, me).len(), 3);
    // A migration written against the wrong old row type is refused too.
    let r = n.run(vec![upgrade(&mut a, c, evolve("wrongprev"), 1_000_000)]);
    let Revert::Upgrade(UpgradeError::Incompatible(m)) = revert(&r[0]) else {
        panic!("{:?}", r[0]);
    };
    assert!(m.contains("does not match the stored schema"), "{m}");
}

/// Gas the chain must charge to migrate one v1 row of `text` (owner unchanged, one new entry).
fn migration_gas(text: &str, owner: Address) -> u64 {
    let c = btb_vm::Contract::from_elf(evolve("migrate")).unwrap();
    let old = (owner, false, text.to_string()).encode();
    let out = c
        .call_transform(
            "migrate_todo_1",
            &old,
            ChainConfig::new(CHAIN_ID).cron_gas_limit,
            ChainConfig::new(CHAIN_ID).vm_memory as usize,
        )
        .unwrap();
    let new = (owner, 1u8, text.to_string()).encode();
    assert_eq!(out.output, new);
    MIGRATE_ROW_GAS + out.gas_used + ROW_BYTE_GAS * (old.len() + new.len()) as u64 + INDEX_ENTRY_GAS
}

#[test]
fn migration_transforms_all_rows_over_several_blocks_with_exact_gas() {
    let rows = 3 * ROWS_PER_BLOCK + 5;
    let mut a = Wallet::new(1);
    let mut b = Wallet::new(2);
    let mut n = net(&[&a, &b]);
    let c = setup(&mut n, &mut a, rows, manifest("evolve"));
    let me = a.addr();
    let before = n.bal(&me);
    let pool_before = n.state.validator_pool_balance().unwrap();
    let budget = 10_000_000u64;
    let r = n.run(vec![upgrade(&mut a, c, evolve("migrate"), budget)]);
    ok(&r[0]);
    let upgrade_fee = r[0].fee;
    // The budget is escrowed; the first batch ran at the end of this block.
    let st = n.state.upgrade_status(&c).unwrap().unwrap();
    assert_eq!(st.phase, "migrating");
    assert_eq!(st.tasks[0].kind, "migrate v1->v2");
    assert_eq!(st.tasks[0].done, ROWS_PER_BLOCK as u64);
    assert_eq!(n.bal(&upgrade_pool()), budget - st.charged_gas);

    // Reads during the migration: the table is locked for everybody (queries and mutations
    // fault), never a mix of old and new rows. Other accounts are unaffected.
    assert!(matches!(
        n.ex.query(&mut n.state, &c, "texts", &(me,).encode(), &me),
        Err(Revert::Fault(m)) if m.contains("TableLocked")
    ));
    let r = n.run(vec![b.call(c, "add", &("late".to_string(),))]);
    assert!(
        matches!(revert(&r[0]), Revert::Fault(m) if m.contains("TableLocked")),
        "{:?}",
        r[0]
    );
    // A second upgrade must wait.
    let r = n.run(vec![upgrade(&mut a, c, evolve("compat"), 1)]);
    assert_eq!(revert(&r[0]), &Revert::Upgrade(UpgradeError::InProgress));

    // Survives a restart of the node in the middle.
    let kv = n.state.into_store();
    n.state = ChainState::open(kv).unwrap();
    let mut guard = 0;
    while n.state.upgrade_status(&c).unwrap().unwrap().phase == "migrating" {
        n.run(vec![]);
        guard += 1;
        assert!(guard < 10);
    }
    while n.state.upgrade_status(&c).unwrap().unwrap().phase != "done" {
        n.run(vec![]);
    }
    let st = n.state.upgrade_status(&c).unwrap().unwrap();
    assert_eq!(st.tasks[0].done, rows as u64);

    // Exact gas: the sum of the per-row costs, paid from the escrow to the validator pool.
    let expect: u64 = (0..rows)
        .map(|i| migration_gas(&format!("todo {i}"), me))
        .sum();
    assert_eq!(st.charged_gas, expect);
    assert_eq!(n.bal(&upgrade_pool()), 0);
    assert!(n.state.validator_pool_balance().unwrap() >= pool_before + expect);
    assert!(n.bal(&me) <= before - upgrade_fee - expect);
    assert_eq!(n.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));

    // Every row is in the new layout and reachable through the new index; the table is open.
    let all = Vec::<String>::decode(&n.query(&c, "with_status", &(1u8,), &me)).unwrap();
    assert_eq!(all.len(), rows);
    assert_eq!(texts(&mut n, &c, me).len(), rows);
    assert!(
        Vec::<String>::decode(&n.query(&c, "with_status", &(0u8,), &me))
            .unwrap()
            .is_empty()
    );
    ok(&n.run(vec![b.call(c, "add", &("after".to_string(),))])[0]);
    assert_eq!(
        Vec::<String>::decode(&n.query(&c, "with_status", &(0u8,), &me))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(texts(&mut n, &c, b.addr()), vec!["after".to_string()]);
}

#[test]
fn failing_migration_rolls_back_to_the_old_code_and_rows() {
    let rows = 2 * ROWS_PER_BLOCK + 10;
    let mut a = Wallet::new(1);
    let mut n = net(&[&a]);
    let c = setup(&mut n, &mut a, rows, manifest("evolve"));
    let me = a.addr();
    // One row the migration cannot handle, in the second batch.
    ok(&n.run(vec![a.call(c, "add", &("bad".to_string(),))])[0]);
    let rows = rows + 1;
    let old = n.state.contract(&c).unwrap().unwrap();
    let before = n.bal(&me);
    let r = n.run(vec![upgrade(&mut a, c, evolve("fail"), 10_000_000)]);
    ok(&r[0]);
    let fee = r[0].fee;
    while !matches!(
        n.state.upgrade_status(&c).unwrap().unwrap().phase,
        "rolled_back" | "done"
    ) {
        n.run(vec![]);
    }
    let st = n.state.upgrade_status(&c).unwrap().unwrap();
    assert_eq!(st.phase, "rolled_back");
    assert!(
        st.error
            .as_deref()
            .unwrap()
            .contains("cannot migrate this row"),
        "{st:?}"
    );
    // The old code, manifest and rows are back; the table is open; the rows still index.
    assert_eq!(n.state.contract(&c).unwrap().unwrap(), old);
    assert_eq!(texts(&mut n, &c, me).len(), rows);
    ok(&n.run(vec![a.call(c, "add", &("again".to_string(),))])[0]);
    assert_eq!(
        u64::decode(&n.query(&c, "count", &(me,), &me)).unwrap(),
        rows as u64 + 1
    );
    // The upgrader paid for the work done before the failure and got the rest back.
    assert_eq!(n.bal(&upgrade_pool()), 0);
    assert!(st.charged_gas > 0);
    let spent = before - n.bal(&me);
    assert!(spent >= fee + st.charged_gas);
    assert_eq!(n.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));

    // An exhausted budget fails the same way.
    let r = n.run(vec![upgrade(&mut a, c, evolve("migrate"), 5_000)]);
    ok(&r[0]);
    let st = n.state.upgrade_status(&c).unwrap().unwrap();
    while !matches!(
        n.state.upgrade_status(&c).unwrap().unwrap().phase,
        "rolled_back" | "done"
    ) {
        n.run(vec![]);
    }
    let st2 = n.state.upgrade_status(&c).unwrap().unwrap();
    assert_eq!(st2.phase, "rolled_back", "{st:?} {st2:?}");
    assert!(st2.error.unwrap().contains("budget"));
    assert_eq!(n.state.contract(&c).unwrap().unwrap(), old);
    assert_eq!(texts(&mut n, &c, me).len(), rows + 1);
}

#[test]
fn only_the_upgrade_authority_can_upgrade() {
    let mut a = Wallet::new(1);
    let mut b = Wallet::new(2);
    let mut n = net(&[&a, &b]);
    let c = setup(&mut n, &mut a, 1, manifest("evolve"));
    let r = n.run(vec![upgrade(&mut b, c, evolve("compat"), 1_000_000)]);
    assert_eq!(revert(&r[0]), &Revert::Upgrade(UpgradeError::NotAuthorized));
    ok(&n.run(vec![upgrade(&mut a, c, evolve("compat"), 1_000_000)])[0]);
    let r = n.run(vec![a.sign(TxKind::Upgrade {
        contract: [9; 32],
        code: evolve("compat").to_vec(),
        manifest: manifest("x"),
        budget_gas: 0,
    })]);
    assert_eq!(
        revert(&r[0]),
        &Revert::Upgrade(UpgradeError::NoSuchContract)
    );

    // An explicit authority replaces the deployer.
    let m = manifest("evolve").with_upgrade_policy(UpgradePolicy::Authority(b.addr()));
    let c2 = setup(&mut n, &mut a, 0, m);
    let r = n.run(vec![upgrade(&mut a, c2, evolve("compat"), 1_000_000)]);
    assert_eq!(revert(&r[0]), &Revert::Upgrade(UpgradeError::NotAuthorized));
    ok(&n.run(vec![upgrade(&mut b, c2, evolve("compat"), 1_000_000)])[0]);
}

#[test]
fn an_immutable_contract_cannot_be_upgraded() {
    let mut a = Wallet::new(1);
    let mut n = net(&[&a]);
    let m = manifest("evolve").with_upgrade_policy(UpgradePolicy::Immutable);
    let c = setup(&mut n, &mut a, 2, m);
    let r = n.run(vec![upgrade(&mut a, c, evolve("compat"), 1_000_000)]);
    assert_eq!(revert(&r[0]), &Revert::Upgrade(UpgradeError::Immutable));
    // An upgrade can also give the right up: the new manifest says `none`.
    let mut n = net(&[&a]);
    a.nonce = 0;
    let c = setup(&mut n, &mut a, 1, manifest("evolve"));
    let m = manifest("evolve").with_upgrade_policy(UpgradePolicy::Immutable);
    ok(&n.run(vec![a.sign(TxKind::Upgrade {
        contract: c,
        code: evolve("compat").to_vec(),
        manifest: m,
        budget_gas: 1_000_000,
    })])[0]);
    let r = n.run(vec![upgrade(&mut a, c, evolve(""), 1_000_000)]);
    assert_eq!(revert(&r[0]), &Revert::Upgrade(UpgradeError::Immutable));
}

#[test]
fn parallel_and_sequential_execution_agree_across_upgrades() {
    let script = |ex: Executor| {
        let mut ws: Vec<Wallet> = (1..=4).map(Wallet::new).collect();
        let refs: Vec<&Wallet> = ws.iter().collect();
        let addr: Vec<Address> = ws.iter().map(Wallet::addr).collect();
        let mut n = Net {
            state: ChainState::from_genesis(MemKv::new(), &genesis(&refs, RICH)).unwrap(),
            ex,
        };
        let mut out: Vec<(Vec<Receipt>, Hash)> = Vec::new();
        let mut step = |n: &mut Net<MemKv>, txs: Vec<Transaction>| {
            let b = n.block_for(txs);
            let (r, root) = n.ex.execute_block(&mut n.state, &b).unwrap();
            out.push((r, root));
        };
        step(&mut n, vec![ws[0].deploy(evolve(""), manifest("evolve"))]);
        let c = contract_address(&addr[0], 0);
        let txs = vec![
            ws[0].transfer(c, UNITS_PER_BTB),
            ws[1].transfer(addr[2], 5),
            ws[2].transfer(addr[3], 7),
        ];
        step(&mut n, txs);
        let txs = (0..40)
            .map(|i| ws[i % 3 + 1].call(c, "add", &(format!("t{i}"),)))
            .collect();
        step(&mut n, txs);
        // The upgrade shares a block with calls and transfers.
        let up = upgrade(&mut ws[0], c, evolve("migrate"), 5_000_000);
        let txs = vec![
            ws[1].transfer(addr[3], 1),
            up,
            ws[2].call(c, "add", &("x".to_string(),)),
            ws[3].transfer(addr[1], 2),
        ];
        step(&mut n, txs);
        for _ in 0..4 {
            step(&mut n, vec![]);
        }
        step(&mut n, vec![ws[1].call(c, "add", &("y".to_string(),))]);
        let store = n.state.store().scan(&[0], None, false, usize::MAX).unwrap();
        (out, store)
    };
    let mut par = Executor::parallel(4);
    par.parallel_min_txs = 2;
    let (a, sa) = script(Executor::sequential());
    let (b, sb) = script(par);
    assert_eq!(a, b);
    assert_eq!(sa, sb);
    // The upgrade block actually migrated something and the later add worked.
    assert!(a[3].0.iter().any(|r| r.status == Status::Success));
}
