//! The validator's side of the cross-chain keys (`docs/CROSS_CHAIN.md`): key shares at rest,
//! preprocessed nonces that can never be used twice, the FROST signing rounds over the oracle
//! side channel, evidence of misbehaviour, and the vault duties (attesting deposits, proposing
//! withdrawal transactions).
//!
//! # Signing
//!
//! A request is a pending record on chain (`btb_chain::xsign`). For every pending request the
//! node runs numbered **attempts**:
//!
//! 1. The signer set of attempt `a` is `xsign::select_signers(key, excluded, request, a)`: a pure
//!    function of chain state, the same on every node.
//! 2. **Round 1 (preprocessed).** Every holder keeps a pool of nonce pairs and has published
//!    their commitments in batches (`XMsg::Pool`). A signer announces which of its commitments
//!    it uses for this attempt with a signed `XMsg::Use { counter }`. Nothing is computed
//!    per request in round 1.
//! 3. **Round 2.** Once all `Use` of the set are in, everybody builds the same signing package
//!    (message from chain state, commitments from the pools). Each signer takes its nonce out of
//!    its pool, **persists the pool, and only then** computes its share, signs it with its
//!    consensus key and gossips it (`XMsg::Share`). Everybody aggregates; the first node to
//!    finish gossips `Done`, and the nodes submit the signature on chain
//!    (`CrossOp::Signature`), staggered by a hash rank so that usually one transaction is sent.
//! 4. **Robustness.** An attempt that does not finish within the timeout, or in which a signer is
//!    proven faulty, is abandoned and attempt `a + 1` (another signer set) starts. A share that
//!    does not verify, or two different `Use` for one attempt, produce evidence
//!    (`CrossOp::Evidence`) that anyone can check on chain and that excludes the signer from
//!    later signer sets.
//!
//! # Nonce safety
//!
//! A nonce pair is removed from the pool and the pool file is rewritten (written to a temporary
//! file, synced, renamed) **before** the share leaves the process. A crash loses the nonce; no
//! crash, restart or retry can use it again: [`NonceStore::take`] returns it once. Nonces are
//! generated from fresh OS randomness (never derived from the share), and the announcement made
//! for each `(request, attempt)` is persisted with the pool, so a restarted node repeats its
//! announcement instead of making a second, different one. A node without a data directory keeps
//! this state in memory only and must not be restarted while it holds requests in flight.

use btb_chain::vault::{self, Observer};
use btb_chain::xdkg::{self, XKey};
use btb_chain::xdkg_agent::XAgent;
use btb_chain::xsign::{self, EvidenceOp, Proof, SignRequest};
use btb_chain::CrossOp;
use btb_oracle::frost::{self, KeyShare, NoncePool, Scheme, SigningPackage};
use btb_runtime::KvStore;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Commitments per `Pool` message (an oracle frame holds 2 KiB).
pub const POOL_CHUNK: usize = 8;
/// Pending requests looked at per pass.
pub const MAX_PENDING: usize = 32;
/// Attempts ahead of ours that we follow.
pub const MAX_JUMP: u32 = 8;
/// Messages parked for requests we have not seen yet.
const MAX_EARLY: usize = 512;
const MAX_USES_KEPT: usize = 4096;

/// Tuning knobs (tests shrink them).
#[derive(Debug, Clone)]
pub struct CrossParams {
    /// Nonce pairs kept ready per key; topped up when half are used.
    pub pool_target: usize,
    /// An attempt that has not finished after this long is abandoned.
    pub attempt_timeout: Duration,
    /// Delay between the submitters of a finished signature (by hash rank).
    pub submit_stagger: Duration,
    /// Re-send a validator duty (attestation, proposal) after this many blocks.
    pub resend_blocks: u64,
}

impl Default for CrossParams {
    fn default() -> Self {
        CrossParams {
            pool_target: 32,
            attempt_timeout: Duration::from_secs(4),
            submit_stagger: Duration::from_millis(400),
            resend_blocks: 6,
        }
    }
}

// ------------------------------------------------------------------ wire

const T_POOL: u8 = 1;
const T_POOL_WANT: u8 = 2;
const T_USE: u8 = 3;
const T_SHARE: u8 = 4;
const T_DONE: u8 = 5;
const T_WANT: u8 = 6;

/// A message of the signing side channel (the body of `OracleMsg::Cross`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XMsg {
    /// A holder's new nonce commitments, for counters `first, first + 1, ...`.
    Pool {
        scheme: Scheme,
        origin: u64,
        holder: u32,
        first: u64,
        commitments: Vec<Vec<u8>>,
    },
    /// "Send me your commitments for this key."
    PoolWant { scheme: Scheme, origin: u64 },
    /// A signer announces the commitment it uses for an attempt.
    Use {
        scheme: Scheme,
        origin: u64,
        request: u64,
        attempt: u32,
        holder: u32,
        counter: u64,
        sig: [u8; 64],
    },
    /// A signer's round-2 share, bound to the package it was computed for.
    Share {
        scheme: Scheme,
        origin: u64,
        request: u64,
        attempt: u32,
        holder: u32,
        package: [u8; 32],
        share: Vec<u8>,
        sig: [u8; 64],
    },
    /// A finished signature.
    Done { request: u64, signature: Vec<u8> },
    /// "Send me the signature of this request."
    Want { request: u64 },
}

struct W(Vec<u8>);
impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
}

struct R<'a>(&'a [u8]);
impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (a, b) = (self.0.get(..n)?, self.0.get(n..)?);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn scheme(&mut self) -> Option<Scheme> {
        Scheme::from_byte(self.u8()?)
    }
    fn bytes(&mut self, max: usize) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        if n > max {
            return None;
        }
        Some(self.take(n)?.to_vec())
    }
    fn arr<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }
}

impl XMsg {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(Vec::new());
        match self {
            XMsg::Pool {
                scheme,
                origin,
                holder,
                first,
                commitments,
            } => {
                w.u8(T_POOL);
                w.u8(scheme.byte());
                w.u64(*origin);
                w.u32(*holder);
                w.u64(*first);
                w.u32(commitments.len() as u32);
                for c in commitments {
                    w.bytes(c);
                }
            }
            XMsg::PoolWant { scheme, origin } => {
                w.u8(T_POOL_WANT);
                w.u8(scheme.byte());
                w.u64(*origin);
            }
            XMsg::Use {
                scheme,
                origin,
                request,
                attempt,
                holder,
                counter,
                sig,
            } => {
                w.u8(T_USE);
                w.u8(scheme.byte());
                w.u64(*origin);
                w.u64(*request);
                w.u32(*attempt);
                w.u32(*holder);
                w.u64(*counter);
                w.0.extend_from_slice(sig);
            }
            XMsg::Share {
                scheme,
                origin,
                request,
                attempt,
                holder,
                package,
                share,
                sig,
            } => {
                w.u8(T_SHARE);
                w.u8(scheme.byte());
                w.u64(*origin);
                w.u64(*request);
                w.u32(*attempt);
                w.u32(*holder);
                w.0.extend_from_slice(package);
                w.bytes(share);
                w.0.extend_from_slice(sig);
            }
            XMsg::Done { request, signature } => {
                w.u8(T_DONE);
                w.u64(*request);
                w.bytes(signature);
            }
            XMsg::Want { request } => {
                w.u8(T_WANT);
                w.u64(*request);
            }
        }
        w.0
    }

    /// Total decoder: `None` for anything malformed or with trailing bytes.
    pub fn decode(b: &[u8]) -> Option<XMsg> {
        let mut r = R(b);
        let m = match r.u8()? {
            T_POOL => {
                let scheme = r.scheme()?;
                let origin = r.u64()?;
                let holder = r.u32()?;
                let first = r.u64()?;
                let n = r.u32()? as usize;
                if n > POOL_CHUNK {
                    return None;
                }
                let mut commitments = Vec::with_capacity(n);
                for _ in 0..n {
                    commitments.push(r.bytes(128)?);
                }
                XMsg::Pool {
                    scheme,
                    origin,
                    holder,
                    first,
                    commitments,
                }
            }
            T_POOL_WANT => XMsg::PoolWant {
                scheme: r.scheme()?,
                origin: r.u64()?,
            },
            T_USE => XMsg::Use {
                scheme: r.scheme()?,
                origin: r.u64()?,
                request: r.u64()?,
                attempt: r.u32()?,
                holder: r.u32()?,
                counter: r.u64()?,
                sig: r.arr()?,
            },
            T_SHARE => XMsg::Share {
                scheme: r.scheme()?,
                origin: r.u64()?,
                request: r.u64()?,
                attempt: r.u32()?,
                holder: r.u32()?,
                package: r.arr()?,
                share: r.bytes(128)?,
                sig: r.arr()?,
            },
            T_DONE => XMsg::Done {
                request: r.u64()?,
                signature: r.bytes(128)?,
            },
            T_WANT => XMsg::Want { request: r.u64()? },
            _ => return None,
        };
        r.0.is_empty().then_some(m)
    }
}

// ------------------------------------------------------------------ files

/// Write `bytes` to `path` so that either the old or the new content survives a crash: a
/// private temporary file, synced, renamed over the target, and the directory synced.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&tmp)
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    f.write_all(bytes)
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))?;
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

const NONCE_MAGIC: &[u8; 10] = b"BTBNONCE\x00\x01";

/// A holder's nonce pool for one key, with the announcements it made, persisted on every change
/// that matters for safety.
#[derive(Debug, Default)]
pub struct NonceStore {
    path: Option<PathBuf>,
    pool: NoncePool,
    /// `(request, attempt) -> counter` announced in a `Use`.
    uses: BTreeMap<(u64, u32), u64>,
}

