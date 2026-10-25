//! Parallel block execution (Block-STM style).
//!
//! The result of `run` is, byte for byte, what running the transactions one at a time in block
//! order gives (receipts, final writes, fee pools); the sequential executor is the oracle and
//! the differential tests compare the two. See `docs/CHAIN.md` ("Parallel execution").
//!
//! # Engine
//!
//! * **Multi-version memory** (`MvMemory`): every speculative write is stored under
//!   `(key, tx index, incarnation)`. Index 0 holds the writes of `begin_block`; transaction `t`
//!   is index `t + 1` and reads the latest write of an index below its own, else the committed
//!   base.
//! * **Speculative execution**: workers run transactions with the unchanged sequential
//!   `Ctx::apply_tx` over an `MvView` (a `KvStore` that reads from the memory and logs every
//!   point read with the version it saw and every scan with its result).
//! * **Validation**: a transaction is valid if every logged read still returns what it saw.
//! * **Re-execution**: an invalid transaction has its writes marked `ESTIMATE` and runs again
//!   with a new incarnation. A reader that meets an estimate waits until the writer has
//!   re-executed (writers always have a lower index and are claimed first, so this cannot
//!   deadlock) instead of reading a value that is about to change.
//! * The engine works in waves (execute the pending set, validate everything that could be
//!   affected, repeat) until a validation pass fails nothing. At that point every transaction
//!   read exactly what the sequential order gives it; by induction on the index the outputs
//!   are the sequential ones whatever the thread timing was. The commit is deterministic: the
//!   final value of a key is its highest-index write.
//!
//! # Fee pools
//!
//! Every transaction credits the validator and relay pool balances, which would make all of
//! them conflict. The credits commute, so they are deferred: receipts carry the fees, the sums
//! are credited once after the parallel phase. If any final execution reads or writes a pool
//! balance (a transfer to a pool address, a contract asking for its balance), the deferral
//! would be visible, so the whole block is run sequentially instead.
//!
//! # Fast path (execution level)
//!
//! Transactions whose account footprint is statically known and closed skip the multi-version
//! memory entirely; see `classify`. Everything else is a "dynamic" transaction handled above.

use crate::exec::{BlockError, Ctx, Origin, Receipt, Status};
use crate::state::{nonce_key, ChainConfig, StateError};
use crate::tx::{Address, Hash, Manifest, Transaction, TxKind};
use crate::{relay_pool, slash_vault, validator_pool, RELAY_SHARE_BPS};
use btb_runtime::kv::{BatchOp, Entry as KvEntry, KvError, KvStore, Overlay, WriteBatch};
use btb_runtime::{layout, ReadSet, Tables};
use btb_vm::Contract;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

type Base<'a> = &'a (dyn KvStore + Sync);
type Val = Option<Arc<Vec<u8>>>;
type Cache = BTreeMap<Hash, Arc<Contract>>;

/// What the parallel phase did (for tests and benchmarks).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParStats {
    /// Transactions executed by the fast path (no conflict tracking).
    pub fast_txs: usize,
    /// Transactions executed by the multi-version engine.
    pub stm_txs: usize,
    /// Executions of those, re-executions included (`stm_txs` when nothing conflicted).
    pub executions: u64,
    pub waves: u32,
    /// The parallel result was discarded and the block ran sequentially.
    pub fell_back: bool,
}

pub(crate) struct ParOut {
    pub receipts: Vec<Receipt>,
    /// Final writes of the transactions (begin-block writes and pool credits excluded).
    pub writes: WriteBatch,
    pub relay: u64,
    pub validator: u64,
    pub any_fee: bool,
    pub deploys: Vec<(Address, Manifest)>,
    pub cache: Cache,
    pub stats: ParStats,
}

pub(crate) enum ParResult {
    Done(Box<ParOut>),
    /// The block cannot be run in parallel (or the assumptions did not hold); run it
    /// sequentially. Deterministic: depends only on the block and the state.
    Fallback(ParStats),
    Err(BlockError),
}

// ------------------------------------------------------------------ multi-version memory

struct VEntry {
    inc: u32,
    val: Val,
    estimate: bool,
}

type Versions = BTreeMap<usize, VEntry>;

#[derive(Default)]
struct MvMemory {
    map: RwLock<BTreeMap<Vec<u8>, Versions>>,
}

enum Lookup {
    Base,
    Found { idx: usize, inc: u32, val: Val },
    Estimate,
}

