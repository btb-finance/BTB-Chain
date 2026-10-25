//! JSON-RPC views of the native DEX (`btb_chain::dex`) for the DEX page at `GET /dex`.
//!
//! All amounts are decimal strings (they can exceed 2^53), ids and addresses are hex.
//! An asset parameter is `"btb"` or the 64-hex id of a user token.
//!
//! | Method | Parameters | Result |
//! | --- | --- | --- |
//! | `btb_dexConfig` | `[]` | `{batch_blocks, fee_bps, height, next_clearing, vault, fee_account, max_open_orders}` |
//! | `btb_dexAssets` | `[]` | BTB and up to 256 user tokens |
//! | `btb_dexPairs` | `[]` | pairs with their last clearing |
//! | `btb_dexPair` | `[asset, asset]` | the pair id of two assets (either order) |
//! | `btb_dexBook` | `[pair_hex]` | open asks and bids, recent clearings |
//! | `btb_dexAccount` | `[address_hex]` | open orders and closed-order results |
//! | `btb_dexBalance` | `[address_hex, asset]` | balance of one asset |

use crate::driver::{RpcError, RpcResult};
use crate::hex;
use btb_chain::dex::{self, Asset, Order, OrderResult, Outcome, Side};
use btb_chain::ChainState;
use btb_runtime::KvStore;
use serde_json::{json, Value};

fn app(e: impl ToString) -> RpcError {
    RpcError::app(e.to_string())
}

fn asset_json<S: KvStore>(st: &ChainState<S>, a: &Asset) -> Result<Value, RpcError> {
    Ok(match a {
        Asset::Btb => {
            json!({"kind": "btb", "id": "btb", "symbol": "BTB", "name": "BTB", "decimals": 9})
        }
        Asset::Token(id) => {
            let info = st.token_info(id).map_err(app)?;
            json!({
                "kind": "token",
                "id": hex::encode(id),
                "symbol": info.as_ref().map_or("?".to_string(), |i| i.symbol.clone()),
                "name": info.as_ref().map_or(String::new(), |i| i.name.clone()),
                "decimals": info.as_ref().map_or(0, |i| i.decimals),
            })
        }
    })
}

fn parse_asset(v: &Value) -> Result<Asset, RpcError> {
    let s = v
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("asset must be \"btb\" or a token id in hex"))?;
    if s.eq_ignore_ascii_case("btb") {
        return Ok(Asset::Btb);
    }
    let b = hex::decode(s).map_err(RpcError::invalid_params)?;
    Ok(Asset::Token(b.try_into().map_err(|_| {
        RpcError::invalid_params("token id must be 32 bytes")
    })?))
}

fn order_json(o: &Order) -> Value {
    json!({
        "id": hex::encode(&o.id),
        "owner": hex::encode(&o.owner),
        "side": if o.side == Side::Ask { "ask" } else { "bid" },
        "base_remaining": o.base_rem.to_string(),
        "escrow": o.escrow.to_string(),
        "price": {"n": o.n.to_string(), "d": o.d.to_string()},
        "batches_left": o.batches_left,
        "placed": o.placed,
        "filled": o.filled.to_string(),
        "quote_traded": o.quote_traded.to_string(),
    })
}

fn result_json(r: &OrderResult) -> Value {
    json!({
        "id": hex::encode(&r.id),
        "outcome": match r.outcome { Outcome::Filled => "filled", Outcome::Expired => "expired", Outcome::Cancelled => "cancelled" },
        "side": if r.side == Side::Ask { "ask" } else { "bid" },
        "base": asset_id(&r.base),
        "quote": asset_id(&r.quote),
        "closed_at": r.closed_at,
        "filled": r.filled.to_string(),
        "quote_traded": r.quote_traded.to_string(),
    })
}

fn asset_id(a: &Asset) -> String {
    match a {
        Asset::Btb => "btb".into(),
        Asset::Token(id) => hex::encode(id),
    }
}

fn clearing_json(c: &dex::Clearing) -> Value {
    json!({
        "height": c.height,
        "price": {"n": c.n.to_string(), "d": c.d.to_string()},
        "base_volume": c.base_volume.to_string(),
        "quote_volume": c.quote_volume.to_string(),
        "orders": c.orders,
    })
}

fn h32(v: &Value) -> Result<[u8; 32], RpcError> {
    hex::decode(
        v.as_str()
            .ok_or_else(|| RpcError::invalid_params("expected a hex string"))?,
    )
    .map_err(RpcError::invalid_params)?
    .try_into()
    .map_err(|_| RpcError::invalid_params("expected 32 bytes"))
}

