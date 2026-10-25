//! Crash-safe node persistence: the consensus safety state and the execution log.
//!
//! Two small files in the data directory, each replaced atomically (write temp, fsync, rename,
//! fsync the directory):
//!
//! * `safety-<epoch>.bin` is the consensus `SafetyState` (last voted round, lock, high QC ...). A
//!   replica that restarts without it could vote twice in a round, so a failed save aborts the
//!   node, and a corrupt file refuses to start instead of silently resetting.
//! * `exec.bin` records which consensus block was executed last, with a write-ahead entry
//!   around each execution. The replica persists "block h is committed" *before* it hands out
//!   the `Commit` action, so a crash right after would otherwise lose block h for good. The
//!   safety file therefore stores the *executed* commit point, never one ahead of the chain; after
//!   a restart the replica re-commits (and re-fetches by sync) whatever the chain still lacks.

use btb_consensus::{Hash, Message, SafetyState, SafetyStorage};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn io_err(what: &str, e: impl std::fmt::Display) -> String {
    format!("{what}: {e}")
}

/// Write `bytes` to `path` atomically and durably.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::File::create(&tmp).map_err(|e| io_err("create temp file", e))?;
    f.write_all(bytes).map_err(|e| io_err("write", e))?;
    f.sync_all().map_err(|e| io_err("fsync", e))?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| io_err("rename", e))?;
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

struct Cur<'a>(&'a [u8]);
impl Cur<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn hash(&mut self) -> Option<Hash> {
        self.take(32)?.try_into().ok()
    }
}

/// Last executed consensus block: its height, round and id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecMeta {
    pub height: u64,
    pub round: u64,
    pub id: Hash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pending {
    meta: ExecMeta,
    chain_height_before: u64,
}

fn put_meta(o: &mut Vec<u8>, m: &ExecMeta) {
    o.extend_from_slice(&m.height.to_le_bytes());
    o.extend_from_slice(&m.round.to_le_bytes());
    o.extend_from_slice(&m.id);
}

fn get_meta(c: &mut Cur) -> Option<ExecMeta> {
    Some(ExecMeta {
        height: c.u64()?,
        round: c.u64()?,
        id: c.hash()?,
    })
}

/// Log of executed consensus blocks (see the module docs).
pub struct ExecLog {
    path: Option<PathBuf>,
    done: Arc<Mutex<ExecMeta>>,
    pending: Option<Pending>,
}

const EXEC_MAGIC: &[u8; 5] = b"BTBE1";

impl ExecLog {
    /// Open (or create) the log. `genesis_id` is the consensus root id; `chain_head_height` is
    /// the height the chain state is at right now, used to settle an interrupted execution.
    pub fn open(
        dir: Option<&Path>,
        genesis_id: Hash,
        chain_head_height: u64,
    ) -> Result<Self, String> {
        let path = dir.map(|d| d.join("exec.bin"));
        let mut done = ExecMeta {
            height: 0,
            round: 0,
            id: genesis_id,
        };
        let mut pending = None;
        if let Some(p) = path.as_ref().filter(|p| p.exists()) {
            let bytes = std::fs::read(p).map_err(|e| io_err("read exec.bin", e))?;
            let corrupt = || "exec.bin is corrupt; refusing to start".to_string();
            let mut c = Cur(&bytes);
            if c.take(5) != Some(&EXEC_MAGIC[..]) {
                return Err(corrupt());
            }
            done = get_meta(&mut c).ok_or_else(corrupt)?;
            if c.take(1).ok_or_else(corrupt)? == [1] {
                pending = Some(Pending {
                    meta: get_meta(&mut c).ok_or_else(corrupt)?,
                    chain_height_before: c.u64().ok_or_else(corrupt)?,
                });
            }
        }
        let mut log = ExecLog {
            path,
            done: Arc::new(Mutex::new(done)),
            pending,
        };
        if let Some(p) = log.pending.take() {
            // Interrupted execution: it happened iff the chain moved past `before`.
            if chain_head_height > p.chain_height_before {
                *log.done.lock().expect("lock") = p.meta;
            }
            log.save()?;
        }
        Ok(log)
    }

