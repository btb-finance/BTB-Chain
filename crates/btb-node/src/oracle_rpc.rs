//! JSON-RPC views of the oracles (`docs/ORACLES.md`).
//!
//! | Method | Parameters | Result |
//! | --- | --- | --- |
//! | `btb_oracleConfig` | `[]` | `null` (no oracles) or `{epoch, key, interim_dealer, dkg_window, max_age_blocks, pairs}`; `key` is `null` while the epoch has none, else `{origin, threshold, group_key, holders}` |
//! | `btb_oracleDkg` | `[]` | the key generations on the board: `[{target, from, status, window_start, window_end, deal_end, committee, deals, disqualified}]` |
//! | `btb_oracleKeyChanges` | `[]` | every change of the group key: `[{epoch, height, degraded_epoch, degraded_height, old_group_key, new_group_key}]`; external consumers of reports must re-trust the new key |
//! | `btb_oracleBeacon` | `[]` or `[height]` | `{epoch, height, signature, seed}`; no height: the last executed block's |
//! | `btb_oraclePrice` | `[pair_id]` | `null` or `{pair, name, price, decimals, height, reporters, paused}`; `paused`: the circuit breaker is open and `price` is the last good value |
//! | `btb_oracleTwap` | `[pair_id, window]` | `null` (no price) or `{pair, name, window, twap, decimals, height}`; an error while paused or for a bad window |
//!
//! The beacon signature verifies under `group_key` against the message
//! `"btb.beacon.v1" || chain_id:u64be || epoch:u64be || height:u64be` (`btb_oracle::beacon_message`),
//! so anyone can check it. Prices are decimal strings (they can exceed 2^53).

use crate::blockstore::BlockStore;
use crate::driver::{RpcError, RpcResult};
use crate::hex;
use btb_chain::ChainState;
use btb_chain::{dkg, oracle};
use btb_oracle::Beacon;
use btb_runtime::KvStore;
use serde_json::{json, Value};

fn app(e: impl ToString) -> RpcError {
    RpcError::app(e.to_string())
}

fn beacon_json(b: &Beacon) -> Value {
    json!({
        "epoch": b.epoch,
        "height": b.height,
        "signature": hex::encode(&b.signature),
        "seed": hex::encode(&b.seed()),
    })
}

