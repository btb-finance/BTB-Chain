//! Networking: libp2p over QUIC with request-response as a one-way message pipe.
//!
//! * **Identity.** A node's libp2p key *is* its validator Ed25519 key, so the peer id is a pure
//!   function of the validator key and QUIC's TLS handshake authenticates it. There is no
//!   separate handshake: a peer is validator `i` iff its authenticated peer id equals the one
//!   derived from key `i` in the genesis. Messages from any other peer are dropped.
//! * **Messages.** Each message is one request on a fresh QUIC stream, acknowledged with one
//!   byte. Broadcast is a send to every connected validator (the validator count is small;
//!   consensus vote broadcast is already all-to-all).
//! * **Bounds.** Frames are capped at `MAX_FRAME` (checked before allocation), each peer has a
//!   token bucket, outbound requests in flight per peer are capped, and a peer that keeps
//!   sending malformed frames is banned for a while.

use crate::wire::{parse_frame, Inbound, MAX_FRAME};
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::request_response::{self, Codec, OutboundRequestId, ProtocolSupport};
use libp2p::swarm::dial_opts::{DialOpts, PeerCondition};
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId, Swarm};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Where parsed inbound frames go: `(validator key of the authenticated sender, frame)`.
pub type Sink = Arc<dyn Fn([u8; 32], Inbound) + Send + Sync>;

const PROTOCOL: &str = "/btb/msg/1";
const MAX_INFLIGHT_PER_PEER: usize = 512;
const STRIKE_LIMIT: u32 = 20;
const STRIKE_WINDOW: Duration = Duration::from_secs(10);
const BAN: Duration = Duration::from_secs(30);

/// The libp2p peer id of a validator key.
pub fn peer_id_of(key: &[u8; 32]) -> Option<PeerId> {
    let pk = libp2p::identity::ed25519::PublicKey::try_from_bytes(key).ok()?;
    Some(PeerId::from_public_key(&libp2p::identity::PublicKey::from(
        pk,
    )))
}

#[derive(Clone, Default)]
pub struct FrameCodec;

impl Codec for FrameCodec {
    type Protocol = &'static str;
    type Request = Vec<u8>;
    type Response = ();

    async fn read_request<T>(&mut self, _: &Self::Protocol, io: &mut T) -> io::Result<Vec<u8>>
    where
        T: AsyncRead + Unpin + Send,
    {
        let mut len = [0u8; 4];
        io.read_exact(&mut len).await?;
        let len = u32::from_be_bytes(len) as usize;
        if len == 0 || len > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad frame length",
            ));
        }
        // Grow with the data actually received; never trust the prefix for allocation.
        let mut buf = Vec::new();
        io.take(len as u64).read_to_end(&mut buf).await?;
        if buf.len() != len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(buf)
    }

    async fn read_response<T>(&mut self, _: &Self::Protocol, io: &mut T) -> io::Result<()>
    where
        T: AsyncRead + Unpin + Send,
    {
        let mut b = [0u8; 1];
        io.read_exact(&mut b).await?;
        Ok(())
    }

    async fn write_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        req: Vec<u8>,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        io.write_all(&(req.len() as u32).to_be_bytes()).await?;
        io.write_all(&req).await?;
        io.close().await
    }

    async fn write_response<T>(&mut self, _: &Self::Protocol, io: &mut T, _: ()) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        io.write_all(&[1]).await?;
        io.close().await
    }
}

pub enum NetCmd {
    Send {
        to: [u8; 32],
        data: Vec<u8>,
    },
    Broadcast {
        data: Vec<u8>,
    },
    AddPeer {
        key: [u8; 32],
        addr: Multiaddr,
    },
    /// Replace the set of validator keys whose messages are accepted (and who get broadcasts).
    SetAllowed(Vec<[u8; 32]>),
}

#[derive(Default)]
pub struct NetStats {
    /// Connected validator peers.
    pub connected: AtomicUsize,
    pub received: AtomicU64,
    pub sent: AtomicU64,
    pub dropped_unknown_peer: AtomicU64,
    pub dropped_rate: AtomicU64,
    pub dropped_malformed: AtomicU64,
    pub dropped_banned: AtomicU64,
    pub dropped_backpressure: AtomicU64,
}

