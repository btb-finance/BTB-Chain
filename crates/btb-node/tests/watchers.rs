//! The chain watchers against local mock servers (no external network): deposits after N
//! confirmations, reorgs, malformed answers, hostile endpoints, and the withdrawal builders
//! checked against the chain's own independent implementation.

mod watch_common;

use bitcoin::consensus::deserialize;
use bitcoin::Transaction;
use btb_chain::vault::{Execution, Observer, VaultUtxo, Withdrawal};
use btb_chain::vault_plan::{check_btc, check_sol, BtcTx};
use btb_node::watch::bitcoin::{assemble, build};
use btb_node::watch::{Backend, BitcoinWatcher, BtcConfig, SolConfig, SolanaWatcher};
use btb_oracle::frost::{addr, Scheme};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use watch_common::*;

const VAULT: [u8; 32] = [0x42; 32];
const ME: [u8; 32] = [0x11; 32];

fn btc_cfg(url: &str, rpc: bool) -> BtcConfig {
    let b = if rpc {
        Backend::Rpc {
            url: url.into(),
            auth: None,
        }
    } else {
        Backend::Esplora { url: url.into() }
    };
    let mut c = BtcConfig::new(b);
    c.hrp = "bcrt".into();
    c.confirmations = 3;
    c.scan_blocks = 20;
    c
}

fn vault_script() -> Vec<u8> {
    addr::taproot_script_pubkey(&VAULT)
}

fn vault_address() -> String {
    addr::taproot_address("bcrt", &VAULT)
}

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

fn new_chain(blocks: usize) -> Arc<Mutex<BtcChain>> {
    let c = Arc::new(Mutex::new(BtcChain::default()));
    c.lock().unwrap().mine(blocks, 1);
    c
}

#[test]
fn esplora_deposit_counts_only_at_depth_and_reorgs_drop_it() {
    let chain = new_chain(10);
    let srv = esplora(chain.clone(), vault_address());
    let w = BitcoinWatcher::new(btc_cfg(&srv.url, false)).unwrap();
    let tx = deposit_tx(&vault_script(), 50_000, Some(&ME), 1);
    let id = chain.lock().unwrap().confirm(&tx, 9); // tip 9: depth 1
    w.set_vault(VAULT);
    w.poll();
    assert!(w.deposits().is_empty(), "depth 1 of 3 must not count");
    chain.lock().unwrap().mine(1, 1); // depth 2
    w.poll();
    assert!(w.deposits().is_empty());
    chain.lock().unwrap().mine(1, 1); // depth 3
    w.poll();
    let d = w.deposits();
    assert_eq!(d.len(), 1, "{:?}", w.last_error());
    let c = &d[0];
    assert_eq!(c.asset, Scheme::Taproot);
    assert_eq!(
        (c.amount, c.index, c.height, c.recipient),
        (50_000, 0, 9, ME)
    );
    let mut internal = unhex(&id);
    internal.reverse();
    assert_eq!(c.txid.to_vec(), internal);
    assert_eq!(hex(&c.block_hash), chain.lock().unwrap().hashes[9]);
    // a reorg that removes the block: the next poll offers nothing
    chain.lock().unwrap().reorg(8, 2);
    chain.lock().unwrap().mine(3, 2);
    w.poll();
    assert!(
        w.deposits().is_empty(),
        "a vanished deposit is never claimed"
    );
    // the transaction re-appears in another block: a different claim (other block hash)
    chain.lock().unwrap().confirm(&tx, 10);
    chain.lock().unwrap().mine(2, 2);
    w.poll();
    let d2 = w.deposits();
    assert_eq!(d2.len(), 1);
    assert_eq!(d2[0].txid, c.txid);
    assert_ne!(d2[0].block_hash, c.block_hash);
    assert_ne!(d2[0].id(), c.id());
    assert_eq!(d2[0].outpoint_id(), c.outpoint_id()); // but the same output: credited once
}

