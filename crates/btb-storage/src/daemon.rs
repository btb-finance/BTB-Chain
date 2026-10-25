//! The storage-node daemon: serves pieces over HTTP, answers the chain's challenges and
//! rebuilds pieces it is assigned in a repair.
//!
//! Loop (every `poll`): ask the chain for this account's duties. For each duty with an open
//! challenge whose piece is on disk, build the chunk proof and send `Prove`. For a duty in
//! `Repairing` state whose piece is not on disk, fetch any `k` other pieces from their holders,
//! rebuild the missing piece, check that all rebuilt pieces reproduce the file id, store the
//! piece and let the next round prove it. Pieces on disk that the chain no longer assigns here
//! are deleted after `gc_after` consecutive successful polls without them.

use crate::chain::{self, Chain, Duty};
use crate::erasure::{self, verify_piece};
use crate::http::{self, FileMeta};
use crate::store::PieceStore;
use crate::{proof, Hash};
use btb_chain::storage::{PieceState, StorageOp};
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct DaemonConfig {
    pub dir: PathBuf,
    /// HTTP listen address (`127.0.0.1:0` picks a free port).
    pub listen: SocketAddr,
    pub poll: Duration,
    /// Largest piece accepted over HTTP.
    pub max_piece_bytes: usize,
    pub gc_after: u32,
}

impl DaemonConfig {
    pub fn new(dir: PathBuf, listen: SocketAddr) -> Self {
        DaemonConfig {
            dir,
            listen,
            poll: Duration::from_millis(250),
            max_piece_bytes: 256 << 20,
            gc_after: 20,
        }
    }
}

#[derive(Default)]
pub struct Stats {
    pub proofs_sent: AtomicU64,
    pub proof_errors: AtomicU64,
    pub repairs_done: AtomicU64,
    pub repair_errors: AtomicU64,
    pub gc_removed: AtomicU64,
}

pub struct StorageNode {
    server: http::Server,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    pub stats: Arc<Stats>,
    pub store: Arc<PieceStore>,
}

impl StorageNode {
    pub fn start(chain: Arc<dyn Chain>, cfg: DaemonConfig) -> Result<StorageNode, String> {
        let store = Arc::new(PieceStore::open(&cfg.dir).map_err(|e| format!("open store: {e}"))?);
        let me = chain.address();
        let auth_chain = chain.clone();
        let auth: http::Authorize = Arc::new(move |file: &Hash, index: u8| {
            let f = chain::file(auth_chain.as_ref(), file)?.ok_or("unknown file")?;
            let p = f
                .pieces
                .iter()
                .find(|p| p.index == index)
                .ok_or("no such piece")?;
            if p.node != me || p.state == PieceState::Vacant {
                return Err("piece is not assigned to this node".into());
            }
            Ok(FileMeta {
                size: f.size,
                k: f.k,
                m: f.m,
                piece_len: f.piece_len,
            })
        });
        let server = http::Server::start(cfg.listen, store.clone(), auth, cfg.max_piece_bytes)
            .map_err(|e| format!("listen {}: {e}", cfg.listen))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::default());
        let worker = {
            let (stop, stats, store) = (stop.clone(), stats.clone(), store.clone());
            std::thread::spawn(move || worker(chain, store, stop, stats, cfg.poll, cfg.gc_after))
        };
        Ok(StorageNode {
            server,
            stop,
            worker: Some(worker),
            stats,
            store,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.server.addr()
    }

    /// Stop answering (the HTTP server and the proof loop).
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        self.server.stop();
    }
}