impl NonceStore {
    /// Open (or create empty) the store at `path`. A damaged file is an error: starting from an
    /// empty pool would be safe for nonces but would forget announcements.
    pub fn open(path: Option<PathBuf>) -> Result<NonceStore, String> {
        let mut s = NonceStore {
            path,
            ..Default::default()
        };
        let Some(p) = s.path.clone() else {
            return Ok(s);
        };
        match std::fs::read(&p) {
            Ok(b) => {
                let bad = || format!("{}: damaged nonce store", p.display());
                let rest = b.strip_prefix(NONCE_MAGIC.as_slice()).ok_or_else(bad)?;
                let mut r = R(rest);
                let pool_bytes = r.bytes(1 << 24).ok_or_else(bad)?;
                s.pool = NoncePool::decode(&pool_bytes).ok_or_else(bad)?;
                let n = r.u32().ok_or_else(bad)? as usize;
                for _ in 0..n {
                    let (req, att, c) = (
                        r.u64().ok_or_else(bad)?,
                        r.u32().ok_or_else(bad)?,
                        r.u64().ok_or_else(bad)?,
                    );
                    s.uses.insert((req, att), c);
                }
                if !r.0.is_empty() {
                    return Err(bad());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("read {}: {e}", p.display())),
        }
        Ok(s)
    }

    fn save(&self) -> Result<(), String> {
        let Some(p) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        let mut w = W(NONCE_MAGIC.to_vec());
        w.bytes(&self.pool.encode());
        w.u32(self.uses.len() as u32);
        for ((req, att), c) in &self.uses {
            w.u64(*req);
            w.u32(*att);
            w.u64(*c);
        }
        write_atomic(p, &w.0)
    }

    pub fn live(&self) -> usize {
        self.pool.len()
    }

    pub fn commitment(&self, counter: u64) -> Option<&[u8]> {
        self.pool.commitment(counter)
    }

    pub fn announced(&self, request: u64, attempt: u32) -> Option<u64> {
        self.uses.get(&(request, attempt)).copied()
    }

    /// Generate nonces up to `target` and persist; returns the new `(counter, commitment)`.
    pub fn top_up(
        &mut self,
        share: &KeyShare,
        target: usize,
    ) -> Result<Vec<(u64, Vec<u8>)>, String> {
        use rand::RngCore;
        let fresh = self
            .pool
            .top_up(share, target, || {
                let mut e = [0u8; 32];
                rand::rngs::OsRng.fill_bytes(&mut e);
                e
            })
            .map_err(|e| format!("nonce generation: {e:?}"))?;
        if !fresh.is_empty() {
            self.save()?;
        }
        Ok(fresh)
    }

    /// The lowest live counter that no announcement has claimed.
    pub fn free_counter(&self) -> Option<u64> {
        let claimed: BTreeSet<u64> = self.uses.values().copied().collect();
        self.pool.counters().find(|c| !claimed.contains(c))
    }

    /// Record (and persist) that `counter` is announced for `(request, attempt)`.
    pub fn announce(&mut self, request: u64, attempt: u32, counter: u64) -> Result<(), String> {
        self.uses.insert((request, attempt), counter);
        if self.uses.len() > MAX_USES_KEPT {
            let first = *self.uses.keys().next().expect("non-empty");
            self.uses.remove(&first);
        }
        self.save()
    }

    /// Forget the announcements of requests that are finished.
    pub fn forget_requests(&mut self, keep: impl Fn(u64) -> bool) {
        let before = self.uses.len();
        self.uses.retain(|(req, _), _| keep(*req));
        if self.uses.len() != before {
            let _ = self.save();
        }
    }

    /// Take the nonces of `counter` out of the pool **and persist the pool** before returning
    /// them. `Ok(None)`: already used (or never issued). On `Err` the nonces are gone from
    /// memory too, so nothing can be signed with them.
    pub fn take(&mut self, counter: u64) -> Result<Option<Vec<u8>>, String> {
        let n = self.pool.take(counter);
        if n.is_some() {
            self.save()?;
        }
        Ok(n)
    }

    pub fn clear(&mut self) {
        self.pool.clear();
        let _ = self.save();
    }
}

/// Shares of the cross-chain keys at rest: `<dir>/xshare-<scheme>-<origin>.json`, mode `0600`,
/// optionally encrypted like the beacon shares (`crate::dkg::ShareStore`).
#[derive(Debug, Clone, Default)]
pub struct XShareStore {
    dir: Option<PathBuf>,
    passphrase: Option<String>,
}

impl XShareStore {
    pub fn new(dir: Option<PathBuf>, passphrase: Option<String>) -> XShareStore {
        XShareStore { dir, passphrase }
    }

    fn path(&self, scheme: Scheme, origin: u64) -> Option<PathBuf> {
        self.dir
            .as_ref()
            .map(|d| d.join(format!("xshare-{}-{origin}.bin", scheme.name())))
    }

    pub fn save(&self, scheme: Scheme, origin: u64, share: &KeyShare) -> Result<(), String> {
        let (Some(dir), Some(path)) = (self.dir.as_ref(), self.path(scheme, origin)) else {
            return Ok(());
        };
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let raw = share.encode();
        let body = match &self.passphrase {
            None => {
                let mut b = vec![0u8];
                b.extend_from_slice(&raw);
                b
            }
            Some(pass) => {
                use chacha20poly1305::aead::{Aead, KeyInit};
                use rand::RngCore;
                let mut salt = [0u8; 16];
                let mut nonce = [0u8; 12];
                rand::rngs::OsRng.fill_bytes(&mut salt);
                rand::rngs::OsRng.fill_bytes(&mut nonce);
                let key = crate::dkg::kdf(pass, &salt);
                let ct = chacha20poly1305::ChaCha20Poly1305::new((&key).into())
                    .encrypt(chacha20poly1305::Nonce::from_slice(&nonce), raw.as_slice())
                    .map_err(|_| "share encryption failed".to_string())?;
                let mut b = vec![1u8];
                b.extend_from_slice(&salt);
                b.extend_from_slice(&nonce);
                b.extend_from_slice(&ct);
                b
            }
        };
        write_atomic(&path, &body)
    }

    pub fn load_all(&self) -> Result<Vec<(Scheme, u64, KeyShare)>, String> {
        let Some(dir) = self.dir.as_ref() else {
            return Ok(Vec::new());
        };
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(rest) = name
                .strip_prefix("xshare-")
                .and_then(|n| n.strip_suffix(".bin"))
            else {
                continue;
            };
            let Some((sch, origin)) = rest.split_once('-') else {
                continue;
            };
            let (Some(scheme), Ok(origin)) = (Scheme::from_name(sch), origin.parse::<u64>()) else {
                continue;
            };
            out.push((scheme, origin, self.load_file(&entry.path())?));
        }
        out.sort_by_key(|(s, o, _)| (*s, *o));
        Ok(out)
    }

    fn load_file(&self, path: &Path) -> Result<KeyShare, String> {
        let b = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let raw = match b.first() {
            Some(0) => b[1..].to_vec(),
            Some(1) => {
                use chacha20poly1305::aead::{Aead, KeyInit};
                let pass = self.passphrase.as_ref().ok_or_else(|| {
                    format!("{} is encrypted: set BTB_KEY_PASSPHRASE", path.display())
                })?;
                if b.len() < 1 + 16 + 12 {
                    return Err(format!("{}: truncated", path.display()));
                }
                let salt: [u8; 16] = b[1..17].try_into().expect("16");
                let key = crate::dkg::kdf(pass, &salt);
                chacha20poly1305::ChaCha20Poly1305::new((&key).into())
                    .decrypt(chacha20poly1305::Nonce::from_slice(&b[17..29]), &b[29..])
                    .map_err(|_| format!("{}: wrong passphrase or damaged file", path.display()))?
            }
            _ => return Err(format!("{}: unknown share format", path.display())),
        };
        KeyShare::decode(&raw).ok_or_else(|| format!("{}: malformed share", path.display()))
    }

    pub fn remove(&self, scheme: Scheme, origin: u64) {
        if let Some(p) = self.path(scheme, origin) {
            let _ = std::fs::remove_file(p);
        }
    }
}

// ------------------------------------------------------------------ the node

/// What the signing code needs from chain state, read once per pass.
#[derive(Debug, Clone, Default)]
pub struct View {
    pub height: u64,
    pub epoch: u64,
    /// The key that signs in `epoch`, and this node's share of it.
    pub keys: BTreeMap<Scheme, (XKey, Option<KeyShare>)>,
    pub excluded: BTreeMap<Scheme, BTreeSet<[u8; 32]>>,
    /// Pending requests, ascending.
    pub pending: Vec<SignRequest>,
}

#[derive(Debug, Clone)]
struct ShareRec {
    package: [u8; 32],
    share: Vec<u8>,
    sig: [u8; 64],
}

#[derive(Debug, Default)]
struct AttemptState {
    signers: Vec<usize>,
    uses: BTreeMap<usize, (u64, [u8; 64])>,
    package: Option<(SigningPackage, [u8; 32])>,
    shares: BTreeMap<usize, ShareRec>,
    mine_signed: bool,
}

#[derive(Debug)]
struct ReqState {
    req: SignRequest,
    attempt: u32,
    attempt_started: Instant,
    attempts: BTreeMap<u32, AttemptState>,
    blamed: BTreeSet<[u8; 32]>,
    signature: Option<Vec<u8>>,
    sig_at: Option<Instant>,
    submitted: Option<Instant>,
    last_want: Option<Instant>,
}

