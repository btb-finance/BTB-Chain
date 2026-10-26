//! A minimal blocking HTTP client for the chain watchers: ureq with rustls, no redirects,
//! bounded time and size. Endpoints come from the operator's config (their own full node or a
//! provider they chose), so private addresses are fine; plain `http://` is only accepted for
//! loopback hosts unless the config says `allow_http`.

use std::time::Duration;

pub struct Http {
    agent: ureq::Agent,
    max: usize,
    allow_http: bool,
}

impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Http")
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64(b: &[u8]) -> String {
    let mut o = String::new();
    for c in b.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                o.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                o.push('=');
            }
        }
    }
    o
}

fn loopback(url: &str) -> bool {
    let rest = url.strip_prefix("http://").unwrap_or("");
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or("");
    let name = if let Some(v6) = host.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    name == "localhost"
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|i| i.is_loopback())
}

impl Http {
    pub fn new(timeout: Duration, max_bytes: usize, allow_http: bool) -> Http {
        Http {
            agent: ureq::AgentBuilder::new()
                .timeout(timeout)
                .redirects(0)
                .user_agent("btb-validator-watcher")
                .build(),
            max: max_bytes,
            allow_http,
        }
    }

    /// Reject URLs that would send credentials or consensus-relevant queries in clear text.
    pub fn check_url(&self, url: &str) -> Result<(), String> {
        if url.starts_with("https://")
            || (url.starts_with("http://") && (self.allow_http || loopback(url)))
        {
            Ok(())
        } else {
            Err(format!(
                "{url}: use https (plain http only for loopback or with allow_http)"
            ))
        }
    }

    /// `(status, body)`. Transport failures and over-long bodies are errors; HTTP error statuses
    /// are returned (JSON-RPC servers answer errors with 4xx/5xx and a body).
    pub fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, String)],
        body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>), String> {
        use std::io::Read;
        self.check_url(url)?;
        let mut r = self.agent.request(method, url);
        for (n, v) in headers {
            r = r.set(n, v);
        }
        let res = match body {
            Some(b) => r.send_bytes(b),
            None => r.call(),
        };
        let resp = match res {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(ureq::Error::Transport(t)) => return Err(format!("{url}: {t}")),
        };
        let status = resp.status();
        let mut out = Vec::new();
        resp.into_reader()
            .take(self.max as u64 + 1)
            .read_to_end(&mut out)
            .map_err(|e| format!("{url}: {e}"))?;
        if out.len() > self.max {
            return Err(format!("{url}: response too large"));
        }
        Ok((status, out))
    }

    /// GET, success statuses only.
    pub fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        let (s, b) = self.request("GET", url, &[], None)?;
        if (200..300).contains(&s) {
            Ok(b)
        } else {
            Err(format!("{url}: http status {s}"))
        }
    }

    /// POST a JSON-RPC request and return its `result`; an `error` member or a missing result is
    /// an error. `auth` is a ready `Authorization` header value.
    pub fn json_rpc(
        &self,
        url: &str,
        auth: Option<&str>,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let req =
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut h = vec![("content-type", "application/json".to_string())];
        if let Some(a) = auth {
            h.push(("authorization", a.to_string()));
        }
        let (status, body) = self.request("POST", url, &h, Some(req.to_string().as_bytes()))?;
        let v: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| format!("{method}: malformed response (http {status}): {e}"))?;
        if !v.is_object() {
            return Err(format!("{method}: response is not an object"));
        }
        if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
            return Err(format!("{method}: {e}"));
        }
        match v.get("result") {
            Some(r) => Ok(r.clone()),
            None => Err(format!("{method}: no result (http {status})")),
        }
    }
}
