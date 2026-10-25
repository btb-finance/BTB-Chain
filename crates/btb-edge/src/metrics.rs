//! Counters, rendered in the Prometheus text format at `/metrics`.

use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct Metrics {
    c: Mutex<BTreeMap<&'static str, u64>>,
}

impl Metrics {
    pub fn inc(&self, name: &'static str) {
        self.add(name, 1);
    }
    pub fn add(&self, name: &'static str, n: u64) {
        *self
            .c
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(name)
            .or_insert(0) += n;
    }
    pub fn get(&self, name: &str) -> u64 {
        self.c
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .copied()
            .unwrap_or(0)
    }
    /// `extra` are gauges the caller adds (cache sizes).
    pub fn render(&self, extra: &[(&str, u64)]) -> String {
        let mut out = String::new();
        for (k, v) in self.c.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            out.push_str(&format!("btb_edge_{k} {v}\n"));
        }
        for (k, v) in extra {
            out.push_str(&format!("btb_edge_{k} {v}\n"));
        }
        out
    }
}
