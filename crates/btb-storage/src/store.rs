//! Pieces on disk: `<dir>/<file id hex>/<index>.piece`.
//!
//! A piece file is `path_len:u8 || path (32 bytes each) || piece bytes`, where `path` is the
//! Merkle path from the piece's root to the file tree root (given by the uploader and checked
//! against the file id before it is stored). Files are written to a temporary name and renamed,
//! so a crash never leaves a half-written piece under its final name.

use crate::{hex, Hash};
use std::io;
use std::path::{Path, PathBuf};

pub struct PieceStore {
    dir: PathBuf,
}

impl PieceStore {
    pub fn open(dir: &Path) -> io::Result<PieceStore> {
        std::fs::create_dir_all(dir)?;
        Ok(PieceStore {
            dir: dir.to_path_buf(),
        })
    }

    fn file_dir(&self, file: &Hash) -> PathBuf {
        self.dir.join(hex(file))
    }

    fn path_of(&self, file: &Hash, index: u8) -> PathBuf {
        self.file_dir(file).join(format!("{index}.piece"))
    }

    pub fn put(&self, file: &Hash, index: u8, piece: &[u8], path: &[Hash]) -> io::Result<()> {
        if path.len() > 255 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "path too long"));
        }
        let dir = self.file_dir(file);
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!("{index}.tmp"));
        let mut buf = Vec::with_capacity(1 + path.len() * 32 + piece.len());
        buf.push(path.len() as u8);
        for h in path {
            buf.extend_from_slice(h);
        }
        buf.extend_from_slice(piece);
        std::fs::write(&tmp, &buf)?;
        std::fs::rename(&tmp, self.path_of(file, index))
    }

    pub fn has(&self, file: &Hash, index: u8) -> bool {
        self.path_of(file, index).is_file()
    }

    /// The piece and its Merkle path, or `None` if not stored (or the file is damaged).
    pub fn get(&self, file: &Hash, index: u8) -> io::Result<Option<(Vec<u8>, Vec<Hash>)>> {
        let b = match std::fs::read(self.path_of(file, index)) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let n = usize::from(*b.first().ok_or_else(|| bad("empty piece file"))?);
        let head = 1 + n * 32;
        if b.len() < head {
            return Err(bad("truncated piece file"));
        }
        let path = b[1..head]
            .chunks(32)
            .map(|c| c.try_into().expect("32 bytes"))
            .collect();
        Ok(Some((b[head..].to_vec(), path)))
    }

    pub fn remove(&self, file: &Hash, index: u8) -> io::Result<()> {
        match std::fs::remove_file(self.path_of(file, index)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let _ = std::fs::remove_dir(self.file_dir(file)); // only if empty
        Ok(())
    }

    /// Every stored `(file, index)`.
    pub fn list(&self) -> io::Result<Vec<(Hash, u8)>> {
        let mut out = Vec::new();
        for d in std::fs::read_dir(&self.dir)? {
            let d = d?;
            let Some(file) = d.file_name().to_str().and_then(|n| crate::unhex32(n).ok()) else {
                continue;
            };
            for f in std::fs::read_dir(d.path())? {
                let f = f?;
                if let Some(i) = f
                    .file_name()
                    .to_str()
                    .and_then(|n| n.strip_suffix(".piece"))
                    .and_then(|n| n.parse::<u8>().ok())
                {
                    out.push((file, i));
                }
            }
        }
        out.sort();
        Ok(out)
    }
}

fn bad(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_list_remove() {
        let d = std::env::temp_dir().join(format!("btb-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let s = PieceStore::open(&d).unwrap();
        let f = [7u8; 32];
        assert!(s.get(&f, 0).unwrap().is_none());
        s.put(&f, 2, b"hello", &[[1; 32], [2; 32]]).unwrap();
        s.put(&f, 0, b"x", &[]).unwrap();
        assert_eq!(
            s.get(&f, 2).unwrap().unwrap(),
            (b"hello".to_vec(), vec![[1; 32], [2; 32]])
        );
        assert_eq!(s.list().unwrap(), vec![(f, 0), (f, 2)]);
        s.remove(&f, 2).unwrap();
        s.remove(&f, 0).unwrap();
        assert!(s.list().unwrap().is_empty());
        assert!(!d.join(hex(&f)).exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
