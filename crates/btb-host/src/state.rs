//! Shared state of a hosting node: the recording store, recent blocks and live subscriptions.

use btb_chain::{ChainState, Executor};
use btb_node::blockstore::StoredBlock;
use btb_runtime::merkle::{TAG_NODE, TAG_ROOT};
use btb_runtime::{KvError, KvStore, MemKv, SubId, SubscriptionManager, WriteBatch};
use std::any::Any;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

/// Blocks a hosting node remembers for proof evidence (header and consensus block).
pub const KEEP_BLOCKS: usize = 1024;
/// Events buffered per subscriber; a client that falls this far behind is dropped.
pub const CLIENT_BACKLOG: usize = 64;

/// In-memory chain state that also remembers which keys each committed batch wrote, so the
/// block hook can ask the `SubscriptionManager` which subscriptions to re-run. The Merkle tree's
/// own node and root keys are left out (queries never read them).
#[derive(Default)]
pub struct Recording {
    inner: MemKv,
    written: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Recording {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn written(&self) -> Arc<Mutex<Vec<Vec<u8>>>> {
        self.written.clone()
    }
}

impl KvStore for Recording {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, KvError> {
        self.inner.get(key)
    }
    fn scan(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        reverse: bool,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, KvError> {
        self.inner.scan(start, end, reverse, limit)
    }
    fn commit(&mut self, batch: WriteBatch) -> Result<(), KvError> {
        {
            let mut w = self.written.lock().unwrap_or_else(|e| e.into_inner());
            for op in batch.ops() {
                let key = match op {
                    btb_runtime::kv::BatchOp::Put(k, _) | btb_runtime::kv::BatchOp::Delete(k) => k,
                };
                if !matches!(key.first(), Some(&(TAG_NODE | TAG_ROOT))) {
                    w.push(key.clone());
                }
            }
        }
        self.inner.commit(batch)
    }
}

/// The chain state type a hosting node runs on.
pub type HostState = ChainState<Recording>;

pub(crate) struct Client {
    pub caller: [u8; 32],
    pub tx: SyncSender<Vec<u8>>,
}

#[derive(Default)]
pub(crate) struct Subs {
    pub mgr: SubscriptionManager,
    pub clients: BTreeMap<SubId, Client>,
}

pub struct Shared {
    pub(crate) subs: Mutex<Subs>,
    pub(crate) log: Mutex<VecDeque<StoredBlock>>,
    pub(crate) written: Arc<Mutex<Vec<Vec<u8>>>>,
    pub(crate) head: AtomicU64,
    pub(crate) conns: AtomicUsize,
    pub(crate) stop: std::sync::atomic::AtomicBool,
    pub default_contract: Option<[u8; 32]>,
    pub max_connections: usize,
}

impl Shared {
    pub(crate) fn new(
        written: Arc<Mutex<Vec<Vec<u8>>>>,
        default_contract: Option<[u8; 32]>,
        max_connections: usize,
    ) -> Self {
        Shared {
            subs: Mutex::new(Subs::default()),
            log: Mutex::new(VecDeque::new()),
            written,
            head: AtomicU64::new(0),
            conns: AtomicUsize::new(0),
            stop: std::sync::atomic::AtomicBool::new(false),
            default_contract,
            max_connections,
        }
    }

    /// Driver-thread hook: runs after every executed block. Remembers the block and re-runs the
    /// subscriptions whose read set the block wrote to, pushing the results that changed.
    pub(crate) fn on_block(&self, any: &mut dyn Any, b: &StoredBlock) {
        let Some(state) = any.downcast_mut::<HostState>() else {
            return;
        };
        {
            let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
            log.push_back(b.clone());
            while log.len() > KEEP_BLOCKS {
                log.pop_front();
            }
        }
        self.head
            .store(b.header.height, std::sync::atomic::Ordering::SeqCst);
        let keys: Vec<Vec<u8>> =
            std::mem::take(&mut *self.written.lock().unwrap_or_else(|e| e.into_inner()));
        let mut guard = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        let subs = &mut *guard;
        if subs.clients.is_empty() {
            return;
        }
        let exec = Executor::sequential();
        let mut dead = Vec::new();
        for id in subs.mgr.affected_by_keys(keys.iter().map(Vec::as_slice)) {
            let Some(client) = subs.clients.get(&id) else {
                continue;
            };
            let caller = client.caller;
            let change = subs.mgr.refresh(id, |c, f, a| {
                exec.query_tracked(state, c, f, a, &caller)
                    .map(|(out, _, reads)| (out, reads))
            });
            match change {
                Ok(Some(ch)) => match client.tx.try_send(ch.result) {
                    Ok(()) => {}
                    // Gone, or too slow to keep up: drop it rather than buffer without bound.
                    Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => dead.push(id),
                },
                Ok(None) => {}
                Err(_) => dead.push(id),
            }
        }
        for id in dead {
            subs.mgr.unsubscribe(id);
            subs.clients.remove(&id);
        }
    }
}
