//! Request coalescing: concurrent callers with the same key share one execution.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Condvar, Mutex};

struct Slot<V> {
    result: Mutex<Option<Option<V>>>,
    done: Condvar,
}

/// Runs a closure once per key at a time; callers that arrive meanwhile wait for its result.
pub struct Flight<K, V> {
    slots: Mutex<HashMap<K, Arc<Slot<V>>>>,
}

impl<K, V> Default for Flight<K, V> {
    fn default() -> Self {
        Flight {
            slots: Mutex::new(HashMap::new()),
        }
    }
}

struct Guard<'a, K: Hash + Eq + Clone, V: Clone> {
    flight: &'a Flight<K, V>,
    key: K,
    slot: Arc<Slot<V>>,
    value: Option<V>,
}

impl<K: Hash + Eq + Clone, V: Clone> Drop for Guard<'_, K, V> {
    fn drop(&mut self) {
        // Remove first, so a caller arriving after the result is stored starts a new flight.
        self.flight
            .slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
        *self.slot.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(self.value.take());
        self.slot.done.notify_all();
    }
}

impl<K: Hash + Eq + Clone, V: Clone> Flight<K, V> {
    /// Run `f` unless a call with `key` is already running, in which case wait for it.
    /// Returns the value and whether this caller was the one that ran `f`. `None` only for a
    /// waiter whose leader panicked.
    pub fn run(&self, key: K, f: impl FnOnce() -> V) -> (Option<V>, bool) {
        let (slot, leader) = {
            let mut m = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            match m.get(&key) {
                Some(s) => (s.clone(), false),
                None => {
                    let s = Arc::new(Slot {
                        result: Mutex::new(None),
                        done: Condvar::new(),
                    });
                    m.insert(key.clone(), s.clone());
                    (s, true)
                }
            }
        };
        if leader {
            let mut g = Guard {
                flight: self,
                key,
                slot,
                value: None,
            };
            let v = f();
            g.value = Some(v.clone());
            drop(g);
            (Some(v), true)
        } else {
            let mut r = slot.result.lock().unwrap_or_else(|e| e.into_inner());
            while r.is_none() {
                r = slot.done.wait(r).unwrap_or_else(|e| e.into_inner());
            }
            (r.clone().flatten(), false)
        }
    }
}
