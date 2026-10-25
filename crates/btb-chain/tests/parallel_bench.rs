//! Throughput of sequential vs parallel block execution.
//!
//! Run in release mode:
//! `cargo test --release -p btb-chain --test parallel_bench -- --ignored --nocapture`

mod support;
use btb_chain::*;
use btb_runtime::MemKv;
use btb_vm::FunctionKind;
use std::time::Instant;
use support::*;

const RICH: u64 = 10 * UNITS_PER_BTB;
fn txs_per_block() -> usize {
    std::env::var("BTB_BENCH_TXS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4_000)
}
const BLOCKS: usize = 3;

type Workload = fn(&mut [Wallet], &Address) -> Vec<Transaction>;

/// Disjoint sender/recipient pairs: nothing conflicts (fast path eligible).
fn disjoint_transfers(w: &mut [Wallet], _c: &Address) -> Vec<Transaction> {
    let n = w.len() / 2;
    (0..txs_per_block())
        .map(|k| {
            let i = k % n;
            let to = w[n + i].addr();
            w[i].transfer(to, 1_000)
        })
        .collect()
}

/// One transfer per sender to its own recipient plus a counter call: low conflict through the
/// multi-version engine only (the counter contract is hot, transfers are independent).
fn mixed_low(w: &mut [Wallet], c: &Address) -> Vec<Transaction> {
    let n = w.len() / 2;
    (0..txs_per_block())
        .map(|k| {
            let i = k % n;
            if k % 8 == 0 {
                w[i].call_with(*c, FunctionKind::Mutation, "increment", &(1u64,), 100_000)
            } else {
                let to = w[n + i].addr();
                w[i].transfer(to, 1_000)
            }
        })
        .collect()
}

/// Everyone pays one account.
fn hot_transfers(w: &mut [Wallet], _c: &Address) -> Vec<Transaction> {
    let hot = w[0].addr();
    (0..txs_per_block())
        .map(|k| {
            let i = 1 + k % (w.len() - 1);
            w[i].transfer(hot, 1_000)
        })
        .collect()
}

/// Everyone increments the same counter.
fn hot_counter(w: &mut [Wallet], c: &Address) -> Vec<Transaction> {
    (0..txs_per_block())
        .map(|k| {
            let i = k % w.len();
            w[i].call_with(*c, FunctionKind::Mutation, "increment", &(1u64,), 100_000)
        })
        .collect()
}

fn run(ex: Executor, f: Workload) -> (f64, Option<ParStats>) {
    let mut w: Vec<Wallet> = (0..=255u8).map(Wallet::new).collect();
    let refs: Vec<&Wallet> = w.iter().collect();
    let mut net = Net::new(MemKv::new(), &genesis(&refs, RICH));
    net.ex = ex;
    net.ex.check_supply = false;
    let c = deployed(&net.run(vec![w[0].deploy(elf("counter"), manifest("counter"))])[0]);
    net.run(vec![w[0].transfer(c, 5_000_000_000)]);
    for x in w.iter_mut() {
        x.nonce = net.state.nonce(&x.addr()).unwrap();
    }
    let blocks: Vec<Vec<Transaction>> = (0..BLOCKS).map(|_| f(&mut w, &c)).collect();
    let mut secs = 0.0;
    let mut stats = None;
    for txs in blocks {
        let b = net.block_for(txs);
        let t = Instant::now();
        let (_, _, s) = net.ex.execute_block_stats(&mut net.state, &b).unwrap();
        secs += t.elapsed().as_secs_f64();
        stats = s;
    }
    ((txs_per_block() * BLOCKS) as f64 / secs, stats)
}

#[test]
#[ignore = "benchmark: run with --release -- --ignored --nocapture"]
fn throughput_sequential_vs_parallel() {
    println!(
        "\n{} CPUs, {} txs x {BLOCKS} blocks per cell, tx/s (speedup vs sequential)",
        std::thread::available_parallelism().map_or(1, |n| n.get()),
        txs_per_block()
    );
    let loads: [(&str, Workload); 4] = [
        ("disjoint transfers (low conflict)", disjoint_transfers),
        ("transfers + counter calls (low conflict)", mixed_low),
        ("all pay one account (high conflict)", hot_transfers),
        ("all call one counter (high conflict)", hot_counter),
    ];
    for (name, f) in loads {
        let (seq, _) = run(Executor::sequential(), f);
        println!("{name}\n  sequential        {seq:>9.0}");
        for t in [1, 2, 4] {
            let (p, s) = run(Executor::parallel(t), f);
            let s = s.unwrap_or_default();
            println!(
                "  parallel x{t}       {p:>9.0}  ({:.2}x)  fast {} stm {} executions {} waves {}",
                p / seq,
                s.fast_txs,
                s.stm_txs,
                s.executions,
                s.waves
            );
        }
        let mut nf = Executor::parallel(4);
        nf.fast_path = false;
        let (p, s) = run(nf, f);
        let s = s.unwrap_or_default();
        println!(
            "  parallel x4 nofast {p:>8.0}  ({:.2}x)  executions {} waves {}",
            p / seq,
            s.executions,
            s.waves
        );
    }
}