/// Cheap handle to a running network task.
#[derive(Clone)]
pub struct NetHandle {
    cmd: mpsc::UnboundedSender<NetCmd>,
    pub stats: Arc<NetStats>,
    listen: Arc<Mutex<Vec<Multiaddr>>>,
}

impl NetHandle {
    pub fn send(&self, to: [u8; 32], data: Vec<u8>) {
        let _ = self.cmd.send(NetCmd::Send { to, data });
    }
    pub fn broadcast(&self, data: Vec<u8>) {
        let _ = self.cmd.send(NetCmd::Broadcast { data });
    }
    /// Tell the network where validator `index` can be dialed.
    pub fn add_peer(&self, key: [u8; 32], addr: Multiaddr) {
        let _ = self.cmd.send(NetCmd::AddPeer { key, addr });
    }
    pub fn set_allowed(&self, keys: Vec<[u8; 32]>) {
        let _ = self.cmd.send(NetCmd::SetAllowed(keys));
    }
    /// Addresses the node is listening on (empty until the listener is up).
    pub fn listen_addrs(&self) -> Vec<Multiaddr> {
        self.listen.lock().expect("lock").clone()
    }
    pub fn connected(&self) -> usize {
        self.stats.connected.load(Ordering::Relaxed)
    }
}

pub struct NetParams {
    /// Ed25519 secret of this node (also its validator key).
    pub secret: [u8; 32],
    pub listen: Multiaddr,
    /// Validator keys whose messages are accepted at the start.
    pub validators: Vec<[u8; 32]>,
    /// Dialable validators: (key, address with or without a `/p2p/<id>` suffix).
    pub peers: Vec<([u8; 32], Multiaddr)>,
    pub rate_per_sec: f64,
    pub rate_burst: f64,
    /// Testing only: inbound messages are delivered this long after arrival.
    pub delay: Duration,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

struct Strikes {
    count: u32,
    since: Instant,
}

/// Start the network task on `rt`. Parsed inbound frames arrive on `inbound` as
/// `(validator key of the authenticated sender, frame)`.
pub fn start(
    rt: &tokio::runtime::Handle,
    p: NetParams,
    inbound: Sink,
) -> Result<NetHandle, String> {
    let _guard = rt.enter();
    let mut secret = p.secret;
    let kp = libp2p::identity::Keypair::ed25519_from_bytes(&mut secret)
        .map_err(|e| format!("libp2p key: {e}"))?;
    let mut swarm: Swarm<request_response::Behaviour<FrameCodec>> =
        libp2p::SwarmBuilder::with_existing_identity(kp)
            .with_tokio()
            .with_quic()
            .with_behaviour(|_| {
                request_response::Behaviour::with_codec(
                    FrameCodec,
                    [(PROTOCOL, ProtocolSupport::Full)],
                    request_response::Config::default()
                        .with_request_timeout(Duration::from_secs(5))
                        .with_max_concurrent_streams(MAX_INFLIGHT_PER_PEER),
                )
            })
            .map_err(|e| format!("behaviour: {e}"))?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(24 * 3600)))
            .build();
    swarm
        .listen_on(p.listen.clone())
        .map_err(|e| format!("listen on {}: {e}", p.listen))?;

    let mut ids: HashMap<PeerId, [u8; 32]> = HashMap::new();
    let mut peer_of: HashMap<[u8; 32], PeerId> = HashMap::new();
    for k in p.validators.iter().chain(p.peers.iter().map(|(k, _)| k)) {
        if let Some(id) = peer_id_of(k) {
            peer_of.insert(*k, id);
        }
    }
    for k in &p.validators {
        if let Some(id) = peer_of.get(k) {
            ids.insert(*id, *k);
        }
    }
    let mut addrs: HashMap<[u8; 32], Multiaddr> = HashMap::new();
    for (k, a) in p.peers {
        addrs.insert(k, normalize(&peer_of, &k, a)?);
    }

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let handle = NetHandle {
        cmd: cmd_tx,
        stats: Arc::new(NetStats::default()),
        listen: Arc::new(Mutex::new(Vec::new())),
    };
    let task = NetTask {
        swarm,
        ids,
        peer_of,
        addrs,
        cmd_rx,
        inbound,
        handle: handle.clone(),
        buckets: HashMap::new(),
        strikes: HashMap::new(),
        banned: HashMap::new(),
        inflight: HashMap::new(),
        req_peer: HashMap::new(),
        conns: HashMap::new(),
        rate: (p.rate_per_sec, p.rate_burst),
        delay: p.delay,
        delayed: VecDeque::new(),
    };
    rt.spawn(task.run());
    Ok(handle)
}