fn worker(
    chain: Arc<dyn Chain>,
    store: Arc<PieceStore>,
    stop: Arc<AtomicBool>,
    stats: Arc<Stats>,
    poll: Duration,
    gc_after: u32,
) {
    let me = chain.address();
    let mut attempts: BTreeMap<(Hash, u8, u64), u32> = BTreeMap::new();
    let mut repair_at: BTreeMap<(Hash, u8), Instant> = BTreeMap::new();
    let mut orphan_polls: BTreeMap<(Hash, u8), u32> = BTreeMap::new();
    while !stop.load(Ordering::Relaxed) {
        match chain::duties(chain.as_ref(), &me) {
            Err(e) => eprintln!("btb-storage: duties: {e}"),
            Ok(duties) => {
                for d in &duties {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    duty(
                        chain.as_ref(),
                        &store,
                        d,
                        &stats,
                        &mut attempts,
                        &mut repair_at,
                    );
                }
                gc(&store, &duties, &mut orphan_polls, gc_after, &stats);
            }
        }
        let until = Instant::now() + poll;
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn duty(
    chain: &dyn Chain,
    store: &PieceStore,
    d: &Duty,
    stats: &Stats,
    attempts: &mut BTreeMap<(Hash, u8, u64), u32>,
    repair_at: &mut BTreeMap<(Hash, u8), Instant>,
) {
    let key = (d.file, d.index);
    if !store.has(&d.file, d.index) && d.state == PieceState::Repairing {
        let ready = repair_at.get(&key).is_none_or(|t| Instant::now() >= *t);
        if ready {
            match repair(chain, store, d) {
                Ok(()) => {
                    stats.repairs_done.fetch_add(1, Ordering::Relaxed);
                    repair_at.remove(&key);
                }
                Err(e) => {
                    stats.repair_errors.fetch_add(1, Ordering::Relaxed);
                    eprintln!(
                        "btb-storage: repair {}:{}: {e}",
                        crate::hex(&d.file),
                        d.index
                    );
                    repair_at.insert(key, Instant::now() + Duration::from_millis(500));
                }
            }
        }
    }
    let Some(ch) = d.challenge else { return };
    let tries = attempts.entry((d.file, d.index, ch.deadline)).or_default();
    if *tries >= 3 || !store.has(&d.file, d.index) {
        return;
    }
    *tries += 1;
    let built = store
        .get(&d.file, d.index)
        .map_err(|e| e.to_string())
        .and_then(|p| p.ok_or_else(|| "piece vanished".to_string()))
        .and_then(|(piece, path)| {
            http::chunk_proof(&piece, ch.chunk as usize, path).ok_or("chunk out of range".into())
        });
    match built.and_then(|p| {
        chain.send(StorageOp::Prove {
            file: d.file,
            index: d.index,
            proof: p,
        })
    }) {
        Ok(_) => {
            stats.proofs_sent.fetch_add(1, Ordering::Relaxed);
            *attempts
                .get_mut(&(d.file, d.index, ch.deadline))
                .expect("set") = 3;
        }
        Err(e) => {
            stats.proof_errors.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "btb-storage: prove {}:{}: {e}",
                crate::hex(&d.file),
                d.index
            );
        }
    }
    if attempts.len() > 4096 {
        attempts.clear();
    }
}

/// Rebuild piece `d.index` from any `k` other pieces and store it.
fn repair(chain: &dyn Chain, store: &PieceStore, d: &Duty) -> Result<(), String> {
    let f = chain::file(chain, &d.file)?.ok_or("file no longer exists")?;
    let n = usize::from(f.k) + usize::from(f.m);
    let me = chain.address();
    let mut slots: Vec<Option<Vec<u8>>> = vec![None; n];
    let mut got = 0;
    for p in &f.pieces {
        if p.index == d.index || p.state != PieceState::Holding {
            continue;
        }
        // Pieces this node holds itself are read from disk, the others from their holders.
        let fetched = if p.node == me {
            match store.get(&d.file, p.index) {
                Ok(Some(v)) => Ok(v),
                _ => continue,
            }
        } else {
            http::get_piece(&p.endpoint, &d.file, p.index)
        };
        let Ok((bytes, path)) = fetched else {
            continue;
        };
        if !verify_piece(
            &f.id,
            f.size,
            f.k,
            f.m,
            f.piece_len,
            p.index.into(),
            &bytes,
            &path,
        ) {
            continue; // a node serving garbage is simply not used
        }
        slots[usize::from(p.index)] = Some(bytes);
        got += 1;
        if got >= usize::from(f.k) {
            break;
        }
    }
    if got < usize::from(f.k) {
        return Err(format!("only {got} of {} needed pieces reachable", f.k));
    }
    erasure::reconstruct(&mut slots, f.k, f.m).map_err(|e| e.to_string())?;
    let roots: Vec<Hash> = slots
        .iter()
        .map(|p| proof::piece_root(p.as_ref().expect("reconstructed")))
        .collect();
    if proof::file_id_of_pieces(&roots, f.size, f.k, f.m) != f.id {
        return Err("rebuilt pieces do not reproduce the file id".into());
    }
    let mine = slots[usize::from(d.index)].as_ref().expect("reconstructed");
    store
        .put(
            &d.file,
            d.index,
            mine,
            &erasure::piece_path(&roots, d.index.into()),
        )
        .map_err(|e| e.to_string())
}

/// Delete pieces the chain no longer assigns here.
fn gc(
    store: &PieceStore,
    duties: &[Duty],
    orphan_polls: &mut BTreeMap<(Hash, u8), u32>,
    after: u32,
    stats: &Stats,
) {
    let live: BTreeSet<(Hash, u8)> = duties.iter().map(|d| (d.file, d.index)).collect();
    let Ok(local) = store.list() else { return };
    orphan_polls.retain(|k, _| local.contains(k) && !live.contains(k));
    for k in local {
        if live.contains(&k) {
            continue;
        }
        let c = orphan_polls.entry(k).or_default();
        *c += 1;
        if *c >= after && store.remove(&k.0, k.1).is_ok() {
            stats.gc_removed.fetch_add(1, Ordering::Relaxed);
            orphan_polls.remove(&k);
        }
    }
}
