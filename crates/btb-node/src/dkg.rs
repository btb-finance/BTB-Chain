//! The validator's side of the on-chain beacon DKG: the share store and the driver-facing agent.
//!
//! The protocol itself (`btb_chain::dkg`, `btb_chain::dkg_agent`) is deterministic and reads
//! only committed chain state. This module adds what a process needs around it:
//!
//! * **Key material at rest.** A finished share is written to `<dir>/share-<origin>.json` with
//!   file mode `0600` (unix). With `BTB_KEY_PASSPHRASE` set the secret is encrypted
//!   (ChaCha20-Poly1305 under a key derived from the passphrase with 200,000 rounds of salted
//!   SHA-256; not memory-hard, so use a long passphrase); the node then needs the passphrase
//!   at every start. Shares need not be stored at all: a node that lost them re-derives them
//!   from the deals on chain (as long as the DKG board still holds the session) with its
//!   consensus key, so the file is a cache that saves a replay.
//! * **Erasure.** Once a newer key has been in use for a whole epoch, the shares of older key
//!   generations are deleted (resharing leaves old shares mathematically valid for the same
//!   group key, so honest holders must not keep them).
//! * **Sending.** [`DkgNode::step`] returns the operations the driver signs with the consensus
//!   key and submits.

use btb_chain::dkg::{self, DkgOp};
use btb_chain::dkg_agent::Agent;
use btb_oracle::KeyShare;
use btb_runtime::KvStore;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use ed25519_dalek::SigningKey;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const KDF_ROUNDS: u32 = 200_000;
const MAGIC_ENC: &str = "btb.share.enc.v1";

/// Where finished shares are kept.
#[derive(Debug, Clone, Default)]
pub struct ShareStore {
    dir: Option<PathBuf>,
    passphrase: Option<String>,
}

pub(crate) fn kdf(pass: &str, salt: &[u8; 16]) -> [u8; 32] {
    let mut k: [u8; 32] = Sha256::new()
        .chain_update(b"btb.share.kdf.v1")
        .chain_update(salt)
        .chain_update(pass.as_bytes())
        .finalize()
        .into();
    for _ in 0..KDF_ROUNDS {
        k = Sha256::new()
            .chain_update(salt)
            .chain_update(k)
            .chain_update(pass.as_bytes())
            .finalize()
            .into();
    }
    k
}

impl ShareStore {
    pub fn new(dir: Option<PathBuf>, passphrase: Option<String>) -> ShareStore {
        ShareStore { dir, passphrase }
    }

    fn path(&self, origin: u64) -> Option<PathBuf> {
        self.dir
            .as_ref()
            .map(|d| d.join(format!("share-{origin}.json")))
    }

    /// Persist `share` (the share of the key generated for epoch `origin`).
    pub fn save(&self, origin: u64, share: &KeyShare) -> Result<(), String> {
        let (Some(dir), Some(path)) = (self.dir.as_ref(), self.path(origin)) else {
            return Ok(());
        };
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let raw = share.encode();
        let body = match &self.passphrase {
            None => json!({"origin": origin, "share": crate::hex::encode(&raw)}),
            Some(pass) => {
                use rand::RngCore;
                let mut salt = [0u8; 16];
                let mut nonce = [0u8; 12];
                rand::rngs::OsRng.fill_bytes(&mut salt);
                rand::rngs::OsRng.fill_bytes(&mut nonce);
                let key = kdf(pass, &salt);
                let ct = ChaCha20Poly1305::new((&key).into())
                    .encrypt(Nonce::from_slice(&nonce), raw.as_slice())
                    .map_err(|_| "share encryption failed".to_string())?;
                let mut blob = salt.to_vec();
                blob.extend_from_slice(&nonce);
                blob.extend_from_slice(&ct);
                json!({"origin": origin, "enc": MAGIC_ENC, "data": crate::hex::encode(&blob)})
            }
        };
        crate::config::write_private(&path, &serde_json::to_string_pretty(&body).expect("json"))
    }

