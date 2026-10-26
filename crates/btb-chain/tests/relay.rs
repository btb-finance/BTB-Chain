//! The relay fee share: a signed `relayer` earns 20% of a user transaction's fee.

mod support;
use btb_chain::*;
use btb_runtime::MemKv;
use support::*;

const RICH: u64 = 20_000 * UNITS_PER_BTB;
const BOND: u64 = 10_000 * UNITS_PER_BTB;

fn net(wallets: &[&Wallet]) -> Net<MemKv> {
    Net::new(MemKv::new(), &genesis(wallets, RICH))
}

fn register(h: &mut Wallet) -> Transaction {
    h.sign(TxKind::Host(HostOp::Register {
        url: b"http://host.example:8080".to_vec(),
    }))
}

fn pay(to: Address) -> TxKind {
    TxKind::Transfer { to, amount: 1_000 }
}

#[test]
fn registered_relayer_gets_exactly_twenty_percent() {
    let (mut user, mut host, other) = (Wallet::new(1), Wallet::new(2), Wallet::new(3));
    let mut n = net(&[&user, &host, &other]);
    let r = n.run(vec![register(&mut host)]);
    ok(&r[0]);
    let host_before = n.bal(&host.addr());
    let pool_before = n.state.validator_pool_balance().unwrap();
    let r = n.run(vec![user.sign_relayed(pay(other.addr()), Some(host.addr()))]);
    ok(&r[0]);
    let fee = r[0].fee;
    assert_eq!(fee, TRANSFER_GAS);
    assert_eq!(r[0].relayer, Some(host.addr()));
    let share = fee * RELAY_SHARE_BPS / 10_000;
    assert_eq!(share * 5, fee, "20%");
    assert_eq!(n.bal(&host.addr()), host_before + share);
    assert_eq!(
        n.state.validator_pool_balance().unwrap(),
        pool_before + fee - share
    );
    assert_eq!(n.state.relay_pool_balance().unwrap(), 0);
    assert_eq!(n.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));
}

#[test]
fn without_a_usable_relayer_the_validator_pool_gets_it_all() {
    let (mut user, mut host, other) = (Wallet::new(1), Wallet::new(2), Wallet::new(3));
    let mut n = net(&[&user, &host, &other]);
    // none, an unregistered host, the sender itself, and a host that unregistered
    ok(&n.run(vec![register(&mut host)])[0]);
    ok(&n.run(vec![host.sign(TxKind::Host(HostOp::Unregister))])[0]);
    let before = n.state.validator_pool_balance().unwrap();
    let host_bal = n.bal(&host.addr());
    let me = user.addr();
    let txs = vec![
        user.sign_relayed(pay(other.addr()), None),
        user.sign_relayed(pay(other.addr()), Some(host.addr())),
        user.sign_relayed(pay(other.addr()), Some(me)),
        user.sign_relayed(pay(other.addr()), Some([0x77; 32])),
    ];
    let r = n.run(txs);
    for x in &r {
        ok(x);
        assert_eq!(x.relayer, None);
    }
    let fees: u64 = r.iter().map(|x| x.fee).sum();
    assert_eq!(n.state.validator_pool_balance().unwrap(), before + fees);
    assert_eq!(n.bal(&host.addr()), host_bal);
    assert_eq!(n.bal(&[0x77; 32]), 0);
    assert_eq!(n.state.relay_pool_balance().unwrap(), 0);
    assert_eq!(n.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));
}

#[test]
fn bond_is_required_locked_and_counted() {
    let (mut poor, mut host, other) = (Wallet::new(1), Wallet::new(2), Wallet::new(3));
    let mut cfg = ChainConfig::new(CHAIN_ID);
    cfg.host_unbonding_blocks = 5;
    let mut g = genesis_with(&[&host, &other], RICH, cfg);
    let t = g.allocations.len() - 1;
    g.allocations[t].1 -= 5 * UNITS_PER_BTB;
    g.allocations.push((poor.addr(), 5 * UNITS_PER_BTB));
    let mut n = Net::new(MemKv::new(), &g);
    // too poor for the bond: refused, not registered, nothing locked
    let r = n.run(vec![register(&mut poor)]);
    assert_eq!(
        r[0].status,
        Status::Reverted(Revert::Host(HostError::InsufficientBond))
    );
    assert!(!is_registered(&n, &poor));
    // registering locks exactly the bond (plus the gas fee)
    let before = n.bal(&host.addr());
    let r = n.run(vec![register(&mut host)]);
    ok(&r[0]);
    assert_eq!(n.bal(&host.addr()), before - BOND - r[0].fee);
    assert_eq!(n.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));
    // re-registering only changes the URL: no second bond
    let before = n.bal(&host.addr());
    let r = n.run(vec![register(&mut host)]);
    ok(&r[0]);
    assert_eq!(n.bal(&host.addr()), before - r[0].fee);
    // unregistering stops the relay share at once; the bond stays locked until it matures
    let r = n.run(vec![host.sign(TxKind::Host(HostOp::Unregister))]);
    ok(&r[0]);
    let locked = n.bal(&host.addr());
    assert!(!is_registered(&n, &host));
    assert_eq!(n.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));
    let unbonding = n.state.host_unbonding(&host.addr()).unwrap();
    assert_eq!(unbonding.len(), 1);
    assert_eq!(unbonding[0].1, BOND);
    let mature = unbonding[0].0;
    let head = n.state.head().unwrap().height;
    assert_eq!(mature, head + 5);
    n.empty_blocks(mature - head - 1);
    assert_eq!(n.bal(&host.addr()), locked, "still locked before maturity");
    assert_eq!(n.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));
    n.run(vec![]);
    assert_eq!(n.bal(&host.addr()), locked + BOND, "released at maturity");
    assert!(n.state.host_unbonding(&host.addr()).unwrap().is_empty());
    assert_eq!(n.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));
}