/// Handle one `btb_dex*` method against the head state.
pub fn handle<S: KvStore>(st: &ChainState<S>, method: &str, p: &[Value]) -> RpcResult {
    let arg = |i: usize| {
        p.get(i)
            .ok_or_else(|| RpcError::invalid_params(format!("missing parameter {i}")))
    };
    let height = st.head().map_err(app)?.height;
    let cfg = st.config();
    match method {
        "btb_dexConfig" => {
            let b = cfg.dex_batch_blocks;
            Ok(json!({
                "batch_blocks": b,
                "fee_bps": cfg.dex_fee_bps,
                "height": height,
                // the next block whose end clears: the next multiple of the window
                "next_clearing": (height / b + 1) * b,
                "vault": hex::encode(&dex::dex_vault()),
                "fee_account": hex::encode(&dex::dex_fees()),
                "max_open_orders": dex::MAX_OPEN_ORDERS,
                "max_batches": dex::MAX_BATCHES,
            }))
        }
        "btb_dexAssets" => {
            let mut out = vec![asset_json(st, &Asset::Btb)?];
            for (id, _) in st.tokens(256).map_err(app)? {
                out.push(asset_json(st, &Asset::Token(id))?);
            }
            Ok(Value::Array(out))
        }
        "btb_dexPairs" => {
            let mut out = Vec::new();
            for pi in st.dex_pairs().map_err(app)? {
                out.push(json!({
                    "id": hex::encode(&pi.id),
                    "base": asset_json(st, &pi.base)?,
                    "quote": asset_json(st, &pi.quote)?,
                    "last": st.dex_last_price(&pi.id).map_err(app)?.as_ref().map(clearing_json),
                }));
            }
            Ok(Value::Array(out))
        }
        "btb_dexPair" => {
            let (a, b) = (parse_asset(arg(0)?)?, parse_asset(arg(1)?)?);
            if a == b {
                return Err(RpcError::invalid_params("the two assets are the same"));
            }
            let (base, quote) = dex::pair_of(&a, &b);
            Ok(json!({
                "id": hex::encode(&dex::pair_id(&base, &quote)),
                "base": asset_json(st, &base)?,
                "quote": asset_json(st, &quote)?,
            }))
        }
        "btb_dexBook" => {
            let pair = h32(arg(0)?)?;
            let orders = st.dex_book(&pair).map_err(app)?;
            let (mut asks, mut bids): (Vec<&Order>, Vec<&Order>) =
                orders.iter().partition(|o| o.side == Side::Ask);
            // best first: asks by ascending price, bids by descending price
            let cmp = |a: &&Order, b: &&Order| {
                dex::cmp_price((a.n, a.d), (b.n, b.d)).then(a.id.cmp(&b.id))
            };
            asks.sort_by(cmp);
            bids.sort_by(|a, b| cmp(b, a));
            let info = st
                .dex_pairs()
                .map_err(app)?
                .into_iter()
                .find(|x| x.id == pair);
            let (base, quote) = match info {
                Some(i) => (asset_json(st, &i.base)?, asset_json(st, &i.quote)?),
                None => (Value::Null, Value::Null),
            };
            Ok(json!({
                "pair": hex::encode(&pair),
                "base": base,
                "quote": quote,
                "height": height,
                "asks": asks.into_iter().map(order_json).collect::<Vec<_>>(),
                "bids": bids.into_iter().map(order_json).collect::<Vec<_>>(),
                "history": st.dex_clearings(&pair, 30).map_err(app)?.iter().map(clearing_json).collect::<Vec<_>>(),
            }))
        }
        "btb_dexAccount" => {
            let a = h32(arg(0)?)?;
            Ok(json!({
                "orders": st.dex_orders_of(&a).map_err(app)?.iter().map(|o| {
                    let mut j = order_json(o);
                    j["base"] = json!(asset_id(&o.base));
                    j["quote"] = json!(asset_id(&o.quote));
                    j["pair"] = json!(hex::encode(&o.pair()));
                    j
                }).collect::<Vec<_>>(),
                "results": st.dex_results_of(&a, 100).map_err(app)?.iter().map(result_json).collect::<Vec<_>>(),
            }))
        }
        "btb_dexBalance" => {
            let a = h32(arg(0)?)?;
            let v = match parse_asset(arg(1)?)? {
                Asset::Btb => u128::from(st.balance(&a).map_err(app)?),
                Asset::Token(id) => st.token_balance(&id, &a).map_err(app)?,
            };
            Ok(json!(v.to_string()))
        }
        _ => Err(RpcError {
            code: -32601,
            message: format!("unknown method {method}"),
        }),
    }
}