#[test]
fn esplora_ignores_unconfirmed_memoless_ambiguous_and_wrong_script() {
    let chain = new_chain(10);
    let srv = esplora(chain.clone(), vault_address());
    let w = BitcoinWatcher::new(btc_cfg(&srv.url, false)).unwrap();
    {
        let mut c = chain.lock().unwrap();
        c.mempool(&deposit_tx(&vault_script(), 1_000, Some(&ME), 1)); // unconfirmed
        c.confirm(&deposit_tx(&vault_script(), 2_000, None, 2), 2); // no memo
        let mut two = deposit_tx(&vault_script(), 3_000, Some(&ME), 3);
        two.output.push(two.output[1].clone()); // two OP_RETURN outputs
        c.confirm(&two, 2);
        c.confirm(&deposit_tx(&[0x51, 0x20, 9, 9, 9], 4_000, Some(&ME), 4), 2); // not the vault
        c.confirm(&deposit_tx(&vault_script(), 5_000, Some(&ME), 5), 2); // the only good one
    }
    w.set_vault(VAULT);
    w.poll();
    let d = w.deposits();
    assert_eq!(d.len(), 1, "{:?}", w.last_error());
    assert_eq!(d[0].amount, 5_000);
}

#[test]
fn esplora_malformed_json_and_lying_raw_transactions_produce_no_claims() {
    let chain = new_chain(10);
    let srv = esplora(chain.clone(), vault_address());
    let w = BitcoinWatcher::new(btc_cfg(&srv.url, false)).unwrap();
    let tx = deposit_tx(&vault_script(), 50_000, Some(&ME), 1);
    let id = chain.lock().unwrap().confirm(&tx, 2);
    w.set_vault(VAULT);
    chain.lock().unwrap().garbage = true;
    w.poll();
    assert!(w.deposits().is_empty());
    assert!(
        w.last_error().unwrap().contains("txs"),
        "{:?}",
        w.last_error()
    );
    chain.lock().unwrap().garbage = false;
    // the endpoint serves other bytes under the txid: the recomputed txid does not match
    let fake = deposit_tx(&vault_script(), 99_999_999, Some(&ME), 77);
    chain
        .lock()
        .unwrap()
        .swap_raw
        .insert(id, bitcoin::consensus::serialize(&fake));
    w.poll();
    assert!(w.deposits().is_empty());
    assert!(
        w.last_error().unwrap().contains("another txid"),
        "{:?}",
        w.last_error()
    );
}

#[test]
fn bitcoind_rpc_backend_scans_blocks_and_rejects_a_block_that_does_not_match_its_hash() {
    let chain = new_chain(12);
    let srv = bitcoind(chain.clone());
    let w = BitcoinWatcher::new(btc_cfg(&srv.url, true)).unwrap();
    let tx = deposit_tx(&vault_script(), 77_000, Some(&ME), 1);
    chain.lock().unwrap().confirm(&tx, 8); // tip 11: depth 4
    w.set_vault(VAULT);
    w.poll();
    let d = w.deposits();
    assert_eq!(d.len(), 1, "{:?}", w.last_error());
    assert_eq!((d[0].amount, d[0].height, d[0].recipient), (77_000, 8, ME));
    // not deep enough on a shorter chain
    let chain2 = new_chain(10);
    let srv2 = bitcoind(chain2.clone());
    let w2 = BitcoinWatcher::new(btc_cfg(&srv2.url, true)).unwrap();
    chain2.lock().unwrap().confirm(&tx, 9);
    w2.set_vault(VAULT);
    w2.poll();
    assert!(w2.deposits().is_empty());
    // a node that serves a different block under the requested hash is rejected
    let chain3 = new_chain(12);
    let srv3 = bitcoind(chain3.clone());
    let w3 = BitcoinWatcher::new(btc_cfg(&srv3.url, true)).unwrap();
    {
        let mut c = chain3.lock().unwrap();
        c.confirm(&tx, 8);
        let liar = mk_block(vec![deposit_tx(
            &vault_script(),
            1_000_000_000,
            Some(&ME),
            9,
        )]);
        c.blocks.insert(8, liar);
    }
    w3.set_vault(VAULT);
    w3.poll();
    assert!(w3.deposits().is_empty());
    assert!(
        w3.last_error().unwrap().contains("does not match"),
        "{:?}",
        w3.last_error()
    );
}

