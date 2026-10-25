//! Reed-Solomon erasure coding of a file into pieces, and the Merkle commitment over them.
//!
//! A file of `size` bytes becomes `k` data pieces (the file cut into equal parts, zero padded)
//! and `m` parity pieces, each `proof::piece_len(size, k)` bytes. **Any `k` of the `k + m`
//! pieces** rebuild the file. The file id binds the pieces' Merkle roots, the size and `k`, `m`
//! (`proof::file_id_of_pieces`), so a fetched file is verified by encoding it again and
//! comparing ids.

use crate::proof;
use crate::Hash;
use reed_solomon_erasure::galois_8::ReedSolomon;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErasureError {
    /// `k` or `m` is zero, or `k + m` exceeds 255 (the field size).
    BadShape,
    /// Fewer than `k` pieces are available.
    NotEnoughPieces {
        have: usize,
        need: usize,
    },
    /// Pieces of different lengths, or the wrong number of slots.
    BadPieces,
    Codec(String),
}

impl std::fmt::Display for ErasureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErasureError::BadShape => write!(f, "k and m must be at least 1 and k + m at most 255"),
            ErasureError::NotEnoughPieces { have, need } => {
                write!(f, "only {have} pieces available, {need} needed")
            }
            ErasureError::BadPieces => write!(f, "pieces have inconsistent lengths"),
            ErasureError::Codec(e) => write!(f, "reed-solomon: {e}"),
        }
    }
}
impl std::error::Error for ErasureError {}

/// A file split into pieces, with its commitment.
#[derive(Debug, Clone)]
pub struct Encoded {
    pub k: u8,
    pub m: u8,
    pub size: u64,
    pub piece_len: usize,
    /// `k` data pieces then `m` parity pieces.
    pub pieces: Vec<Vec<u8>>,
    /// Merkle root of each piece.
    pub roots: Vec<Hash>,
    /// The file id: what the chain stores.
    pub id: Hash,
}

impl Encoded {
    /// Siblings from piece `index`'s root to the file tree root.
    pub fn piece_path(&self, index: usize) -> Vec<Hash> {
        piece_path(&self.roots, index)
    }
}

pub fn piece_path(roots: &[Hash], index: usize) -> Vec<Hash> {
    let leaves: Vec<Hash> = roots.iter().map(proof::piece_leaf).collect();
    proof::path(&leaves, index)
}

fn codec(k: u8, m: u8) -> Result<ReedSolomon, ErasureError> {
    if k == 0 || m == 0 || usize::from(k) + usize::from(m) > 255 {
        return Err(ErasureError::BadShape);
    }
    ReedSolomon::new(k.into(), m.into()).map_err(|e| ErasureError::Codec(e.to_string()))
}

/// Split `data` into `k + m` pieces.
pub fn encode(data: &[u8], k: u8, m: u8) -> Result<Encoded, ErasureError> {
    let rs = codec(k, m)?;
    let size = data.len() as u64;
    let piece_len = proof::piece_len(size.max(1), k.into()) as usize;
    let mut pieces: Vec<Vec<u8>> = Vec::with_capacity(usize::from(k) + usize::from(m));
    for i in 0..usize::from(k) {
        let mut p = vec![0u8; piece_len];
        let from = (i * piece_len).min(data.len());
        let to = ((i + 1) * piece_len).min(data.len());
        p[..to - from].copy_from_slice(&data[from..to]);
        pieces.push(p);
    }
    pieces.extend((0..m).map(|_| vec![0u8; piece_len]));
    rs.encode(&mut pieces)
        .map_err(|e| ErasureError::Codec(e.to_string()))?;
    let roots: Vec<Hash> = pieces.iter().map(|p| proof::piece_root(p)).collect();
    let id = proof::file_id_of_pieces(&roots, size, k, m);
    Ok(Encoded {
        k,
        m,
        size,
        piece_len,
        pieces,
        roots,
        id,
    })
}

/// Fill every `None` slot of `pieces` (`k + m` slots) from any `k` present ones.
pub fn reconstruct(pieces: &mut [Option<Vec<u8>>], k: u8, m: u8) -> Result<(), ErasureError> {
    let rs = codec(k, m)?;
    if pieces.len() != usize::from(k) + usize::from(m) {
        return Err(ErasureError::BadPieces);
    }
    let have = pieces.iter().filter(|p| p.is_some()).count();
    if have < usize::from(k) {
        return Err(ErasureError::NotEnoughPieces {
            have,
            need: k.into(),
        });
    }
    let len = pieces.iter().flatten().map(Vec::len).next().unwrap_or(0);
    if pieces.iter().flatten().any(|p| p.len() != len) {
        return Err(ErasureError::BadPieces);
    }
    rs.reconstruct(pieces)
        .map_err(|e| ErasureError::Codec(e.to_string()))
}

