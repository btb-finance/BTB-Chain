//! Upstream hosting nodes: fetch with failover, and live subscriptions.

use crate::wire::{self, UResp};
use std::io::BufRead;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Validates a response before the edge accepts it (hash checks and the like). A failed check
/// counts like a transport error: the next host is tried.
pub type Check<'a> = &'a dyn Fn(&UResp) -> Result<(), String>;

/// A stream of Server-Sent-Event payloads (`data:` lines).
pub type Events = Box<dyn Iterator<Item = Result<String, String>> + Send>;

/// Where the edge gets its data. [`HttpUpstream`] talks to hosting nodes; tests use fakes.
pub trait Upstream: Send + Sync {
    /// `GET target` from the first host that answers and passes `check`. Statuses below 500
    /// are answers (a 404 is not a reason to try another host), 5xx and transport errors fail
    /// over. `check` runs on every answer.
    fn get(
        &self,
        target: &str,
        headers: &[(String, String)],
        check: Check<'_>,
    ) -> Result<UResp, String>;

    /// Open an SSE stream (`/c/<contract>/subscribe/<fn>?...`).
    fn subscribe(&self, target: &str) -> Result<Events, String>;
}

struct Health {
    host: String,
    down_until: Option<Instant>,
}

/// Several hosting nodes; remembers which one last worked and sets failing ones aside for
/// `cooldown`.
pub struct HttpUpstream {
    hosts: Mutex<Vec<Health>>,
    timeout: Duration,
    cooldown: Duration,
    max_body: usize,
}

impl HttpUpstream {
    pub fn new(hosts: Vec<String>, timeout: Duration, cooldown: Duration, max_body: usize) -> Self {
        HttpUpstream {
            hosts: Mutex::new(
                hosts
                    .into_iter()
                    .map(|host| Health {
                        host,
                        down_until: None,
                    })
                    .collect(),
            ),
            timeout,
            cooldown,
            max_body,
        }
    }

    /// Hosts to try, healthy ones first, in configured order.
    fn order(&self) -> Vec<String> {
        let now = Instant::now();
        let h = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        let up = h.iter().filter(|x| x.down_until.is_none_or(|t| t <= now));
        let down = h.iter().filter(|x| x.down_until.is_some_and(|t| t > now));
        up.chain(down).map(|x| x.host.clone()).collect()
    }

    fn mark(&self, host: &str, ok: bool) {
        let mut h = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(x) = h.iter_mut().find(|x| x.host == host) {
            x.down_until = if ok {
                None
            } else {
                Some(Instant::now() + self.cooldown)
            };
        }
    }
}

impl Upstream for HttpUpstream {
    fn get(
        &self,
        target: &str,
        headers: &[(String, String)],
        check: Check<'_>,
    ) -> Result<UResp, String> {
        let mut errors = Vec::new();
        for host in self.order() {
            let r = wire::request(
                &host,
                "GET",
                target,
                headers,
                &[],
                self.timeout,
                self.max_body,
            )
            .and_then(|r| {
                if r.status >= 500 {
                    Err(format!("{host}: status {}", r.status))
                } else {
                    check(&r).map(|_| r).map_err(|e| format!("{host}: {e}"))
                }
            });
            match r {
                Ok(r) => {
                    self.mark(&host, true);
                    return Ok(r);
                }
                Err(e) => {
                    self.mark(&host, false);
                    errors.push(e);
                }
            }
        }
        Err(format!("no upstream answered: {}", errors.join("; ")))
    }

    fn subscribe(&self, target: &str) -> Result<Events, String> {
        let mut errors = Vec::new();
        for host in self.order() {
            // Idle streams get a ping every 15 s from hosting nodes; 60 s of silence is a dead one.
            match wire::open_sse(&host, target, Duration::from_secs(60)) {
                Ok(r) => {
                    self.mark(&host, true);
                    return Ok(Box::new(Sse { r, done: false }));
                }
                Err(e) => {
                    self.mark(&host, false);
                    errors.push(e);
                }
            }
        }
        Err(format!(
            "no upstream accepted the subscription: {}",
            errors.join("; ")
        ))
    }
}

struct Sse<R: BufRead> {
    r: R,
    done: bool,
}

impl<R: BufRead + Send> Iterator for Sse<R> {
    type Item = Result<String, String>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            let mut line = String::new();
            match self.r.read_line(&mut line) {
                Ok(0) => {
                    self.done = true;
                    return None;
                }
                Ok(_) => {
                    if let Some(d) = line.trim_end().strip_prefix("data:") {
                        return Some(Ok(d.trim().to_string()));
                    }
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e.to_string()));
                }
            }
        }
    }
}