fn withdrawal(id: u64, asset: Scheme, amount: u64, destination: Vec<u8>) -> Withdrawal {
    Withdrawal {
        id,
        asset,
        owner: ME,
        amount,
        destination,
        height: 1,
        status: 0,
        digest: [0; 32],
        requests: vec![],
        plan: vec![],
    }
}

fn dest_script() -> Vec<u8> {
    let mut d = vec![0x51, 0x20];
    d.extend_from_slice(&[0xAB; 32]);
    d
}

#[test]
fn bitcoin_plan_matches_the_chains_independent_checker_and_selection_is_deterministic() {
    let utxos = vec![
        VaultUtxo {
            txid: [3; 32],
            vout: 1,
            amount: 30_000,
        },
        VaultUtxo {
            txid: [1; 32],
            vout: 0,
            amount: 90_000,
        },
        VaultUtxo {
            txid: [2; 32],
            vout: 0,
            amount: 90_000,
        },
    ];
    let w = withdrawal(0, Scheme::Taproot, 100_000, dest_script());
    let p = build(&w, &VAULT, &utxos, 10).expect("builds");
    // the same inputs in another order give the same transaction
    let mut shuffled = utxos.clone();
    shuffled.reverse();
    assert_eq!(build(&w, &VAULT, &shuffled, 10).unwrap(), p);
    // largest first, ties by outpoint
    let tx = BtcTx::parse(&p.plan).unwrap();
    assert_eq!(
        tx.inputs.iter().map(|i| i.0[0]).collect::<Vec<_>>(),
        vec![1, 2]
    );
    // the chain recomputes txid, payment and every sighash on its own
    let script = vault_script();
    let lookup = |t: &[u8; 32], v: u32| {
        utxos
            .iter()
            .find(|u| &u.txid == t && u.vout == v)
            .map(|u| u.amount)
    };
    let (plan, amounts) = check_btc(&p.plan, &w.destination, w.amount, &script, &lookup).unwrap();
    let prevouts: Vec<(u64, Vec<u8>)> = amounts.iter().map(|a| (*a, script.clone())).collect();
    assert_eq!(p.digest, plan.tx.txid());
    for (i, s) in p.sighashes.iter().enumerate() {
        assert_eq!(
            Some(*s),
            plan.tx.taproot_sighash(i, &prevouts),
            "sighash {i}"
        );
    }
    // pays amount - fee to the destination, change to the vault
    let out: Transaction = deserialize(&p.plan).unwrap();
    assert_eq!(
        out.output[0].script_pubkey.as_bytes(),
        dest_script().as_slice()
    );
    assert_eq!(out.output[1].script_pubkey.as_bytes(), script.as_slice());
    assert_eq!(out.output[1].value.to_sat(), 80_000);
    let fee = 100_000 - out.output[0].value.to_sat();
    assert!((1_000..3_000).contains(&fee), "fee {fee}");
    // a tampered plan fails the chain's check
    let mut bad = p.plan.clone();
    let n = bad.len();
    bad[n - 6] ^= 1;
    assert!(check_btc(&bad, &w.destination, w.amount, &script, &lookup).is_err());
    // not enough funds / dust payout
    assert!(build(
        &withdrawal(1, Scheme::Taproot, 500_000, dest_script()),
        &VAULT,
        &utxos,
        10
    )
    .is_none());
    assert!(build(
        &withdrawal(2, Scheme::Taproot, 1_000, dest_script()),
        &VAULT,
        &utxos,
        10
    )
    .is_none());
    // assembling attaches one 64-byte witness item per input
    let sigs = vec![vec![7u8; 64], vec![8u8; 64]];
    let (raw, txid) = assemble(&p.plan, &sigs).unwrap();
    let signed: Transaction = deserialize(&raw).unwrap();
    assert_eq!(signed.compute_txid(), txid);
    assert_eq!(
        signed.compute_txid().to_string(),
        out.compute_txid().to_string()
    ); // txid ignores witnesses
    assert_eq!(signed.input[0].witness.len(), 1);
    assert!(assemble(&p.plan, &sigs[..1]).is_none());
}

