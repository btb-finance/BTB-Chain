//! Static site files published by a contract's deployer. See `docs/HOSTING.md`.
//!
//! A contract can own a small static website (HTML, CSS, JS, images) that hosting nodes serve at
//! `/site/<contract>/<path>`. The files live in chain state, so every validator and every
//! follower holds the same bytes, and a Merkle proof of a file against a state root works like
//! any other key.
//!
//! | Tag | Key | Value |
//! | --- | --- | --- |
//! | `0x32` site file | `32 contract path` | `SHA-256(content) \|\| content` |
//!
//! The stored hash makes the file content addressed: the response header, the `ETag` and the
//! proof all name it. Only the **deployer** of the contract may publish or remove files
//! (`SiteOp`, transaction tag 9), so a site cannot be defaced by anyone else. Files are limited
//! to [`MAX_FILE_BYTES`] and a contract to [`MAX_FILES`]: validators hold every byte, so
//! large media belongs on storage nodes (planned, `docs/SCALING.md`), not here. Publishing
//! costs gas per byte, at the same rate as contract code; there is no storage deposit, the fee
//! is what pays for the space.

use crate::state::{contract_key, ContractRecord, StateError};
use crate::tx::{Address, Hash};
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_runtime::kv::{KvError, KvStore};
use sha2::{Digest, Sha256};

pub const TAG_SITE: u8 = 0x32;

/// Largest single file (it must fit in one block next to other transactions).
pub const MAX_FILE_BYTES: usize = 96 * 1024;
/// Most files per contract.
pub const MAX_FILES: usize = 256;
pub const MAX_PATH_BYTES: usize = 128;
/// Fixed gas of a site transaction, plus [`SITE_BYTE_GAS`] per content byte.
pub const SITE_GAS: u64 = 20_000;
pub const SITE_BYTE_GAS: u64 = 100;

/// A site transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiteOp {
    /// Create or replace `path` of `contract`. Only the contract's deployer may do this.
    Publish {
        contract: Address,
        path: String,
        content: Vec<u8>,
    },
    /// Delete `path` of `contract`. Only the contract's deployer may do this.
    Remove { contract: Address, path: String },
}

impl SiteOp {
    pub fn gas(&self) -> u64 {
        match self {
            SiteOp::Publish { content, .. } => {
                SITE_GAS.saturating_add(SITE_BYTE_GAS.saturating_mul(content.len() as u64))
            }
            SiteOp::Remove { .. } => SITE_GAS,
        }
    }

    /// Wire form: `sub:u8 || fields` (the transaction tag is written by `tx.rs`).
    pub(crate) fn encode_to(&self, out: &mut Vec<u8>) {
        match self {
            SiteOp::Publish {
                contract,
                path,
                content,
            } => {
                0u8.encode_to(out);
                contract.encode_to(out);
                path.encode_to(out);
                content.encode_to(out);
            }
            SiteOp::Remove { contract, path } => {
                1u8.encode_to(out);
                contract.encode_to(out);
                path.encode_to(out);
            }
        }
    }

    /// `Ok(Err(sub))` is an unknown sub-tag.
    pub(crate) fn decode_from(r: &mut Reader<'_>) -> Result<Result<Self, u8>, DecodeError> {
        Ok(Ok(match u8::decode_from(r)? {
            0 => SiteOp::Publish {
                contract: <[u8; 32]>::decode_from(r)?,
                path: String::decode_from(r)?,
                content: Vec::decode_from(r)?,
            },
            1 => SiteOp::Remove {
                contract: <[u8; 32]>::decode_from(r)?,
                path: String::decode_from(r)?,
            },
            t => return Ok(Err(t)),
        }))
    }

    /// Checks that need no state: path shape and sizes.
    pub fn check_static(&self) -> Result<(), SiteError> {
        match self {
            SiteOp::Publish { path, content, .. } => {
                check_path(path)?;
                if content.len() > MAX_FILE_BYTES {
                    return Err(SiteError::FileTooLarge);
                }
                Ok(())
            }
            SiteOp::Remove { path, .. } => check_path(path),
        }
    }
}

/// Why a site transaction was refused (the fee is still charged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiteError {
    NoSuchContract,
    /// The sender did not deploy the contract.
    NotDeployer,
    BadPath,
    FileTooLarge,
    TooManyFiles,
    NoSuchFile,
}

/// A path is 1..=128 bytes of `[A-Za-z0-9._~-]` segments separated by single `/`: no leading
/// or trailing slash, no empty, `.` or `..` segment.
pub fn check_path(path: &str) -> Result<(), SiteError> {
    let ok = !path.is_empty()
        && path.len() <= MAX_PATH_BYTES
        && path.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'))
        });
    if ok {
        Ok(())
    } else {
        Err(SiteError::BadPath)
    }
}

/// State key of a site file.
pub fn site_key(contract: &Address, path: &str) -> Vec<u8> {
    let mut k = vec![TAG_SITE];
    k.extend_from_slice(contract);
    k.extend_from_slice(path.as_bytes());
    k
}

/// SHA-256 of file content: the file's content address.
pub fn content_hash(content: &[u8]) -> Hash {
    Sha256::digest(content).into()
}

/// A stored site file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteFile {
    pub hash: Hash,
    pub content: Vec<u8>,
}

impl SiteFile {
    /// The value stored under [`site_key`].
    pub fn to_value(&self) -> Vec<u8> {
        let mut v = self.hash.to_vec();
        v.extend_from_slice(&self.content);
        v
    }

