//! Jellyfish-style sparse Merkle tree state commitment.
//!
//! The tree is 16-ary over the nibbles of `SHA-256(key)`. A subtree holding exactly one entry
//! is a single leaf placed at the shallowest position (path compression), so the shape, and
//! therefore the root, depends only on the set of `(key, value)` pairs, never on write order or
//! history.
//!
//! - empty subtree: `EMPTY` (32 zero bytes)
//! - leaf: `SHA256(0x00 || key_hash || SHA256(value))`
//! - internal node (two or more entries): `SHA256(0x01 || child_0 || .. || child_15)`
//!
//! Nodes are content-addressed and stored in the `KvStore` (tag `0x06` by node hash); the root
//! of each version is stored under tag `0x07` by version. Old nodes are never overwritten, so
//! every committed version stays queryable. Layout details are in `docs/STATE.md`.

use crate::kv::{BatchOp, KvError, KvStore, WriteBatch};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;

pub type Hash = [u8; 32];

/// Root of the empty tree and hash of every empty subtree.
pub const EMPTY: Hash = [0u8; 32];

/// Key tag for tree nodes: `06 node_hash`.
pub const TAG_NODE: u8 = 0x06;
/// Key tag for version roots: `07 version:u64 BE`.
pub const TAG_ROOT: u8 = 0x07;

const DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MerkleError {
    Kv(KvError),
    Corrupt(String),
    /// `apply` needs a version greater than every stored one.
    VersionNotNewer {
        given: u64,
        latest: u64,
    },
    UnknownVersion(u64),
}

impl fmt::Display for MerkleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MerkleError::Kv(e) => write!(f, "{e}"),
            MerkleError::Corrupt(m) => write!(f, "corrupt merkle node: {m}"),
            MerkleError::VersionNotNewer { given, latest } => {
                write!(f, "version {given} is not newer than {latest}")
            }
            MerkleError::UnknownVersion(v) => write!(f, "unknown version {v}"),
        }
    }
}
impl std::error::Error for MerkleError {}
impl From<KvError> for MerkleError {
    fn from(e: KvError) -> Self {
        MerkleError::Kv(e)
    }
}

/// Proof that a key is present with a value, or absent, under a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proof {
    /// One entry per internal node on the path, root first: the 15 sibling hashes in nibble
    /// order, skipping the nibble the path takes.
    pub siblings: Vec<Vec<Hash>>,
    /// The leaf where the path ends as `(key_hash, value_hash)`, or `None` for an empty slot.
    /// For a non-inclusion proof this may be a different key's leaf.
    pub leaf: Option<(Hash, Hash)>,
}

pub fn hash_key(key: &[u8]) -> Hash {
    Sha256::digest(key).into()
}

fn hash_value(value: &[u8]) -> Hash {
    Sha256::digest(value).into()
}

fn leaf_hash(key_hash: &Hash, value_hash: &Hash) -> Hash {
    let mut h = Sha256::new();
    h.update([0x00]);
    h.update(key_hash);
    h.update(value_hash);
    h.finalize().into()
}

fn internal_hash(children: &[Hash; 16]) -> Hash {
    let mut h = Sha256::new();
    h.update([0x01]);
    for c in children {
        h.update(c);
    }
    h.finalize().into()
}

fn nibble(key_hash: &Hash, depth: usize) -> usize {
    let b = key_hash[depth / 2];
    (if depth.is_multiple_of(2) {
        b >> 4
    } else {
        b & 0x0f
    }) as usize
}

fn node_key(hash: &Hash) -> Vec<u8> {
    let mut k = vec![TAG_NODE];
    k.extend_from_slice(hash);
    k
}

fn root_key(version: u64) -> Vec<u8> {
    let mut k = vec![TAG_ROOT];
    k.extend_from_slice(&version.to_be_bytes());
    k
}

enum Node {
    Leaf {
        key_hash: Hash,
        value_hash: Hash,
        value: Vec<u8>,
    },
    Internal(Box<[Hash; 16]>),
}

impl Node {
    fn encode(&self) -> Vec<u8> {
        match self {
            Node::Leaf {
                key_hash,
                value_hash,
                value,
            } => {
                let mut v = vec![0x00];
                v.extend_from_slice(key_hash);
                v.extend_from_slice(value_hash);
                v.extend_from_slice(value);
                v
            }
            Node::Internal(ch) => {
                let mut v = vec![0x01];
                for c in ch.iter() {
                    v.extend_from_slice(c);
                }
                v
            }
        }
    }