    pub fn done(&self) -> ExecMeta {
        *self.done.lock().expect("lock")
    }

    /// Shared handle to the executed commit point (read by `FileSafety`).
    pub fn done_handle(&self) -> Arc<Mutex<ExecMeta>> {
        self.done.clone()
    }

    /// Write-ahead entry: block `meta` is about to be executed on a chain at `before`.
    pub fn begin(&mut self, meta: ExecMeta, before: u64) -> Result<(), String> {
        self.pending = Some(Pending {
            meta,
            chain_height_before: before,
        });
        self.save()
    }

    /// The block was executed (or deterministically skipped).
    pub fn finish(&mut self) -> Result<(), String> {
        if let Some(p) = self.pending.take() {
            *self.done.lock().expect("lock") = p.meta;
        }
        self.save()
    }

    fn save(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut o = EXEC_MAGIC.to_vec();
        put_meta(&mut o, &self.done());
        match &self.pending {
            Some(p) => {
                o.push(1);
                put_meta(&mut o, &p.meta);
                o.extend_from_slice(&p.chain_height_before.to_le_bytes());
            }
            None => o.push(0),
        }
        write_atomic(path, &o)
    }
}

/// File-backed `SafetyStorage`.
pub struct FileSafety {
    path: PathBuf,
    exec: Arc<Mutex<ExecMeta>>,
    /// The finalized chain this epoch extends: (height, id). Commit points at or below it are
    /// recorded as exactly this block, which is what a fresh replica of the epoch starts from.
    anchor: (u64, Hash),
}

const SAFETY_MAGIC: &[u8; 5] = b"BTBS1";

impl FileSafety {
    /// Safety state of `epoch` in `dir/safety-<epoch>.bin`.
    pub fn new(dir: &Path, exec: Arc<Mutex<ExecMeta>>, epoch: u64, anchor: (u64, Hash)) -> Self {
        FileSafety {
            path: dir.join(format!("safety-{epoch}.bin")),
            exec,
            anchor,
        }
    }

    /// Check up front that an existing file is readable (so a corrupt one stops startup).
    pub fn check(&self) -> Result<(), String> {
        if self.path.exists() {
            self.read()?;
        }
        Ok(())
    }

    fn read(&self) -> Result<SafetyState, String> {
        let bytes = std::fs::read(&self.path).map_err(|e| io_err("read safety.bin", e))?;
        let corrupt = || "safety.bin is corrupt; refusing to start".to_string();
        let mut c = Cur(&bytes);
        if c.take(5) != Some(&SAFETY_MAGIC[..]) {
            return Err(corrupt());
        }
        let last_voted_round = c.u64().ok_or_else(corrupt)?;
        let locked_round = c.u64().ok_or_else(corrupt)?;
        let last_proposed_round = c.u64().ok_or_else(corrupt)?;
        let committed_height = c.u64().ok_or_else(corrupt)?;
        let committed_round = c.u64().ok_or_else(corrupt)?;
        let committed_id = c.hash().ok_or_else(corrupt)?;
        let n = c.u64().ok_or_else(corrupt)? as usize;
        let qc_bytes = c.take(n).ok_or_else(corrupt)?;
        let high_qc = match Message::decode(qc_bytes) {
            Some(Message::Catchup { qc, .. }) => qc,
            _ => return Err(corrupt()),
        };
        Ok(SafetyState {
            last_voted_round,
            locked_round,
            last_proposed_round,
            high_qc,
            committed_height,
            committed_round,
            committed_id,
        })
    }
}

impl SafetyStorage for FileSafety {
    fn load(&self) -> Option<SafetyState> {
        if !self.path.exists() {
            return None;
        }
        // `check` has already vetted the file; an unreadable one here must not look like "fresh".
        Some(self.read().unwrap_or_else(|e| panic!("{e}")))
    }