fn is_registered(n: &Net<MemKv>, w: &Wallet) -> bool {
    n.state.host_url(&w.addr()).unwrap().is_some()
}

#[test]
fn bonded_self_rebate_is_a_priced_discount() {
    // A sender that bonds a second account and names it earns 20% back: documented, bounded
    // by the locked bond, and supply neutral.
    let (mut user, mut alt, other) = (Wallet::new(1), Wallet::new(2), Wallet::new(3));
    let mut n = net(&[&user, &alt, &other]);
    ok(&n.run(vec![register(&mut alt)])[0]);
    let before = n.bal(&alt.addr());
    let r = n.run(vec![user.sign_relayed(pay(other.addr()), Some(alt.addr()))]);
    ok(&r[0]);
    assert_eq!(n.bal(&alt.addr()), before + r[0].fee / 5);
    assert_eq!(n.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));
}

#[test]
fn registry_rules() {
    let mut h = Wallet::new(2);
    let mut n = net(&[&h]);
    let r = n.run(vec![h.sign(TxKind::Host(HostOp::Unregister))]);
    assert_eq!(
        r[0].status,
        Status::Reverted(Revert::Host(HostError::NotRegistered))
    );
    // an over-long URL never enters a block
    let long = h.sign(TxKind::Host(HostOp::Register {
        url: vec![b'x'; 300],
    }));
    assert_eq!(
        n.run(vec![long])[0].status,
        Status::Invalid(Invalid::TooLarge)
    );
}

#[test]
fn the_relayer_is_covered_by_the_signature() {
    let (mut user, host, thief, other) = (
        Wallet::new(1),
        Wallet::new(2),
        Wallet::new(4),
        Wallet::new(3),
    );
    let mut n = net(&[&user, &host, &thief, &other]);
    let tx = user.sign_relayed(pay(other.addr()), Some(host.addr()));
    assert!(tx.verify_signature());
    // swap the relayer
    let mut swapped = tx.clone();
    swapped.body.relayer = Some(thief.addr());
    assert!(!swapped.verify_signature());
    // drop it
    let mut dropped = tx.clone();
    dropped.body.relayer = None;
    assert!(!dropped.verify_signature());
    // add one to a transaction that had none
    let mut plain = Wallet::new(1).sign(pay(other.addr()));
    assert!(plain.verify_signature());
    plain.body.relayer = Some(thief.addr());
    assert!(!plain.verify_signature());
    // flip a byte of the relayer in the wire form
    let mut wire = tx.encode();
    wire[17] ^= 1;
    let bytes_tampered = Transaction::decode(&wire).unwrap();
    assert!(!bytes_tampered.verify_signature());
    // the executor refuses them all and charges nothing
    let before = n.bal(&user.addr());
    for bad in [swapped, dropped, bytes_tampered] {
        let r = n.run(vec![bad]);
        assert_eq!(r[0].status, Status::Invalid(Invalid::BadSignature));
    }
    assert_eq!(n.bal(&user.addr()), before);
    assert_eq!(n.bal(&thief.addr()), RICH);
}