impl MvMemory {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<Vec<u8>, Versions>> {
        self.map.read().unwrap_or_else(|e| e.into_inner())
    }
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<Vec<u8>, Versions>> {
        self.map.write().unwrap_or_else(|e| e.into_inner())
    }

    fn lookup(&self, key: &[u8], below: usize) -> Lookup {
        let g = self.read();
        match g.get(key).and_then(|v| v.range(..below).next_back()) {
            None => Lookup::Base,
            Some((_, e)) if e.estimate => Lookup::Estimate,
            Some((idx, e)) => Lookup::Found {
                idx: *idx,
                inc: e.inc,
                val: e.val.clone(),
            },
        }
    }

    /// Replace everything index `idx` wrote: `old` keys that are not written again disappear.
    fn apply(&self, idx: usize, inc: u32, writes: Vec<(Vec<u8>, Val)>, old: &[Vec<u8>]) {
        let mut g = self.write();
        for k in old {
            if writes
                .binary_search_by(|(w, _)| w.as_slice().cmp(k))
                .is_err()
            {
                if let Some(v) = g.get_mut(k) {
                    v.remove(&idx);
                    if v.is_empty() {
                        g.remove(k);
                    }
                }
            }
        }
        for (k, val) in writes {
            g.entry(k).or_default().insert(
                idx,
                VEntry {
                    inc,
                    val,
                    estimate: false,
                },
            );
        }
    }

    fn mark_estimate(&self, idx: usize, keys: &[Vec<u8>]) {
        let mut g = self.write();
        for k in keys {
            if let Some(e) = g.get_mut(k).and_then(|v| v.get_mut(&idx)) {
                e.estimate = true;
            }
        }
    }
}

/// A scan as seen by the transaction at `below`: the base merged with the visible
/// multi-version writes (same merge rules as `Overlay`). `None` if it met an estimate.
fn scan_at(
    mv: &MvMemory,
    base: Base<'_>,
    below: usize,
    start: &[u8],
    end: Option<&[u8]>,
    reverse: bool,
    limit: usize,
) -> Result<Option<Vec<KvEntry>>, KvError> {
    if end.is_some_and(|e| e <= start) || limit == 0 {
        return Ok(Some(Vec::new()));
    }
    let hi = end.map_or(Bound::Unbounded, Bound::Excluded);
    let mut local: Vec<(Vec<u8>, Val)> = Vec::new();
    {
        let g = mv.read();
        for (k, vers) in g.range::<[u8], _>((Bound::Included(start), hi)) {
            if let Some((_, e)) = vers.range(..below).next_back() {
                if e.estimate {
                    return Ok(None);
                }
                local.push((k.clone(), e.val.clone()));
            }
        }
    }
    let want = limit.saturating_add(local.len());
    let mut merged: BTreeMap<Vec<u8>, Vec<u8>> =
        base.scan(start, end, reverse, want)?.into_iter().collect();
    let cutoff = if merged.len() >= want {
        if reverse {
            merged.keys().next().cloned()
        } else {
            merged.keys().next_back().cloned()
        }
    } else {
        None
    };
    for (k, v) in local {
        if let Some(c) = &cutoff {
            if if reverse { k < *c } else { k > *c } {
                continue;
            }
        }
        match v {
            Some(v) => {
                merged.insert(k, (*v).clone());
            }
            None => {
                merged.remove(&k);
            }
        }
    }
    Ok(Some(if reverse {
        merged.into_iter().rev().take(limit).collect()
    } else {
        merged.into_iter().take(limit).collect()
    }))
}

#[derive(PartialEq, Eq)]
enum Seen {
    Base,
    Mv(usize, u32),
}

enum ReadRec {
    Point(Vec<u8>, Seen),
    Scan {
        start: Vec<u8>,
        end: Option<Vec<u8>>,
        reverse: bool,
        limit: usize,
        result: Vec<KvEntry>,
    },
}

/// What transaction `below - 1` sees: the base plus the multi-version writes of lower indices.
struct MvView<'a> {
    mv: &'a MvMemory,
    base: Base<'a>,
    below: usize,
    /// Set when a worker panicked: waiting readers give up instead of spinning forever.
    abort: &'a AtomicBool,
    log: RefCell<Vec<ReadRec>>,
}