    fn decode(b: &[u8]) -> Result<Node, MerkleError> {
        let bad = |m: &str| MerkleError::Corrupt(m.to_string());
        match b.first() {
            Some(0x00) if b.len() >= 65 => Ok(Node::Leaf {
                key_hash: b[1..33].try_into().unwrap(),
                value_hash: b[33..65].try_into().unwrap(),
                value: b[65..].to_vec(),
            }),
            Some(0x01) if b.len() == 1 + 16 * 32 => {
                let mut ch = [EMPTY; 16];
                for (i, c) in ch.iter_mut().enumerate() {
                    c.copy_from_slice(&b[1 + i * 32..33 + i * 32]);
                }
                Ok(Node::Internal(Box::new(ch)))
            }
            _ => Err(bad("bad node encoding")),
        }
    }
}

fn load(kv: &impl KvStore, hash: &Hash) -> Result<Node, MerkleError> {
    let raw = kv
        .get(&node_key(hash))?
        .ok_or_else(|| MerkleError::Corrupt("missing node".into()))?;
    Node::decode(&raw)
}

/// `(key_hash, value)` change; `None` deletes.
type Change = (Hash, Option<Vec<u8>>);
/// Live entry used when building subtrees: `(key_hash, value)`.
type Entry = (Hash, Vec<u8>);

fn put_leaf(out: &mut WriteBatch, key_hash: Hash, value: Vec<u8>) -> Hash {
    let value_hash = hash_value(&value);
    let h = leaf_hash(&key_hash, &value_hash);
    let node = Node::Leaf {
        key_hash,
        value_hash,
        value,
    };
    out.put(node_key(&h), node.encode());
    h
}

fn put_internal(out: &mut WriteBatch, ch: [Hash; 16]) -> Hash {
    let h = internal_hash(&ch);
    out.put(node_key(&h), Node::Internal(Box::new(ch)).encode());
    h
}

/// Build the canonical subtree for sorted, distinct entries at `depth`.
fn build(out: &mut WriteBatch, depth: usize, entries: Vec<Entry>) -> Hash {
    match entries.len() {
        0 => EMPTY,
        1 => {
            let (k, v) = entries.into_iter().next().unwrap();
            put_leaf(out, k, v)
        }
        _ => {
            let mut groups: Vec<Vec<Entry>> = (0..16).map(|_| Vec::new()).collect();
            for e in entries {
                groups[nibble(&e.0, depth)].push(e);
            }
            let mut ch = [EMPTY; 16];
            for (i, g) in groups.into_iter().enumerate() {
                ch[i] = build(out, depth + 1, g);
            }
            put_internal(out, ch)
        }
    }
}

/// Apply sorted, distinct-key `changes` to the subtree `node` at `depth`.
fn update(
    kv: &impl KvStore,
    out: &mut WriteBatch,
    node: Hash,
    depth: usize,
    changes: &[Change],
) -> Result<Hash, MerkleError> {
    if changes.is_empty() {
        return Ok(node);
    }
    if node == EMPTY {
        let live = changes
            .iter()
            .filter_map(|(k, v)| v.clone().map(|v| (*k, v)))
            .collect();
        return Ok(build(out, depth, live));
    }
    match load(kv, &node)? {
        Node::Leaf {
            key_hash, value, ..
        } => {
            let mut map: BTreeMap<Hash, Vec<u8>> = BTreeMap::new();
            map.insert(key_hash, value);
            for (k, v) in changes {
                match v {
                    Some(v) => map.insert(*k, v.clone()),
                    None => map.remove(k),
                };
            }
            Ok(build(out, depth, map.into_iter().collect()))
        }
        Node::Internal(mut ch) => {
            let mut start = 0;
            while start < changes.len() {
                let n = nibble(&changes[start].0, depth);
                let mut end = start;
                while end < changes.len() && nibble(&changes[end].0, depth) == n {
                    end += 1;
                }
                ch[n] = update(kv, out, ch[n], depth + 1, &changes[start..end])?;
                start = end;
            }
            let mut nonempty = ch.iter().filter(|c| **c != EMPTY);
            match (nonempty.next(), nonempty.next()) {
                (None, _) => Ok(EMPTY),
                (Some(only), None) => {
                    // A lone leaf moves up (canonical form); a lone internal node stays.
                    if matches!(load_pending(kv, out, only)?, Node::Leaf { .. }) {
                        Ok(*only)
                    } else {
                        Ok(put_internal(out, *ch))
                    }
                }
                _ => Ok(put_internal(out, *ch)),
            }
        }
    }
}

