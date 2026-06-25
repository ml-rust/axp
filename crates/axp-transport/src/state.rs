//! Shared application state threaded through axum handlers.

use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU64, Ordering},
};

use axp_core::{JobEngine, ProviderRegistry, SessionStore};
use axp_proto::SessionId;

/// Shared state for all axum request handlers.
///
/// All fields are cheap to clone: `SessionStore` and `JobEngine` are
/// `Arc`-backed; `registry` is wrapped in an `Arc<RwLock<_>>` here because
/// `ProviderRegistry` is neither `Clone` nor `Send + Sync` on its own.
///
/// # Important invariant
///
/// `sessions` MUST be the same [`SessionStore`] that `engine` was built from
/// (i.e. `engine = JobEngine::new(sessions.clone(), ...)`).  `session.open`
/// inserts into the store that the engine observes for authorization checks —
/// mismatching them is a logic error.
#[derive(Clone)]
pub struct AppState {
    /// Live session store, shared with the job engine.
    pub sessions: SessionStore,

    /// Async job execution engine. Holds a reference to the same session store.
    pub engine: JobEngine,

    /// Capability provider registry, guarded by an `RwLock` because
    /// [`ProviderRegistry`] is not `Clone`.
    pub registry: Arc<RwLock<ProviderRegistry>>,

    /// Monotonically-increasing counter used to mint unique session ids.
    pub session_counter: Arc<AtomicU64>,
}

impl AppState {
    /// Atomically allocate the next session id and return it formatted as
    /// `s_<n>`.
    ///
    /// Uses `Relaxed` ordering — the returned id is unique within this process
    /// but no cross-thread synchronization of other state is implied.
    pub fn next_session_id(&self) -> SessionId {
        let n = self.session_counter.fetch_add(1, Ordering::Relaxed);
        SessionId(format!("s_{n}"))
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use axp_core::{JobStore, SessionStore};

    use super::*;

    fn make_state() -> AppState {
        let sessions = SessionStore::new();
        let engine = JobEngine::new(sessions.clone(), JobStore::new());
        AppState {
            sessions,
            engine,
            registry: Arc::new(RwLock::new(ProviderRegistry::new())),
            session_counter: Arc::new(AtomicU64::new(1)),
        }
    }

    #[test]
    fn next_session_id_is_unique_and_prefixed() {
        let state = make_state();
        let id1 = state.next_session_id();
        let id2 = state.next_session_id();
        assert!(
            id1.as_str().starts_with("s_"),
            "expected s_ prefix: {}",
            id1.as_str()
        );
        assert_ne!(id1, id2, "consecutive ids must differ");
    }

    #[test]
    fn next_session_id_starts_at_one() {
        let state = make_state();
        let id = state.next_session_id();
        assert_eq!(id.as_str(), "s_1");
    }

    #[test]
    fn clone_shares_counter() {
        let state = make_state();
        let clone = state.clone();
        let _id1 = state.next_session_id(); // consumes 1
        let id2 = clone.next_session_id(); // should consume 2
        assert_eq!(id2.as_str(), "s_2");
    }
}
