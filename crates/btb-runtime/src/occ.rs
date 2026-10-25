//! Optimistic transactions for mutations.
//!
//! A mutation runs against a snapshot (`Occ::snapshot` gives its sequence number; the store is
//! only read, writes sit in an `Overlay`) and yields a `Txn` with its writes and read set.
//! `Occ::commit` validates: if any write committed after the snapshot touched a key in the read
//! set (exact key, or inside a scanned range), the result is `Conflict` and nothing is applied.
//! Otherwise the batch is applied and the commit sequence number advances.
//!
//! Determinism: validation depends only on the commit log and the order of `commit` calls, so
//! the same transactions committed in the same order (the block order) give the same outcomes
//! everywhere. A transaction with no writes (a query) always commits and never conflicts.
//! The log holds one write-key list per commit since the last `prune`; a snapshot older than
//! the log is treated as conflicting.

use crate::kv::{KvError, KvStore, WriteBatch};
use crate::readset::ReadSet;
use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OccError<E> {
    /// A key this transaction read was written by a later commit.
    Conflict,
    Kv(KvError),
    /// The closure's own error (never retried).
    Failed(E),
}

/// An executed, not yet committed transaction.
#[derive(Debug, Clone)]
pub struct Txn<T> {
    pub snapshot: u64,
    pub reads: ReadSet,
    pub batch: WriteBatch,
    pub output: T,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed<T> {
    pub output: T,
    /// Commit sequence number (unchanged for read-only transactions).
    pub seq: u64,
    /// What was applied, e.g. for `SubscriptionManager::affected`.
    pub batch: WriteBatch,
    /// Executions used (1 = no retry); 1 for plain `commit`.
    pub attempts: u32,
}

pub struct Occ<S: KvStore> {
    store: S,
    seq: u64,
    /// `(seq, written keys)` for commits with seq > `log_base`.
    log: VecDeque<(u64, Vec<Vec<u8>>)>,
    log_base: u64,
}

impl<S: KvStore> Occ<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            seq: 0,
            log: VecDeque::new(),
            log_base: 0,
        }
    }
    pub fn store(&self) -> &S {
        &self.store
    }
    pub fn into_store(self) -> S {
        self.store
    }
    /// Sequence number of the latest commit; a transaction started now has this snapshot.
    pub fn snapshot(&self) -> u64 {
        self.seq
    }

    /// Forget log entries up to `seq`; transactions with an older snapshot will conflict.
    /// Call with the oldest snapshot still in flight.
    pub fn prune(&mut self, seq: u64) {
        while self.log.front().is_some_and(|(s, _)| *s <= seq) {
            self.log.pop_front();
        }
        self.log_base = self.log_base.max(seq.min(self.seq));
    }

    fn conflicts(&self, txn_snapshot: u64, reads: &ReadSet) -> bool {
        if txn_snapshot < self.log_base {
            return true;
        }
        self.log
            .iter()
            .filter(|(s, _)| *s > txn_snapshot)
            .any(|(_, keys)| keys.iter().any(|k| reads.contains_key(k)))
    }

    /// Validate and apply. Conflicting transactions change nothing.
    pub fn commit<T, E>(&mut self, txn: Txn<T>) -> Result<Committed<T>, OccError<E>> {
        if txn.batch.is_empty() {
            return Ok(Committed {
                output: txn.output,
                seq: self.seq,
                batch: txn.batch,
                attempts: 1,
            });
        }
        if self.conflicts(txn.snapshot, &txn.reads) {
            return Err(OccError::Conflict);
        }
        use crate::kv::BatchOp;
        let keys = txn
            .batch
            .ops()
            .iter()
            .map(|op| match op {
                BatchOp::Put(k, _) | BatchOp::Delete(k) => k.clone(),
            })
            .collect();
        self.store.commit(txn.batch.clone()).map_err(OccError::Kv)?;
        self.seq += 1;
        self.log.push_back((self.seq, keys));
        Ok(Committed {
            output: txn.output,
            seq: self.seq,
            batch: txn.batch,
            attempts: 1,
        })
    }

    /// Commit transactions in the given order; each outcome is independent.
    pub fn commit_all<T, E>(
        &mut self,
        txns: Vec<Txn<T>>,
    ) -> Vec<Result<Committed<T>, OccError<E>>> {
        txns.into_iter().map(|t| self.commit(t)).collect()
    }

    /// Run `f` against a fresh snapshot and commit; on `Conflict` run it again, at most
    /// `max_retries` more times (so up to `max_retries + 1` executions). `f` gets the store
    /// and the snapshot number and returns `(output, writes, reads)`. An `Err` from `f` is
    /// returned as `Failed` without retry.
    pub fn execute_with_retry<T, E>(
        &mut self,
        max_retries: u32,
        mut f: impl FnMut(&S, u64) -> Result<(T, WriteBatch, ReadSet), E>,
    ) -> Result<Committed<T>, OccError<E>> {
        let (mut c, attempts) = execute_with_retry(max_retries, |_| {
            let snapshot = self.seq;
            let (output, batch, reads) = f(&self.store, snapshot).map_err(OccError::Failed)?;
            self.commit(Txn {
                snapshot,
                reads,
                batch,
                output,
            })
        })?;
        c.attempts = attempts;
        Ok(c)
    }
}

/// Call `attempt` (1-based attempt number; it should take a fresh snapshot, execute and commit)
/// until it does not return `Conflict`, at most `max_retries` more times after the first.
/// Returns the value and the number of executions. Any other error is returned at once.
pub fn execute_with_retry<T, E>(
    max_retries: u32,
    mut attempt: impl FnMut(u32) -> Result<T, OccError<E>>,
) -> Result<(T, u32), OccError<E>> {
    let mut n = 0;
    loop {
        n += 1;
        match attempt(n) {
            Ok(v) => return Ok((v, n)),
            Err(OccError::Conflict) if n <= max_retries => {}
            Err(e) => return Err(e),
        }
    }
}
