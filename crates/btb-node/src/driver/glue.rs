//! The driver's side of HTTPS actions and signed reports (`crate::actions`, `crate::reports`):
//! starting the fetches, gossiping and collecting votes and partials, the RPC views. Kept in its
//! own file (a child module of `driver`, so it sees the driver's private state).

use super::*;
use btb_chain::actions as chain_actions;
use std::collections::BTreeSet;

/// How long the state nonce of the consensus key may stand still while transactions are in
/// flight before they are given up and the nonce restarts from the state.
const CKEY_STALL: Duration = Duration::from_secs(8);

/// Nonce bookkeeping for the consensus key's account (see `Driver::next_ckey_nonce`).
#[derive(Debug, Default)]
pub(super) struct CkeyNonce {
    next: u64,
    seen_state: u64,
    since: Option<Instant>,
}

impl CkeyNonce {
    fn next(&mut self, state: u64, now: Instant) -> u64 {
        if state != self.seen_state || self.since.is_none() {
            self.seen_state = state;
            self.since = Some(now);
        }
        if self.next > state && now.duration_since(self.since.expect("set")) > CKEY_STALL {
            self.next = state;
            self.since = Some(now);
        }
        self.next = self.next.max(state);
        self.next
    }

    pub(super) fn used(&mut self, nonce: u64) {
        self.next = self.next.max(nonce + 1);
    }
}

