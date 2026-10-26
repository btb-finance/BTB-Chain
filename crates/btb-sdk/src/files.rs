//! File storage from contracts (`docs/STORAGE.md`, "Files from contracts").
//!
//! A contract never holds file bytes. It holds the 32-byte **root** of a file that lives on
//! storage nodes (or, in `btb dev`, on the dev server's disk). A client uploads the bytes off
//! chain, gets the root back, and passes it to a mutation:
//!
//! ```ignore
//! #[btb::mutation]
//! pub fn attach(note_id: u64, root: [u8; 32]) {
//!     // The file must be stored and paid for, otherwise anyone could point a note at junk.
//!     let f = files::require_stored(&root);
//!     Notes::update(note_id, Note { root, size: f.size, ..note });
//! }
//!
//! #[btb::query]
//! pub fn is_available(root: [u8; 32]) -> bool {
//!     files::is_stored(&root)
//! }
//! ```
//!
//! Who pays: either the client sends a `StoreFile` transaction (`btb store`), or the contract
//! pays from its own balance with [`pin`], which takes the same escrow a `Store` would.

use crate::host::{abort, ecall};

/// Host-call numbers and statuses; checked against `btb_vm::host::abi` by a test in
/// `btb-tests`.
pub mod num {
    pub const FILE_INFO: u64 = 70;
    pub const FILE_PIN: u64 = 71;
    pub const FILE_PIN_REQUEST_LEN: usize = 46;
    pub const FILE_UNKNOWN: u8 = 0;
    pub const FILE_STORED: u8 = 1;
    pub const FILE_EXPIRED: u8 = 2;
    pub const FILE_PIN_OK: u64 = 0;
    pub const FILE_PIN_EXISTS: u64 = 1;
    pub const FILE_PIN_INSUFFICIENT: u64 = 2;
    pub const FILE_PIN_NO_NODES: u64 = 3;
    pub const FILE_PIN_INVALID: u64 = 4;
    pub const FILE_PIN_UNSUPPORTED: u64 = 5;
    pub const NOT_FOUND: u64 = u64::MAX;
}

/// A file root: the 64-hex id `btb store` prints and the upload returns.
pub type Root = [u8; 32];

/// Where a root stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    /// The chain has never seen this root (or the host has no file storage).
    Unknown,
    /// Stored and paid: the nodes are being paid and challenged.
    Stored,
    /// It was stored and its paid periods have all run.
    Expired,
}

/// What [`info`] returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileInfo {
    /// Bytes, 0 for an unknown root.
    pub size: u64,
    pub status: FileStatus,
    /// Block height by which the paid periods have all run; 0 for an unknown root.
    pub expiry: u64,
}

impl FileInfo {
    pub fn is_stored(&self) -> bool {
        self.status == FileStatus::Stored
    }
}

/// Size, status and expiry of `root`. Readable in queries, mutations and crons (400 gas). A root
/// the chain does not know is `FileStatus::Unknown`, not an error.
pub fn info(root: &Root) -> FileInfo {
    let mut out = [0u8; 24];
    let r = ecall(
        num::FILE_INFO,
        root.as_ptr() as u64,
        out.as_mut_ptr() as u64,
        0,
        0,
        0,
    );
    if r != 0 {
        // A host without file storage: nothing is stored there.
        return FileInfo {
            size: 0,
            status: FileStatus::Unknown,
            expiry: 0,
        };
    }
    FileInfo {
        size: u64::from_le_bytes(out[..8].try_into().unwrap_or([0; 8])),
        status: match out[8] {
            num::FILE_STORED => FileStatus::Stored,
            num::FILE_EXPIRED => FileStatus::Expired,
            _ => FileStatus::Unknown,
        },
        expiry: u64::from_le_bytes(out[16..24].try_into().unwrap_or([0; 8])),
    }
}

/// Is `root` stored and paid right now?
pub fn is_stored(root: &Root) -> bool {
    info(root).is_stored()
}

/// The file's info, aborting unless it is stored and paid.
pub fn require_stored(root: &Root) -> FileInfo {
    let f = info(root);
    if !f.is_stored() {
        abort("file is not stored");
    }
    f
}

/// Why [`pin`] failed. Nothing was paid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinError {
    /// The root is already stored and live.
    Exists,
    /// This contract's balance cannot cover the escrow.
    Insufficient,
    /// Fewer eligible storage nodes than pieces.
    NoNodes,
    /// Bad shape: size 0 or too large, `k`/`m` out of range, `periods` 0 or too large.
    Invalid,
    /// The host has no file storage.
    Unsupported,
}

impl PinError {
    pub fn message(&self) -> &'static str {
        match self {
            PinError::Exists => "file is already stored",
            PinError::Insufficient => "contract balance cannot cover the storage escrow",
            PinError::NoNodes => "not enough storage nodes",
            PinError::Invalid => "bad file shape",
            PinError::Unsupported => "this host has no file storage",
        }
    }
}

/// Store `root` for `periods` payment periods, paying the escrow from **this contract's
/// balance**. Mutations and crons only (30,000 gas, also when refused). `size`, `k` and `m` are
/// the values the upload returned next to the root: the root commits to them but the chain
/// cannot recover them from it. The pieces must then be uploaded to the assigned storage nodes
/// (the upload service does this once the pin is on chain).
pub fn pin(root: &Root, size: u64, k: u8, m: u8, periods: u32) -> Result<(), PinError> {
    let mut req = [0u8; num::FILE_PIN_REQUEST_LEN];
    req[..32].copy_from_slice(root);
    req[32..40].copy_from_slice(&size.to_le_bytes());
    req[40] = k;
    req[41] = m;
    req[42..46].copy_from_slice(&periods.to_le_bytes());
    match ecall(
        num::FILE_PIN,
        req.as_ptr() as u64,
        req.len() as u64,
        0,
        0,
        0,
    ) {
        num::FILE_PIN_OK => Ok(()),
        num::FILE_PIN_EXISTS => Err(PinError::Exists),
        num::FILE_PIN_INSUFFICIENT => Err(PinError::Insufficient),
        num::FILE_PIN_NO_NODES => Err(PinError::NoNodes),
        num::FILE_PIN_INVALID => Err(PinError::Invalid),
        _ => Err(PinError::Unsupported),
    }
}

/// [`pin`], but a root that is already stored counts as done.
pub fn ensure_pinned(root: &Root, size: u64, k: u8, m: u8, periods: u32) -> Result<(), PinError> {
    match pin(root, size, k, m, periods) {
        Err(PinError::Exists) => Ok(()),
        r => r,
    }
}