    /// Parse a stored value; `None` if it is shorter than a hash or the hash does not match
    /// (what a node should treat as corrupt).
    pub fn from_value(v: &[u8]) -> Option<SiteFile> {
        let hash: Hash = v.get(..32)?.try_into().ok()?;
        let content = v[32..].to_vec();
        (content_hash(&content) == hash).then_some(SiteFile { hash, content })
    }
}

/// Read a published file from any store holding chain state.
pub fn get(kv: &impl KvStore, contract: &Address, path: &str) -> Result<Option<SiteFile>, KvError> {
    Ok(kv
        .get(&site_key(contract, path))?
        .and_then(|v| SiteFile::from_value(&v)))
}

/// Paths and content hashes of a contract's files, in path order.
pub fn list(kv: &impl KvStore, contract: &Address) -> Result<Vec<(String, Hash)>, KvError> {
    let mut start = vec![TAG_SITE];
    start.extend_from_slice(contract);
    let mut end = start.clone();
    // The contract address is fixed-length, so the next address value bounds the prefix.
    for b in end.iter_mut().rev() {
        if *b == 0xff {
            *b = 0;
        } else {
            *b += 1;
            break;
        }
    }
    let rows = kv.scan(&start, Some(&end), false, MAX_FILES + 1)?;
    Ok(rows
        .into_iter()
        .filter_map(|(k, v)| {
            let path = String::from_utf8(k.get(33..)?.to_vec()).ok()?;
            Some((path, v.get(..32)?.try_into().ok()?))
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fail {
    Tx(SiteError),
    State(StateError),
}
impl From<SiteError> for Fail {
    fn from(e: SiteError) -> Self {
        Fail::Tx(e)
    }
}
impl From<KvError> for Fail {
    fn from(e: KvError) -> Self {
        Fail::State(e.into())
    }
}
impl From<StateError> for Fail {
    fn from(e: StateError) -> Self {
        Fail::State(e)
    }
}

/// Apply a site transaction from `sender`. On success returns the content hash (publish) or an
/// empty output (remove).
pub(crate) fn apply(kv: &mut impl KvStore, sender: &Address, op: &SiteOp) -> Result<Vec<u8>, Fail> {
    op.check_static()?;
    let (contract, path) = match op {
        SiteOp::Publish { contract, path, .. } | SiteOp::Remove { contract, path } => {
            (contract, path)
        }
    };
    let rec = match kv.get(&contract_key(contract))? {
        None => return Err(SiteError::NoSuchContract.into()),
        Some(b) => ContractRecord::decode(&b).map_err(StateError::from)?,
    };
    if &rec.deployer != sender {
        return Err(SiteError::NotDeployer.into());
    }
    let key = site_key(contract, path);
    match op {
        SiteOp::Publish { content, .. } => {
            if kv.get(&key)?.is_none() && list(kv, contract)?.len() >= MAX_FILES {
                return Err(SiteError::TooManyFiles.into());
            }
            let file = SiteFile {
                hash: content_hash(content),
                content: content.clone(),
            };
            kv.put(&key, &file.to_value())?;
            Ok(file.hash.to_vec())
        }
        SiteOp::Remove { .. } => {
            if kv.get(&key)?.is_none() {
                return Err(SiteError::NoSuchFile.into());
            }
            kv.delete(&key)?;
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        for ok in ["index.html", "a/b/c.js", "img/logo-1_x~.png", "a"] {
            assert_eq!(check_path(ok), Ok(()), "{ok}");
        }
        for bad in [
            "", "/", "/a", "a/", "a//b", "../x", "a/../b", "./a", "a b", "a\\b", "ä.html", "a?b",
            "a#b", "a%2e",
        ] {
            assert_eq!(check_path(bad), Err(SiteError::BadPath), "{bad:?}");
        }
        assert_eq!(check_path(&"a".repeat(129)), Err(SiteError::BadPath));
        assert_eq!(check_path(&"a".repeat(128)), Ok(()));
    }

    #[test]
    fn op_round_trip_and_gas() {
        for op in [
            SiteOp::Publish {
                contract: [3; 32],
                path: "index.html".into(),
                content: b"<h1>hi</h1>".to_vec(),
            },
            SiteOp::Remove {
                contract: [4; 32],
                path: "x/y".into(),
            },
        ] {
            let mut b = Vec::new();
            op.encode_to(&mut b);
            let mut r = Reader::new(&b, 1 << 20);
            assert_eq!(SiteOp::decode_from(&mut r).unwrap(), Ok(op.clone()));
            assert_eq!(r.remaining(), 0);
        }
        let p = SiteOp::Publish {
            contract: [0; 32],
            path: "a".into(),
            content: vec![0; 10],
        };
        assert_eq!(p.gas(), SITE_GAS + 10 * SITE_BYTE_GAS);
    }

    #[test]
    fn value_hash_is_checked() {
        let f = SiteFile {
            hash: content_hash(b"abc"),
            content: b"abc".to_vec(),
        };
        assert_eq!(SiteFile::from_value(&f.to_value()), Some(f.clone()));
        let mut v = f.to_value();
        *v.last_mut().unwrap() ^= 1;
        assert_eq!(SiteFile::from_value(&v), None);
        assert_eq!(SiteFile::from_value(&[0; 5]), None);
    }
}