#[test]
fn broadcast_is_idempotent_and_tracked_to_confirmation() {
    let chain = new_chain(10);
    let srv = esplora(chain.clone(), vault_address());
    let w = BitcoinWatcher::new(btc_cfg(&srv.url, false)).unwrap();
    let utxos = vec![VaultUtxo {
        txid: [1; 32],
        vout: 0,
        amount: 90_000,
    }];
    let mut wd = withdrawal(5, Scheme::Taproot, 50_000, dest_script());
    let p = w.prepare(&wd, &VAULT, &utxos).unwrap();
    wd.plan = p.plan;
    assert_eq!(w.execution(5), Execution::Unknown);
    w.broadcast(&wd, &[vec![5u8; 64]]);
    w.broadcast(&wd, &[vec![5u8; 64]]);
    w.poll();
    assert_eq!(chain.lock().unwrap().broadcasts.len(), 1);
    assert_eq!(w.execution(5), Execution::Pending);
    let id = w.txid_of(5).unwrap();
    chain.lock().unwrap().conf.insert(id, 10);
    chain.lock().unwrap().mine(1, 1); // tip 10: depth 1
    w.poll();
    assert_eq!(w.execution(5), Execution::Pending);
    chain.lock().unwrap().mine(2, 1); // depth 3
    w.poll();
    assert_eq!(w.execution(5), Execution::Confirmed);
    assert_eq!(chain.lock().unwrap().broadcasts.len(), 1);
}

#[test]
fn fee_estimates_are_rounded_up_to_buckets_and_never_below_the_floor() {
    let chain = new_chain(10);
    let srv = esplora(chain.clone(), vault_address());
    let mut cfg = btc_cfg(&srv.url, false);
    cfg.fee_estimate_blocks = Some(3);
    cfg.fee_rate = 5;
    let w = BitcoinWatcher::new(cfg).unwrap();
    assert_eq!(w.fee_rate(), 5);
    w.poll();
    assert_eq!(w.fee_rate(), 15); // 12.4 -> 13 -> bucket of 5
}

// ------------------------------------------------------------------ Solana

fn sol_cfg(url: &str) -> SolConfig {
    let mut c = SolConfig::new(url.into());
    c.blockhash_window = 16;
    c
}

fn memo() -> String {
    hex(&ME)
}

#[test]
fn solana_deposits_are_finalized_strictly_parsed_and_need_exactly_one_memo() {
    let chain = Arc::new(Mutex::new(SolChain {
        slot: 100,
        ..Default::default()
    }));
    let srv = solana(chain.clone());
    let w = SolanaWatcher::new(sol_cfg(&srv.url)).unwrap();
    {
        let mut c = chain.lock().unwrap();
        c.txs
            .push(sol_deposit(&[1; 64], 90, &VAULT, 2_000_000, Some(&memo())));
        c.txs
            .push(sol_deposit(&[2; 64], 91, &VAULT, 3_000_000, None)); // no memo
        c.txs.push(sol_deposit(
            &[3; 64],
            92,
            &VAULT,
            4_000_000,
            Some("not hex at all"),
        ));
        // a lying balance: the transfer says 5M but the vault grew by less
        let mut liar = sol_deposit(&[4; 64], 93, &VAULT, 5_000_000, Some(&memo()));
        liar.2["meta"]["postBalances"][1] = serde_json::json!(1);
        c.txs.push(liar);
        // a failed transaction
        let mut failed = sol_deposit(&[5; 64], 94, &VAULT, 6_000_000, Some(&memo()));
        failed.2["meta"]["err"] = serde_json::json!({"InstructionError": [0, "Custom"]});
        c.txs.push(failed);
        // an answer for another signature
        let mut other = sol_deposit(&[6; 64], 95, &VAULT, 7_000_000, Some(&memo()));
        other.2["transaction"]["signatures"][0] = serde_json::json!("1111");
        c.txs.push(other);
        // not finalized yet (slot above the finalized one): not listed
        c.txs
            .push(sol_deposit(&[8; 64], 150, &VAULT, 8_000_000, Some(&memo())));
    }
    w.set_vault(VAULT);
    w.poll();
    let d = w.deposits();
    assert_eq!(d.len(), 1, "{:?}", w.last_error());
    let c = &d[0];
    assert_eq!(c.asset, Scheme::Ed25519);
    assert_eq!(
        (c.amount, c.recipient, c.height, c.index),
        (2_000_000, ME, 90, 1)
    );
    assert_eq!(c.txid, [1u8; 32]);
    assert_eq!(c.block_hash, blockhash(90));
    assert!(
        w.last_error().unwrap().contains("grew by less")
            || w.last_error().unwrap().contains("first signature")
    );
}

