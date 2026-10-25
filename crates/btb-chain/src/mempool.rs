//! The mempool: pending transactions, ordered for block building.
//!
//! Admission checks everything that does not need a block: chain id, kind (no queries or
//! actions), signature, size and gas limits, a nonce that is not already used, and a balance
//! that can cover this transaction. Duplicates are rejected; a second transaction for the same
//! `(sender, nonce)` replaces the first only if it pays a higher fee.
//!
//! Selection orders by fee (highest first, ties by transaction id), but each sender's
//! transactions come out in strict nonce order starting at the sender's next nonce, so a
//! block never contains a nonce gap. "Fee" is the amount the transaction reserves,
//! `gas_limit * gas_price` (fixed for deploys and transfers); `max_fee` is only a cap.

use crate::exec::{check_static, max_cost, Invalid};
use crate::state::ChainConfig;
use crate::tx::{Address, Hash, Transaction};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

#[derive(Debug, Clone, Copy)]
pub struct MempoolConfig {
    pub max_txs: usize,
    pub max_per_sender: usize,
    /// Largest encoded transaction accepted.
    pub max_tx_bytes: usize,
}

impl Default for MempoolConfig {
    fn default() -> Self {
        MempoolConfig {
            max_txs: 50_000,
            max_per_sender: 64,
            max_tx_bytes: 1 << 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MempoolError {
    /// Failed the stateless checks (chain id, kind, signature, limits).
    Invalid(Invalid),
    TooLarge,
    Duplicate,
    NonceTooLow {
        next: u64,
        got: u64,
    },
    /// Same sender and nonce as a pending transaction, without a higher fee.
    NonceTaken,
    InsufficientBalance {
        needed: u64,
        available: u64,
    },
    PoolFull,
    SenderFull,
}

#[derive(Debug, Clone)]
struct Entry {
    tx: Transaction,
    id: Hash,
    fee: u64,
}

pub struct Mempool {
    chain: ChainConfig,
    cfg: MempoolConfig,
    by_sender: BTreeMap<Address, BTreeMap<u64, Entry>>,
    ids: BTreeSet<Hash>,
    len: usize,
}

impl Mempool {
    pub fn new(chain: ChainConfig, cfg: MempoolConfig) -> Self {
        Mempool {
            chain,
            cfg,
            by_sender: BTreeMap::new(),
            ids: BTreeSet::new(),
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn contains(&self, id: &Hash) -> bool {
        self.ids.contains(id)
    }

    /// Admit a transaction. `next_nonce` and `balance` are the sender's current values in the
    /// head state.
    pub fn insert(
        &mut self,
        tx: Transaction,
        next_nonce: u64,
        balance: u64,
    ) -> Result<Hash, MempoolError> {
        let wire = tx.encode();
        if wire.len() > self.cfg.max_tx_bytes {
            return Err(MempoolError::TooLarge);
        }
        let id = tx.id();
        if self.ids.contains(&id) {
            return Err(MempoolError::Duplicate);
        }
        check_static(&tx, &self.chain).map_err(MempoolError::Invalid)?;
        let nonce = tx.body.nonce;
        if nonce < next_nonce {
            return Err(MempoolError::NonceTooLow {
                next: next_nonce,
                got: nonce,
            });
        }
        let needed = max_cost(&tx, &self.chain).expect("checked by check_static");
        if balance < needed {
            return Err(MempoolError::InsufficientBalance {
                needed,
                available: balance,
            });
        }
        let fee = tx.gas_limit() * self.chain.gas_price;
        let sender = tx.sender();
        let slot = self.by_sender.get(&sender);
        let replaced = slot.and_then(|m| m.get(&nonce));
        if let Some(old) = replaced {
            if fee <= old.fee {
                return Err(MempoolError::NonceTaken);
            }
        } else {
            if self.len >= self.cfg.max_txs {
                return Err(MempoolError::PoolFull);
            }
            if slot.map_or(0, BTreeMap::len) >= self.cfg.max_per_sender {
                return Err(MempoolError::SenderFull);
            }
        }
        let old = self
            .by_sender
            .entry(sender)
            .or_default()
            .insert(nonce, Entry { tx, id, fee });
        match old {
            Some(o) => {
                self.ids.remove(&o.id);
            }
            None => self.len += 1,
        }
        self.ids.insert(id);
        Ok(id)
    }

    /// Pick transactions for the next block: highest fee first, per-sender nonce order from
    /// each sender's `next_nonce`, at most `max_txs` transactions and `max_gas` total gas
    /// limit. Does not remove them (call `remove` / `prune` after the block executes).
    pub fn select(
        &self,
        next_nonce: impl Fn(&Address) -> u64,
        max_txs: usize,
        max_gas: u64,
    ) -> Vec<Transaction> {
        // (fee, Reverse(id)): highest fee first, then the smallest id, so ties are stable.
        let mut heap: BinaryHeap<(u64, Reverse<Hash>, Address, u64)> = BinaryHeap::new();
        for (sender, m) in &self.by_sender {
            let n = next_nonce(sender);
            if let Some(e) = m.get(&n) {
                heap.push((e.fee, Reverse(e.id), *sender, n));
            }
        }
        let (mut out, mut gas) = (Vec::new(), 0u64);
        while let Some((_, _, sender, n)) = heap.pop() {
            if out.len() >= max_txs {
                break;
            }
            let e = &self.by_sender[&sender][&n];
            let Some(g) = gas.checked_add(e.tx.gas_limit()).filter(|g| *g <= max_gas) else {
                // Does not fit: this sender's later nonces cannot be included either.
                continue;
            };
            gas = g;
            out.push(e.tx.clone());
            if let Some(nx) = self.by_sender[&sender].get(&(n + 1)) {
                heap.push((nx.fee, Reverse(nx.id), sender, n + 1));
            }
        }
        out
    }

    /// Remove specific transactions (for example those in a finalized block).
    pub fn remove(&mut self, txs: &[Transaction]) {
        for tx in txs {
            let sender = tx.sender();
            let Some(m) = self.by_sender.get_mut(&sender) else {
                continue;
            };
            if m.get(&tx.body.nonce).is_some_and(|e| e.id == tx.id()) {
                let e = m.remove(&tx.body.nonce).expect("present");
                self.ids.remove(&e.id);
                self.len -= 1;
            }
            if m.is_empty() {
                self.by_sender.remove(&sender);
            }
        }
    }

    /// Drop every transaction whose nonce is below its sender's `next_nonce` (already used).
    pub fn prune(&mut self, next_nonce: impl Fn(&Address) -> u64) {
        let senders: Vec<Address> = self.by_sender.keys().copied().collect();
        for s in senders {
            let n = next_nonce(&s);
            let m = self.by_sender.get_mut(&s).expect("listed");
            let stale: Vec<u64> = m.range(..n).map(|(k, _)| *k).collect();
            for k in stale {
                let e = m.remove(&k).expect("listed");
                self.ids.remove(&e.id);
                self.len -= 1;
            }
            if m.is_empty() {
                self.by_sender.remove(&s);
            }
        }
    }
}
