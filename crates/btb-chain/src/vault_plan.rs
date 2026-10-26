//! Chain-side checks of the unsigned external transactions that validators propose for a
//! withdrawal (`docs/CROSS_CHAIN.md`, "Withdrawal plans").
//!
//! * **Bitcoin.** The plan is the transaction without witnesses (the bytes that hash to its
//!   txid). The chain parses it, checks the inputs against the vault's UTXO set, that output 0
//!   pays the destination, that the optional output 1 returns change to the vault, that the
//!   outflow equals the burned amount (up to dust), and recomputes the BIP-341 key-path
//!   signature hashes itself. Nothing here trusts the proposers.
//! * **Solana.** The plan is a legacy message with exactly one System `Transfer` from the vault
//!   to the destination of the burned amount. The message itself is what FROST signs.
//!
//! Pure functions: no state, no I/O.

use sha2::{Digest, Sha256};

/// Smallest output Bitcoin Core relays for a P2TR script (dust limit).
pub const DUST: u64 = 330;
pub const MAX_INPUTS: usize = 16;
pub const MAX_PLAN: usize = 4096;

pub fn sha256d(b: &[u8]) -> [u8; 32] {
    Sha256::digest(Sha256::digest(b)).into()
}

fn tagged(tag: &str, data: &[u8]) -> [u8; 32] {
    let t = Sha256::digest(tag.as_bytes());
    let mut h = Sha256::new();
    h.update(t);
    h.update(t);
    h.update(data);
    h.finalize().into()
}

/// An unsigned (witness-free) Bitcoin transaction as the chain understands it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtcTx {
    pub version: i32,
    /// `(txid in internal byte order, vout, sequence)`
    pub inputs: Vec<([u8; 32], u32, u32)>,
    /// `(value, scriptPubKey)`
    pub outputs: Vec<(u64, Vec<u8>)>,
    pub lock_time: u32,
}

struct Rd<'a>(&'a [u8]);
impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (a, b) = (self.0.get(..n)?, self.0.get(n..)?);
        self.0 = b;
        Some(a)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    /// Canonical CompactSize only.
    fn varint(&mut self) -> Option<u64> {
        let f = self.take(1)?[0];
        Some(match f {
            0..=0xFC => u64::from(f),
            0xFD => {
                let v = u64::from(u16::from_le_bytes(self.take(2)?.try_into().ok()?));
                (v >= 0xFD).then_some(v)?
            }
            0xFE => {
                let v = u64::from(self.u32()?);
                (v > 0xFFFF).then_some(v)?
            }
            _ => return None,
        })
    }
}

fn put_varint(o: &mut Vec<u8>, n: u64) {
    match n {
        0..=0xFC => o.push(n as u8),
        0xFD..=0xFFFF => {
            o.push(0xFD);
            o.extend_from_slice(&(n as u16).to_le_bytes());
        }
        _ => {
            o.push(0xFE);
            o.extend_from_slice(&(n as u32).to_le_bytes());
        }
    }
}

impl BtcTx {
    /// Strict parser: legacy serialization, empty scriptSigs, no trailing bytes.
    pub fn parse(b: &[u8]) -> Option<BtcTx> {
        if b.len() > MAX_PLAN {
            return None;
        }
        let mut r = Rd(b);
        let version = r.u32()? as i32;
        let n_in = r.varint()? as usize;
        if n_in == 0 || n_in > MAX_INPUTS {
            return None;
        }
        let mut inputs = Vec::with_capacity(n_in);
        for _ in 0..n_in {
            let txid: [u8; 32] = r.take(32)?.try_into().ok()?;
            let vout = r.u32()?;
            if r.varint()? != 0 {
                return None; // scriptSig must be empty (taproot key-path spends)
            }
            inputs.push((txid, vout, r.u32()?));
        }
        let n_out = r.varint()? as usize;
        if n_out == 0 || n_out > 8 {
            return None;
        }
        let mut outputs = Vec::with_capacity(n_out);
        for _ in 0..n_out {
            let v = r.u64()?;
            let l = r.varint()? as usize;
            outputs.push((v, r.take(l)?.to_vec()));
        }
        let lock_time = r.u32()?;
        r.0.is_empty().then_some(BtcTx {
            version,
            inputs,
            outputs,
            lock_time,
        })
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut o = self.version.to_le_bytes().to_vec();
        put_varint(&mut o, self.inputs.len() as u64);
        for (t, v, s) in &self.inputs {
            o.extend_from_slice(t);
            o.extend_from_slice(&v.to_le_bytes());
            o.push(0);
            o.extend_from_slice(&s.to_le_bytes());
        }
        put_varint(&mut o, self.outputs.len() as u64);
        for (v, s) in &self.outputs {
            o.extend_from_slice(&v.to_le_bytes());
            put_varint(&mut o, s.len() as u64);
            o.extend_from_slice(s);
        }
        o.extend_from_slice(&self.lock_time.to_le_bytes());
        o
    }

    /// The txid in internal byte order (what outpoints and claims carry).
    pub fn txid(&self) -> [u8; 32] {
        sha256d(&self.serialize())
    }