#[test]
fn solana_malformed_answers_do_not_mint_and_the_anchor_blockhash_is_windowed() {
    let chain = Arc::new(Mutex::new(SolChain {
        slot: 100,
        ..Default::default()
    }));
    let srv = solana(chain.clone());
    let w = SolanaWatcher::new(sol_cfg(&srv.url)).unwrap();
    chain.lock().unwrap().garbage = true;
    w.set_vault(VAULT);
    w.poll();
    assert!(w.deposits().is_empty());
    assert!(w.last_error().is_some());
    chain.lock().unwrap().garbage = false;
    // slot 100 -> anchor 96; if 96 was skipped, the previous block counts
    chain.lock().unwrap().skipped.insert(96);
    w.poll();
    let wd = withdrawal(0, Scheme::Ed25519, 1_000_000, vec![0xCD; 32]);
    let p = w.prepare(&wd, &VAULT).expect("anchor known");
    assert_eq!(&p.plan[100..132], &blockhash(95));
    // another validator whose finalized slot is 103 proposes the same plan
    let chain2 = Arc::new(Mutex::new(SolChain {
        slot: 103,
        skipped: [96].into(),
        ..Default::default()
    }));
    let srv2 = solana(chain2);
    let w2 = SolanaWatcher::new(sol_cfg(&srv2.url)).unwrap();
    w2.poll();
    assert_eq!(w2.prepare(&wd, &VAULT).unwrap(), p);
    // the chain's checker accepts it, and refuses a different amount or destination
    check_sol(&p.plan, &VAULT, &wd.destination, wd.amount).unwrap();
    assert!(check_sol(&p.plan, &VAULT, &wd.destination, wd.amount + 1).is_err());
    assert!(check_sol(&p.plan, &VAULT, &[1u8; 32], wd.amount).is_err());
    // a destination that is not 32 bytes cannot be prepared
    assert!(w
        .prepare(&withdrawal(1, Scheme::Ed25519, 5, vec![1; 31]), &VAULT)
        .is_none());
}

#[test]
fn solana_broadcast_is_sent_once_and_tracked_to_finalized() {
    let chain = Arc::new(Mutex::new(SolChain {
        slot: 100,
        ..Default::default()
    }));
    let srv = solana(chain.clone());
    let w = SolanaWatcher::new(sol_cfg(&srv.url)).unwrap();
    w.poll();
    let mut wd = withdrawal(3, Scheme::Ed25519, 1_000_000, vec![0xCD; 32]);
    wd.plan = w.prepare(&wd, &VAULT).unwrap().plan;
    w.broadcast(&wd, &[vec![9u8; 64]]);
    w.poll();
    w.poll();
    assert_eq!(chain.lock().unwrap().sent.len(), 1);
    let wire = chain.lock().unwrap().sent[0].clone();
    assert_eq!(wire[0], 1);
    assert_eq!(&wire[1..65], &[9u8; 64][..]);
    assert_eq!(&wire[65..], wd.plan.as_slice());
    assert_eq!(w.execution(3), Execution::Pending);
    let sig = w.signature_of(3).unwrap();
    chain.lock().unwrap().status.insert(sig, "finalized".into());
    w.poll();
    assert_eq!(w.execution(3), Execution::Confirmed);
    // the Observer-level default for an unconfigured chain is "nothing to wait for"
    let none = btb_node::watch::Watchers::start(None, None).unwrap();
    assert_eq!(none.execution(&wd), Execution::Confirmed);
    wait("a poll counter", || w.polls() >= 3);
}
