//! Crash-safety persistence hook. The replica calls `save` *before* any message that
//! depends on the new state is returned, so a restart can never double vote or double propose.

use crate::types::*;
use std::sync::{Arc, Mutex};

/// The minimum state that must survive a crash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafetyState {
    pub last_voted_round: Round,
    pub locked_round: Round,
    pub last_proposed_round: Round,
    pub high_qc: QuorumCert,
    pub committed_height: Height,
    pub committed_round: Round,
    pub committed_id: Hash,
}

pub trait SafetyStorage {
    fn load(&self) -> Option<SafetyState>;
    fn save(&mut self, state: &SafetyState);
}

/// In-memory storage; clones share state (so it survives a simulated restart).
#[derive(Clone, Default)]
pub struct MemoryStorage(Arc<Mutex<Option<SafetyState>>>);

impl SafetyStorage for MemoryStorage {
    fn load(&self) -> Option<SafetyState> {
        self.0.lock().ok().and_then(|g| g.clone())
    }
    fn save(&mut self, state: &SafetyState) {
        if let Ok(mut g) = self.0.lock() {
            *g = Some(state.clone());
        }
    }
}
