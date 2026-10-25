//! BTB storage nodes (off-chain half of `btb_chain::storage`; design in `docs/STORAGE.md`).
//!
//! * [`erasure`]: split a file into `k` data + `m` parity pieces (Reed-Solomon over GF(2^8)),
//!   rebuild any missing pieces from any `k`, and compute the file id (the Merkle commitment the
//!   chain stores) with [`proof`].
//! * [`store`]: pieces on disk. [`http`]: the piece API (`PUT`/`GET` with Merkle paths, chunk
//!   proofs) and its client.
//! * [`daemon`]: a storage node: serves pieces, answers the chain's challenges with proofs,
//!   rebuilds pieces it is assigned in a repair.
//! * [`client`]: `store` (encode, pay, upload) and `fetch` (download any `k`, decode, verify).
//!
//! The chain is reached through the [`chain::Chain`] trait (implemented by `btb-cli` over the
//! node's JSON-RPC), so everything here is testable without a network.

pub mod chain;
pub mod client;
pub mod daemon;
pub mod erasure;
pub mod http;
pub mod store;

pub use btb_chain::storage::proof;
pub use btb_chain::tx::{Address, Hash};

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return Err(format!("bad hex {s:?}"));
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|_| format!("bad hex {s:?}")))
        .collect()
}

pub fn unhex32(s: &str) -> Result<[u8; 32], String> {
    unhex(s)?
        .try_into()
        .map_err(|_| format!("{s:?} is not 32 bytes of hex"))
}