impl MvView<'_> {
    fn wait(&self) -> Result<(), KvError> {
        if self.abort.load(Ordering::Relaxed) {
            return Err(KvError("parallel executor aborted".into()));
        }
        std::thread::yield_now();
        Ok(())
    }
}

impl KvStore for MvView<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, KvError> {
        loop {
            match self.mv.lookup(key, self.below) {
                Lookup::Estimate => self.wait()?,
                Lookup::Found { idx, inc, val } => {
                    self.log
                        .borrow_mut()
                        .push(ReadRec::Point(key.to_vec(), Seen::Mv(idx, inc)));
                    return Ok(val.map(|v| (*v).clone()));
                }
                Lookup::Base => {
                    let v = self.base.get(key)?;
                    self.log
                        .borrow_mut()
                        .push(ReadRec::Point(key.to_vec(), Seen::Base));
                    return Ok(v);
                }
            }
        }
    }

    fn scan(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        reverse: bool,
        limit: usize,
    ) -> Result<Vec<KvEntry>, KvError> {
        loop {
            match scan_at(self.mv, self.base, self.below, start, end, reverse, limit)? {
                None => self.wait()?,
                Some(result) => {
                    self.log.borrow_mut().push(ReadRec::Scan {
                        start: start.to_vec(),
                        end: end.map(<[u8]>::to_vec),
                        reverse,
                        limit,
                        result: result.clone(),
                    });
                    return Ok(result);
                }
            }
        }
    }

    fn commit(&mut self, _batch: WriteBatch) -> Result<(), KvError> {
        Err(KvError("speculative view is read-only".into()))
    }
}

// ------------------------------------------------------------------ engine

struct TxOut {
    result: Result<Receipt, StateError>,
    reads: Vec<ReadRec>,
    wkeys: Vec<Vec<u8>>,
}

/// Per-worker scratch: tables registered lazily from manifests, parsed contracts.
struct Local {
    tables: Tables,
    registered: BTreeSet<Address>,
    cache: Cache,
}

struct AbortOnPanic<'a>(&'a AtomicBool);
impl Drop for AbortOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.store(true, Ordering::Relaxed);
        }
    }
}

struct Engine<'a> {
    mv: MvMemory,
    base: Base<'a>,
    txs: &'a [Transaction],
    cfg: ChainConfig,
    height: u64,
    slots: Vec<Mutex<Option<TxOut>>>,
    incs: Vec<AtomicU32>,
    abort: AtomicBool,
    executions: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl<'a> Engine<'a> {
    fn execute(&self, t: usize, local: &mut Local) {
        let inc = self.incs[t].fetch_add(1, Ordering::Relaxed) + 1;
        self.executions.fetch_add(1, Ordering::Relaxed);
        let view = MvView {
            mv: &self.mv,
            base: self.base,
            below: t + 1,
            abort: &self.abort,
            log: RefCell::new(Vec::new()),
        };
        let mut ctx = Ctx {
            overlay: Overlay::new(&view),
            tables: std::mem::take(&mut local.tables),
            cache: &mut local.cache,
            cfg: self.cfg,
            height: self.height,
            defer_fees: true,
            lazy_tables: true,
            registered: std::mem::take(&mut local.registered),
        };
        let result = ctx.apply_tx(&self.txs[t]);
        let Ctx {
            overlay,
            tables,
            registered,
            ..
        } = ctx;
        local.tables = tables;
        local.registered = registered;
        let (batch, _) = overlay.into_parts();
        let reads = view.log.take();
        let mut writes: Vec<(Vec<u8>, Val)> = Vec::new();
        if result.is_ok() {
            for op in batch.ops() {
                match op {
                    BatchOp::Put(k, v) => writes.push((k.clone(), Some(Arc::new(v.clone())))),
                    BatchOp::Delete(k) => writes.push((k.clone(), None)),
                }
            }
        }
        // (`into_batch` yields one op per key in key order, which `apply` relies on.)
        let wkeys: Vec<Vec<u8>> = writes.iter().map(|(k, _)| k.clone()).collect();
        let mut slot = lock(&self.slots[t]);
        let old = slot.take().map(|o| o.wkeys).unwrap_or_default();
        self.mv.apply(t + 1, inc, writes, &old);
        *slot = Some(TxOut {
            result,
            reads,
            wkeys,
        });
    }

    /// Everything above a failed transaction that read a key the failed ones wrote is
    /// re-executed in the same wave (it would fail validation after their re-execution
    /// anyway). Without this a chain of dependent transactions needs one wave per link.
    /// Conservative and always safe: an unneeded re-execution only costs time.
    fn close_over_dependents(&self, stm: &[usize], failed: Vec<usize>) -> Vec<usize> {
        let Some(&first) = failed.first() else {
            return failed;
        };
        let mut all: BTreeSet<usize> = failed.into_iter().collect();
        let mut dirty: BTreeSet<Vec<u8>> = BTreeSet::new();
        for &t in stm.iter().filter(|&&t| t >= first) {
            let slot = lock(&self.slots[t]);
            let Some(out) = slot.as_ref() else {
                continue;
            };
            let hit = all.contains(&t)
                || out.reads.iter().any(|r| match r {
                    ReadRec::Point(k, _) => dirty.contains(k),
                    ReadRec::Scan { start, end, .. } => {
                        let hi = end.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
                        end.as_deref().is_none_or(|e| e > start.as_slice())
                            && dirty
                                .range::<[u8], _>((Bound::Included(start.as_slice()), hi))
                                .next()
                                .is_some()
                    }
                });
            if hit {
                all.insert(t);
                dirty.extend(out.wkeys.iter().cloned());
            }
        }
        all.into_iter().collect()
    }

    fn valid(&self, t: usize) -> bool {
        let slot = lock(&self.slots[t]);
        let Some(out) = slot.as_ref() else {
            return false;
        };
        out.reads.iter().all(|r| match r {
            ReadRec::Point(k, seen) => match self.mv.lookup(k, t + 1) {
                Lookup::Estimate => false,
                Lookup::Base => *seen == Seen::Base,
                Lookup::Found { idx, inc, .. } => *seen == Seen::Mv(idx, inc),
            },
            ReadRec::Scan {
                start,
                end,
                reverse,
                limit,
                result,
            } => matches!(
                scan_at(&self.mv, self.base, t + 1, start, end.as_deref(), *reverse, *limit),
                Ok(Some(r)) if r == *result
            ),
        })
    }
}