/// Load a node that may have been created earlier in this same batch (not yet in the store).
fn load_pending(kv: &impl KvStore, out: &WriteBatch, hash: &Hash) -> Result<Node, MerkleError> {
    let key = node_key(hash);
    for op in out.ops().iter().rev() {
        if let BatchOp::Put(k, v) = op {
            if *k == key {
                return Node::decode(v);
            }
        }
    }
    load(kv, hash)
}

fn latest_version(kv: &impl KvStore) -> Result<Option<u64>, MerkleError> {
    let r = kv.scan(&[TAG_ROOT], Some(&[TAG_ROOT + 1]), true, 1)?;
    match r.first() {
        None => Ok(None),
        Some((k, _)) if k.len() == 9 => Ok(Some(u64::from_be_bytes(k[1..9].try_into().unwrap()))),
        Some(_) => Err(MerkleError::Corrupt("bad root key".into())),
    }
}

/// Root committed at exactly `version`, if any.
pub fn root(kv: &impl KvStore, version: u64) -> Result<Option<Hash>, MerkleError> {
    match kv.get(&root_key(version))? {
        None => Ok(None),
        Some(v) => v
            .try_into()
            .map(Some)
            .map_err(|_| MerkleError::Corrupt("bad root value".into())),
    }
}

/// Compute the new root and the node/root writes, without committing anything.
///
/// `batch` is applied on top of the latest committed version (the empty tree if none). Within a
/// batch a later op on the same key wins; the result is independent of the order of ops on
/// different keys. `version` must exceed every stored version.
pub fn prepare(
    kv: &impl KvStore,
    version: u64,
    batch: &WriteBatch,
) -> Result<(Hash, WriteBatch), MerkleError> {
    let latest = latest_version(kv)?;
    if let Some(l) = latest {
        if version <= l {
            return Err(MerkleError::VersionNotNewer {
                given: version,
                latest: l,
            });
        }
    }
    let base = match latest {
        Some(l) => root(kv, l)?.ok_or_else(|| MerkleError::Corrupt("missing root".into()))?,
        None => EMPTY,
    };
    let mut merged: BTreeMap<Hash, Option<Vec<u8>>> = BTreeMap::new();
    for op in batch.ops() {
        match op {
            BatchOp::Put(k, v) => merged.insert(hash_key(k), Some(v.clone())),
            BatchOp::Delete(k) => merged.insert(hash_key(k), None),
        };
    }
    let changes: Vec<Change> = merged.into_iter().collect();
    let mut out = WriteBatch::new();
    let new_root = update(kv, &mut out, base, 0, &changes)?;
    out.put(root_key(version), new_root.to_vec());
    Ok((new_root, out))
}

/// Apply `batch` as `version` and commit the tree nodes and root to `kv` atomically.
pub fn apply(kv: &mut impl KvStore, version: u64, batch: &WriteBatch) -> Result<Hash, MerkleError> {
    let (r, out) = prepare(kv, version, batch)?;
    kv.commit(out)?;
    Ok(r)
}

/// Value of `key` at `version` (if present) and a proof of presence or absence.
pub fn get_with_proof(
    kv: &impl KvStore,
    version: u64,
    key: &[u8],
) -> Result<(Option<Vec<u8>>, Proof), MerkleError> {
    let mut node = root(kv, version)?.ok_or(MerkleError::UnknownVersion(version))?;
    let kh = hash_key(key);
    let mut siblings = Vec::new();
    for depth in 0..=DEPTH {
        if node == EMPTY {
            return Ok((
                None,
                Proof {
                    siblings,
                    leaf: None,
                },
            ));
        }
        match load(kv, &node)? {
            Node::Leaf {
                key_hash,
                value_hash,
                value,
            } => {
                let found = (key_hash == kh).then_some(value);
                return Ok((
                    found,
                    Proof {
                        siblings,
                        leaf: Some((key_hash, value_hash)),
                    },
                ));
            }
            Node::Internal(ch) => {
                if depth >= DEPTH {
                    return Err(MerkleError::Corrupt("tree too deep".into()));
                }
                let n = nibble(&kh, depth);
                siblings.push(
                    ch.iter()
                        .enumerate()
                        .filter(|(i, _)| *i != n)
                        .map(|(_, h)| *h)
                        .collect(),
                );
                node = ch[n];
            }
        }
    }
    Err(MerkleError::Corrupt("tree too deep".into()))
}

