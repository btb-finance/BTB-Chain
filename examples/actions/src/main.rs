//! Example contract: HTTPS actions with transforms, and reports for other chains
//! (`docs/ORACLES.md`, sections 3 and 4).
//!
//! Build: `./build.sh` (writes `actions.elf`).
//!
//! * `quote` asks every validator to fetch a URL. The response carries a changing nonce, so the
//!   `quote` transform keeps only the `price=` line; the validators then agree byte for byte.
//! * `raw` is the same fetch without a transform: validators that see different bytes disagree.
//! * `post` sends a body and signs the whole response body.
//! * `trap` and `spin` have transforms that misbehave (a state write, an endless loop); the
//!   validators sign a failure instead of a result.
//! * `leak` is an action whose first phase tries to write state: the VM traps it (read-only).
//! * Every `<name>_callback` must start with `action::require_callback()`: only the chain, after
//!   the validators agreed, may call it. Results are appended to the `results` log.
//! * `publish` asks the committee to sign a report for other chains.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use btb_sdk::btb;
use btb_sdk::host;
use btb_sdk::oracle::action::{self, HttpRequest, HttpResponse};
use btb_sdk::oracle::report;
use btb_sdk::prelude::String;
use btb_sdk::Address;

const RESULTS: &[u8] = b"results";

/// Append `(status, data)` to the log.
fn log(id: &Address, args: &[u8], status: u8, data: &[u8]) {
    let mut all = host::storage_get(RESULTS).unwrap_or_default();
    all.push(status);
    all.extend_from_slice(&(data.len() as u32).to_le_bytes());
    all.extend_from_slice(data);
    all.extend_from_slice(&(args.len() as u32).to_le_bytes());
    all.extend_from_slice(args);
    all.extend_from_slice(&id.0);
    host::storage_set(RESULTS, &all);
}

/// The value after `key=` on its own line, if any.
fn line_value<'a>(body: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    body.split(|b| *b == b'\n').find_map(|l| {
        l.strip_prefix(key)
            .and_then(|r| r.strip_prefix(b"="))
            .map(|v| v.strip_suffix(b"\r").unwrap_or(v))
    })
}

#[btb::action]
pub fn quote(url: String) -> HttpRequest {
    HttpRequest::get(url).limits(4096, 2000)
}

/// Keep only the stable `price=` line, as a number (the body also carries a nonce and a
/// timestamp that differ between validators). The result the validators sign is the encoding of
/// this return value: 8 bytes, little endian.
#[btb::transform("quote")]
pub fn quote_transform(resp: HttpResponse) -> u64 {
    if resp.status != 200 {
        host::abort("upstream not ok");
    }
    let Some(v) = line_value(&resp.body, b"price") else {
        host::abort("no price in response");
    };
    if v.is_empty() || v.len() > 18 || !v.iter().all(u8::is_ascii_digit) {
        host::abort("price is not a number");
    }
    v.iter().fold(0u64, |n, d| n * 10 + u64::from(d - b'0'))
}

#[btb::mutation]
pub fn quote_callback(id: Address, args: Vec<u8>, status: u8, data: Vec<u8>) {
    action::require_callback();
    log(&id, &args, status, &data);
}

#[btb::action]
pub fn raw(url: String) -> HttpRequest {
    HttpRequest::get(url).limits(4096, 2000)
}

#[btb::mutation]
pub fn raw_callback(id: Address, args: Vec<u8>, status: u8, data: Vec<u8>) {
    action::require_callback();
    log(&id, &args, status, &data);
}

#[btb::action]
pub fn post(url: String, body: Vec<u8>) -> HttpRequest {
    HttpRequest::post(url, body)
        .header("content-type", "text/plain")
        .limits(4096, 2000)
}

#[btb::mutation]
pub fn post_callback(id: Address, args: Vec<u8>, status: u8, data: Vec<u8>) {
    action::require_callback();
    log(&id, &args, status, &data);
}

#[btb::action]
pub fn trap(url: String) -> HttpRequest {
    HttpRequest::get(url).limits(4096, 2000)
}

/// Wrong: a transform that writes state. The VM traps it, the validators sign a failure.
#[btb::transform("trap")]
pub fn trap_transform(resp: HttpResponse) -> Vec<u8> {
    host::storage_set(b"x", &resp.body);
    resp.body
}

#[btb::mutation]
pub fn trap_callback(id: Address, args: Vec<u8>, status: u8, data: Vec<u8>) {
    action::require_callback();
    log(&id, &args, status, &data);
}

#[btb::action]
pub fn spin(url: String) -> HttpRequest {
    HttpRequest::get(url).limits(4096, 2000)
}

/// Wrong: a transform that never ends. It runs out of gas.
#[btb::transform("spin")]
pub fn spin_transform(resp: HttpResponse) -> Vec<u8> {
    let mut n = resp.body.len() as u64;
    loop {
        n = n.wrapping_mul(6364136223846793005).wrapping_add(1);
        if n == 0 {
            return resp.body;
        }
    }
}

#[btb::mutation]
pub fn spin_callback(id: Address, args: Vec<u8>, status: u8, data: Vec<u8>) {
    action::require_callback();
    log(&id, &args, status, &data);
}

/// Wrong: the first phase writes state. It runs read-only, so the VM traps it.
#[btb::action]
pub fn leak(url: String) -> HttpRequest {
    host::storage_set(b"leak", url.as_bytes());
    HttpRequest::get(url)
}

#[btb::mutation]
pub fn leak_callback(id: Address, args: Vec<u8>, status: u8, data: Vec<u8>) {
    action::require_callback();
    log(&id, &args, status, &data);
}

/// An action with no callback: the chain refuses to take requests for it.
#[btb::action]
pub fn orphan(url: String) -> HttpRequest {
    HttpRequest::get(url)
}

/// An action whose first phase returns a request the chain must refuse (a user-info URL).
#[btb::action]
pub fn sneaky(url: String) -> HttpRequest {
    let _ = url;
    HttpRequest::get("https://user:pw@api.example.com/")
}

#[btb::mutation]
pub fn sneaky_callback(id: Address, args: Vec<u8>, status: u8, data: Vec<u8>) {
    action::require_callback();
    log(&id, &args, status, &data);
}

/// Wrong: the second lock of a callback, called by a user. It aborts.
#[btb::mutation]
pub fn guarded() {
    action::require_callback();
}

/// The result log: `status:u8 || len:u32 || data || len:u32 || args || id[32]` per delivery.
#[btb::query]
pub fn last() -> Vec<u8> {
    host::storage_get(RESULTS).unwrap_or_default()
}

/// Ask the beacon committee to sign `data` as a report for other chains; returns its id.
#[btb::mutation]
pub fn publish(data: Vec<u8>) -> u64 {
    report::request(&data)
}

/// Wrong: a query asking for a report. The VM traps it (no state changes in a query).
#[btb::query]
pub fn cheat_report() -> u64 {
    report::request(b"x")
}