    fn save(&mut self, state: &SafetyState) {
        // Never record a commit point ahead of what was executed (see module docs).
        let done = *self.exec.lock().expect("lock");
        let (h, r, id) = if state.committed_height > done.height {
            if done.height <= self.anchor.0 {
                (self.anchor.0, 0, self.anchor.1)
            } else {
                (done.height, done.round, done.id)
            }
        } else {
            (
                state.committed_height,
                state.committed_round,
                state.committed_id,
            )
        };
        let qc = Message::Catchup {
            qc: state.high_qc.clone(),
            tc: None,
        }
        .encode();
        let mut o = SAFETY_MAGIC.to_vec();
        for v in [
            state.last_voted_round,
            state.locked_round,
            state.last_proposed_round,
            h,
            r,
        ] {
            o.extend_from_slice(&v.to_le_bytes());
        }
        o.extend_from_slice(&id);
        o.extend_from_slice(&(qc.len() as u64).to_le_bytes());
        o.extend_from_slice(&qc);
        if let Err(e) = write_atomic(&self.path, &o) {
            // Continuing to vote without durable safety state could double vote after a crash.
            eprintln!("btb-node: FATAL: cannot persist consensus safety state: {e}");
            std::process::abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use btb_consensus::Context;

    fn state(ctx: &Context) -> SafetyState {
        SafetyState {
            last_voted_round: 7,
            locked_round: 6,
            last_proposed_round: 5,
            high_qc: ctx.genesis_qc(),
            committed_height: 9,
            committed_round: 8,
            committed_id: [3; 32],
        }
    }

    fn ctx() -> Context {
        let k = ed25519_dalek::SigningKey::from_bytes(&[1; 32]);
        let set = btb_consensus::ValidatorSet::new(vec![btb_consensus::Validator {
            key: k.verifying_key().to_bytes(),
            stake: 1,
        }])
        .unwrap();
        Context::new(b"t", set)
    }

    #[test]
    fn safety_roundtrip_clamps_commit_point_to_executed() {
        let dir = tempfile::tempdir().unwrap();
        let log = ExecLog::open(Some(dir.path()), [0; 32], 0).unwrap();
        let mut fs = FileSafety::new(dir.path(), log.done_handle(), 0, (0, [4; 32]));
        assert!(fs.load().is_none());
        let c = ctx();
        fs.save(&state(&c));
        let back = fs.load().unwrap();
        // Nothing executed yet: the commit point is the anchor, never ahead of the chain.
        assert_eq!((back.committed_height, back.committed_id), (0, [4; 32]));
        assert_eq!(back.last_voted_round, 7);
        assert_eq!(back.high_qc, c.genesis_qc());
    }

    #[test]
    fn corrupt_files_refuse_to_start() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("safety-0.bin"), b"nonsense").unwrap();
        let log = ExecLog::open(Some(dir.path()), [0; 32], 0).unwrap();
        let fs = FileSafety::new(dir.path(), log.done_handle(), 0, (0, [0; 32]));
        assert!(fs.check().is_err());
        std::fs::write(dir.path().join("exec.bin"), b"BTBE1 truncated").unwrap();
        assert!(ExecLog::open(Some(dir.path()), [0; 32], 0).is_err());
    }

    #[test]
    fn interrupted_execution_is_settled_from_the_chain_height() {
        let meta = ExecMeta {
            height: 1,
            round: 2,
            id: [5; 32],
        };
        for (chain_height, executed) in [(0, false), (1, true)] {
            let dir = tempfile::tempdir().unwrap();
            let mut log = ExecLog::open(Some(dir.path()), [0; 32], 0).unwrap();
            log.begin(meta, 0).unwrap(); // crash before `finish`
            drop(log);
            let log = ExecLog::open(Some(dir.path()), [0; 32], chain_height).unwrap();
            assert_eq!(log.done().height, u64::from(executed));
        }
    }
}
