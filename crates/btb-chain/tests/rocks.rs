//! The same blocks on RocksDB and in memory give identical receipts and roots, and RocksDB
//! state survives a reopen.
#![cfg(feature = "rocksdb")]

mod support;
use btb_chain::*;
use btb_runtime::rocks::RocksKv;
use btb_runtime::MemKv;
use support::*;

struct TempDir(std::path::PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn rocks_matches_memory_and_reopens() {
    let dir = TempDir(std::env::temp_dir().join(format!("btb-chain-rocks-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    let (mut a, mut b) = (Wallet::new(1), Wallet::new(2));
    let g = genesis(&[&a, &b], 10_000_000_000);
    let mut mem = Net::new(MemKv::new(), &g);
    let mut rocks = Net::new(RocksKv::open(&dir.0).unwrap(), &g);
    let mut m = manifest("counter");
    m.crons = vec![("tick".into(), 2)];
    let c = contract_address(&a.addr(), 0);
    let blocks = vec![
        vec![a.deploy(elf("counter"), m)],
        vec![a.transfer(c, 100_000_000), b.transfer(a.addr(), 7)],
        vec![a.call(c, "increment", &(9u64,))],
        vec![],
        vec![],
    ];
    for txs in blocks {
        let r1 = mem.run(txs.clone());
        let r2 = rocks.run(txs);
        assert_eq!(r1, r2);
        assert_eq!(mem.state.root().unwrap(), rocks.state.root().unwrap());
    }
    let root = rocks.state.root().unwrap();
    drop(rocks);
    let mut reopened = Net {
        state: ChainState::open(RocksKv::open(&dir.0).unwrap()).unwrap(),
        ex: Executor::new(),
    };
    assert_eq!(reopened.state.root().unwrap(), root);
    assert_eq!(reopened.state.head().unwrap().height, 5);
    // Both continue identically after the reopen.
    let tx = a.call(c, "increment", &(1u64,));
    assert_eq!(mem.run(vec![tx.clone()]), reopened.run(vec![tx]));
    assert_eq!(mem.state.root().unwrap(), reopened.state.root().unwrap());
}