/// Rebuild the file of `size` bytes from at least `k` pieces (`None` = missing).
pub fn decode(
    mut pieces: Vec<Option<Vec<u8>>>,
    k: u8,
    m: u8,
    size: u64,
) -> Result<Vec<u8>, ErasureError> {
    reconstruct(&mut pieces, k, m)?;
    let mut out = Vec::new();
    for p in pieces.iter().take(k.into()) {
        out.extend_from_slice(p.as_ref().ok_or(ErasureError::BadPieces)?);
    }
    out.truncate(size as usize);
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
/// Check that `piece` is piece `index` of file `id` (`path` from its root to the tree root).
pub fn verify_piece(
    id: &Hash,
    size: u64,
    k: u8,
    m: u8,
    piece_len: u64,
    index: usize,
    piece: &[u8],
    path: &[Hash],
) -> bool {
    if piece.len() as u64 != piece_len || !piece.len().is_multiple_of(proof::CHUNK) {
        return false;
    }
    let n = usize::from(k) + usize::from(m);
    let Some(tree) =
        proof::root_from_path(proof::piece_leaf(&proof::piece_root(piece)), index, n, path)
    else {
        return false;
    };
    &proof::file_id(&tree, size, k, m) == id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| ((i * 7919) >> 3) as u8 ^ (i >> 9) as u8)
            .collect()
    }

    #[test]
    fn round_trip_with_up_to_m_missing() {
        for (n, k, m) in [
            (1usize, 1u8, 1u8),
            (1000, 2, 1),
            (5000, 4, 2),
            (1 << 20, 4, 2),
            (12345, 3, 4),
        ] {
            let d = data(n);
            let e = encode(&d, k, m).unwrap();
            assert_eq!(e.pieces.len(), usize::from(k + m));
            assert!(e.pieces.iter().all(|p| p.len() == e.piece_len));
            // Drop every subset of exactly m pieces for small shapes.
            let total = usize::from(k + m);
            for mask in 0u32..(1 << total) {
                if mask.count_ones() as usize > usize::from(m) {
                    continue;
                }
                if n > 100_000 && mask % 7 != 0 {
                    continue;
                }
                let have: Vec<Option<Vec<u8>>> = (0..total)
                    .map(|i| (mask >> i & 1 == 0).then(|| e.pieces[i].clone()))
                    .collect();
                assert_eq!(
                    decode(have, k, m, n as u64).unwrap(),
                    d,
                    "n={n} k={k} m={m} mask={mask:b}"
                );
            }
            // m + 1 missing is not enough.
            let have: Vec<Option<Vec<u8>>> = (0..total)
                .map(|i| (i > usize::from(m)).then(|| e.pieces[i].clone()))
                .collect();
            assert!(matches!(
                decode(have, k, m, n as u64),
                Err(ErasureError::NotEnoughPieces { .. })
            ));
        }
    }

    #[test]
    fn reconstruct_restores_the_exact_pieces_and_the_id() {
        let d = data(30_000);
        let e = encode(&d, 4, 2).unwrap();
        let mut have: Vec<Option<Vec<u8>>> = e.pieces.iter().cloned().map(Some).collect();
        have[1] = None;
        have[5] = None;
        reconstruct(&mut have, 4, 2).unwrap();
        for (a, b) in have.iter().zip(&e.pieces) {
            assert_eq!(a.as_ref().unwrap(), b);
        }
    }

    #[test]
    fn id_commits_to_content_size_and_shape() {
        let d = data(9000);
        let a = encode(&d, 3, 2).unwrap();
        assert_eq!(a.id, encode(&d, 3, 2).unwrap().id);
        let mut d2 = d.clone();
        d2[4000] ^= 1;
        assert_ne!(a.id, encode(&d2, 3, 2).unwrap().id);
        assert_ne!(a.id, encode(&d, 3, 3).unwrap().id);
        assert_ne!(a.id, encode(&d[..8999], 3, 2).unwrap().id);
    }

    #[test]
    fn piece_verification() {
        let e = encode(&data(7000), 3, 2).unwrap();
        for i in 0..5 {
            let path = e.piece_path(i);
            let pl = e.piece_len as u64;
            assert!(verify_piece(
                &e.id,
                e.size,
                3,
                2,
                pl,
                i,
                &e.pieces[i],
                &path
            ));
            let mut bad = e.pieces[i].clone();
            bad[0] ^= 1;
            assert!(!verify_piece(&e.id, e.size, 3, 2, pl, i, &bad, &path));
            assert!(!verify_piece(
                &e.id,
                e.size,
                3,
                2,
                pl,
                (i + 1) % 5,
                &e.pieces[i],
                &path
            ));
        }
    }

    #[test]
    fn bad_shapes() {
        assert!(matches!(encode(b"x", 0, 1), Err(ErasureError::BadShape)));
        assert!(matches!(encode(b"x", 1, 0), Err(ErasureError::BadShape)));
        assert!(matches!(
            encode(b"x", 200, 100),
            Err(ErasureError::BadShape)
        ));
    }
}
