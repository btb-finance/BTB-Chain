//! `store` and `fetch`: what `btb store <file>` and `btb fetch <root>` do.

use crate::chain::{self, Chain};
use crate::erasure::{self, verify_piece};
use crate::http;
use crate::{hex, Address, Hash};
use btb_chain::storage::StorageOp;

/// Result of [`store`].
#[derive(Debug, Clone)]
pub struct Stored {
    pub id: Hash,
    pub size: u64,
    pub k: u8,
    pub m: u8,
    /// `(node, endpoint)` of each piece, by index.
    pub assignments: Vec<(Address, String)>,
    /// Pieces that could not be uploaded (the file is less durable until they are repaired
    /// from the others, which the chain does after the first missed challenge).
    pub failed: Vec<(u8, String)>,
}

/// Encode `data`, pay for `periods` payment periods, and upload each piece to the node the
/// chain assigned it to.
pub fn store(chain: &dyn Chain, data: &[u8], k: u8, m: u8, periods: u32) -> Result<Stored, String> {
    if data.is_empty() {
        return Err("cannot store an empty file".into());
    }
    let enc = erasure::encode(data, k, m).map_err(|e| e.to_string())?;
    chain.send(StorageOp::Store {
        root: enc.id,
        size: enc.size,
        k,
        m,
        periods,
    })?;
    let view =
        chain::file(chain, &enc.id)?.ok_or("the chain does not know the file after Store")?;
    let mut failed = Vec::new();
    let results: Vec<(u8, Result<(), String>)> = std::thread::scope(|sc| {
        let hs: Vec<_> = view
            .pieces
            .iter()
            .map(|p| {
                let (enc, p) = (&enc, p);
                sc.spawn(move || {
                    let i = usize::from(p.index);
                    (
                        p.index,
                        http::put_piece(
                            &p.endpoint,
                            &enc.id,
                            p.index,
                            &enc.pieces[i],
                            &enc.piece_path(i),
                        ),
                    )
                })
            })
            .collect();
        hs.into_iter()
            .map(|h| h.join().expect("upload thread"))
            .collect()
    });
    for (i, r) in results {
        if let Err(e) = r {
            failed.push((i, e));
        }
    }
    Ok(Stored {
        id: enc.id,
        size: enc.size,
        k,
        m,
        assignments: view
            .pieces
            .iter()
            .map(|p| (p.node, p.endpoint.clone()))
            .collect(),
        failed,
    })
}

/// Download any `k` verified pieces of file `id`, decode, and check the result against `id`.
pub fn fetch(chain: &dyn Chain, id: &Hash) -> Result<Vec<u8>, String> {
    let f = chain::file(chain, id)?
        .ok_or_else(|| format!("the chain does not know file {}", hex(id)))?;
    let n = usize::from(f.k) + usize::from(f.m);
    let mut slots: Vec<Option<Vec<u8>>> = vec![None; n];
    let (mut got, mut errors) = (0usize, Vec::new());
    for p in &f.pieces {
        if got >= usize::from(f.k) {
            break;
        }
        match http::get_piece(&p.endpoint, id, p.index) {
            Ok((bytes, path)) => {
                if verify_piece(
                    id,
                    f.size,
                    f.k,
                    f.m,
                    f.piece_len,
                    p.index.into(),
                    &bytes,
                    &path,
                ) {
                    slots[usize::from(p.index)] = Some(bytes);
                    got += 1;
                } else {
                    errors.push(format!(
                        "piece {} from {}: fails verification",
                        p.index, p.endpoint
                    ));
                }
            }
            Err(e) => errors.push(e),
        }
    }
    if got < usize::from(f.k) {
        return Err(format!(
            "only {got} of {} needed pieces reachable: {}",
            f.k,
            errors.join("; ")
        ));
    }
    let data = erasure::decode(slots, f.k, f.m, f.size).map_err(|e| e.to_string())?;
    // The id commits to every piece: encoding again must reproduce it.
    let again = erasure::encode(&data, f.k, f.m).map_err(|e| e.to_string())?;
    if &again.id != id {
        return Err("decoded data does not match the file id".into());
    }
    Ok(data)
}
