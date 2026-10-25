//! The driver's side of HTTPS actions and signed reports (`crate::actions`, `crate::reports`):
//! starting the fetches, gossiping and collecting votes and partials, the RPC views. Kept in its
//! own file (a child module of `driver`, so it sees the driver's private state).

use super::*;
use btb_chain::actions as chain_actions;
use std::collections::BTreeSet;

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
