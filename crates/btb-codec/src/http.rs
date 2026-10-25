//! The two values an HTTPS action exchanges with the chain (`docs/ORACLES.md`, section 3).
//!
//! An `#[btb::action]` function returns an [`HttpRequest`]: what every validator should fetch.
//! A `#[btb::transform]` function takes the [`HttpResponse`] a validator got and returns the
//! bytes the validators sign. Both live here so the SDK (contracts) and the chain agree on the
//! encoding without depending on each other.

use crate::{Decode, DecodeError, Encode, Reader};
use alloc::string::String;
use alloc::vec::Vec;

pub const METHOD_GET: u8 = 0;
pub const METHOD_POST: u8 = 1;

/// What an action asks the validators to fetch.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HttpRequest {
    /// `METHOD_GET` or `METHOD_POST`.
    pub method: u8,
    /// `https://host[:port]/path?query` (plain `http` only where a node allows it for tests).
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Largest response body a validator reads; the chain caps it.
    pub max_response_bytes: u32,
    /// Whole-request time limit in milliseconds; the chain caps it.
    pub timeout_ms: u32,
}

impl HttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        HttpRequest {
            method: METHOD_GET,
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
            max_response_bytes: 16 * 1024,
            timeout_ms: 5_000,
        }
    }

    pub fn post(url: impl Into<String>, body: Vec<u8>) -> Self {
        HttpRequest {
            method: METHOD_POST,
            body,
            ..Self::get(url)
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn limits(mut self, max_response_bytes: u32, timeout_ms: u32) -> Self {
        self.max_response_bytes = max_response_bytes;
        self.timeout_ms = timeout_ms;
        self
    }
}

impl Encode for HttpRequest {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.method.encode_to(out);
        self.url.encode_to(out);
        self.headers.encode_to(out);
        self.body.encode_to(out);
        self.max_response_bytes.encode_to(out);
        self.timeout_ms.encode_to(out);
    }
}

impl Decode for HttpRequest {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(HttpRequest {
            method: u8::decode_from(r)?,
            url: String::decode_from(r)?,
            headers: Vec::decode_from(r)?,
            body: Vec::decode_from(r)?,
            max_response_bytes: u32::decode_from(r)?,
            timeout_ms: u32::decode_from(r)?,
        })
    }
}

/// What one validator got back; the input of the transform function.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Encode for HttpResponse {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.status.encode_to(out);
        self.body.encode_to(out);
    }
}

impl Decode for HttpResponse {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(HttpResponse {
            status: u16::decode_from(r)?,
            body: Vec::decode_from(r)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let q = HttpRequest::post("https://a.example/x?y=1", b"{}".to_vec())
            .header("accept", "application/json")
            .limits(100, 200);
        assert_eq!(HttpRequest::decode(&q.encode()).unwrap(), q);
        let r = HttpResponse {
            status: 200,
            body: b"hi".to_vec(),
        };
        assert_eq!(HttpResponse::decode(&r.encode()).unwrap(), r);
        assert!(HttpResponse::decode(&[1]).is_err());
    }
}
