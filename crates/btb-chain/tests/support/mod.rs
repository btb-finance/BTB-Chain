//! Shared test helpers: wallets, a tiny test network and the example-contract build helper
//! shared with `btb-tests`.
#![allow(dead_code)]

#[path = "../../../btb-tests/tests/common/mod.rs"]
pub mod common;

use btb_chain::*;
use btb_codec::Encode;
use btb_runtime::KvStore;
use btb_vm::FunctionKind;
use std::sync::OnceLock;

pub const CHAIN_ID: u64 = 7;
pub const GAS: u64 = 2_000_000;

type ElfCache = std::sync::Mutex<Vec<(&'static str, &'static [u8])>>;

pub fn elf(name: &'static str) -> &'static [u8] {
    static CACHE: OnceLock<ElfCache> = OnceLock::new();
    let m = CACHE.get_or_init(Default::default);
    let mut g = m.lock().unwrap();
    if let Some((_, e)) = g.iter().find(|(n, _)| *n == name) {
        return e;
    }
    let e: &'static [u8] = Box::leak(common::build_example(name).into_boxed_slice());
    g.push((name, e));
    e
}

pub struct Wallet {
    pub key: SigningKey,
    pub nonce: u64,
    pub chain_id: u64,
}

impl Wallet {
    pub fn new(seed: u8) -> Self {
        Wallet {
            key: SigningKey::from_bytes(&[seed; 32]),
            nonce: 0,
            chain_id: CHAIN_ID,
        }
    }

    pub fn addr(&self) -> Address {
        account_address(&self.key.verifying_key().to_bytes())
    }

    pub fn sign(&mut self, kind: TxKind) -> Transaction {
        let tx = Transaction::sign(
            TxBody {
                chain_id: self.chain_id,
                nonce: self.nonce,
                kind,
            },
            &self.key,
        );
        self.nonce += 1;
        tx
    }

    pub fn transfer(&mut self, to: Address, amount: u64) -> Transaction {
        self.sign(TxKind::Transfer { to, amount })
    }

    pub fn deploy(&mut self, code: &[u8], manifest: Manifest) -> Transaction {
        self.sign(TxKind::Deploy {
            code: code.to_vec(),
            manifest,
        })
    }

    pub fn call(&mut self, contract: Address, f: &str, args: &impl Encode) -> Transaction {
        self.call_with(contract, FunctionKind::Mutation, f, args, GAS)
    }

    pub fn call_with(
        &mut self,
        contract: Address,
        kind: FunctionKind,
        f: &str,
        args: &impl Encode,
        gas_limit: u64,
    ) -> Transaction {
        self.sign(TxKind::Call {
            contract,
            kind,
            function: f.into(),
            args: args.encode(),
            gas_limit,
            max_fee: u64::MAX,
        })
    }
}

pub fn manifest(name: &str) -> Manifest {
    Manifest {
        name: name.into(),
        ..Default::default()
    }
}

pub fn treasury() -> Address {
    [0xEE; 32]
}

/// Genesis giving each wallet `each` units and the rest to the treasury.
pub fn genesis(wallets: &[&Wallet], each: u64) -> GenesisConfig {
    genesis_with(wallets, each, ChainConfig::new(CHAIN_ID))
}

pub fn genesis_with(wallets: &[&Wallet], each: u64, config: ChainConfig) -> GenesisConfig {
    let mut allocations: Vec<(Address, u64)> = wallets.iter().map(|w| (w.addr(), each)).collect();
    let used = each * wallets.len() as u64;
    allocations.push((treasury(), TOTAL_SUPPLY - used));
    GenesisConfig {
        config,
        timestamp: 1_000,
        allocations,
    }
}

pub struct Net<S: KvStore> {
    pub state: ChainState<S>,
    pub ex: Executor,
}

impl<S: KvStore> Net<S> {
    pub fn new(kv: S, g: &GenesisConfig) -> Self {
        Net {
            state: ChainState::from_genesis(kv, g).unwrap(),
            ex: Executor::new(),
        }
    }

    pub fn block_for(&self, txs: Vec<Transaction>) -> Block {
        let head = self.state.head().unwrap();
        Block::new(
            head.height + 1,
            head.hash,
            head.timestamp + 1_000,
            self.state.root().unwrap(),
            txs,
        )
    }

    /// Execute a block of `txs`; returns the receipts.
    pub fn run(&mut self, txs: Vec<Transaction>) -> Vec<Receipt> {
        let b = self.block_for(txs);
        let (receipts, root) = self.ex.execute_block(&mut self.state, &b).unwrap();
        assert_eq!(root, self.state.root().unwrap());
        receipts
    }

    pub fn empty_blocks(&mut self, n: u64) -> Vec<Receipt> {
        (0..n).flat_map(|_| self.run(vec![])).collect()
    }

    pub fn bal(&self, a: &Address) -> u64 {
        self.state.balance(a).unwrap()
    }

    pub fn query(&mut self, c: &Address, f: &str, args: &impl Encode, who: &Address) -> Vec<u8> {
        self.ex
            .query(&mut self.state, c, f, &args.encode(), who)
            .unwrap()
            .0
    }
}

pub fn ok(r: &Receipt) {
    assert_eq!(r.status, Status::Success, "{r:?}");
}

/// The contract address a successful deploy receipt reports.
pub fn deployed(r: &Receipt) -> Address {
    ok(r);
    r.output.clone().try_into().unwrap()
}