fn run_workers<T: Send>(
    threads: usize,
    tasks: usize,
    abort: &AtomicBool,
    body: impl Fn() -> T + Sync,
) -> Vec<T> {
    let n = threads.clamp(1, tasks.max(1));
    std::thread::scope(|s| {
        let body = &body;
        let handles: Vec<_> = (0..n)
            .map(|_| {
                s.spawn(move || {
                    let _g = AbortOnPanic(abort);
                    body()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| match h.join() {
                Ok(v) => v,
                Err(p) => std::panic::resume_unwind(p),
            })
            .collect()
    })
}

// ------------------------------------------------------------------ classification

/// Split the block into statically-confined components (the fast path) and the rest.
///
/// A transaction's *account footprint* is the set of accounts whose nonce or balance it can
/// touch, known from the transaction alone:
///
/// | kind | accounts |
/// | --- | --- |
/// | `Transfer` | sender, recipient |
/// | `Deploy` | sender, the new contract address |
/// | `Stake`, `Unstake`, `RegisterValidator` | sender |
/// | `SubmitEvidence` | sender, the slash vault |
/// | `Call` | sender, the contract (a contract may also move tokens to other accounts) |
///
/// Transactions that share an account are joined (union-find). A component is *confined*
/// (fast path) iff it holds only `Transfer`s and none of its accounts is a fee pool or the
/// slash vault. Its transactions run in block order on one worker, over the committed base
/// only, with no multi-version memory and no validation. The first caller of `Call` can reach
/// accounts outside its footprint, so after the run `run` checks that no dynamic transaction
/// read or wrote a confined account, and that each confined transaction stayed inside its own
/// footprint; if either fails the whole block is re-run sequentially.
fn classify(txs: &[Transaction]) -> (Vec<Vec<usize>>, Vec<usize>) {
    let mut ids: BTreeMap<Address, usize> = BTreeMap::new();
    let mut parent: Vec<usize> = Vec::new();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    let special = [validator_pool(), relay_pool(), slash_vault()];
    let mut tx_acct: Vec<usize> = Vec::with_capacity(txs.len());
    let mut bad: Vec<bool> = Vec::new(); // per account id: special address
    let mut id_of = |a: Address, parent: &mut Vec<usize>, bad: &mut Vec<bool>| -> usize {
        *ids.entry(a).or_insert_with(|| {
            parent.push(parent.len());
            bad.push(special.contains(&a));
            parent.len() - 1
        })
    };
    for tx in txs {
        let s = tx.sender();
        let a = id_of(s, &mut parent, &mut bad);
        let others: Vec<Address> = match &tx.body.kind {
            TxKind::Transfer { to, .. } => vec![*to],
            TxKind::Deploy { .. } => vec![crate::tx::contract_address(&s, tx.body.nonce)],
            TxKind::Call { contract, .. } => vec![*contract],
            TxKind::SubmitEvidence { .. } => vec![slash_vault()],
            _ => vec![],
        };
        for o in others {
            let b = id_of(o, &mut parent, &mut bad);
            let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
            parent[rb] = ra;
        }
        tx_acct.push(a);
    }
    let n = parent.len();
    let mut dynamic = vec![false; n];
    for (i, b) in bad.iter().enumerate() {
        if *b {
            let r = find(&mut parent, i);
            dynamic[r] = true;
        }
    }
    for (i, tx) in txs.iter().enumerate() {
        if !matches!(tx.body.kind, TxKind::Transfer { .. }) {
            let r = find(&mut parent, tx_acct[i]);
            dynamic[r] = true;
        }
    }
    let mut comps: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    let mut stm = Vec::new();
    for (i, &acct) in tx_acct.iter().enumerate() {
        let r = find(&mut parent, acct);
        if dynamic[r] {
            stm.push(i);
        } else {
            comps.entry(r).or_default().push(i);
        }
    }
    // Largest components first so the cursor balances well; the result does not depend on
    // the order.
    let mut comps: Vec<Vec<usize>> = comps.into_values().collect();
    comps.sort_by_key(|c| std::cmp::Reverse(c.len()));
    (comps, stm)
}

/// The nonce and balance keys of the accounts a confined transaction may touch.
fn footprint_keys(tx: &Transaction, out: &mut BTreeSet<Vec<u8>>) {
    let mut add = |a: &Address| {
        out.insert(nonce_key(a));
        out.insert(layout::balance_key(a));
    };
    add(&tx.sender());
    if let TxKind::Transfer { to, .. } = &tx.body.kind {
        add(to);
    }
}

// ------------------------------------------------------------------ run

/// Execute `txs` (after `begin_block`, whose writes are `pre`) in parallel. See the module
/// docs. `Done` carries exactly what the sequential loop would have produced.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    base: Base<'_>,
    pre: &[(Vec<u8>, Option<Vec<u8>>)],
    txs: &[Transaction],
    cfg: ChainConfig,
    height: u64,
    cache: &Cache,
    threads: usize,
    use_fast_path: bool,
) -> ParResult {
    let mut stats = ParStats::default();
    let (comps, stm) = if use_fast_path {
        classify(txs)
    } else {
        (Vec::new(), (0..txs.len()).collect())
    };
    if stm.is_empty() && comps.len() == 1 {
        // A single confined component runs in order on one worker: nothing to gain.
        return ParResult::Fallback(stats);
    }
    stats.fast_txs = comps.iter().map(Vec::len).sum();
    stats.stm_txs = stm.len();

    let eng = Engine {
        mv: MvMemory::default(),
        base,
        txs,
        cfg,
        height,
        slots: (0..txs.len()).map(|_| Mutex::new(None)).collect(),
        incs: (0..txs.len()).map(|_| AtomicU32::new(0)).collect(),
        abort: AtomicBool::new(false),
        executions: AtomicU64::new(0),
    };
    eng.mv.apply(
        0,
        0,
        pre.iter()
            .map(|(k, v)| (k.clone(), v.clone().map(Arc::new)))
            .collect(),
        &[],
    );
    let mut caches: Vec<Cache> = Vec::new();

    // ---- fast path: confined components, one worker each, no conflict tracking
    type CompOut = (
        Vec<(usize, Result<Receipt, StateError>)>,
        WriteBatch,
        ReadSet,
    );
    let mut fast_out: Vec<(usize, CompOut)> = Vec::new();
    if !comps.is_empty() {
        let cursor = AtomicUsize::new(0);
        let results = run_workers(threads, comps.len(), &eng.abort, || {
            let mut local = Local {
                tables: Tables::new(),
                registered: BTreeSet::new(),
                cache: cache.clone(),
            };
            let mut done: Vec<(usize, CompOut)> = Vec::new();
            loop {
                let c = cursor.fetch_add(1, Ordering::Relaxed);
                if c >= comps.len() {
                    break;
                }
                // Index 1 sees only the begin-block writes (index 0).
                let view = MvView {
                    mv: &eng.mv,
                    base,
                    below: 1,
                    abort: &eng.abort,
                    log: RefCell::new(Vec::new()),
                };
                let mut ctx = Ctx {
                    overlay: Overlay::new(&view),
                    tables: std::mem::take(&mut local.tables),
                    cache: &mut local.cache,
                    cfg,
                    height,
                    defer_fees: true,
                    lazy_tables: true,
                    registered: std::mem::take(&mut local.registered),
                };
                let mut outs = Vec::new();
                for &i in &comps[c] {
                    let r = ctx.apply_tx(&txs[i]);
                    let failed = r.is_err();
                    outs.push((i, r));
                    if failed {
                        break;
                    }
                }
                let Ctx {
                    overlay,
                    tables,
                    registered,
                    ..
                } = ctx;
                local.tables = tables;
                local.registered = registered;
                let (batch, reads) = overlay.into_parts();
                done.push((c, (outs, batch, reads)));
            }
            (done, local.cache)
        });
        for (done, c) in results {
            fast_out.extend(done);
            caches.push(c);
        }
        fast_out.sort_by_key(|(c, _)| *c);
    }

    // ---- footprint check of the fast path (defensive; see `classify`)
    let mut fast_keys: BTreeSet<Vec<u8>> = BTreeSet::new();
    for comp in &comps {
        for &i in comp {
            footprint_keys(&txs[i], &mut fast_keys);
        }
    }
    for (c, (_, batch, reads)) in &fast_out {
        let mut own = BTreeSet::new();
        for &i in &comps[*c] {
            footprint_keys(&txs[i], &mut own);
        }
        let ok = reads.ranges().next().is_none()
            && reads.points().all(|k| own.contains(k))
            && batch.ops().iter().all(|op| match op {
                BatchOp::Put(k, _) | BatchOp::Delete(k) => own.contains(k),
            });
        if !ok {
            stats.fell_back = true;
            return ParResult::Fallback(stats);
        }
    }

    // ---- dynamic transactions: waves of execute / validate
    let mut todo: Vec<usize> = stm.clone();
    let mut known = cache.clone();
    for c in &caches {
        known.extend(c.iter().map(|(k, v)| (*k, v.clone())));
    }
    while !todo.is_empty() {
        stats.waves += 1;
        let cursor = AtomicUsize::new(0);
        let results = run_workers(threads, todo.len(), &eng.abort, || {
            let mut local = Local {
                tables: Tables::new(),
                registered: BTreeSet::new(),
                cache: known.clone(),
            };
            loop {
                let n = cursor.fetch_add(1, Ordering::Relaxed);
                if n >= todo.len() {
                    break;
                }
                eng.execute(todo[n], &mut local);
            }
            local.cache
        });
        for c in results {
            known.extend(c);
        }
        if eng.abort.load(Ordering::Relaxed) {
            return ParResult::Err(BlockError::State(StateError(
                "parallel executor aborted".into(),
            )));
        }
        // Only transactions above the lowest re-executed one can have seen stale data.
        let lowest = todo[0];
        let cands: Vec<usize> = stm.iter().copied().filter(|&t| t > lowest).collect();
        let cursor = AtomicUsize::new(0);
        let failed: Vec<Vec<usize>> = run_workers(threads, cands.len(), &eng.abort, || {
            let mut bad = Vec::new();
            loop {
                let n = cursor.fetch_add(1, Ordering::Relaxed);
                if n >= cands.len() {
                    break;
                }
                if !eng.valid(cands[n]) {
                    bad.push(cands[n]);
                }
            }
            bad
        });
        let mut failed: Vec<usize> = failed.into_iter().flatten().collect();
        failed.sort_unstable();
        let failed = eng.close_over_dependents(&stm, failed);
        for &t in &failed {
            let keys = lock(&eng.slots[t])
                .as_ref()
                .map(|o| o.wkeys.clone())
                .unwrap_or_default();
            eng.mv.mark_estimate(t + 1, &keys);
        }
        todo = failed;
    }
    stats.executions = eng.executions.load(Ordering::Relaxed);

    // ---- the first error wins, as in the sequential loop
    let mut first_err: Option<(usize, StateError)> = None;
    for (_, (outs, _, _)) in &fast_out {
        for (i, r) in outs {
            if let Err(e) = r {
                if first_err.as_ref().is_none_or(|(j, _)| i < j) {
                    first_err = Some((*i, e.clone()));
                }
            }
        }
    }
    let slots: Vec<Option<TxOut>> = eng
        .slots
        .into_iter()
        .map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner()))
        .collect();
    for &t in &stm {
        if let Some(Err(e)) = slots[t].as_ref().map(|o| &o.result) {
            if first_err.as_ref().is_none_or(|(j, _)| t < *j) {
                first_err = Some((t, e.clone()));
            }
        }
    }

    // ---- no dynamic transaction may have seen or touched a pool balance or a confined
    // account (their effects would differ from the sequential order)
    let pools = [
        layout::balance_key(&validator_pool()),
        layout::balance_key(&relay_pool()),
    ];
    for &t in &stm {
        let Some(out) = slots[t].as_ref() else {
            continue;
        };
        let hit = |k: &[u8]| pools.iter().any(|p| p == k) || fast_keys.contains(k);
        let mut bad = out.wkeys.iter().any(|k| hit(k));
        for r in &out.reads {
            bad |= match r {
                ReadRec::Point(k, _) => hit(k),
                ReadRec::Scan { start, end, .. } => {
                    let hi = end.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
                    let any_fast = end.as_deref().is_none_or(|e| e > start.as_slice())
                        && fast_keys
                            .range::<[u8], _>((Bound::Included(start.as_slice()), hi))
                            .next()
                            .is_some();
                    any_fast
                        || pools.iter().any(|p| {
                            p.as_slice() >= start.as_slice()
                                && end.as_deref().is_none_or(|e| p.as_slice() < e)
                        })
                }
            };
        }
        if bad {
            stats.fell_back = true;
            return ParResult::Fallback(stats);
        }
    }
    if let Some((_, e)) = first_err {
        return ParResult::Err(BlockError::State(e));
    }

    // ---- assemble receipts in block order
    let mut receipts: Vec<Option<Receipt>> = (0..txs.len()).map(|_| None).collect();
    let mut writes = WriteBatch::new();
    for (_, (outs, batch, _)) in fast_out {
        for (i, r) in outs {
            receipts[i] = r.ok();
        }
        for op in batch.ops() {
            match op {
                BatchOp::Put(k, v) => writes.put(k.clone(), v.clone()),
                BatchOp::Delete(k) => writes.delete(k.clone()),
            }
        }
    }
    for &t in &stm {
        receipts[t] = slots[t]
            .as_ref()
            .and_then(|o| o.result.as_ref().ok().cloned());
    }
    // Final value of each key written by a dynamic transaction: its highest-index write.
    {
        let g = eng.mv.read();
        for (k, vers) in g.iter() {
            if let Some((idx, e)) = vers.iter().next_back() {
                if *idx == 0 {
                    continue; // begin-block write, already in the caller's overlay
                }
                match &e.val {
                    Some(v) => writes.put(k.clone(), (**v).clone()),
                    None => writes.delete(k.clone()),
                }
            }
        }
    }
    let Some(receipts) = receipts.into_iter().collect::<Option<Vec<_>>>() else {
        return ParResult::Err(BlockError::State(StateError("missing receipt".into())));
    };

    // ---- fees and deploys, in block order
    let (mut relay, mut validator, mut any_fee) = (0u64, 0u64, false);
    let mut deploys = Vec::new();
    for (tx, r) in txs.iter().zip(&receipts) {
        debug_assert!(matches!(r.origin, Origin::Tx { .. }));
        if r.fee > 0 {
            any_fee = true;
            let rp = (u128::from(r.fee) * u128::from(RELAY_SHARE_BPS) / 10_000) as u64;
            let (Some(a), Some(b)) = (relay.checked_add(rp), validator.checked_add(r.fee - rp))
            else {
                return ParResult::Err(BlockError::State(StateError("balance overflow".into())));
            };
            relay = a;
            validator = b;
        }
        if let (TxKind::Deploy { manifest, .. }, Status::Success) = (&tx.body.kind, &r.status) {
            if let Ok(addr) = Address::try_from(r.output.as_slice()) {
                deploys.push((addr, manifest.clone()));
            }
        }
    }
    ParResult::Done(Box::new(ParOut {
        receipts,
        writes,
        relay,
        validator,
        any_fee,
        deploys,
        cache: known,
        stats,
    }))
}