#[test]
fn old_format_transactions_still_decode_verify_and_run() {
    let (mut user, other) = (Wallet::new(1), Wallet::new(3));
    let mut n = net(&[&user, &other]);
    let tx = user.transfer(other.addr(), 1_000);
    // The v1 wire form, written by hand: chain_id || nonce || tag 2 || to || amount || key || sig.
    let mut legacy = Vec::new();
    legacy.extend_from_slice(&CHAIN_ID.to_le_bytes());
    legacy.extend_from_slice(&0u64.to_le_bytes());
    legacy.push(2);
    legacy.extend_from_slice(&other.addr());
    legacy.extend_from_slice(&1_000u64.to_le_bytes());
    legacy.extend_from_slice(&tx.public_key);
    legacy.extend_from_slice(&tx.signature);
    assert_eq!(tx.encode(), legacy, "no relayer: byte-identical to v1");
    let back = Transaction::decode(&legacy).unwrap();
    assert_eq!(back.body.relayer, None);
    assert!(back.verify_signature());
    assert_eq!(back.id(), tx.id());
    let r = n.run(vec![back]);
    ok(&r[0]);
    assert_eq!(r[0].relayer, None);
    // and a relayed one is the v2 form: high tag bit, 32 bytes after the tag
    let relayed = user.sign_relayed(pay(other.addr()), Some([5; 32]));
    let w = relayed.encode();
    assert_eq!(w.len(), legacy.len() + 32);
    assert_eq!(w[16], 2 | 0x80);
    assert_eq!(&w[17..49], &[5u8; 32]);
    assert_eq!(Transaction::decode(&w).unwrap(), relayed);
}

#[test]
fn parallel_pays_the_same_relayers_as_sequential() {
    let mut users: Vec<Wallet> = (1..=8).map(Wallet::new).collect();
    let mut hosts: Vec<Wallet> = (20..=22).map(Wallet::new).collect();
    let refs: Vec<&Wallet> = users.iter().chain(hosts.iter()).collect();
    let g = genesis(&refs, RICH);
    let mut seq = Net::new(MemKv::new(), &g);
    seq.ex = Executor::sequential();
    let mut par = Net::new(MemKv::new(), &g);
    par.ex = Executor::parallel(4);
    par.ex.parallel_min_txs = 2;
    let regs: Vec<_> = hosts.iter_mut().map(register).collect();
    let (a, b) = (seq.run(regs.clone()), par.run(regs));
    assert_eq!(a, b);
    let start: Vec<u64> = hosts.iter().map(|h| par.bal(&h.addr())).collect();
    let before = |h: &Wallet| start[hosts.iter().position(|x| x.addr() == h.addr()).unwrap()];
    let mut txs = Vec::new();
    for round in 0..3 {
        for (i, u) in users.iter_mut().enumerate() {
            let h = hosts[(i + round) % 3].addr();
            txs.push(u.sign_relayed(pay([100 + i as u8; 32]), Some(h)));
        }
    }
    let (a, b) = (seq.run(txs.clone()), par.run(txs));
    assert_eq!(a, b);
    assert_eq!(seq.state.root().unwrap(), par.state.root().unwrap());
    let total: u64 = a.iter().map(|r| r.fee).sum();
    let paid: u64 = hosts.iter().map(|h| par.bal(&h.addr()) - before(h)).sum();
    assert_eq!(paid, total / 5, "20% of every fee went to a relayer");
    assert_eq!(par.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));
}

fn h(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `sdk-ts/test/fixtures/tx.json`: the TypeScript signer must produce these exact bytes.
/// Regenerate with `BTB_WRITE_FIXTURES=1 cargo test -p btb-chain --test relay`.
#[test]
fn ts_fixture_matches() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let sign = |relayer: Option<Address>, kind: TxKind| {
        Transaction::sign(
            TxBody {
                chain_id: 7,
                nonce: 3,
                relayer,
                session: None,
                kind,
            },
            &key,
        )
    };
    let call = TxKind::Call {
        contract: [0xC0; 32],
        kind: btb_vm::FunctionKind::Mutation,
        function: "increment".into(),
        args: vec![1, 2, 3],
        gas_limit: 500_000,
        max_fee: u64::MAX,
    };
    let cases = [
        ("transfer", None, pay([9; 32])),
        ("transfer_relayed", Some([5; 32]), pay([9; 32])),
        ("call_relayed", Some([5; 32]), call),
    ];
    let mut out = format!(
        "{{\n  \"seed\": \"{}\",\n  \"public_key\": \"{}\",\n  \"txs\": [\n",
        h(&[7; 32]),
        h(&key.verifying_key().to_bytes())
    );
    let rows: Vec<String> = cases
        .into_iter()
        .map(|(name, relayer, kind)| {
            let tx = sign(relayer, kind);
            assert!(tx.verify_signature());
            let call = matches!(tx.body.kind, TxKind::Call { .. });
            format!(
                "    {{\"name\": \"{name}\", \"relayer\": {}, \"call\": {call}, \"wire\": \"{}\", \"id\": \"{}\"}}",
                relayer.map_or("null".into(), |r| format!("\"{}\"", h(&r))),
                h(&tx.encode()),
                h(&tx.id())
            )
        })
        .collect();
    out.push_str(&rows.join(",\n"));
    out.push_str("\n  ]\n}\n");
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../sdk-ts/test/fixtures/tx.json"
    );
    if std::env::var_os("BTB_WRITE_FIXTURES").is_some() {
        std::fs::write(path, &out).unwrap();
    }
    assert_eq!(std::fs::read_to_string(path).unwrap(), out, "stale fixture");
}
