//! BTB Chain validator node: libp2p/QUIC networking (`net`), the consensus driver (`driver`),
//! storage for crash safety (`storage`), a JSON-RPC API (`rpc`) and configuration (`config`).
//! See `docs/NODE.md`.

pub mod actions;
pub mod attest;
pub mod blockstore;
pub mod channel_rpc;
pub mod config;
pub mod dex_rpc;
pub mod dkg;
pub mod driver;
pub mod explorer;
pub mod hex;
pub mod net;
pub mod oracle;
pub mod oracle_rpc;
pub mod reports;
pub mod rpc;
pub mod storage;
pub mod storage_rpc;
pub mod sync;
pub mod wire;

use btb_chain::{ChainState, GenesisConfig};
use btb_runtime::KvStore;
use config::{GenesisFile, NodeParams};
use driver::{Input, RpcResult};
use libp2p::multiaddr::Protocol;
use libp2p::Multiaddr;
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Create the chain in an empty store, or reopen the one that is there.
pub fn open_chain<S: KvStore>(kv: S, genesis: &GenesisFile) -> Result<ChainState<S>, String> {
    let g: GenesisConfig = genesis.chain_genesis();
    let empty = kv
        .scan(&[0], None, false, 1)
        .map_err(|e| e.to_string())?
        .is_empty();
    if empty {
        ChainState::from_genesis_full(
            kv,
            &g,
            &genesis.genesis_validators(),
            genesis.oracle.as_ref(),
        )
        .map_err(|e| format!("genesis: {e:?}"))
    } else {
        ChainState::open(kv).map_err(|e| format!("open chain state: {e}"))
    }
}

/// The Ed25519 key inside an ed25519 peer id.
fn peer_key(id: &libp2p::PeerId) -> Option<[u8; 32]> {
    let pk = libp2p::identity::PublicKey::try_decode_protobuf(id.as_ref().digest()).ok()?;
    pk.try_into_ed25519().ok().map(|k| k.to_bytes())
}

/// A running node: a driver thread plus a tokio runtime for the network and the API.
pub struct Node {
    rt: Option<tokio::runtime::Runtime>,
    tx: Sender<Input>,
    net: net::NetHandle,
    thread: Option<JoinHandle<()>>,
    rpc_addr: Option<SocketAddr>,
}

impl Node {
    /// Start the node. `state` is the chain state (see `open_chain`).
    pub fn start<S: KvStore + Send + 'static>(
        params: NodeParams,
        state: ChainState<S>,
    ) -> Result<Node, String> {
        Self::start_with_hook(params, state, None)
    }

    /// Like [`Node::start`], with a hook the driver thread calls after every executed block
    /// (see `driver::BlockHook`). A node whose key is not in the validator set follows the
    /// chain by block sync and executes it, which is what a hosting node does.
    pub fn start_with_hook<S: KvStore + Send + 'static>(
        params: NodeParams,
        state: ChainState<S>,
        hook: Option<driver::BlockHook>,
    ) -> Result<Node, String> {
        let set = params.genesis.validator_set()?;
        let keys: Vec<[u8; 32]> = (0..set.len() as u32)
            .map(|i| set.get(i).expect("in range").key)
            .collect();
        let me_key = params.key.verifying_key().to_bytes();
        let mut peers = Vec::new();
        for s in &params.peers {
            let a: Multiaddr = s.parse().map_err(|e| format!("peer address {s}: {e}"))?;
            let id = a
                .iter()
                .find_map(|p| match p {
                    Protocol::P2p(id) => Some(id),
                    _ => None,
                })
                .ok_or_else(|| format!("peer address {s} needs a /p2p/<peer id> suffix"))?;
            let key = peer_key(&id).ok_or_else(|| format!("peer {id} is not an ed25519 id"))?;
            if key != me_key {
                peers.push((key, a));
            }
        }
        let listen: Multiaddr = params
            .listen
            .parse()
            .map_err(|e| format!("listen address {}: {e}", params.listen))?;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("btb-net")
            .enable_all()
            .build()
            .map_err(|e| format!("tokio runtime: {e}"))?;
        let (tx, rx) = channel::<Input>();
        let sink_tx = tx.clone();
        let sink: net::Sink = Arc::new(move |from, msg| {
            let _ = sink_tx.send(Input::Net(from, msg));
        });
        let net = net::start(
            rt.handle(),
            net::NetParams {
                secret: params.key.to_bytes(),
                listen,
                validators: keys,
                peers,
                rate_per_sec: params.rate_per_sec,
                rate_burst: params.rate_burst,
                delay: Duration::from_millis(params.test_one_way_delay_ms),
                followers: params.serve_followers,
            },
            sink,
        )?;
        let rpc_addr = match params.rpc {
            Some(addr) => Some(rt.block_on(rpc::serve(addr, tx.clone()))?),
            None => None,
        };
        let (ready_tx, ready_rx) = channel();
        let net2 = net.clone();
        let thread = std::thread::Builder::new()
            .name("btb-driver".into())
            .spawn(move || driver::run(params, state, rx, net2, ready_tx, hook))
            .map_err(|e| format!("spawn driver: {e}"))?;
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => return Err("driver thread died during startup".into()),
        }
        Ok(Node {
            rt: Some(rt),
            tx,
            net,
            thread: Some(thread),
            rpc_addr,
        })
    }

    /// Blocking JSON-RPC call to the driver (the same methods as the HTTP API).
    pub fn call(&self, method: &str, params: Value) -> RpcResult {
        let rt = self.rt.as_ref().expect("running");
        rt.block_on(rpc::call(&self.tx, method, params))
    }

    /// Run `f` on the driver thread against the chain state and return its result. It runs
    /// between two blocks, so it sees a consistent head, and it blocks block execution while it
    /// runs: keep it short. `S` must be the store the node was started with.
    pub fn with_chain<S, R, F>(&self, f: F) -> Result<R, String>
    where
        S: KvStore + Send + 'static,
        R: Send + 'static,
        F: FnOnce(&mut ChainState<S>) -> R + Send + 'static,
    {
        let (tx, rx) = channel();
        let job: driver::StateJob = Box::new(move |any| {
            if let Some(state) = any.downcast_mut::<ChainState<S>>() {
                let _ = tx.send(f(state));
            }
        });
        self.tx
            .send(Input::Job(job))
            .map_err(|_| "node is shutting down".to_string())?;
        rx.recv_timeout(Duration::from_secs(30))
            .map_err(|_| "node did not run the job (wrong store type or shutting down)".to_string())
    }

    pub fn net(&self) -> &net::NetHandle {
        &self.net
    }

    /// Bound RPC address, if the HTTP API is enabled.
    pub fn rpc_addr(&self) -> Option<SocketAddr> {
        self.rpc_addr
    }

    /// Block the calling thread until Ctrl-C or until the driver stops.
    pub fn run_until_stopped(&self) {
        let rt = self.rt.as_ref().expect("running");
        let t = self.thread.as_ref().expect("running");
        rt.block_on(async {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = async { while !t.is_finished() { tokio::time::sleep(Duration::from_millis(200)).await } } => {}
            }
        });
    }

    /// Stop the node (like a crash, as far as other nodes can tell).
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let _ = self.tx.send(Input::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(rt) = self.rt.take() {
            rt.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop();
    }
}