/// Counters for status RPC and tests.
#[derive(Debug, Default, Clone)]
pub struct CrossStats {
    pub deals_sent: u64,
    pub complaints_sent: u64,
    pub signatures_made: u64,
    pub attempts_abandoned: u64,
    pub evidence_sent: u64,
    pub attestations_sent: u64,
    pub proposals_sent: u64,
    pub nonces_used: u64,
}

pub struct CrossNode {
    chain_id: u64,
    key: SigningKey,
    me: [u8; 32],
    params: CrossParams,
    pub agent: XAgent,
    shares: XShareStore,
    nonce_dir: Option<PathBuf>,
    nonces: BTreeMap<(Scheme, u64), NonceStore>,
    /// Published commitments of every holder (ours included), by key origin.
    commits: BTreeMap<(Scheme, u64, u32), BTreeMap<u64, Vec<u8>>>,
    reqs: BTreeMap<u64, ReqState>,
    early: Vec<(Option<[u8; 32]>, XMsg)>,
    pool_want_at: BTreeMap<(Scheme, u64, u32), Instant>,
    observer: Option<Arc<dyn Observer>>,
    /// Transactions sent and not yet seen in a block.
    pub pending: Vec<btb_chain::Hash>,
    /// Messages to send: `(Some(peer))` direct, `None` broadcast.
    pub outbox: Vec<(Option<[u8; 32]>, XMsg)>,
    sent: BTreeMap<[u8; 32], u64>,
    first_open_withdrawal: u64,
    first_unfinished: u64,
    pub stats: CrossStats,
    /// A new share appeared (the driver may want to report it).
    pub changed: bool,
    last_error: Option<String>,
}

fn h(parts: &[&[u8]]) -> [u8; 32] {
    let mut s = Sha256::new();
    for p in parts {
        s.update(p);
    }
    s.finalize().into()
}

fn verify_ed(key: &[u8; 32], digest: &[u8; 32], sig: &[u8; 64]) -> bool {
    VerifyingKey::from_bytes(key).ok().is_some_and(|vk| {
        vk.verify_strict(digest, &Signature::from_bytes(sig))
            .is_ok()
    })
}

