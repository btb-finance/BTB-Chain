//! Canonical binary encoding of messages. Decoding is total: any byte string yields
//! `None` or a structurally valid value, never a panic, and allocation is bounded by input size.

use crate::types::*;

const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

struct W(Vec<u8>);
impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn hash(&mut self, h: &Hash) {
        self.0.extend_from_slice(h);
    }
    fn sig(&mut self, s: &Sig) {
        self.0.extend_from_slice(&s.0);
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
}

struct R<'a> {
    buf: &'a [u8],
    pos: usize,
}
impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    fn left(&self) -> usize {
        self.buf.len() - self.pos
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn hash(&mut self) -> Option<Hash> {
        self.take(32)?.try_into().ok()
    }
    fn sig(&mut self) -> Option<Sig> {
        Some(Sig(self.take(64)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        if n > MAX_PAYLOAD {
            return None;
        }
        Some(self.take(n)?.to_vec())
    }
    /// Element count that cannot exceed what the remaining input could hold.
    fn count(&mut self, min_elem: usize) -> Option<usize> {
        let n = self.u32()? as usize;
        if n > MAX_VALIDATORS * 4 || n.checked_mul(min_elem)? > self.left() {
            return None;
        }
        Some(n)
    }
    fn flag(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
}

fn put_qc(w: &mut W, q: &QuorumCert) {
    w.u64(q.round);
    w.hash(&q.block_id);
    w.u32(q.votes.len() as u32);
    for (i, s) in &q.votes {
        w.u32(*i);
        w.sig(s);
    }
}
fn get_qc(r: &mut R) -> Option<QuorumCert> {
    let round = r.u64()?;
    let block_id = r.hash()?;
    let n = r.count(68)?;
    let mut votes = Vec::with_capacity(n);
    for _ in 0..n {
        votes.push((r.u32()?, r.sig()?));
    }
    Some(QuorumCert {
        round,
        block_id,
        votes,
    })
}
fn put_tc(w: &mut W, t: &TimeoutCert) {
    w.u64(t.round);
    w.u32(t.entries.len() as u32);
    for e in &t.entries {
        w.u32(e.voter);
        w.u64(e.qc_round);
        w.sig(&e.sig);
    }
    put_qc(w, &t.high_qc);
}
fn get_tc(r: &mut R) -> Option<TimeoutCert> {
    let round = r.u64()?;
    let n = r.count(76)?;
    let mut entries = Vec::with_capacity(n);
    for _ in 0..n {
        entries.push(TcEntry {
            voter: r.u32()?,
            qc_round: r.u64()?,
            sig: r.sig()?,
        });
    }
    Some(TimeoutCert {
        round,
        entries,
        high_qc: get_qc(r)?,
    })
}
fn put_block(w: &mut W, b: &Block) {
    w.u64(b.round);
    w.u64(b.height);
    w.hash(&b.parent);
    put_qc(w, &b.justify);
    w.u32(b.proposer);
    w.hash(&b.payload_hash);
    w.bytes(&b.payload);
}
fn get_block(r: &mut R) -> Option<Block> {
    Some(Block {
        round: r.u64()?,
        height: r.u64()?,
        parent: r.hash()?,
        justify: get_qc(r)?,
        proposer: r.u32()?,
        payload_hash: r.hash()?,
        payload: r.bytes()?,
    })
}
fn put_opt_tc(w: &mut W, t: &Option<TimeoutCert>) {
    match t {
        Some(t) => {
            w.u8(1);
            put_tc(w, t)
        }
        None => w.u8(0),
    }
}
fn get_opt_tc(r: &mut R) -> Option<Option<TimeoutCert>> {
    Some(if r.flag()? { Some(get_tc(r)?) } else { None })
}
fn put_ev(w: &mut W, e: &Evidence) {
    let (kind, who, round, a, b) = match e {
        Evidence::DoubleVote { voter, round, a, b } => (0, voter, round, a, b),
        Evidence::DoubleProposal {
            proposer,
            round,
            a,
            b,
        } => (1, proposer, round, a, b),
    };
    w.u8(kind);
    w.u32(*who);
    w.u64(*round);
    w.hash(&a.0);
    w.sig(&a.1);
    w.hash(&b.0);
    w.sig(&b.1);
}
fn get_ev(r: &mut R) -> Option<Evidence> {
    let kind = r.u8()?;
    let who = r.u32()?;
    let round = r.u64()?;
    let a = (r.hash()?, r.sig()?);
    let b = (r.hash()?, r.sig()?);
    match kind {
        0 => Some(Evidence::DoubleVote {
            voter: who,
            round,
            a,
            b,
        }),
        1 => Some(Evidence::DoubleProposal {
            proposer: who,
            round,
            a,
            b,
        }),
        _ => None,
    }
}

pub(crate) fn encode_qc(q: &QuorumCert) -> Vec<u8> {
    let mut w = W(Vec::new());
    put_qc(&mut w, q);
    w.0
}
pub(crate) fn encode_tc(t: &TimeoutCert) -> Vec<u8> {
    let mut w = W(Vec::new());
    put_tc(&mut w, t);
    w.0
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(Vec::new());
        match self {
            Message::Propose(p) => {
                w.u8(0);
                put_block(&mut w, &p.block);
                put_opt_tc(&mut w, &p.tc);
                w.sig(&p.sig);
            }
            Message::Vote(v) => {
                w.u8(1);
                w.u64(v.round);
                w.hash(&v.block_id);
                w.u32(v.voter);
                w.sig(&v.sig);
                w.sig(&v.proposal_sig);
            }
            Message::Timeout(t) => {
                w.u8(2);
                w.u64(t.round);
                put_qc(&mut w, &t.high_qc);
                w.u32(t.voter);
                w.sig(&t.sig);
                match &t.seen_proposal {
                    Some((h, s)) => {
                        w.u8(1);
                        w.hash(h);
                        w.sig(s);
                    }
                    None => w.u8(0),
                }
            }
            Message::Catchup { qc, tc } => {
                w.u8(3);
                put_qc(&mut w, qc);
                put_opt_tc(&mut w, tc);
            }
            Message::SyncRequest { from, block_id } => {
                w.u8(4);
                w.u32(*from);
                w.hash(block_id);
            }
            Message::SyncResponse { blocks } => {
                w.u8(5);
                w.u32(blocks.len() as u32);
                for b in blocks {
                    put_block(&mut w, b);
                }
            }
            Message::Evidence(e) => {
                w.u8(6);
                put_ev(&mut w, e);
            }
        }
        w.0
    }

    /// Total decoder: `None` on any malformed input (including trailing bytes).
    pub fn decode(buf: &[u8]) -> Option<Message> {
        let mut r = R { buf, pos: 0 };
        let m = match r.u8()? {
            0 => Message::Propose(Proposal {
                block: get_block(&mut r)?,
                tc: get_opt_tc(&mut r)?,
                sig: r.sig()?,
            }),
            1 => Message::Vote(Vote {
                round: r.u64()?,
                block_id: r.hash()?,
                voter: r.u32()?,
                sig: r.sig()?,
                proposal_sig: r.sig()?,
            }),
            2 => Message::Timeout(Timeout {
                round: r.u64()?,
                high_qc: get_qc(&mut r)?,
                voter: r.u32()?,
                sig: r.sig()?,
                seen_proposal: if r.flag()? {
                    Some((r.hash()?, r.sig()?))
                } else {
                    None
                },
            }),
            3 => Message::Catchup {
                qc: get_qc(&mut r)?,
                tc: get_opt_tc(&mut r)?,
            },
            4 => Message::SyncRequest {
                from: r.u32()?,
                block_id: r.hash()?,
            },
            5 => {
                let n = r.count(132)?;
                let mut blocks = Vec::with_capacity(n.min(256));
                for _ in 0..n {
                    blocks.push(get_block(&mut r)?);
                }
                Message::SyncResponse { blocks }
            }
            6 => Message::Evidence(get_ev(&mut r)?),
            _ => return None,
        };
        if r.left() != 0 {
            return None;
        }
        Some(m)
    }
}