    /// Every share on disk, `(origin, share)`. A file that cannot be read or decrypted is an
    /// error (silently ignoring it would hide a wrong passphrase).
    pub fn load_all(&self) -> Result<Vec<(u64, KeyShare)>, String> {
        let Some(dir) = self.dir.as_ref() else {
            return Ok(Vec::new());
        };
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(origin) = name
                .strip_prefix("share-")
                .and_then(|n| n.strip_suffix(".json"))
                .and_then(|n| n.parse::<u64>().ok())
            else {
                continue;
            };
            out.push((origin, self.load_file(&entry.path())?));
        }
        out.sort_by_key(|(o, _)| *o);
        Ok(out)
    }

    fn load_file(&self, path: &Path) -> Result<KeyShare, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let v: Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let raw = if v.get("enc").and_then(Value::as_str) == Some(MAGIC_ENC) {
            let pass = self.passphrase.as_ref().ok_or_else(|| {
                format!("{} is encrypted: set BTB_KEY_PASSPHRASE", path.display())
            })?;
            let blob = crate::hex::decode(
                v.get("data")
                    .and_then(Value::as_str)
                    .ok_or("share file needs `data`")?,
            )?;
            if blob.len() < 28 {
                return Err(format!("{}: truncated", path.display()));
            }
            let salt: [u8; 16] = blob[..16].try_into().expect("16");
            let key = kdf(pass, &salt);
            ChaCha20Poly1305::new((&key).into())
                .decrypt(Nonce::from_slice(&blob[16..28]), &blob[28..])
                .map_err(|_| format!("{}: wrong passphrase or damaged file", path.display()))?
        } else {
            crate::hex::decode(
                v.get("share")
                    .and_then(Value::as_str)
                    .ok_or("share file needs `share`")?,
            )?
        };
        KeyShare::decode(&raw).ok_or_else(|| format!("{}: malformed share", path.display()))
    }

    pub fn remove(&self, origin: u64) {
        if let Some(p) = self.path(origin) {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// The agent plus its store, as the driver uses them.
pub struct DkgNode {
    pub agent: Agent,
    store: ShareStore,
    /// Transactions sent and not yet seen in a block.
    pub pending: Vec<btb_chain::Hash>,
    /// Statistics for `btb_status`.
    pub deals_sent: u64,
    pub complaints_sent: u64,
    /// A new share was derived since the flag was last cleared.
    pub changed: bool,
}

impl DkgNode {
    pub fn new(
        chain_id: u64,
        key: SigningKey,
        store: ShareStore,
        interim: Option<KeyShare>,
    ) -> Result<DkgNode, String> {
        let mut agent = Agent::new(chain_id, key);
        let mut agent_is_empty = true;
        for (origin, share) in store.load_all()? {
            agent.insert_share(origin, share);
            agent_is_empty = false;
        }
        if let Some(s) = interim {
            agent.insert_share(0, s);
            agent_is_empty = false;
        }
        Ok(DkgNode {
            agent,
            store,
            pending: Vec::new(),
            deals_sent: 0,
            complaints_sent: 0,
            changed: !agent_is_empty,
        })
    }

    /// Look at the committed state; persist new shares, erase superseded ones, and return the
    /// operations to send.
    pub fn step(
        &mut self,
        kv: &impl KvStore,
        next_height: u64,
        epoch: u64,
    ) -> Result<Vec<DkgOp>, String> {
        let step = self
            .agent
            .step(kv, next_height)
            .map_err(|e| e.to_string())?;
        for (origin, share) in &step.new_shares {
            self.store.save(*origin, share)?;
            self.changed = true;
        }
        // Erase shares of generations older than the key now in use.
        if let Some(cur) = dkg::epoch_key(kv, epoch).map_err(|e| e.to_string())? {
            for origin in self.agent.forget_before(cur.origin) {
                self.store.remove(origin);
            }
        }
        for op in &step.ops {
            match op {
                DkgOp::Deal { .. } => self.deals_sent += 1,
                DkgOp::Complain { .. } => self.complaints_sent += 1,
            }
        }
        Ok(step.ops)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use btb_oracle::bls::Fr;

    fn share() -> KeyShare {
        KeyShare {
            index: 2,
            secret: Fr::from_u64(12345),
        }
    }

    #[test]
    fn shares_round_trip_plain_and_encrypted_and_stay_private() {
        let dir = tempfile::tempdir().unwrap();
        let plain = ShareStore::new(Some(dir.path().join("a")), None);
        plain.save(3, &share()).unwrap();
        assert_eq!(plain.load_all().unwrap(), vec![(3, share())]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("a/share-3.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "share files are private to the owner");
        }

        let enc = ShareStore::new(Some(dir.path().join("b")), Some("correct horse".into()));
        enc.save(4, &share()).unwrap();
        let text = std::fs::read_to_string(dir.path().join("b/share-4.json")).unwrap();
        assert!(!text.contains(&crate::hex::encode(&share().encode())));
        assert_eq!(enc.load_all().unwrap(), vec![(4, share())]);
        // wrong or missing passphrase is an error, never a silent empty store
        let wrong = ShareStore::new(Some(dir.path().join("b")), Some("battery staple".into()));
        assert!(wrong.load_all().is_err());
        let none = ShareStore::new(Some(dir.path().join("b")), None);
        assert!(none.load_all().is_err());
        enc.remove(4);
        assert!(enc.load_all().unwrap().is_empty());
        // no directory: nothing is stored, nothing fails
        let mem = ShareStore::default();
        mem.save(1, &share()).unwrap();
        assert!(mem.load_all().unwrap().is_empty());
    }
}