impl CrossNode {
    pub fn new(
        chain_id: u64,
        key: SigningKey,
        dir: Option<PathBuf>,
        passphrase: Option<String>,
        params: CrossParams,
        observer: Option<Arc<dyn Observer>>,
    ) -> Result<CrossNode, String> {
        let shares = XShareStore::new(dir.clone(), passphrase);
        let mut agent = XAgent::new(chain_id, key.clone());
        let mut any = false;
        for (scheme, origin, share) in shares.load_all()? {
            agent.insert_share(scheme, origin, share);
            any = true;
        }
        let me = key.verifying_key().to_bytes();
        Ok(CrossNode {
            chain_id,
            key,
            me,
            params,
            agent,
            shares,
            nonce_dir: dir,
            nonces: BTreeMap::new(),
            commits: BTreeMap::new(),
            reqs: BTreeMap::new(),
            early: Vec::new(),
            pool_want_at: BTreeMap::new(),
            observer,
            pending: Vec::new(),
            outbox: Vec::new(),
            sent: BTreeMap::new(),
            first_open_withdrawal: 0,
            first_unfinished: 0,
            stats: CrossStats::default(),
            changed: any,
            last_error: None,
        })
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    fn fail(&mut self, e: String) {
        eprintln!("btb-node: cross-chain: {e}");
        self.last_error = Some(e);
    }

    /// Snapshot of what the signing code reads from the chain.
    pub fn view(&self, kv: &impl KvStore, height: u64, epoch: u64) -> Result<View, String> {
        let mut v = View {
            height,
            epoch,
            ..Default::default()
        };
        for scheme in Scheme::ALL {
            if let Some((_, key)) = xdkg::key_at(kv, scheme, epoch).map_err(|e| e.to_string())? {
                let share = self
                    .agent
                    .share_for_epoch(kv, scheme, epoch)
                    .map_err(|e| e.to_string())?
                    .map(|(_, s)| s);
                v.excluded.insert(
                    scheme,
                    xsign::excluded(kv, scheme, epoch).map_err(|e| e.to_string())?,
                );
                v.keys.insert(scheme, (key, share));
            }
        }
        v.pending = xsign::pending_from(kv, 0, MAX_PENDING).map_err(|e| e.to_string())?;
        Ok(v)
    }

    fn nonce_store(&mut self, scheme: Scheme, origin: u64) -> Result<&mut NonceStore, String> {
        if !self.nonces.contains_key(&(scheme, origin)) {
            let path = self
                .nonce_dir
                .as_ref()
                .map(|d| d.join(format!("nonces-{}-{origin}.bin", scheme.name())));
            self.nonces
                .insert((scheme, origin), NonceStore::open(path)?);
        }
        Ok(self
            .nonces
            .get_mut(&(scheme, origin))
            .expect("just inserted"))
    }

    // -------------------------------------------------------------- per block

    /// After a block was executed: key generation duties, share persistence and erasure, nonce
    /// pools, vault duties. Returns the operations to submit (signed by the consensus key).
    pub fn on_block(
        &mut self,
        kv: &impl KvStore,
        next_height: u64,
        epoch: u64,
    ) -> Result<Vec<CrossOp>, String> {
        let step = self
            .agent
            .step(kv, next_height)
            .map_err(|e| e.to_string())?;
        for (scheme, origin, share) in &step.new_shares {
            self.shares.save(*scheme, *origin, share)?;
            self.changed = true;
        }
        let mut ops = step.ops;
        for op in &ops {
            match op {
                CrossOp::Deal { .. } => self.stats.deals_sent += 1,
                CrossOp::Complain { .. } => self.stats.complaints_sent += 1,
                _ => {}
            }
        }
        // Erase shares (and nonces) of generations older than the key now in use: resharing
        // leaves old shares valid for the same group key, so honest holders must not keep them.
        for scheme in Scheme::ALL {
            let Some((_, cur)) = xdkg::key_at(kv, scheme, epoch).map_err(|e| e.to_string())? else {
                continue;
            };
            for origin in self.agent.forget_before(scheme, cur.origin) {
                self.shares.remove(scheme, origin);
            }
            let old: Vec<(Scheme, u64)> = self
                .nonces
                .keys()
                .filter(|(s, o)| *s == scheme && *o < cur.origin)
                .copied()
                .collect();
            for k in old {
                if let Some(mut st) = self.nonces.remove(&k) {
                    st.clear();
                }
                if let Some(d) = &self.nonce_dir {
                    let _ =
                        std::fs::remove_file(d.join(format!("nonces-{}-{}.bin", k.0.name(), k.1)));
                }
            }
            self.commits
                .retain(|(s, o, _), _| *s != scheme || *o >= cur.origin);
        }
        // preprocessing: keep a pool of nonces ready and publish new commitments
        for scheme in Scheme::ALL {
            let Some((key, share)) = self
                .agent
                .share_for_epoch(kv, scheme, epoch)
                .map_err(|e| e.to_string())?
            else {
                continue;
            };
            self.ensure_pool(scheme, key.origin, &share, false)?;
        }
        ops.extend(self.vault_duties(kv, next_height, epoch)?);
        Ok(ops)
    }

    /// Make sure the pool of `(scheme, key.origin)` is filled; publish new commitments.
    pub fn ensure_pool(
        &mut self,
        scheme: Scheme,
        origin: u64,
        k: &KeyShare,
        force: bool,
    ) -> Result<(), String> {
        let target = self.params.pool_target;
        let holder = k.index as u32;
        let store = self.nonce_store(scheme, origin)?;
        if !force && store.live() * 2 > target {
            return Ok(());
        }
        let fresh = store.top_up(k, target)?;
        // remember our own commitments for package building
        let own = self.commits.entry((scheme, origin, holder)).or_default();
        for (c, comm) in &fresh {
            own.insert(*c, comm.clone());
        }
        for chunk in fresh.chunks(POOL_CHUNK) {
            self.outbox.push((
                None,
                XMsg::Pool {
                    scheme,
                    origin,
                    holder,
                    first: chunk[0].0,
                    commitments: chunk.iter().map(|(_, c)| c.clone()).collect(),
                },
            ));
        }
        Ok(())
    }

    // -------------------------------------------------------------- signing

    /// Make progress on every pending request. Returns operations to submit.
    pub fn tick(&mut self, view: &View, now: Instant) -> Vec<CrossOp> {
        let mut ops = Vec::new();
        let ids: BTreeSet<u64> = view.pending.iter().map(|r| r.id).collect();
        // drop state of requests the chain no longer lists as pending
        self.reqs.retain(|id, _| ids.contains(id));
        for st in self.nonces.values_mut() {
            st.forget_requests(|r| ids.contains(&r));
        }
        for r in &view.pending {
            self.reqs.entry(r.id).or_insert_with(|| ReqState {
                req: r.clone(),
                attempt: 0,
                attempt_started: now,
                attempts: BTreeMap::new(),
                blamed: BTreeSet::new(),
                signature: None,
                sig_at: None,
                submitted: None,
                last_want: None,
            });
        }
        // messages that arrived before their request
        let early = std::mem::take(&mut self.early);
        for (from, m) in early {
            self.handle(from, m, view, now, &mut ops);
        }
        for id in ids {
            self.step_request(id, view, now, &mut ops);
        }
        ops
    }

    fn step_request(&mut self, id: u64, view: &View, now: Instant, ops: &mut Vec<CrossOp>) {
        let Some(rs) = self.reqs.get(&id) else {
            return;
        };
        let scheme = rs.req.scheme;
        let Some((key, my_share)) = view.keys.get(&scheme) else {
            return;
        };
        if let Some(sig) = rs.signature.clone() {
            self.maybe_submit(id, key, sig, now, ops);
            return;
        }
        // time out the attempt
        {
            let rs = self.reqs.get_mut(&id).expect("exists");
            if now.duration_since(rs.attempt_started) >= self.params.attempt_timeout {
                rs.attempt = (rs.attempt + 1).min(xsign::MAX_ATTEMPT);
                rs.attempt_started = now;
                self.stats.attempts_abandoned += 1;
            }
            // ask around now and then: someone may already have the signature
            let due = rs
                .last_want
                .is_none_or(|t| now.duration_since(t) >= self.params.attempt_timeout / 2);
            if due && rs.attempt > 0 {
                rs.last_want = Some(now);
                self.outbox.push((None, XMsg::Want { request: id }));
            }
        }
        let attempt = self.reqs[&id].attempt;
        let blamed = self.reqs[&id].blamed.clone();
        let mut excluded = view.excluded.get(&scheme).cloned().unwrap_or_default();
        excluded.extend(blamed);
        self.reqs
            .get_mut(&id)
            .expect("exists")
            .attempts
            .entry(attempt)
            .or_insert_with(|| AttemptState {
                signers: xsign::select_signers(key, scheme, &excluded, id, attempt),
                ..Default::default()
            });
        let Some(my_holder) = key.holder_index(&self.me) else {
            // not a holder: nothing to contribute, but aggregation below still needs shares
            self.try_aggregate(id, key, now, ops);
            return;
        };
        let in_set = self.reqs[&id].attempts[&attempt]
            .signers
            .contains(&my_holder);
        // round 1: announce the nonce commitment for this attempt
        if in_set
            && my_share.is_some()
            && !self.reqs[&id].attempts[&attempt]
                .uses
                .contains_key(&my_holder)
        {
            if let Err(e) = self.announce(id, attempt, scheme, key, my_holder) {
                self.fail(e);
            }
        }
        self.try_build_package(id, key, now);
        // round 2: sign
        if in_set {
            if let Some(share) = my_share {
                if let Err(e) = self.try_sign(id, key, share, my_holder) {
                    self.fail(e);
                }
            }
        }
        self.try_aggregate(id, key, now, ops);
    }

    fn announce(
        &mut self,
        id: u64,
        attempt: u32,
        scheme: Scheme,
        key: &XKey,
        my_holder: usize,
    ) -> Result<(), String> {
        let origin = key.origin;
        let store = self.nonce_store(scheme, origin)?;
        // a restarted node repeats the announcement it made before
        let counter = match store.announced(id, attempt) {
            Some(c) => c,
            None => {
                let Some(c) = store.free_counter() else {
                    return Ok(()); // pool empty; on_block refills it
                };
                store.announce(id, attempt, c)?;
                c
            }
        };
        let digest = xsign::use_digest(self.chain_id, scheme, origin, id, attempt, counter);
        let sig = self.key.sign(&digest).to_bytes();
        self.reqs
            .get_mut(&id)
            .expect("exists")
            .attempts
            .get_mut(&attempt)
            .expect("exists")
            .uses
            .insert(my_holder, (counter, sig));
        self.outbox.push((
            None,
            XMsg::Use {
                scheme,
                origin,
                request: id,
                attempt,
                holder: my_holder as u32,
                counter,
                sig,
            },
        ));
        Ok(())
    }

    fn commitment_of(
        &self,
        scheme: Scheme,
        origin: u64,
        holder: usize,
        counter: u64,
    ) -> Option<Vec<u8>> {
        self.commits
            .get(&(scheme, origin, holder as u32))?
            .get(&counter)
            .cloned()
    }

    fn try_build_package(&mut self, id: u64, key: &XKey, now: Instant) {
        let rs = &self.reqs[&id];
        let scheme = rs.req.scheme;
        let at = &rs.attempts[&rs.attempt];
        if at.package.is_some() || at.signers.iter().any(|s| !at.uses.contains_key(s)) {
            return;
        }
        let mut commitments = BTreeMap::new();
        let mut missing = Vec::new();
        for s in &at.signers {
            match self.commitment_of(scheme, key.origin, *s, at.uses[s].0) {
                Some(c) => {
                    commitments.insert(*s, c);
                }
                None => missing.push(*s),
            }
        }
        if !missing.is_empty() {
            for s in missing {
                let k = (scheme, key.origin, s as u32);
                let due = self
                    .pool_want_at
                    .get(&k)
                    .is_none_or(|t| now.duration_since(*t) >= Duration::from_millis(500));
                if due {
                    self.pool_want_at.insert(k, now);
                    self.outbox.push((
                        Some(key.holders[s]),
                        XMsg::PoolWant {
                            scheme,
                            origin: key.origin,
                        },
                    ));
                }
            }
            return;
        }
        let pkg = SigningPackage {
            message: rs.req.message.to_vec(),
            commitments,
        };
        let hash = xsign::package_hash(&pkg.encode());
        let attempt = rs.attempt;
        self.reqs
            .get_mut(&id)
            .expect("exists")
            .attempts
            .get_mut(&attempt)
            .expect("exists")
            .package = Some((pkg, hash));
    }

    fn try_sign(
        &mut self,
        id: u64,
        key: &XKey,
        share: &KeyShare,
        my_holder: usize,
    ) -> Result<(), String> {
        let rs = &self.reqs[&id];
        let (scheme, attempt) = (rs.req.scheme, rs.attempt);
        let at = &rs.attempts[&attempt];
        let Some((pkg, hash)) = at.package.clone() else {
            return Ok(());
        };
        if at.mine_signed {
            return Ok(());
        }
        let counter = at.uses[&my_holder].0;
        // The package is exactly what chain state dictates: the request's message and the
        // commitments the signers announced. Take the nonce, persist, then sign.
        self.reqs
            .get_mut(&id)
            .expect("exists")
            .attempts
            .get_mut(&attempt)
            .expect("exists")
            .mine_signed = true;
        let nonces = self.nonce_store(scheme, key.origin)?.take(counter)?;
        let Some(nonces) = nonces else {
            return Ok(()); // already used in an earlier life of this node: this attempt is lost
        };
        self.stats.nonces_used += 1;
        let share_bytes = match frost::sign_share(share, &pkg, &nonces) {
            Ok(s) => s,
            Err(e) => return Err(format!("signing request {id}: {e:?}")),
        };
        let digest = xsign::share_digest(
            self.chain_id,
            scheme,
            key.origin,
            id,
            attempt,
            &hash,
            &share_bytes,
        );
        let sig = self.key.sign(&digest).to_bytes();
        let rec = ShareRec {
            package: hash,
            share: share_bytes.clone(),
            sig,
        };
        self.reqs
            .get_mut(&id)
            .expect("exists")
            .attempts
            .get_mut(&attempt)
            .expect("exists")
            .shares
            .insert(my_holder, rec);
        self.outbox.push((
            None,
            XMsg::Share {
                scheme,
                origin: key.origin,
                request: id,
                attempt,
                holder: my_holder as u32,
                package: hash,
                share: share_bytes,
                sig,
            },
        ));
        Ok(())
    }

    fn try_aggregate(&mut self, id: u64, key: &XKey, now: Instant, ops: &mut Vec<CrossOp>) {
        let rs = &self.reqs[&id];
        let attempt = rs.attempt;
        let at = &rs.attempts[&attempt];
        let Some((pkg, hash)) = at.package.clone() else {
            return;
        };
        let mut shares = BTreeMap::new();
        for s in &at.signers {
            match at.shares.get(s) {
                Some(r) if r.package == hash => {
                    shares.insert(*s, r.share.clone());
                }
                _ => return, // not all shares in yet (or one is for another package)
            }
        }
        let message = rs.req.message.clone();
        let scheme = rs.req.scheme;
        match frost::aggregate(&key.group, &pkg, &shares) {
            Ok(sig) if frost::verify(&key.group, &message, &sig) => {
                self.stats.signatures_made += 1;
                self.finish(id, sig, now);
            }
            Ok(_) | Err(frost::AggregateError::Invalid) | Err(frost::AggregateError::Malformed) => {
                self.abandon(id, now);
            }
            Err(frost::AggregateError::NotEnough) => {}
            Err(frost::AggregateError::Culprits(c)) => {
                for holder in c {
                    let Some(rec) = self.reqs[&id].attempts[&attempt]
                        .shares
                        .get(&holder)
                        .cloned()
                    else {
                        continue;
                    };
                    let ev = EvidenceOp {
                        request: id,
                        attempt,
                        origin: key.origin,
                        holder: holder as u32,
                        proof: Proof::BadShare {
                            package: pkg.encode(),
                            share: rec.share,
                            sig: rec.sig,
                        },
                    };
                    self.push_evidence(ev, ops);
                    self.reqs
                        .get_mut(&id)
                        .expect("exists")
                        .blamed
                        .insert(key.holders[holder]);
                }
                let _ = scheme;
                self.abandon(id, now);
            }
        }
    }

    fn push_evidence(&mut self, ev: EvidenceOp, ops: &mut Vec<CrossOp>) {
        let mut tag = ev.request.to_le_bytes().to_vec();
        tag.extend_from_slice(&ev.attempt.to_le_bytes());
        tag.extend_from_slice(&ev.holder.to_le_bytes());
        tag.push(match ev.proof {
            Proof::BadShare { .. } => 0,
            Proof::Equivocation { .. } => 1,
        });
        let id = h(&[b"btb.node.evidence", &tag]);
        if self.sent.insert(id, 0).is_none() {
            self.stats.evidence_sent += 1;
            ops.push(CrossOp::Evidence(ev));
        }
    }

    /// Give up on the current attempt and start the next one.
    fn abandon(&mut self, id: u64, now: Instant) {
        if let Some(rs) = self.reqs.get_mut(&id) {
            rs.attempt = (rs.attempt + 1).min(xsign::MAX_ATTEMPT);
            rs.attempt_started = now;
            self.stats.attempts_abandoned += 1;
        }
    }

    fn finish(&mut self, id: u64, sig: Vec<u8>, now: Instant) {
        if let Some(rs) = self.reqs.get_mut(&id) {
            if rs.signature.is_none() {
                rs.signature = Some(sig.clone());
                rs.sig_at = Some(now);
                self.outbox.push((
                    None,
                    XMsg::Done {
                        request: id,
                        signature: sig,
                    },
                ));
            }
        }
    }

    fn maybe_submit(
        &mut self,
        id: u64,
        key: &XKey,
        sig: Vec<u8>,
        now: Instant,
        ops: &mut Vec<CrossOp>,
    ) {
        let Some(rs) = self.reqs.get_mut(&id) else {
            return;
        };
        let Some(at) = rs.sig_at else { return };
        // Only holders submit, in the order of a hash rank, one stagger apart.
        let Some(me) = key.holder_index(&self.me) else {
            return;
        };
        let mut ranked: Vec<([u8; 32], usize)> = (0..key.holders.len())
            .map(|i| {
                (
                    h(&[
                        b"btb.node.submit",
                        &id.to_le_bytes(),
                        &(i as u32).to_le_bytes(),
                    ]),
                    i,
                )
            })
            .collect();
        ranked.sort();
        let rank = ranked.iter().position(|(_, i)| *i == me).unwrap_or(0) as u32;
        let due = at + self.params.submit_stagger * rank;
        let again = rs.submitted.is_none_or(|t| {
            now.duration_since(t) >= self.params.attempt_timeout.max(Duration::from_secs(2))
        });
        if now >= due && again {
            rs.submitted = Some(now);
            ops.push(CrossOp::Signature {
                request: id,
                signature: sig,
            });
        }
    }

    // -------------------------------------------------------------- messages

    /// A message from the validator whose key is `from` (`None`: from ourselves in tests).
    pub fn on_msg(&mut self, from: [u8; 32], msg: XMsg, view: &View, now: Instant) -> Vec<CrossOp> {
        let mut ops = Vec::new();
        self.handle(Some(from), msg, view, now, &mut ops);
        ops
    }

    fn handle(
        &mut self,
        from: Option<[u8; 32]>,
        msg: XMsg,
        view: &View,
        now: Instant,
        ops: &mut Vec<CrossOp>,
    ) {
        let Some(from) = from else { return };
        match msg {
            XMsg::Pool {
                scheme,
                origin,
                holder,
                first,
                commitments,
            } => {
                let Some((key, _)) = view.keys.get(&scheme) else {
                    return;
                };
                if key.origin != origin || key.holders.get(holder as usize) != Some(&from) {
                    return;
                }
                let map = self.commits.entry((scheme, origin, holder)).or_default();
                for (i, c) in commitments.into_iter().enumerate() {
                    if map.len() >= 4 * self.params.pool_target.max(64) {
                        break;
                    }
                    if frost::valid_commitments(scheme, &c) {
                        map.entry(first + i as u64).or_insert(c);
                    }
                }
            }
            XMsg::PoolWant { scheme, origin } => {
                let Some((key, Some(share))) = view.keys.get(&scheme) else {
                    return;
                };
                if key.origin != origin || !key.holders.contains(&from) {
                    return;
                }
                let holder = share.index as u32;
                let Some(map) = self.commits.get(&(scheme, origin, holder)) else {
                    return;
                };
                // our live commitments only: spent ones are useless to anyone
                let live: Vec<(u64, Vec<u8>)> = match self.nonces.get(&(scheme, origin)) {
                    Some(st) => map
                        .iter()
                        .filter(|(c, _)| st.commitment(**c).is_some())
                        .map(|(c, v)| (*c, v.clone()))
                        .collect(),
                    None => Vec::new(),
                };
                let mut i = 0;
                while i < live.len() {
                    let mut j = i + 1;
                    while j < live.len() && j - i < POOL_CHUNK && live[j].0 == live[j - 1].0 + 1 {
                        j += 1;
                    }
                    self.outbox.push((
                        Some(from),
                        XMsg::Pool {
                            scheme,
                            origin,
                            holder,
                            first: live[i].0,
                            commitments: live[i..j].iter().map(|(_, c)| c.clone()).collect(),
                        },
                    ));
                    i = j;
                }
            }
            XMsg::Use {
                scheme,
                origin,
                request,
                attempt,
                holder,
                counter,
                sig,
            } => self.on_use(
                from, scheme, origin, request, attempt, holder, counter, sig, view, now, ops,
            ),
            XMsg::Share {
                scheme,
                origin,
                request,
                attempt,
                holder,
                package,
                share,
                sig,
            } => self.on_share(
                from, scheme, origin, request, attempt, holder, package, share, sig, view, now,
            ),
            XMsg::Done { request, signature } => {
                let Some(rs) = self.reqs.get(&request) else {
                    return;
                };
                if rs.signature.is_some() {
                    return;
                }
                let Some((key, _)) = view.keys.get(&rs.req.scheme) else {
                    return;
                };
                if frost::verify(&key.group, &rs.req.message, &signature) {
                    self.finish(request, signature, now);
                    // no need to re-broadcast what we just heard
                    self.outbox.retain(
                        |(_, m)| !matches!(m, XMsg::Done { request: r, .. } if *r == request),
                    );
                }
            }
            XMsg::Want { request } => {
                if let Some(sig) = self.reqs.get(&request).and_then(|r| r.signature.clone()) {
                    self.outbox.push((
                        Some(from),
                        XMsg::Done {
                            request,
                            signature: sig,
                        },
                    ));
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_use(
        &mut self,
        from: [u8; 32],
        scheme: Scheme,
        origin: u64,
        request: u64,
        attempt: u32,
        holder: u32,
        counter: u64,
        sig: [u8; 64],
        view: &View,
        now: Instant,
        ops: &mut Vec<CrossOp>,
    ) {
        let Some((key, _)) = view.keys.get(&scheme) else {
            return;
        };
        if key.origin != origin || key.holders.get(holder as usize) != Some(&from) {
            return;
        }
        let Some(rs) = self.reqs.get(&request) else {
            if self.early.len() < MAX_EARLY {
                self.early.push((
                    Some(from),
                    XMsg::Use {
                        scheme,
                        origin,
                        request,
                        attempt,
                        holder,
                        counter,
                        sig,
                    },
                ));
            }
            return;
        };
        if rs.req.scheme != scheme || rs.signature.is_some() {
            return;
        }
        let digest = xsign::use_digest(self.chain_id, scheme, origin, request, attempt, counter);
        if !verify_ed(&from, &digest, &sig) {
            return;
        }
        let cur = rs.attempt;
        if attempt < cur.saturating_sub(MAX_JUMP) || attempt > cur + MAX_JUMP {
            return;
        }
        let mut excluded = view.excluded.get(&scheme).cloned().unwrap_or_default();
        excluded.extend(rs.blamed.iter().copied());
        let rs = self.reqs.get_mut(&request).expect("exists");
        if attempt > rs.attempt {
            rs.attempt = attempt; // follow the others
            rs.attempt_started = now;
        }
        let at = rs.attempts.entry(attempt).or_insert_with(|| AttemptState {
            signers: xsign::select_signers(key, scheme, &excluded, request, attempt),
            ..Default::default()
        });
        let h = holder as usize;
        if !at.signers.contains(&h) {
            return; // not part of this attempt's set
        }
        match at.uses.get(&h) {
            None => {
                at.uses.insert(h, (counter, sig));
            }
            Some((c, _)) if *c == counter => {}
            Some((c, s)) => {
                // two different announcements for one attempt: provable
                let ev = EvidenceOp {
                    request,
                    attempt,
                    origin,
                    holder,
                    proof: Proof::Equivocation {
                        counter_a: *c,
                        sig_a: *s,
                        counter_b: counter,
                        sig_b: sig,
                    },
                };
                rs.blamed.insert(from);
                self.push_evidence(ev, ops);
                self.abandon(request, now);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_share(
        &mut self,
        from: [u8; 32],
        scheme: Scheme,
        origin: u64,
        request: u64,
        attempt: u32,
        holder: u32,
        package: [u8; 32],
        share: Vec<u8>,
        sig: [u8; 64],
        view: &View,
        now: Instant,
    ) {
        let Some((key, _)) = view.keys.get(&scheme) else {
            return;
        };
        if key.origin != origin || key.holders.get(holder as usize) != Some(&from) {
            return;
        }
        let Some(rs) = self.reqs.get(&request) else {
            if self.early.len() < MAX_EARLY {
                self.early.push((
                    Some(from),
                    XMsg::Share {
                        scheme,
                        origin,
                        request,
                        attempt,
                        holder,
                        package,
                        share,
                        sig,
                    },
                ));
            }
            return;
        };
        if rs.req.scheme != scheme || rs.signature.is_some() {
            return;
        }
        let digest = xsign::share_digest(
            self.chain_id,
            scheme,
            origin,
            request,
            attempt,
            &package,
            &share,
        );
        if !verify_ed(&from, &digest, &sig) {
            return;
        }
        let cur = rs.attempt;
        if attempt < cur.saturating_sub(MAX_JUMP) || attempt > cur + MAX_JUMP {
            return;
        }
        let mut excluded = view.excluded.get(&scheme).cloned().unwrap_or_default();
        excluded.extend(rs.blamed.iter().copied());
        let rs = self.reqs.get_mut(&request).expect("exists");
        if attempt > rs.attempt {
            rs.attempt = attempt;
            rs.attempt_started = now;
        }
        let at = rs.attempts.entry(attempt).or_insert_with(|| AttemptState {
            signers: xsign::select_signers(key, scheme, &excluded, request, attempt),
            ..Default::default()
        });
        let h = holder as usize;
        if at.signers.contains(&h) {
            at.shares.entry(h).or_insert(ShareRec {
                package,
                share,
                sig,
            });
        }
    }

    // -------------------------------------------------------------- vault duties

    fn sent_recently(&mut self, id: [u8; 32], height: u64) -> bool {
        match self.sent.get(&id) {
            Some(at) if height < at + self.params.resend_blocks => true,
            _ => {
                self.sent.insert(id, height);
                if self.sent.len() > 8192 {
                    let first = *self.sent.keys().next().expect("non-empty");
                    self.sent.remove(&first);
                }
                false
            }
        }
    }

    fn vault_duties(
        &mut self,
        kv: &impl KvStore,
        height: u64,
        epoch: u64,
    ) -> Result<Vec<CrossOp>, String> {
        let Some(obs) = self.observer.clone() else {
            return Ok(Vec::new());
        };
        let mut ops = Vec::new();
        let me = self.me;
        let mut keys = BTreeMap::new();
        for scheme in Scheme::ALL {
            if let Some((_, k)) = xdkg::key_at(kv, scheme, epoch).map_err(|e| e.to_string())? {
                if let Ok(out) = k.group.output_key() {
                    keys.insert(scheme, out);
                }
            }
        }
        // deposits: vote for every observed deposit that is not credited and not voted yet
        let mut claims = Vec::new();
        for (scheme, vkey) in &keys {
            for c in obs.deposits(*scheme, vkey) {
                let id = c.id();
                if vault::is_credited(kv, &id).map_err(|e| e.to_string())?
                    || vault::claim_votes(kv, &id)
                        .map_err(|e| e.to_string())?
                        .contains(&me)
                    || self.sent_recently(id, height)
                {
                    continue;
                }
                claims.push(c);
            }
        }
        for chunk in claims.chunks(vault::MAX_CLAIMS) {
            self.stats.attestations_sent += chunk.len() as u64;
            ops.push(CrossOp::Attest {
                claims: chunk.to_vec(),
            });
        }
        let utxos = vault::utxos(kv, 4096).map_err(|e| e.to_string())?;
        // withdrawals: propose the unsigned transaction for burned ones
        let mut first_open = None;
        for w in vault::withdrawals_from(kv, self.first_open_withdrawal, 64)
            .map_err(|e| e.to_string())?
        {
            if w.status != vault::STATUS_BURNED {
                if first_open.is_none() {
                    self.first_open_withdrawal = w.id + 1;
                }
                continue;
            }
            first_open.get_or_insert(w.id);
            let Some(vkey) = keys.get(&w.asset) else {
                continue;
            };
            let Some(p) = obs.prepare_with(&w, vkey, &utxos) else {
                continue;
            };
            let proposal = vault::proposal_id(w.id, &p.digest, &p.sighashes, &p.plan);
            if vault::prepare_votes(kv, w.id, &proposal)
                .map_err(|e| e.to_string())?
                .contains(&me)
                || self.sent_recently(proposal, height)
            {
                continue;
            }
            self.stats.proposals_sent += 1;
            ops.push(CrossOp::Prepare {
                withdrawal: w.id,
                digest: p.digest,
                sighashes: p.sighashes,
                plan: p.plan,
            });
        }
        // signed withdrawals: assemble the external transaction and broadcast it (the observer
        // queues the work and tracks it until it is final)
        let mut all_final = true;
        for w in
            vault::withdrawals_from(kv, self.first_unfinished, 64).map_err(|e| e.to_string())?
        {
            if w.status != vault::STATUS_SIGNED {
                all_final = false;
                continue;
            }
            let mut sigs = Vec::new();
            for rid in &w.requests {
                match xsign::get(kv, *rid)
                    .map_err(|e| e.to_string())?
                    .and_then(|r| r.signature)
                {
                    Some(s) => sigs.push(s),
                    None => break,
                }
            }
            if sigs.len() != w.requests.len() {
                all_final = false;
                continue;
            }
            let Some(vkey) = keys.get(&w.asset) else {
                all_final = false;
                continue;
            };
            match obs.execution(&w) {
                vault::Execution::Confirmed | vault::Execution::Failed => {
                    if all_final {
                        self.first_unfinished = w.id + 1;
                    }
                }
                _ => {
                    all_final = false;
                    obs.broadcast(&w, vkey, &sigs);
                }
            }
        }
        Ok(ops)
    }

    // -------------------------------------------------------------- views for RPC

    /// The signature for `request` that this node has (not necessarily on chain yet).
    pub fn local_signature(&self, request: u64) -> Option<&[u8]> {
        self.reqs.get(&request)?.signature.as_deref()
    }

    pub fn in_flight(&self) -> usize {
        self.reqs.len()
    }

    /// Current attempt of a request (for tests and status).
    pub fn attempt_of(&self, request: u64) -> Option<u32> {
        self.reqs.get(&request).map(|r| r.attempt)
    }

    pub fn pool_size(&self, scheme: Scheme, origin: u64) -> usize {
        self.nonces
            .get(&(scheme, origin))
            .map_or(0, NonceStore::live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use btb_oracle::dkg::Params;
    use btb_oracle::frost::{Dealing, Mode, Transcript};

    pub(crate) fn keygen(scheme: Scheme, n: usize) -> Vec<KeyShare> {
        let params = Params::standard(n);
        let mut t = Transcript::new(scheme, Mode::Fresh, params).unwrap();
        let deals: Vec<Dealing> = (0..n)
            .map(|d| Dealing::fresh(scheme, d, params, &[d as u8 + 1; 32]).unwrap())
            .collect();
        for d in &deals {
            t.add_commitments(d.dealer, d.commitments.clone()).unwrap();
        }
        let out = t.finalize().unwrap();
        (0..n)
            .map(|j| {
                let got = (0..n).map(|d| (d, deals[d].share_for(j))).collect();
                out.derive_share(j, &got).unwrap()
            })
            .collect()
    }

    fn ckey(i: usize) -> SigningKey {
        SigningKey::from_bytes(&[200 + i as u8; 32])
    }

    fn params() -> CrossParams {
        CrossParams {
            pool_target: 8,
            attempt_timeout: Duration::from_millis(1_000),
            submit_stagger: Duration::from_millis(100),
            resend_blocks: 2,
        }
    }

    type Tamper = Box<dyn Fn(&XMsg) -> Vec<XMsg>>;

    struct Bus {
        nodes: Vec<CrossNode>,
        key: XKey,
        online: Vec<bool>,
        tamper: BTreeMap<usize, Tamper>,
        base: Instant,
        t: Duration,
        pending: Vec<SignRequest>,
        excluded: BTreeSet<[u8; 32]>,
        /// operations nodes asked to submit: (node, op)
        submitted: Vec<(usize, CrossOp)>,
    }

    impl Bus {
        fn new(scheme: Scheme, dirs: Vec<Option<PathBuf>>) -> Bus {
            let shares = keygen(scheme, 4);
            let key = XKey {
                origin: 0,
                group: shares[0].group.clone(),
                holders: (0..4).map(|i| ckey(i).verifying_key().to_bytes()).collect(),
            };
            let mut nodes = Vec::new();
            for (i, share) in shares.iter().enumerate() {
                let mut n =
                    CrossNode::new(7, ckey(i), dirs[i].clone(), None, params(), None).unwrap();
                n.agent.insert_share(scheme, 0, share.clone());
                n.ensure_pool(scheme, 0, share, true).unwrap();
                nodes.push(n);
            }
            Bus {
                nodes,
                key,
                online: vec![true; 4],
                tamper: BTreeMap::new(),
                base: Instant::now(),
                t: Duration::ZERO,
                pending: Vec::new(),
                excluded: BTreeSet::new(),
                submitted: Vec::new(),
            }
        }

        fn request(&mut self, id: u64, scheme: Scheme, message: [u8; 32]) {
            self.pending.push(SignRequest {
                id,
                scheme,
                message: message.to_vec(),
                height: 1,
                tag: btb_chain::xsign::RequestTag::Raw,
                signature: None,
                done_height: 0,
            });
        }

        fn view(&self, i: usize, scheme: Scheme, shares: &[KeyShare]) -> View {
            let mut keys = BTreeMap::new();
            keys.insert(scheme, (self.key.clone(), Some(shares[i].clone())));
            let mut excluded = BTreeMap::new();
            excluded.insert(scheme, self.excluded.clone());
            View {
                height: 10,
                epoch: 0,
                keys,
                excluded,
                pending: self.pending.clone(),
            }
        }

        /// One round: every online node ticks, then messages flow until quiet.
        fn round(&mut self, scheme: Scheme, shares: &[KeyShare], step: Duration) {
            self.t += step;
            let now = self.base + self.t;
            for i in 0..4 {
                if !self.online[i] {
                    continue;
                }
                let view = self.view(i, scheme, shares);
                let ops = self.nodes[i].tick(&view, now);
                self.submitted.extend(ops.into_iter().map(|o| (i, o)));
            }
            for _ in 0..20 {
                let mut any = false;
                for i in 0..4 {
                    let out: Vec<_> = self.nodes[i].outbox.drain(..).collect();
                    for (to, m) in out {
                        any = true;
                        if !self.online[i] {
                            continue;
                        }
                        let msgs = match self.tamper.get(&i) {
                            Some(f) => f(&m),
                            None => vec![m],
                        };
                        for m in msgs {
                            for j in 0..4 {
                                if j == i
                                    || !self.online[j]
                                    || to.is_some_and(|t| t != self.key.holders[j])
                                {
                                    continue;
                                }
                                let view = self.view(j, scheme, shares);
                                let ops = self.nodes[j].on_msg(
                                    self.key.holders[i],
                                    m.clone(),
                                    &view,
                                    now,
                                );
                                self.submitted.extend(ops.into_iter().map(|o| (j, o)));
                            }
                        }
                    }
                }
                if !any {
                    break;
                }
            }
        }

        fn run_until_signed(
            &mut self,
            scheme: Scheme,
            shares: &[KeyShare],
            id: u64,
            max_rounds: usize,
        ) -> bool {
            for _ in 0..max_rounds {
                self.round(scheme, shares, Duration::from_millis(50));
                if (0..4).any(|i| self.online[i] && self.nodes[i].local_signature(id).is_some()) {
                    // let the Done gossip settle
                    self.round(scheme, shares, Duration::from_millis(50));
                    return true;
                }
            }
            false
        }
    }

    #[test]
    fn xmsg_codec_is_total_and_round_trips() {
        let msgs = vec![
            XMsg::Pool {
                scheme: Scheme::Taproot,
                origin: 3,
                holder: 1,
                first: 9,
                commitments: vec![vec![1; 70], vec![2; 70]],
            },
            XMsg::PoolWant {
                scheme: Scheme::Ed25519,
                origin: 1,
            },
            XMsg::Use {
                scheme: Scheme::Ed25519,
                origin: 1,
                request: 5,
                attempt: 2,
                holder: 3,
                counter: 7,
                sig: [9; 64],
            },
            XMsg::Share {
                scheme: Scheme::Taproot,
                origin: 1,
                request: 5,
                attempt: 2,
                holder: 3,
                package: [4; 32],
                share: vec![5; 33],
                sig: [6; 64],
            },
            XMsg::Done {
                request: 5,
                signature: vec![7; 64],
            },
            XMsg::Want { request: 5 },
        ];
        for m in &msgs {
            let b = m.encode();
            assert!(b.len() < 2000, "fits an oracle frame");
            assert_eq!(XMsg::decode(&b).as_ref(), Some(m));
            for cut in 0..b.len() {
                assert!(XMsg::decode(&b[..cut]).is_none(), "{m:?} cut {cut}");
            }
            let mut longer = b.clone();
            longer.push(0);
            assert!(XMsg::decode(&longer).is_none());
        }
        assert!(XMsg::decode(&[]).is_none());
        assert!(XMsg::decode(&[99, 1, 2]).is_none());
        // too many commitments in a pool message
        let big = XMsg::Pool {
            scheme: Scheme::Ed25519,
            origin: 0,
            holder: 0,
            first: 0,
            commitments: vec![vec![1; 8]; POOL_CHUNK + 1],
        };
        assert!(XMsg::decode(&big.encode()).is_none());
    }

    #[test]
    fn four_nodes_sign_a_request_for_both_schemes_with_a_chain_valid_signature() {
        for scheme in Scheme::ALL {
            let shares = keygen(scheme, 4);
            let mut bus = Bus::new(scheme, vec![None; 4]);
            // make Bus use the same shares everywhere
            bus.key.group = shares[0].group.clone();
            for (i, n) in bus.nodes.iter_mut().enumerate() {
                n.agent.insert_share(scheme, 0, shares[i].clone());
                n.ensure_pool(scheme, 0, &shares[i], true).unwrap();
            }
            bus.request(0, scheme, [0x11; 32]);
            assert!(bus.run_until_signed(scheme, &shares, 0, 40));
            let sig = bus
                .nodes
                .iter()
                .find_map(|n| n.local_signature(0))
                .unwrap()
                .to_vec();
            assert!(
                frost::verify(&bus.key.group, &[0x11; 32], &sig),
                "{scheme:?}"
            );
            // every node learned it (Done gossip), attempt 0 was enough
            assert!(bus
                .nodes
                .iter()
                .all(|n| n.local_signature(0) == Some(sig.as_slice())));
            assert!(bus.nodes.iter().all(|n| n.attempt_of(0) == Some(0)));
            // exactly t nonces were spent
            let spent: u64 = bus.nodes.iter().map(|n| n.stats.nonces_used).sum();
            assert_eq!(spent, 3);
            // some node submits the signature on chain, and the submission is staggered
            for _ in 0..10 {
                bus.round(scheme, &shares, Duration::from_millis(100));
            }
            let subs: Vec<_> = bus
                .submitted
                .iter()
                .filter(|(_, o)| matches!(o, CrossOp::Signature { .. }))
                .collect();
            assert!(!subs.is_empty());
        }
    }

    #[test]
    fn an_offline_signer_is_rotated_away_by_the_next_attempts() {
        let scheme = Scheme::Ed25519;
        let shares = keygen(scheme, 4);
        let mut bus = Bus::new(scheme, vec![None; 4]);
        bus.key.group = shares[0].group.clone();
        for (i, n) in bus.nodes.iter_mut().enumerate() {
            n.agent.insert_share(scheme, 0, shares[i].clone());
            n.ensure_pool(scheme, 0, &shares[i], true).unwrap();
        }
        bus.online[0] = false;
        // pick a request id whose attempt-0 set includes the offline holder, to force a rotation
        let none = BTreeSet::new();
        let id = (0..64u64)
            .find(|id| xsign::select_signers(&bus.key, scheme, &none, *id, 0).contains(&0))
            .unwrap();
        bus.request(id, scheme, [0x22; 32]);
        assert!(
            bus.run_until_signed(scheme, &shares, id, 400),
            "must eventually sign with 3 of 4"
        );
        let node = &bus.nodes[1];
        assert!(
            node.attempt_of(id).unwrap() >= 1,
            "attempt 0 could not finish"
        );
        let sig = node.local_signature(id).unwrap();
        assert!(frost::verify(&bus.key.group, &[0x22; 32], sig));
    }

    #[test]
    fn fewer_than_threshold_online_never_produces_a_signature() {
        let scheme = Scheme::Taproot;
        let shares = keygen(scheme, 4);
        let mut bus = Bus::new(scheme, vec![None; 4]);
        bus.key.group = shares[0].group.clone();
        for (i, n) in bus.nodes.iter_mut().enumerate() {
            n.agent.insert_share(scheme, 0, shares[i].clone());
            n.ensure_pool(scheme, 0, &shares[i], true).unwrap();
        }
        bus.online[2] = false;
        bus.online[3] = false;
        bus.request(0, scheme, [0x33; 32]);
        assert!(!bus.run_until_signed(scheme, &shares, 0, 300));
        assert!(bus.nodes[0].stats.signatures_made == 0 && bus.nodes[1].stats.signatures_made == 0);
        // attempts keep rotating, but no nonce is ever spent on a package that cannot complete
        // more than once per announcement
        assert!(bus.nodes[0].stats.nonces_used <= bus.nodes[0].stats.attempts_abandoned + 1);
    }

    #[test]
    fn a_signer_that_sends_a_bad_share_is_blamed_with_evidence_and_the_retry_succeeds() {
        let scheme = Scheme::Taproot;
        let shares = keygen(scheme, 4);
        let mut bus = Bus::new(scheme, vec![None; 4]);
        bus.key.group = shares[0].group.clone();
        for (i, n) in bus.nodes.iter_mut().enumerate() {
            n.agent.insert_share(scheme, 0, shares[i].clone());
            n.ensure_pool(scheme, 0, &shares[i], true).unwrap();
        }
        // holder 1 corrupts its share, but signs the corrupted share like a real faulty node
        let evil = 1usize;
        let ek = ckey(evil);
        let chain = 7u64;
        bus.tamper.insert(
            evil,
            Box::new(move |m| match m {
                XMsg::Share {
                    scheme,
                    origin,
                    request,
                    attempt,
                    holder,
                    package,
                    share,
                    ..
                } => {
                    let mut bad = share.clone();
                    let last = bad.len() - 1;
                    bad[last] ^= 1;
                    let d = xsign::share_digest(
                        chain, *scheme, *origin, *request, *attempt, package, &bad,
                    );
                    vec![XMsg::Share {
                        scheme: *scheme,
                        origin: *origin,
                        request: *request,
                        attempt: *attempt,
                        holder: *holder,
                        package: *package,
                        share: bad,
                        sig: ek.sign(&d).to_bytes(),
                    }]
                }
                other => vec![other.clone()],
            }),
        );
        let none = BTreeSet::new();
        let id = (0..64u64)
            .find(|id| xsign::select_signers(&bus.key, scheme, &none, *id, 0).contains(&evil))
            .unwrap();
        bus.request(id, scheme, [0x44; 32]);
        assert!(bus.run_until_signed(scheme, &shares, id, 200));
        let sig = bus.nodes[0].local_signature(id).unwrap();
        assert!(frost::verify(&bus.key.group, &[0x44; 32], sig));
        // honest nodes filed provable evidence against holder 1
        let evidence: Vec<_> = bus
            .submitted
            .iter()
            .filter_map(|(n, o)| match o {
                CrossOp::Evidence(e) => Some((*n, e.clone())),
                _ => None,
            })
            .collect();
        assert!(!evidence.is_empty(), "evidence was produced");
        assert!(evidence.iter().all(|(n, e)| *n != evil
            && e.holder == evil as u32
            && matches!(e.proof, Proof::BadShare { .. })));
        // and the evidence verifies the way the chain checks it
        let (_, e) = &evidence[0];
        let Proof::BadShare {
            package,
            share,
            sig,
        } = &e.proof
        else {
            unreachable!()
        };
        let pkg = SigningPackage::decode(package).unwrap();
        assert!(!frost::verify_signature_share(
            &bus.key.group,
            &pkg,
            evil,
            share
        ));
        let d = xsign::share_digest(
            chain,
            scheme,
            0,
            id,
            e.attempt,
            &xsign::package_hash(package),
            share,
        );
        assert!(verify_ed(&bus.key.holders[evil], &d, sig));
        // the final attempt did not use the culprit
        let n0 = &bus.nodes[0];
        assert!(n0.attempt_of(id).unwrap() >= 1);
    }

    #[test]
    fn announcing_two_different_nonces_for_one_attempt_is_provable() {
        let scheme = Scheme::Ed25519;
        let shares = keygen(scheme, 4);
        let mut bus = Bus::new(scheme, vec![None; 4]);
        bus.key.group = shares[0].group.clone();
        for (i, n) in bus.nodes.iter_mut().enumerate() {
            n.agent.insert_share(scheme, 0, shares[i].clone());
            n.ensure_pool(scheme, 0, &shares[i], true).unwrap();
        }
        let evil = 2usize;
        let ek = ckey(evil);
        // holder 2 tells nodes 0 and 1 one counter, and node 3 another
        let none = BTreeSet::new();
        let id = (0..64u64)
            .find(|id| xsign::select_signers(&bus.key, scheme, &none, *id, 0).contains(&evil))
            .unwrap();
        bus.request(id, scheme, [0x55; 32]);
        bus.online[evil] = false; // its real announcements stay out of the way
        bus.round(scheme, &shares, Duration::from_millis(10));
        let now = bus.base + bus.t;
        let view = bus.view(0, scheme, &shares);
        let mk = |counter: u64| {
            let d = xsign::use_digest(7, scheme, 0, id, 0, counter);
            XMsg::Use {
                scheme,
                origin: 0,
                request: id,
                attempt: 0,
                holder: evil as u32,
                counter,
                sig: ek.sign(&d).to_bytes(),
            }
        };
        let k = bus.key.holders[evil];
        let first = bus.nodes[0].on_msg(k, mk(5), &view, now);
        assert!(first.is_empty());
        let second = bus.nodes[0].on_msg(k, mk(6), &view, now);
        assert!(second.iter().any(|o| matches!(
            o,
            CrossOp::Evidence(EvidenceOp { proof: Proof::Equivocation { .. }, holder, .. }) if *holder == evil as u32
        )));
        // the same evidence is not filed twice
        let third = bus.nodes[0].on_msg(k, mk(6), &view, now);
        assert!(third.is_empty());
        // forged, misattributed or out-of-set messages do nothing
        let forged = XMsg::Use {
            scheme,
            origin: 0,
            request: id,
            attempt: 0,
            holder: evil as u32,
            counter: 8,
            sig: [1; 64],
        };
        assert!(bus.nodes[1].on_msg(k, forged, &view, now).is_empty());
        let spoofed = mk(9);
        assert!(bus.nodes[1]
            .on_msg(bus.key.holders[0], spoofed, &view, now)
            .is_empty());
    }

    #[test]
    fn a_restarted_node_cannot_reuse_a_nonce_and_repeats_its_announcement() {
        let scheme = Scheme::Ed25519;
        let shares = keygen(scheme, 4);
        let dir = tempfile::tempdir().unwrap();
        let dirs: Vec<Option<PathBuf>> = (0..4)
            .map(|i| Some(dir.path().join(format!("n{i}"))))
            .collect();
        let mut bus = Bus::new(scheme, dirs.clone());
        bus.key.group = shares[0].group.clone();
        for (i, n) in bus.nodes.iter_mut().enumerate() {
            n.agent.insert_share(scheme, 0, shares[i].clone());
            n.ensure_pool(scheme, 0, &shares[i], true).unwrap();
        }
        let none = BTreeSet::new();
        let id = (0..64u64)
            .find(|id| xsign::select_signers(&bus.key, scheme, &none, *id, 0).contains(&0))
            .unwrap();
        bus.request(id, scheme, [0x66; 32]);
        assert!(bus.run_until_signed(scheme, &shares, id, 40));
        let used = bus.nodes[0].stats.nonces_used;
        assert_eq!(used, 1, "node 0 signed once");
        // the pool on disk no longer has the spent nonce
        let path = dirs[0].as_ref().unwrap().join("nonces-ed25519-0.bin");
        let st = NonceStore::open(Some(path.clone())).unwrap();
        let counter = st.announced(id, 0).expect("announcement was persisted");
        assert!(
            st.commitment(counter).is_none(),
            "spent nonce is gone from disk"
        );
        let mut st = st;
        assert_eq!(st.take(counter).unwrap(), None, "and cannot be taken again");
        assert_eq!(st.live(), 7);

        // Restart node 0 from disk and ask it to sign the same attempt again with a package it
        // is willing to build (the same one): it repeats its announcement but has no nonce to
        // sign with, so it produces no second share.
        let mut again = CrossNode::new(7, ckey(0), dirs[0].clone(), None, params(), None).unwrap();
        again.agent.insert_share(scheme, 0, shares[0].clone());
        // learn the others' commitments again
        for j in 1..4 {
            let view = bus.view(0, scheme, &shares);
            for (_, m) in bus.nodes[j].outbox.clone() {
                let _ = again.on_msg(bus.key.holders[j], m, &view, bus.base);
            }
            let want = XMsg::PoolWant { scheme, origin: 0 };
            let vj = bus.view(j, scheme, &shares);
            let _ = bus.nodes[j].on_msg(bus.key.holders[0], want, &vj, bus.base);
            let out: Vec<_> = bus.nodes[j].outbox.drain(..).collect();
            for (to, m) in out {
                if to == Some(bus.key.holders[0]) {
                    let _ = again.on_msg(
                        bus.key.holders[j],
                        m,
                        &bus.view(0, scheme, &shares),
                        bus.base,
                    );
                }
            }
        }
        let mut view = bus.view(0, scheme, &shares);
        view.pending = bus.pending.clone();
        let now = bus.base + Duration::from_secs(1);
        let _ = again.tick(&view, now);
        let announced: Vec<_> = again
            .outbox
            .iter()
            .filter_map(|(_, m)| match m {
                XMsg::Use {
                    counter,
                    request,
                    attempt,
                    ..
                } if *request == id && *attempt == 0 => Some(*counter),
                _ => None,
            })
            .collect();
        assert_eq!(
            announced,
            vec![counter],
            "the same announcement, not a new one"
        );
        assert_eq!(again.stats.nonces_used, 0, "no nonce was spent again");
        assert!(!again
            .outbox
            .iter()
            .any(|(_, m)| matches!(m, XMsg::Share { attempt: 0, request, .. } if *request == id)));
        // new counters never collide with spent ones
        let after = NonceStore::open(Some(path)).unwrap();
        assert!(after.commitment(counter).is_none());
    }

    #[test]
    fn nonce_store_survives_a_crash_between_taking_and_signing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("n.bin");
        let shares = keygen(Scheme::Taproot, 4);
        let mut st = NonceStore::open(Some(path.clone())).unwrap();
        st.top_up(&shares[0], 4).unwrap();
        let c = st.free_counter().unwrap();
        st.announce(5, 0, c).unwrap();
        assert_ne!(
            st.free_counter(),
            Some(c),
            "an announced counter is not handed out again"
        );
        let n = st.take(c).unwrap().unwrap();
        assert!(!n.is_empty());
        // "crash": reopen from disk before anything was signed
        drop(st);
        let mut st = NonceStore::open(Some(path.clone())).unwrap();
        assert_eq!(st.take(c).unwrap(), None);
        assert_eq!(st.live(), 3);
        // top-ups continue the counter sequence
        let fresh = st.top_up(&shares[0], 5).unwrap();
        assert_eq!(
            fresh.iter().map(|(c, _)| *c).collect::<Vec<_>>(),
            vec![4, 5]
        );
        // file permissions are private
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0);
        }
        // damage is an error, not a silent empty pool
        std::fs::write(&path, b"junk").unwrap();
        assert!(NonceStore::open(Some(path)).is_err());
    }

    #[test]
    fn shares_are_stored_privately_and_encrypted_with_a_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let s = &keygen(Scheme::Ed25519, 4)[2];
        let plain = XShareStore::new(Some(dir.path().join("a")), None);
        plain.save(Scheme::Ed25519, 3, s).unwrap();
        let loaded = plain.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!((loaded[0].0, loaded[0].1), (Scheme::Ed25519, 3));
        assert_eq!(&loaded[0].2, s);
        let enc = XShareStore::new(Some(dir.path().join("b")), Some("correct horse".into()));
        enc.save(Scheme::Taproot, 4, &keygen(Scheme::Taproot, 4)[0])
            .unwrap();
        let raw = std::fs::read(dir.path().join("b/xshare-taproot-4.bin")).unwrap();
        let sh = &keygen(Scheme::Taproot, 4)[0];
        assert!(!raw.windows(32).any(|w| w == sh.secret));
        assert_eq!(enc.load_all().unwrap().len(), 1);
        assert!(
            XShareStore::new(Some(dir.path().join("b")), Some("wrong".into()))
                .load_all()
                .is_err()
        );
        assert!(XShareStore::new(Some(dir.path().join("b")), None)
            .load_all()
            .is_err());
        enc.remove(Scheme::Taproot, 4);
        assert!(enc.load_all().unwrap().is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("a/xshare-ed25519-3.bin"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
    }
}
