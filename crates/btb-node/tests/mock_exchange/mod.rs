//! A tiny local HTTP server that plays an exchange's REST API in tests. No external network:
//! it binds 127.0.0.1 and the tests point sources at it with `FetchPolicy::test_mode()`.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct MockResp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub delay_ms: u64,
}

impl MockResp {
    pub fn json(body: &str) -> MockResp {
        MockResp {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.as_bytes().to_vec(),
            delay_ms: 0,
        }
    }
}

pub type Handler = dyn Fn(&str) -> MockResp + Send + Sync;

pub struct MockExchange {
    pub port: u16,
    pub hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl Drop for MockExchange {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // wake the accept loop
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
    }
}

impl MockExchange {
    pub fn start(handler: impl Fn(&str) -> MockResp + Send + Sync + 'static) -> MockExchange {
        let handler: Arc<Handler> = Arc::new(handler);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (h2, s2) = (hits.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if s2.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(mut c) = conn else { continue };
                let (handler, hits) = (handler.clone(), h2.clone());
                std::thread::spawn(move || {
                    let _ = c.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 1024];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match c.read(&mut tmp) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    hits.fetch_add(1, Ordering::Relaxed);
                    let line = String::from_utf8_lossy(&buf);
                    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let r = handler(&path);
                    if r.delay_ms > 0 {
                        std::thread::sleep(Duration::from_millis(r.delay_ms));
                    }
                    let mut head = format!(
                        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                        r.status,
                        r.body.len()
                    );
                    for (n, v) in &r.headers {
                        head.push_str(&format!("{n}: {v}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = c.write_all(head.as_bytes());
                    let _ = c.write_all(&r.body);
                });
            }
        });
        MockExchange { port, hits, stop }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }
}
