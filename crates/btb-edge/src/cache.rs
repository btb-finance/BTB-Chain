//! Byte-bounded LRU cache with an optional disk tier.
//!
//! Entries are immutable values behind an `Arc`. Content-addressed entries (site blobs by
//! SHA-256, storage files by root) never expire and may spill to disk when the memory budget is
//! exceeded; query results (keyed by height) live in memory only.

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub type Hash = [u8; 32];

/// A query result variant at one block height.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct QKey {
    pub contract: String,
    pub func: String,
    pub args: String,
    pub caller: String,
    pub proof: bool,
    pub height: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// Site file content, by SHA-256.
    Blob(Hash),
    /// Storage-backed file, by its storage root.
    File(Hash),
    Query(QKey),
}

impl Key {
    fn disk_name(&self) -> Option<String> {
        let h = |p: &str, x: &Hash| format!("{p}-{}", btb_storage::hex(x));
        match self {
            Key::Blob(x) => Some(h("b", x)),
            Key::File(x) => Some(h("f", x)),
            Key::Query(_) => None,
        }
    }
    fn from_disk_name(n: &str) -> Option<Key> {
        let (p, x) = n.split_once('-')?;
        let x = btb_storage::unhex32(x).ok()?;
        match p {
            "b" => Some(Key::Blob(x)),
            "f" => Some(Key::File(x)),
            _ => None,
        }
    }
}

struct Lru {
    map: HashMap<Key, (Arc<Vec<u8>>, u64)>,
    order: BTreeMap<u64, Key>,
    tick: u64,
    bytes: usize,
}

struct Disk {
    dir: PathBuf,
    budget: u64,
    inner: Mutex<DiskIndex>,
}

#[derive(Default)]
struct DiskIndex {
    files: HashMap<String, (u64, u64)>, // name -> (tick, size)
    order: BTreeMap<u64, String>,
    tick: u64,
    bytes: u64,
}

pub struct Cache {
    mem_budget: usize,
    lru: Mutex<Lru>,
    disk: Option<Disk>,
}

impl Cache {
    /// `disk`: directory and byte budget of the disk tier (existing files are adopted).
    pub fn new(mem_budget: usize, disk: Option<(PathBuf, u64)>) -> std::io::Result<Cache> {
        let disk = match disk {
            Some((dir, budget)) if budget > 0 => {
                std::fs::create_dir_all(&dir)?;
                let mut found: Vec<(std::time::SystemTime, String, u64)> = Vec::new();
                for e in std::fs::read_dir(&dir)?.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if Key::from_disk_name(&name).is_none() {
                        let _ = std::fs::remove_file(e.path()); // stray temp file
                        continue;
                    }
                    if let Ok(md) = e.metadata() {
                        found.push((
                            md.modified().unwrap_or(std::time::UNIX_EPOCH),
                            name,
                            md.len(),
                        ));
                    }
                }
                found.sort();
                let mut idx = DiskIndex::default();
                for (_, name, size) in found {
                    idx.tick += 1;
                    idx.files.insert(name.clone(), (idx.tick, size));
                    idx.order.insert(idx.tick, name);
                    idx.bytes += size;
                }
                let d = Disk {
                    dir,
                    budget,
                    inner: Mutex::new(idx),
                };
                d.evict();
                Some(d)
            }
            _ => None,
        };
        Ok(Cache {
            mem_budget,
            lru: Mutex::new(Lru {
                map: HashMap::new(),
                order: BTreeMap::new(),
                tick: 0,
                bytes: 0,
            }),
            disk,
        })
    }

    pub fn get(&self, key: &Key) -> Option<Arc<Vec<u8>>> {
        {
            let mut l = self.lru.lock().unwrap_or_else(|e| e.into_inner());
            l.tick += 1;
            let t = l.tick;
            if let Some((v, old)) = l.map.get_mut(key) {
                let v = v.clone();
                let old = std::mem::replace(old, t);
                l.order.remove(&old);
                l.order.insert(t, key.clone());
                return Some(v);
            }
        }
        let d = self.disk.as_ref()?;
        let v = Arc::new(d.read(key)?);
        self.put_mem(key.clone(), v.clone());
        Some(v)
    }

    pub fn put(&self, key: Key, value: Arc<Vec<u8>>) {
        if value.len() > self.mem_budget {
            // Too big for memory: disk only (if allowed), otherwise not cached at all.
            if let Some(d) = &self.disk {
                d.write(&key, &value);
            }
            return;
        }
        self.put_mem(key, value);
    }

    fn put_mem(&self, key: Key, value: Arc<Vec<u8>>) {
        if value.len() > self.mem_budget {
            return;
        }
        let mut spilled = Vec::new();
        {
            let mut l = self.lru.lock().unwrap_or_else(|e| e.into_inner());
            l.tick += 1;
            let t = l.tick;
            if let Some((old_v, old_t)) = l.map.remove(&key) {
                l.order.remove(&old_t);
                l.bytes -= old_v.len();
            }
            l.bytes += value.len();
            l.order.insert(t, key.clone());
            l.map.insert(key, (value, t));
            while l.bytes > self.mem_budget {
                let Some((_, k)) = l.order.pop_first() else {
                    break;
                };
                if let Some((v, _)) = l.map.remove(&k) {
                    l.bytes -= v.len();
                    spilled.push((k, v));
                }
            }
        }
        if let Some(d) = &self.disk {
            for (k, v) in spilled {
                d.write(&k, &v);
            }
        }
    }

    pub fn remove(&self, key: &Key) {
        let mut l = self.lru.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((v, t)) = l.map.remove(key) {
            l.order.remove(&t);
            l.bytes -= v.len();
        }
    }

    pub fn mem_bytes(&self) -> usize {
        self.lru.lock().unwrap_or_else(|e| e.into_inner()).bytes
    }
    pub fn mem_entries(&self) -> usize {
        self.lru.lock().unwrap_or_else(|e| e.into_inner()).map.len()
    }
    pub fn disk_bytes(&self) -> u64 {
        self.disk.as_ref().map_or(0, |d| {
            d.inner.lock().unwrap_or_else(|e| e.into_inner()).bytes
        })
    }
}