impl<S: KvStore + 'static> Driver<S> {
    /// Pending HTTPS requests and report signing, once per tick.
    pub(super) fn action_tick(&mut self) {
        // finished answers go to the other validators
        for v in self.actions.drain() {
            self.net.broadcast(OracleMsg::Action(v).frame());
        }
        let count = chain_actions::pending_count(self.state.store()).unwrap_or(0);
        let mut ids = BTreeSet::new();
        if count > 0 {
            let pending = chain_actions::pending_list(self.state.store()).unwrap_or_default();
            ids = pending.iter().map(|(id, _)| *id).collect();
            if self.index() != u32::MAX {
                let state = &self.state;
                self.actions.start(&pending, |p| {
                    state.contract(&p.contract).ok().flatten().map(|r| r.code)
                });
            }
        }
        self.actions.prune(&ids);
        self.cross_tick();
        if let Some(o) = self.oracle.as_ref() {
            let msgs = self.reports.step(&self.state, &o.beacons, Instant::now());
            for m in msgs {
                self.net.broadcast(m.frame());
            }
        }
    }

    /// An HTTPS-action or report message from `from`. Returns true when the message was one of
    /// ours (and is handled).
    pub(super) fn on_action_msg(&mut self, from: [u8; 32], msg: &OracleMsg) -> bool {
        let is_validator = self.keys.contains(&from);
        match msg {
            OracleMsg::Action(v) => {
                if is_validator {
                    self.actions.on_vote(v.clone(), &self.keys);
                }
            }
            OracleMsg::ReportPartial { id, index, sig } => {
                if is_validator {
                    self.reports.on_partial(&from, *id, *index, sig);
                }
            }
            OracleMsg::ReportSig { id, sig } => {
                if is_validator {
                    self.reports.on_signature(*id, sig);
                }
            }
            OracleMsg::Cross(bytes) => {
                if is_validator {
                    self.on_cross_msg(from, bytes);
                }
            }
            OracleMsg::ReportWant { id } => {
                if let Some(sig) = self.reports.signature(*id) {
                    self.net
                        .send(from, OracleMsg::ReportSig { id: *id, sig }.frame());
                }
            }
            _ => return false,
        }
        true
    }

    // ------------------------------------------------------------ cross-chain keys and signing

    /// After a block: key generation duties, nonce pools, vault duties.
    pub(super) fn cross_after_block(&mut self) {
        let Ok(head) = self.state.head() else {
            return;
        };
        let epoch = self.state.config().staking.epoch_of_block(head.height + 1);
        let me = self.index();
        let Some(c) = self.cross.as_mut() else {
            return;
        };
        let ops = match c.on_block(self.state.store(), head.height + 1, epoch) {
            Ok(ops) => ops,
            Err(e) => {
                eprintln!("btb-node[{me}]: cross-chain keys: {e}");
                Vec::new()
            }
        };
        self.flush_cross();
        // While catching up on old blocks the windows are long past: send nothing.
        if !ops.is_empty() && !self.applying_sync {
            self.submit_cross(ops);
        }
    }

    /// Periodic: run the signing rounds of pending requests.
    pub(super) fn cross_tick(&mut self) {
        if self.index() == u32::MAX || self.applying_sync {
            return;
        }
        let Ok(head) = self.state.head() else {
            return;
        };
        let epoch = self.state.config().staking.epoch_of_block(head.height + 1);
        let Some(c) = self.cross.as_mut() else {
            return;
        };
        let view = match c.view(self.state.store(), head.height, epoch) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("btb-node: cross-chain view: {e}");
                return;
            }
        };
        let ops = c.tick(&view, Instant::now());
        self.flush_cross();
        if !ops.is_empty() {
            self.submit_cross(ops);
        }
    }

    fn on_cross_msg(&mut self, from: [u8; 32], bytes: &[u8]) {
        let Some(msg) = crate::cross::XMsg::decode(bytes) else {
            return;
        };
        let Ok(head) = self.state.head() else {
            return;
        };
        let epoch = self.state.config().staking.epoch_of_block(head.height + 1);
        let Some(c) = self.cross.as_mut() else {
            return;
        };
        let Ok(view) = c.view(self.state.store(), head.height, epoch) else {
            return;
        };
        let ops = c.on_msg(from, msg, &view, Instant::now());
        self.flush_cross();
        if !ops.is_empty() {
            self.submit_cross(ops);
        }
    }

    fn flush_cross(&mut self) {
        let Some(c) = self.cross.as_mut() else {
            return;
        };
        let out: Vec<_> = c.outbox.drain(..).collect();
        for (to, m) in out {
            let frame = OracleMsg::Cross(m.encode()).frame();
            match to {
                Some(k) => self.net.send(k, frame),
                None => self.net.broadcast(frame),
            }
        }
    }

    /// Sign cross-chain operations with the consensus key and submit them.
    fn submit_cross(&mut self, ops: Vec<btb_chain::CrossOp>) {
        let addr = account_address(&self.signing.verifying_key().to_bytes());
        let mempool = &self.mempool;
        if let Some(c) = self.cross.as_mut() {
            c.pending.retain(|id| mempool.contains(id));
        }
        for op in ops {
            let nonce = self.next_ckey_nonce(&addr);
            let tx = Transaction::sign(
                TxBody {
                    relayer: None,
                    chain_id: self.chain_id,
                    nonce,
                    kind: TxKind::Cross(op),
                },
                &self.signing,
            );
            match self.add_tx(&tx.encode(), None) {
                Ok(id) => {
                    self.ckey_nonce.used(nonce);
                    if let Some(c) = self.cross.as_mut() {
                        c.pending.push(id);
                    }
                }
                Err(e) => eprintln!(
                    "btb-node[{}]: could not submit a cross-chain transaction: {e} \
                     (the consensus key's account must hold a little BTB for fees)",
                    self.index()
                ),
            }
        }
    }

    /// The next free nonce of the consensus key's account. Transactions that were committed but
    /// are not executed yet (blocks waiting for their beacon) have left the mempool while the
    /// state nonce still lags, so the nonce is tracked here instead of derived from the mempool.
    /// If the state nonce stops advancing while we believe transactions are in flight (a
    /// transaction was lost), start over from the state.
    pub(super) fn next_ckey_nonce(&mut self, addr: &Address) -> u64 {
        let state = self.state.nonce(addr).unwrap_or(0);
        self.ckey_nonce.next(state, Instant::now())
    }

    /// `btb_crossKeys`, `btb_crossRequest [id]`, `btb_crossRequests`, `btb_vault [account]`.
    pub(super) fn rpc_cross(&self, method: &str, arr: &[Value]) -> RpcResult {
        use btb_chain::{vault, xdkg, xsign};
        use btb_oracle::frost::{addr, Scheme};
        let app = |e: btb_chain::state::StateError| RpcError::app(e.to_string());
        let kv = self.state.store();
        let head = self.state.head().map_err(app)?;
        let epoch = self.state.config().staking.epoch_of_block(head.height + 1);
        let request_json = |r: &xsign::SignRequest| {
            let local = self
                .cross
                .as_ref()
                .and_then(|c| c.local_signature(r.id))
                .map(hex::encode);
            json!({
                "id": r.id,
                "scheme": r.scheme.name(),
                "message": hex::encode(&r.message),
                "height": r.height,
                "done": r.is_done(),
                "done_height": r.done_height,
                "signature": r.signature.as_ref().map(|s| hex::encode(s)),
                "local_signature": local,
                "tag": match r.tag {
                    xsign::RequestTag::Raw => json!({"kind": "raw"}),
                    xsign::RequestTag::Withdrawal { id, index } =>
                        json!({"kind": "withdrawal", "id": id, "index": index}),
                },
            })
        };
        match method {
            "btb_crossKeys" => {
                let mut out = Vec::new();
                for scheme in Scheme::ALL {
                    let Some((e, k)) = xdkg::key_at(kv, scheme, epoch).map_err(app)? else {
                        out.push(json!({"scheme": scheme.name(), "key": null}));
                        continue;
                    };
                    let output = k.group.output_key().ok();
                    let addresses = output.map(|o| match scheme {
                        Scheme::Taproot => json!({
                            "bitcoin": addr::taproot_address("bc", &o),
                            "bitcoin_testnet": addr::taproot_address("tb", &o),
                            "bitcoin_regtest": addr::taproot_address("bcrt", &o),
                        }),
                        Scheme::Ed25519 => json!({"solana": addr::solana_address(&o)}),
                    });
                    let (credited, burned, supply) = vault::accounting(kv, scheme).map_err(app)?;
                    out.push(json!({
                        "scheme": scheme.name(),
                        "epoch": epoch,
                        "key_epoch": e,
                        "origin": k.origin,
                        "threshold": k.group.threshold,
                        "holders": k.holders.iter().map(|h| hex::encode(h)).collect::<Vec<_>>(),
                        "public_key": hex::encode(&k.group.public_key),
                        "output_key": output.map(|o| hex::encode(&o)),
                        "addresses": addresses,
                        "excluded": xsign::excluded(kv, scheme, epoch).map_err(app)?
                            .iter().map(|h| hex::encode(h)).collect::<Vec<_>>(),
                        "wrapped_token": hex::encode(&vault::token_id(scheme)),
                        "wrapped": {
                            "credited": credited.to_string(),
                            "burned": burned.to_string(),
                            "supply": supply.to_string(),
                        },
                    }));
                }
                Ok(json!(out))
            }
            "btb_crossRequest" => {
                let id = arr
                    .first()
                    .and_then(Value::as_u64)
                    .ok_or_else(|| RpcError::invalid_params("expected [request id]"))?;
                Ok(xsign::get(kv, id)
                    .map_err(app)?
                    .map_or(Value::Null, |r| request_json(&r)))
            }
            "btb_crossRequests" => {
                let pending = xsign::pending_from(kv, 0, 64).map_err(app)?;
                Ok(json!({
                    "next_id": xsign::next_id(kv).map_err(app)?,
                    "pending": pending.iter().map(request_json).collect::<Vec<_>>(),
                }))
            }
            "btb_vault" => {
                let who = match arr.first().and_then(Value::as_str) {
                    Some(s) => Some(
                        hex::decode(s)
                            .ok()
                            .and_then(|b| <[u8; 32]>::try_from(b).ok())
                            .ok_or_else(|| RpcError::invalid_params("expected [account hex]"))?,
                    ),
                    None => None,
                };
                let mut assets = Vec::new();
                for scheme in Scheme::ALL {
                    let (credited, burned, supply) = vault::accounting(kv, scheme).map_err(app)?;
                    assets.push(json!({
                        "scheme": scheme.name(),
                        "token": hex::encode(&vault::token_id(scheme)),
                        "credited": credited.to_string(),
                        "burned": burned.to_string(),
                        "supply": supply.to_string(),
                        "balance": match &who {
                            Some(a) => Some(vault::balance_of(kv, scheme, a).map_err(app)?.to_string()),
                            None => None,
                        },
                    }));
                }
                let next = vault::next_withdrawal_id(kv).map_err(app)?;
                let ws = vault::withdrawals_from(kv, next.saturating_sub(20), 20).map_err(app)?;
                Ok(json!({
                    "assets": assets,
                    "next_withdrawal": next,
                    "utxos": vault::utxos(kv, 100).map_err(app)?.iter().map(|u| json!({
                        "txid": hex::encode(&u.txid),
                        "vout": u.vout,
                        "amount": u.amount.to_string(),
                    })).collect::<Vec<_>>(),
                    "withdrawals": ws.iter().map(|w| json!({
                        "id": w.id,
                        "asset": w.asset.name(),
                        "owner": hex::encode(&w.owner),
                        "amount": w.amount.to_string(),
                        "destination": hex::encode(&w.destination),
                        "status": match w.status {
                            vault::STATUS_BURNED => "burned",
                            vault::STATUS_PREPARED => "prepared",
                            _ => "signed",
                        },
                        "requests": w.requests,
                        "plan": hex::encode(&w.plan),
                    })).collect::<Vec<_>>(),
                }))
            }
            _ => Err(RpcError {
                code: -32601,
                message: format!("method not found: {method}"),
            }),
        }
    }

    /// Votes for the next proposal (those not yet recorded on chain).
    pub(super) fn action_votes_for_proposal(&self) -> Vec<chain_actions::ActionVote> {
        if self.actions.pooled() == 0
            || chain_actions::pending_count(self.state.store()).unwrap_or(0) == 0
        {
            return Vec::new();
        }
        let pending: BTreeSet<Hash> = chain_actions::pending_list(self.state.store())
            .unwrap_or_default()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let store = self.state.store();
        self.actions.for_proposal(&pending, |id| {
            chain_actions::votes_of(store, id)
                .map(|v| v.into_iter().map(|(k, _)| k).collect())
                .unwrap_or_default()
        })
    }

    pub(super) fn rpc_action_requests(&self) -> RpcResult {
        let list = chain_actions::pending_list(self.state.store())
            .map_err(|e| RpcError::app(e.to_string()))?;
        let mut out = Vec::new();
        for (id, p) in list {
            let votes = chain_actions::votes_of(self.state.store(), &id)
                .map(|v| v.len())
                .unwrap_or(0);
            out.push(json!({
                "id": hex::encode(&id),
                "requester": hex::encode(&p.requester),
                "contract": hex::encode(&p.contract),
                "function": p.function,
                "url": p.http().map(|h| h.url),
                "created": p.created,
                "deadline": p.deadline,
                "escrow": p.escrow.to_string(),
                "votes": votes,
            }));
        }
        Ok(json!({
            "pending": out,
            "inflight": self.actions.inflight(),
            "pooled_votes": self.actions.pooled(),
        }))
    }

    /// `btb_oracleReport [id]`: `null` for a report that is not in state, else
    /// `{id, chain_id, contract, height, epoch, data, message, group_key, signature}` where
    /// `signature` is `null` until the committee's partials are combined. `message` is what the
    /// committee signs, `group_key` the epoch's 48-byte BLS public key.
    pub(super) fn rpc_report(&self, arr: &[Value]) -> RpcResult {
        let id = arr
            .first()
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::invalid_params("expected [report id]"))?;
        let Some(r) = btb_chain::reports::get(self.state.store(), id)
            .map_err(|e| RpcError::app(e.to_string()))?
        else {
            return Ok(Value::Null);
        };
        let message = btb_oracle::report::report_message(self.chain_id, &r.contract, r.id, &r.data);
        let (epoch, group) = match self.reports.group_of(id) {
            Some((e, g)) => (Some(e), Some(hex::encode(&g.public_key.to_bytes()))),
            None => (None, None),
        };
        Ok(json!({
            "id": r.id,
            "chain_id": self.chain_id,
            "contract": hex::encode(&r.contract),
            "height": r.height,
            "epoch": epoch,
            "data": hex::encode(&r.data),
            "message": hex::encode(&message),
            "group_key": group,
            "signature": self.reports.signature(id).map(|s| hex::encode(&s)),
        }))
    }
}