/// Strip and check a `/p2p/<id>` suffix: it must be the peer id of validator `key`.
fn normalize(
    peer_of: &HashMap<[u8; 32], PeerId>,
    key: &[u8; 32],
    mut a: Multiaddr,
) -> Result<Multiaddr, String> {
    let want = *peer_of
        .get(key)
        .ok_or_else(|| "peer key is not a valid ed25519 key".to_string())?;
    if let Some(Protocol::P2p(id)) = a.iter().last() {
        if id != want {
            return Err(format!("address {a} names peer {id}, expected {want}"));
        }
        a.pop();
    }
    Ok(a)
}

struct NetTask {
    swarm: Swarm<request_response::Behaviour<FrameCodec>>,
    ids: HashMap<PeerId, [u8; 32]>,
    peer_of: HashMap<[u8; 32], PeerId>,
    addrs: HashMap<[u8; 32], Multiaddr>,
    cmd_rx: mpsc::UnboundedReceiver<NetCmd>,
    inbound: Sink,
    handle: NetHandle,
    buckets: HashMap<PeerId, Bucket>,
    strikes: HashMap<PeerId, Strikes>,
    banned: HashMap<PeerId, Instant>,
    inflight: HashMap<PeerId, usize>,
    req_peer: HashMap<OutboundRequestId, PeerId>,
    conns: HashMap<PeerId, usize>,
    rate: (f64, f64),
    delay: Duration,
    delayed: VecDeque<(Instant, [u8; 32], Inbound)>,
}

type Ev = SwarmEvent<request_response::Event<Vec<u8>, ()>>;