    /// BIP-341 signature hash, SIGHASH_DEFAULT, key-path spend, no annex, of input `idx`;
    /// `prevouts[i]` is `(amount, scriptPubKey)` of input `i`.
    pub fn taproot_sighash(&self, idx: usize, prevouts: &[(u64, Vec<u8>)]) -> Option<[u8; 32]> {
        if prevouts.len() != self.inputs.len() || idx >= self.inputs.len() {
            return None;
        }
        let sha = |f: &dyn Fn(&mut Vec<u8>)| {
            let mut v = Vec::new();
            f(&mut v);
            Sha256::digest(&v)
        };
        let prev = sha(&|v| {
            for (t, n, _) in &self.inputs {
                v.extend_from_slice(t);
                v.extend_from_slice(&n.to_le_bytes());
            }
        });
        let amounts = sha(&|v| {
            for (a, _) in prevouts {
                v.extend_from_slice(&a.to_le_bytes());
            }
        });
        let scripts = sha(&|v| {
            for (_, s) in prevouts {
                put_varint(v, s.len() as u64);
                v.extend_from_slice(s);
            }
        });
        let seqs = sha(&|v| {
            for (_, _, s) in &self.inputs {
                v.extend_from_slice(&s.to_le_bytes());
            }
        });
        let outs = sha(&|v| {
            for (a, s) in &self.outputs {
                v.extend_from_slice(&a.to_le_bytes());
                put_varint(v, s.len() as u64);
                v.extend_from_slice(s);
            }
        });
        let mut m = vec![0x00, 0x00]; // epoch, SIGHASH_DEFAULT
        m.extend_from_slice(&self.version.to_le_bytes());
        m.extend_from_slice(&self.lock_time.to_le_bytes());
        for h in [&prev, &amounts, &scripts, &seqs, &outs] {
            m.extend_from_slice(h);
        }
        m.push(0x00); // spend type: key path, no annex
        m.extend_from_slice(&(idx as u32).to_le_bytes());
        Some(tagged("TapSighash", &m))
    }
}

/// Why a plan was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanError {
    Malformed,
    /// Does not pay what was burned, to whom it was burned for, or change elsewhere.
    Mismatch,
}

/// The checked Bitcoin plan: what the chain needs to update its UTXO set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtcPlan {
    pub tx: BtcTx,
    /// Index and value of the change output returning to the vault, if any.
    pub change: Option<(u32, u64)>,
}

/// Check the structure and the payment of a Bitcoin plan against the burn. `input_amounts[i]`
/// is the vault UTXO value of input `i` (the caller checked they exist).
pub fn check_btc(
    plan: &[u8],
    destination: &[u8],
    amount: u64,
    vault_script: &[u8],
    input_amounts: &dyn Fn(&[u8; 32], u32) -> Option<u64>,
) -> Result<(BtcPlan, Vec<u64>), PlanError> {
    let tx = BtcTx::parse(plan).ok_or(PlanError::Malformed)?;
    if tx.serialize() != plan {
        return Err(PlanError::Malformed);
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut amounts = Vec::new();
    for (t, v, _) in &tx.inputs {
        if !seen.insert((*t, *v)) {
            return Err(PlanError::Mismatch);
        }
        amounts.push(input_amounts(t, *v).ok_or(PlanError::Mismatch)?);
    }
    let sum_in: u64 = amounts
        .iter()
        .try_fold(0u64, |a, v| a.checked_add(*v))
        .ok_or(PlanError::Mismatch)?;
    let (v0, s0) = &tx.outputs[0];
    if s0 != destination || *v0 < DUST {
        return Err(PlanError::Mismatch);
    }
    let change = match tx.outputs.len() {
        1 => None,
        2 if tx.outputs[1].1 == vault_script && tx.outputs[1].0 >= DUST => {
            Some((1u32, tx.outputs[1].0))
        }
        _ => return Err(PlanError::Mismatch),
    };
    let outflow = sum_in
        .checked_sub(change.map_or(0, |c| c.1))
        .ok_or(PlanError::Mismatch)?;
    // the vault pays out exactly what was burned; without a change output a sub-dust remainder
    // may go to the miners
    if outflow < amount || outflow - amount >= DUST || *v0 > amount {
        return Err(PlanError::Mismatch);
    }
    Ok((BtcPlan { tx, change }, amounts))
}

/// A legacy Solana message with one System transfer `from -> to` of `lamports`.
pub fn solana_transfer_message(
    from: &[u8; 32],
    to: &[u8; 32],
    lamports: u64,
    blockhash: &[u8; 32],
) -> Vec<u8> {
    let mut m = vec![1, 0, 1, 3]; // 1 signer, 0 readonly signed, 1 readonly unsigned, 3 keys
    m.extend_from_slice(from);
    m.extend_from_slice(to);
    m.extend_from_slice(&[0u8; 32]); // System program
    m.extend_from_slice(blockhash);
    m.push(1); // one instruction
    m.push(2); // program index: System
    m.push(2); // two accounts
    m.extend_from_slice(&[0, 1]);
    m.push(12); // data length
    m.extend_from_slice(&2u32.to_le_bytes()); // Transfer
    m.extend_from_slice(&lamports.to_le_bytes());
    m
}

/// Check that `plan` is exactly the transfer of `amount` lamports from the vault to
/// `destination` (any blockhash).
pub fn check_sol(
    plan: &[u8],
    vault: &[u8; 32],
    destination: &[u8],
    amount: u64,
) -> Result<(), PlanError> {
    let to: [u8; 32] = destination.try_into().map_err(|_| PlanError::Mismatch)?;
    if plan.len() != 4 + 96 + 32 + 1 + 1 + 1 + 2 + 1 + 12 {
        return Err(PlanError::Malformed);
    }
    let bh: [u8; 32] = plan[100..132].try_into().expect("32");
    if plan == solana_transfer_message(vault, &to, amount, &bh).as_slice() {
        Ok(())
    } else {
        Err(PlanError::Mismatch)
    }
}