pub fn handle<S: KvStore>(
    st: &ChainState<S>,
    store: &BlockStore,
    method: &str,
    params: &[Value],
) -> RpcResult {
    let Some(og) = oracle::load(st.store()).map_err(app)? else {
        return Ok(Value::Null);
    };
    match method {
        "btb_oracleConfig" => {
            let head = st.head().map_err(app)?.height;
            let epoch = st.config().staking.epoch_of_block(head + 1);
            let key = oracle::epoch_key(st.store(), epoch).map_err(app)?;
            Ok(json!({
                "epoch": epoch,
                "key": key.map(|k| json!({
                    "origin": k.origin,
                    "threshold": k.group.threshold,
                    "group_key": hex::encode(&k.group.public_key.to_bytes()),
                    "holders": k.holders.iter().map(|h| hex::encode(h)).collect::<Vec<_>>(),
                })),
                "degraded": dkg::degraded(st.store()).map_err(app)?.map(|d| json!({
                    "epoch": d.epoch,
                    "since_height": d.height,
                    "reachable_holders": d.reachable,
                    "threshold": d.threshold,
                    "abandoned_group_key": hex::encode(&d.old_group),
                })),
                "interim_dealer": !og.group_key.is_empty(),
                "dkg_window": og.dkg_window,
                "max_age_blocks": og.max_age_blocks,
                "rules": {
                    "update_deviation_bps": og.rules.update_deviation_bps,
                    "heartbeat_blocks": og.rules.heartbeat_blocks,
                    "breaker_bps": og.rules.breaker_bps,
                    "breaker_window": og.rules.breaker_window,
                    "breaker_resume_blocks": og.rules.breaker_resume_blocks,
                    "twap_max_window": og.rules.twap_max_window,
                },
                "pairs": og.pairs.iter()
                    .map(|p| json!({"id": p.id, "name": p.name, "decimals": p.decimals}))
                    .collect::<Vec<_>>(),
            }))
        }
        "btb_oracleKeyChanges" => {
            let all = dkg::key_changes(st.store()).map_err(app)?;
            Ok(Value::Array(
                all.iter()
                    .map(|c| {
                        json!({
                            "epoch": c.epoch,
                            "height": c.height,
                            "degraded_epoch": c.degraded_epoch,
                            "degraded_height": c.degraded_height,
                            "old_group_key": hex::encode(&c.old_group),
                            "new_group_key": hex::encode(&c.new_group),
                        })
                    })
                    .collect(),
            ))
        }
        "btb_oracleDkg" => {
            let mut out = Vec::new();
            for s in dkg::sessions(st.store()).map_err(app)? {
                let deals = dkg::deals(st.store(), s.target).map_err(app)?;
                out.push(json!({
                    "target": s.target,
                    "from": s.from,
                    "status": match s.status {
                        dkg::STATUS_OPEN => "open",
                        dkg::STATUS_DONE => "done",
                        _ => "failed",
                    },
                    "window_start": s.window_start,
                    "window_end": s.window_end,
                    "deal_end": s.deal_end,
                    "committee": s.committee().iter().map(|k| hex::encode(k)).collect::<Vec<_>>(),
                    "deals": deals.iter().map(|(d, _)| *d).collect::<Vec<_>>(),
                    "disqualified": deals.iter().filter(|(_, d)| d.disqualified)
                        .map(|(d, _)| *d).collect::<Vec<_>>(),
                }));
            }
            Ok(Value::Array(out))
        }
        "btb_oracleBeacon" => {
            let head = st.head().map_err(app)?.height;
            let h = match params.first() {
                None => head,
                Some(v) => v
                    .as_u64()
                    .ok_or_else(|| RpcError::invalid_params("height must be a number"))?,
            };
            let found = if h == head {
                oracle::last_beacon(st.store()).map_err(app)?
            } else {
                store.beacon(h).and_then(|raw| Beacon::decode(&raw))
            };
            Ok(found.as_ref().map_or(Value::Null, beacon_json))
        }
        "btb_oraclePrice" => {
            let pair = params
                .first()
                .and_then(Value::as_u64)
                .and_then(|x| u32::try_from(x).ok())
                .ok_or_else(|| RpcError::invalid_params("pair id must be a number"))?;
            let Some(cfg) = og.pairs.iter().find(|p| p.id == pair) else {
                return Ok(Value::Null);
            };
            Ok(match oracle::price(st.store(), pair).map_err(app)? {
                None => Value::Null,
                Some(p) => json!({
                    "pair": pair,
                    "name": cfg.name,
                    "price": p.price.to_string(),
                    "decimals": p.decimals,
                    "height": p.height,
                    "reporters": p.reporters,
                    "paused": oracle::paused(st.store(), pair).map_err(app)?,
                }),
            })
        }
        "btb_oracleTwap" => {
            let pair = params
                .first()
                .and_then(Value::as_u64)
                .and_then(|x| u32::try_from(x).ok())
                .ok_or_else(|| RpcError::invalid_params("pair id must be a number"))?;
            let window = params
                .get(1)
                .and_then(Value::as_u64)
                .ok_or_else(|| RpcError::invalid_params("window must be a number"))?;
            let Some(cfg) = og.pairs.iter().find(|p| p.id == pair) else {
                return Ok(Value::Null);
            };
            if oracle::paused(st.store(), pair).map_err(app)? {
                return Err(RpcError::app(
                    "feed paused by its circuit breaker".to_string(),
                ));
            }
            let now = st.head().map_err(app)?.height;
            match btb_runtime::twap::twap(st.store(), pair, now, window) {
                Ok(t) => Ok(json!({
                    "pair": pair,
                    "name": cfg.name,
                    "window": window,
                    "twap": t.to_string(),
                    "decimals": cfg.decimals,
                    "height": now,
                })),
                Err(btb_runtime::twap::TwapError::NoPrice) => Ok(Value::Null),
                Err(btb_runtime::twap::TwapError::BadWindow) => Err(RpcError::invalid_params(
                    "window is 0 or longer than the history",
                )),
            }
        }
        _ => Err(RpcError {
            code: -32601,
            message: format!("method not found: {method}"),
        }),
    }
}