impl Disk {
    fn read(&self, key: &Key) -> Option<Vec<u8>> {
        let name = key.disk_name()?;
        {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            i.tick += 1;
            let t = i.tick;
            let (old, _) = *i.files.get(&name)?;
            i.order.remove(&old);
            i.order.insert(t, name.clone());
            if let Some(f) = i.files.get_mut(&name) {
                f.0 = t;
            }
        }
        let mut b = std::fs::read(self.dir.join(&name)).ok()?;
        // Trailer: SHA-256 of the body, so bit rot or tampering on disk is a miss, not a response.
        if b.len() >= 32 {
            let tail = b.split_off(b.len() - 32);
            if Sha256::digest(&b).as_slice() == tail.as_slice() {
                return Some(b);
            }
        }
        self.forget(&name);
        None
    }

    fn forget(&self, name: &str) {
        let _ = std::fs::remove_file(self.dir.join(name));
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((t, size)) = i.files.remove(name) {
            i.order.remove(&t);
            i.bytes -= size;
        }
    }

    fn write(&self, key: &Key, value: &[u8]) {
        let Some(name) = key.disk_name() else { return };
        let size = value.len() as u64 + 32;
        if size > self.budget {
            return;
        }
        {
            let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if i.files.contains_key(&name) {
                return; // content addressed: already there
            }
        }
        let tmp = self.dir.join(format!("{name}.tmp"));
        let mut buf = Vec::with_capacity(value.len() + 32);
        buf.extend_from_slice(value);
        buf.extend_from_slice(&Sha256::digest(value));
        if std::fs::write(&tmp, &buf)
            .and_then(|_| std::fs::rename(&tmp, self.dir.join(&name)))
            .is_err()
        {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            i.tick += 1;
            let t = i.tick;
            i.files.insert(name.clone(), (t, size));
            i.order.insert(t, name);
            i.bytes += size;
        }
        self.evict();
    }

    fn evict(&self) {
        loop {
            let victim = {
                let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if i.bytes <= self.budget {
                    return;
                }
                i.order.first_key_value().map(|(_, n)| n.clone())
            };
            match victim {
                Some(n) => self.forget(&n),
                None => return,
            }
        }
    }
}