impl NetTask {
    async fn run(mut self) {
        let mut redial = tokio::time::interval(Duration::from_millis(500));
        loop {
            let next_due = self.delayed.front().map(|d| d.0);
            tokio::select! {
                ev = self.swarm.select_next_some() => self.on_event(ev),
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(c) => self.on_cmd(c),
                    None => return, // every handle dropped: shut down
                },
                _ = redial.tick() => self.redial(),
                _ = async { tokio::time::sleep_until(next_due.expect("guarded").into()).await },
                    if next_due.is_some() => self.flush_delayed(),
            }
        }
    }

    fn redial(&mut self) {
        let now = Instant::now();
        self.banned.retain(|_, until| *until > now);
        for (key, addr) in self.addrs.clone() {
            let Some(peer) = self.peer_of.get(&key).copied() else {
                continue;
            };
            if !self.ids.contains_key(&peer) {
                continue;
            }
            if self.conns.get(&peer).copied().unwrap_or(0) > 0 {
                continue;
            }
            let opts = DialOpts::peer_id(peer)
                .condition(PeerCondition::DisconnectedAndNotDialing)
                .addresses(vec![addr])
                .build();
            let _ = self.swarm.dial(opts);
        }
    }

    fn on_cmd(&mut self, c: NetCmd) {
        match c {
            NetCmd::AddPeer { key, addr } => {
                if let Some(id) = peer_id_of(&key) {
                    self.peer_of.insert(key, id);
                }
                if let Ok(a) = normalize(&self.peer_of, &key, addr) {
                    self.addrs.insert(key, a);
                    self.redial();
                }
            }
            NetCmd::SetAllowed(keys) => {
                self.ids.clear();
                for k in keys {
                    if let Some(id) = peer_id_of(&k) {
                        self.peer_of.insert(k, id);
                        self.ids.insert(id, k);
                    }
                }
                self.redial();
            }
            NetCmd::Send { to, data } => {
                if let Some(peer) = self.peer_of.get(&to).copied() {
                    self.send_to(peer, data);
                }
            }
            NetCmd::Broadcast { data } => {
                let peers: Vec<PeerId> = self
                    .conns
                    .iter()
                    .filter(|(p, n)| **n > 0 && self.ids.contains_key(*p))
                    .map(|(p, _)| *p)
                    .collect();
                for p in peers {
                    self.send_to(p, data.clone());
                }
            }
        }
    }

    fn send_to(&mut self, peer: PeerId, data: Vec<u8>) {
        if data.is_empty()
            || data.len() > MAX_FRAME
            || self.conns.get(&peer).copied().unwrap_or(0) == 0
        {
            return;
        }
        let n = self.inflight.entry(peer).or_default();
        if *n >= MAX_INFLIGHT_PER_PEER {
            self.handle
                .stats
                .dropped_backpressure
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        *n += 1;
        let id = self.swarm.behaviour_mut().send_request(&peer, data);
        self.req_peer.insert(id, peer);
        self.handle.stats.sent.fetch_add(1, Ordering::Relaxed);
    }

    fn done_request(&mut self, id: &OutboundRequestId) {
        if let Some(p) = self.req_peer.remove(id) {
            if let Some(n) = self.inflight.get_mut(&p) {
                *n = n.saturating_sub(1);
            }
        }
    }

    fn on_event(&mut self, ev: Ev) {
        use request_response::{Event, Message};
        match ev {
            SwarmEvent::NewListenAddr { address, .. } => {
                self.handle.listen.lock().expect("lock").push(address);
            }
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                if self.ids.contains_key(&peer_id) {
                    let n = self.conns.entry(peer_id).or_default();
                    *n += 1;
                    if *n == 1 {
                        self.handle.stats.connected.fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    // Not a validator: nothing to talk about.
                    let _ = self.swarm.disconnect_peer_id(peer_id);
                }
            }
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                if let Some(n) = self.conns.get_mut(&peer_id) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        self.conns.remove(&peer_id);
                        self.handle.stats.connected.fetch_sub(1, Ordering::Relaxed);
                        self.inflight.remove(&peer_id);
                        self.req_peer.retain(|_, p| *p != peer_id);
                    }
                }
            }
            SwarmEvent::Behaviour(Event::Message { peer, message, .. }) => match message {
                Message::Request {
                    request, channel, ..
                } => {
                    let _ = self.swarm.behaviour_mut().send_response(channel, ());
                    self.on_request(peer, request);
                }
                Message::Response { request_id, .. } => self.done_request(&request_id),
            },
            SwarmEvent::Behaviour(Event::OutboundFailure { request_id, .. }) => {
                self.done_request(&request_id)
            }
            _ => {}
        }
    }

    fn strike(&mut self, peer: PeerId) {
        let now = Instant::now();
        let s = self.strikes.entry(peer).or_insert(Strikes {
            count: 0,
            since: now,
        });
        if now.duration_since(s.since) > STRIKE_WINDOW {
            *s = Strikes {
                count: 0,
                since: now,
            };
        }
        s.count += 1;
        if s.count >= STRIKE_LIMIT {
            self.strikes.remove(&peer);
            self.banned.insert(peer, now + BAN);
        }
    }

    fn on_request(&mut self, peer: PeerId, data: Vec<u8>) {
        let Some(&idx) = self.ids.get(&peer) else {
            self.handle
                .stats
                .dropped_unknown_peer
                .fetch_add(1, Ordering::Relaxed);
            return;
        };
        let now = Instant::now();
        if self.banned.get(&peer).is_some_and(|u| *u > now) {
            self.handle
                .stats
                .dropped_banned
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        let (rate, burst) = self.rate;
        let b = self.buckets.entry(peer).or_insert(Bucket {
            tokens: burst,
            last: now,
        });
        b.tokens = (b.tokens + now.duration_since(b.last).as_secs_f64() * rate).min(burst);
        b.last = now;
        let cost = 1.0 + (data.len() / 4096) as f64;
        if b.tokens < cost {
            self.handle
                .stats
                .dropped_rate
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        b.tokens -= cost;
        self.handle.stats.received.fetch_add(1, Ordering::Relaxed);
        let Some(msg) = parse_frame(&data) else {
            self.handle
                .stats
                .dropped_malformed
                .fetch_add(1, Ordering::Relaxed);
            self.strike(peer);
            return;
        };
        if self.delay.is_zero() {
            (self.inbound)(idx, msg);
        } else {
            self.delayed.push_back((now + self.delay, idx, msg));
        }
    }

    fn flush_delayed(&mut self) {
        let now = Instant::now();
        while self.delayed.front().is_some_and(|d| d.0 <= now) {
            if let Some((_, idx, msg)) = self.delayed.pop_front() {
                (self.inbound)(idx, msg);
            }
        }
    }
}