/// Check that `value` (`None` = absent) for `key` is committed by `root_hash`.
pub fn verify(root_hash: &Hash, key: &[u8], value: Option<&[u8]>, proof: &Proof) -> bool {
    let kh = hash_key(key);
    let levels = proof.siblings.len();
    if levels > DEPTH || proof.siblings.iter().any(|s| s.len() != 15) {
        return false;
    }
    let mut cur = match (&proof.leaf, value) {
        (Some((lk, lv)), Some(v)) => {
            if *lk != kh || *lv != hash_value(v) {
                return false;
            }
            leaf_hash(lk, lv)
        }
        (Some((lk, lv)), None) => {
            // Another key's leaf: it must sit on this key's path and not be this key.
            if *lk == kh || (0..levels).any(|d| nibble(lk, d) != nibble(&kh, d)) {
                return false;
            }
            leaf_hash(lk, lv)
        }
        (None, None) => EMPTY,
        (None, Some(_)) => return false,
    };
    // A lone leaf directly under a parent would have been hoisted: not canonical, but the hash
    // check below is what binds the proof, so no extra rule is needed here.
    for d in (0..levels).rev() {
        let n = nibble(&kh, d);
        let mut ch = [EMPTY; 16];
        let mut it = proof.siblings[d].iter();
        for (i, slot) in ch.iter_mut().enumerate() {
            *slot = if i == n { cur } else { *it.next().unwrap() };
        }
        cur = internal_hash(&ch);
    }
    cur == *root_hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv::MemKv;

    fn batch(items: &[(&str, Option<&str>)]) -> WriteBatch {
        let mut b = WriteBatch::new();
        for (k, v) in items {
            match v {
                Some(v) => b.put(k.as_bytes().to_vec(), v.as_bytes().to_vec()),
                None => b.delete(k.as_bytes().to_vec()),
            }
        }
        b
    }

    /// Independent definition of the root from the full entry set.
    fn naive(entries: &[(Hash, Hash)], depth: usize) -> Hash {
        match entries.len() {
            0 => EMPTY,
            1 => leaf_hash(&entries[0].0, &entries[0].1),
            _ => {
                let mut ch = [EMPTY; 16];
                for (n, slot) in ch.iter_mut().enumerate() {
                    let sub: Vec<_> = entries
                        .iter()
                        .filter(|e| nibble(&e.0, depth) == n)
                        .cloned()
                        .collect();
                    *slot = naive(&sub, depth + 1);
                }
                internal_hash(&ch)
            }
        }
    }

    fn naive_root(map: &BTreeMap<Vec<u8>, Vec<u8>>) -> Hash {
        let e: Vec<_> = map
            .iter()
            .map(|(k, v)| (hash_key(k), hash_value(v)))
            .collect();
        naive(&e, 0)
    }

    #[test]
    fn empty_root_constant() {
        assert_eq!(EMPTY, [0u8; 32]);
        let mut kv = MemKv::new();
        assert_eq!(apply(&mut kv, 1, &WriteBatch::new()).unwrap(), EMPTY);
        assert_eq!(root(&kv, 1).unwrap(), Some(EMPTY));
        assert_eq!(root(&kv, 2).unwrap(), None);
    }

    #[test]
    fn insert_update_delete() {
        let mut kv = MemKv::new();
        let r1 = apply(&mut kv, 1, &batch(&[("a", Some("1"))])).unwrap();
        assert_ne!(r1, EMPTY);
        let r2 = apply(&mut kv, 2, &batch(&[("b", Some("2"))])).unwrap();
        assert_ne!(r2, r1);
        let r3 = apply(&mut kv, 3, &batch(&[("a", Some("9"))])).unwrap();
        assert_ne!(r3, r2);
        // Same value again: unchanged.
        let r4 = apply(&mut kv, 4, &batch(&[("a", Some("9"))])).unwrap();
        assert_eq!(r4, r3);
        // Deleting b returns to the single-entry tree for a=9.
        let r5 = apply(&mut kv, 5, &batch(&[("b", None)])).unwrap();
        let mut kv2 = MemKv::new();
        let direct = apply(&mut kv2, 1, &batch(&[("a", Some("9"))])).unwrap();
        assert_eq!(r5, direct);
        // Deleting a missing key is a no-op; deleting the last key gives EMPTY.
        assert_eq!(apply(&mut kv, 6, &batch(&[("zz", None)])).unwrap(), r5);
        assert_eq!(apply(&mut kv, 7, &batch(&[("a", None)])).unwrap(), EMPTY);
    }

    #[test]
    fn order_independent() {
        let items = [
            ("a", Some("1")),
            ("b", Some("2")),
            ("c", Some("3")),
            ("d", Some("4")),
            ("e", None),
        ];
        let mut rev = items;
        rev.reverse();
        let r1 = apply(&mut MemKv::new(), 1, &batch(&items)).unwrap();
        let r2 = apply(&mut MemKv::new(), 1, &batch(&rev)).unwrap();
        assert_eq!(r1, r2);
        // Splitting across versions gives the same root too.
        let mut kv = MemKv::new();
        apply(&mut kv, 1, &batch(&items[..2])).unwrap();
        let r3 = apply(&mut kv, 2, &batch(&items[2..])).unwrap();
        assert_eq!(r1, r3);
    }

    #[test]
    fn later_op_on_same_key_wins() {
        let r1 = apply(
            &mut MemKv::new(),
            1,
            &batch(&[
                ("a", Some("1")),
                ("a", Some("2")),
                ("b", Some("x")),
                ("b", None),
            ]),
        )
        .unwrap();
        let r2 = apply(&mut MemKv::new(), 1, &batch(&[("a", Some("2"))])).unwrap();
        assert_eq!(r1, r2);
    }

    #[test]
    fn version_must_increase() {
        let mut kv = MemKv::new();
        apply(&mut kv, 5, &batch(&[("a", Some("1"))])).unwrap();
        assert!(matches!(
            apply(&mut kv, 5, &WriteBatch::new()),
            Err(MerkleError::VersionNotNewer { .. })
        ));
        assert!(apply(&mut kv, 4, &WriteBatch::new()).is_err());
    }

    #[test]
    fn proofs_present_and_absent() {
        let mut kv = MemKv::new();
        let mut b = WriteBatch::new();
        for i in 0..50u32 {
            b.put(
                format!("key{i}").into_bytes(),
                format!("val{i}").into_bytes(),
            );
        }
        let r = apply(&mut kv, 1, &b).unwrap();
        for i in 0..50u32 {
            let k = format!("key{i}");
            let (v, p) = get_with_proof(&kv, 1, k.as_bytes()).unwrap();
            assert_eq!(v.as_deref(), Some(format!("val{i}").as_bytes()));
            assert!(verify(&r, k.as_bytes(), v.as_deref(), &p));
            // Claiming absence of a present key fails.
            assert!(!verify(&r, k.as_bytes(), None, &p));
        }
        for i in 0..50u32 {
            let k = format!("missing{i}");
            let (v, p) = get_with_proof(&kv, 1, k.as_bytes()).unwrap();
            assert_eq!(v, None);
            assert!(verify(&r, k.as_bytes(), None, &p));
            assert!(!verify(&r, k.as_bytes(), Some(b"x"), &p));
        }
        // Absence proofs on the empty tree and a single-leaf tree.
        let mut e = MemKv::new();
        let er = apply(&mut e, 1, &WriteBatch::new()).unwrap();
        let (v, p) = get_with_proof(&e, 1, b"x").unwrap();
        assert!(v.is_none() && verify(&er, b"x", None, &p));
        let mut s = MemKv::new();
        let sr = apply(&mut s, 1, &batch(&[("only", Some("1"))])).unwrap();
        let (v, p) = get_with_proof(&s, 1, b"other").unwrap();
        assert!(v.is_none() && p.leaf.is_some() && verify(&sr, b"other", None, &p));
        assert!(matches!(
            get_with_proof(&s, 9, b"x"),
            Err(MerkleError::UnknownVersion(9))
        ));
    }

    #[test]
    fn tampering_fails() {
        let mut kv = MemKv::new();
        let mut b = WriteBatch::new();
        for i in 0..40u32 {
            b.put(format!("k{i}").into_bytes(), format!("v{i}").into_bytes());
        }
        let r = apply(&mut kv, 1, &b).unwrap();
        let (v, p) = get_with_proof(&kv, 1, b"k7").unwrap();
        assert!(verify(&r, b"k7", v.as_deref(), &p));
        assert!(!verify(&r, b"k7", Some(b"wrong"), &p));
        assert!(!verify(&r, b"k8", v.as_deref(), &p));
        let mut bad_root = r;
        bad_root[0] ^= 1;
        assert!(!verify(&bad_root, b"k7", v.as_deref(), &p));
        let mut t = p.clone();
        t.siblings[0][0][0] ^= 1;
        assert!(!verify(&r, b"k7", v.as_deref(), &t));
        let mut t = p.clone();
        t.siblings.pop();
        assert!(!verify(&r, b"k7", v.as_deref(), &t));
        let mut t = p.clone();
        t.siblings[0].pop();
        assert!(!verify(&r, b"k7", v.as_deref(), &t));
        let mut t = p.clone();
        if let Some((_, vh)) = t.leaf.as_mut() {
            vh[0] ^= 1;
        }
        assert!(!verify(&r, b"k7", v.as_deref(), &t));
        // An absence proof only holds for keys on its own path.
        let (_, ap) = get_with_proof(&kv, 1, b"nope").unwrap();
        assert!(verify(&r, b"nope", None, &ap));
        assert!(!verify(&r, b"k7", None, &ap));
    }

    #[test]
    fn old_versions_queryable() {
        let mut kv = MemKv::new();
        let r1 = apply(&mut kv, 10, &batch(&[("a", Some("1")), ("b", Some("2"))])).unwrap();
        let r2 = apply(&mut kv, 20, &batch(&[("a", Some("3")), ("b", None)])).unwrap();
        assert_eq!(root(&kv, 10).unwrap(), Some(r1));
        assert_eq!(root(&kv, 20).unwrap(), Some(r2));
        let (v, p) = get_with_proof(&kv, 10, b"a").unwrap();
        assert_eq!(v.as_deref(), Some(&b"1"[..]));
        assert!(verify(&r1, b"a", v.as_deref(), &p));
        let (v, p) = get_with_proof(&kv, 10, b"b").unwrap();
        assert_eq!(v.as_deref(), Some(&b"2"[..]));
        assert!(verify(&r1, b"b", v.as_deref(), &p));
        let (v, p) = get_with_proof(&kv, 20, b"b").unwrap();
        assert!(v.is_none() && verify(&r2, b"b", None, &p));
        let (v, _) = get_with_proof(&kv, 20, b"a").unwrap();
        assert_eq!(v.as_deref(), Some(&b"3"[..]));
    }

    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn randomized_matches_naive() {
        let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
        let mut kv = MemKv::new();
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut version = 0;
        // Several rounds of mixed inserts, updates and deletes; ~10k distinct keys overall.
        for round in 0..5 {
            let mut b = WriteBatch::new();
            let mut ops = Vec::new();
            for _ in 0..3000 {
                let k = (rng.next() % 10_000).to_be_bytes().to_vec();
                if round > 0 && rng.next().is_multiple_of(4) {
                    ops.push((k, None));
                } else {
                    ops.push((k, Some(rng.next().to_le_bytes().to_vec())));
                }
            }
            for (k, v) in &ops {
                match v {
                    Some(v) => {
                        b.put(k.clone(), v.clone());
                        model.insert(k.clone(), v.clone());
                    }
                    None => {
                        b.delete(k.clone());
                        model.remove(k);
                    }
                }
            }
            version += 1;
            let r = apply(&mut kv, version, &b).unwrap();
            assert_eq!(r, naive_root(&model), "round {round}");
        }
        assert!(model.len() > 2000);
        let r = root(&kv, version).unwrap().unwrap();
        for k in model.keys().take(200).chain([&vec![1u8, 2, 3]]) {
            let (v, p) = get_with_proof(&kv, version, k).unwrap();
            assert_eq!(v.as_ref(), model.get(k));
            assert!(verify(&r, k, v.as_deref(), &p));
        }
    }
}
