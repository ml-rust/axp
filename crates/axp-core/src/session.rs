//! Runtime session model and in-memory session store.
//!
//! A [`Session`] binds a [`SessionId`] to a [`Workspace`], an enforcement tier,
//! and a set of capability grants.  The [`SessionStore`] manages live sessions
//! in memory using a shared, lock-protected [`HashMap`].
//!
//! # Concurrency model
//!
//! [`SessionStore`] is `Clone` and cheap to share across threads — the interior
//! [`Arc`] means all clones share the same underlying map.  All operations are
//! **synchronous** (no async, no tokio).  Locking uses `std::sync::RwLock`.
//!
//! # Panic policy
//!
//! No method in this module calls `unwrap`, `expect`, or `panic!`.  Poisoned
//! locks are recovered with `unwrap_or_else(|p| p.into_inner())`.  This is
//! safe because every critical section performs only infallible operations
//! (`HashMap` inserts/removes), so a panic inside the lock would represent a
//! bug in the Rust standard library itself rather than a broken data invariant;
//! recovery cannot expose inconsistent state.

use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::SystemTime;

use axp_proto::{EnforcementTier, JobId, JobStatusProto, SessionId};

use crate::{Error, Result, capability::CapabilitySet, workspace::Workspace};

// ── AuditEvent ────────────────────────────────────────────────────────────────

/// A single immutable audit-log entry associated with a [`Session`].
#[derive(Debug, Clone)]
pub struct AuditEvent {
    /// Wall-clock time when the event was recorded.
    pub timestamp: SystemTime,
    /// The kind of event.
    pub kind: AuditEventKind,
}

/// The kind of audit event recorded against a session.
///
/// Marked `#[non_exhaustive]` so new event kinds can be added in future
/// versions without breaking existing `match` expressions.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AuditEventKind {
    /// The session was opened (capabilities granted, workspace locked in).
    SessionOpened,
    /// The session was closed (capabilities revoked).
    SessionClosed,
    /// A job was started in this session.
    JobStarted {
        /// The id of the job that started.
        job_id: JobId,
    },
    /// A job in this session reached a terminal state.
    JobFinished {
        /// The id of the job that finished.
        job_id: JobId,
        /// The terminal status the job reached.
        status: JobStatusProto,
    },
}

// ── Session ───────────────────────────────────────────────────────────────────

/// A live agent session binding an identity, workspace, enforcement tier, and
/// capability grants into a single auditable unit.
#[derive(Debug)]
pub struct Session {
    /// The opaque session identifier assigned by the caller.
    pub id: SessionId,
    /// The filesystem workspace root this session is scoped to.
    pub workspace: Workspace,
    /// The sandbox enforcement tier under which this session runs.
    pub tier: EnforcementTier,
    /// The set of capability grants held by this session.
    pub capabilities: CapabilitySet,
    /// The wall-clock time at which this session was created.
    pub created_at: SystemTime,
    /// Ordered audit log (append-only during the session lifetime).
    pub(crate) audit_events: Vec<AuditEvent>,
}

impl Session {
    /// Create a new session (visible inside the crate only; the public entry
    /// point is [`SessionStore::open`]).
    pub(crate) fn new(
        id: SessionId,
        workspace: Workspace,
        tier: EnforcementTier,
        capabilities: CapabilitySet,
    ) -> Self {
        Self {
            id,
            workspace,
            tier,
            capabilities,
            created_at: SystemTime::now(),
            audit_events: Vec::new(),
        }
    }

    /// Append an audit event to this session's log.
    pub fn record_audit(&mut self, kind: AuditEventKind) {
        self.audit_events.push(AuditEvent {
            timestamp: SystemTime::now(),
            kind,
        });
    }

    /// A read-only view of all audit events recorded for this session.
    pub fn audit_events(&self) -> &[AuditEvent] {
        &self.audit_events
    }
}

// ── SessionStore ──────────────────────────────────────────────────────────────

/// Thread-safe, in-memory store of live [`Session`]s.
///
/// Cheap to clone — all clones share the same underlying map via an [`Arc`].
#[derive(Debug, Default, Clone)]
pub struct SessionStore {
    inner: Arc<RwLock<HashMap<SessionId, Arc<RwLock<Session>>>>>,
}

impl SessionStore {
    /// Create a new, empty `SessionStore`.
    pub fn new() -> Self {
        Self::default()
    }

    // ── Poison-recovery helpers ───────────────────────────────────────────────
    //
    // If a thread panics while holding the map lock the lock becomes
    // "poisoned".  We recover the guard instead of propagating the poison
    // because every critical section in this module is infallible (HashMap
    // insert/remove/get, Arc clone) — a panic inside those sections cannot
    // leave the HashMap in a logically broken state.  Recovering avoids both
    // panicking-in-library-code (which would abort callers) and polluting
    // every return type with a poison error.

    fn read_map(&self) -> RwLockReadGuard<'_, HashMap<SessionId, Arc<RwLock<Session>>>> {
        self.inner.read().unwrap_or_else(|p| p.into_inner())
    }

    fn write_map(&self) -> RwLockWriteGuard<'_, HashMap<SessionId, Arc<RwLock<Session>>>> {
        self.inner.write().unwrap_or_else(|p| p.into_inner())
    }

    /// Open a new session and insert it into the store.
    ///
    /// A [`SessionOpened`](AuditEventKind::SessionOpened) event is recorded on
    /// the session before it is returned.  The caller supplies the `id` — the
    /// store does not generate identifiers.
    ///
    /// Returns an `Arc<RwLock<Session>>` handle that the caller can hold
    /// alongside (or instead of) using [`get`](Self::get) later.
    pub fn open(
        &self,
        id: SessionId,
        workspace: Workspace,
        tier: EnforcementTier,
        capabilities: CapabilitySet,
    ) -> Arc<RwLock<Session>> {
        let mut session = Session::new(id.clone(), workspace, tier, capabilities);
        session.record_audit(AuditEventKind::SessionOpened);

        let handle = Arc::new(RwLock::new(session));
        self.write_map().insert(id, Arc::clone(&handle));
        handle
    }

    /// Look up a live session by id, returning a shared handle or `None`.
    pub fn get(&self, id: &SessionId) -> Option<Arc<RwLock<Session>>> {
        self.read_map().get(id).map(Arc::clone)
    }

    /// Remove the session with `id` from the store and record a
    /// [`SessionClosed`](AuditEventKind::SessionClosed) event.
    ///
    /// Returns [`Error::SessionNotFound`] if no session with that id exists.
    pub fn close(&self, id: &SessionId) -> Result<()> {
        let handle = self
            .write_map()
            .remove(id)
            .ok_or_else(|| Error::SessionNotFound(id.clone()))?;

        // Record the close event on the session.  Recover from poison for the
        // same reason as the map lock.
        handle
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .record_audit(AuditEventKind::SessionClosed);

        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axp_proto::EnforcementTier;

    fn make_workspace() -> Workspace {
        // The temp dir only needs to exist while `Workspace::new` canonicalizes
        // it; afterwards the `Workspace` holds the resolved path and `contains`
        // is pure path logic that never touches disk. So we let the `TempDir`
        // guard drop here (auto-cleanup) rather than leaking it on disk.
        let dir = tempfile::tempdir().expect("tempdir");
        Workspace::new(dir.path()).expect("workspace")
    }

    fn make_store() -> SessionStore {
        SessionStore::new()
    }

    fn sid(s: &str) -> SessionId {
        SessionId(s.to_owned())
    }

    #[test]
    fn open_and_get_returns_matching_session() {
        let store = make_store();
        let id = sid("s_1");
        let ws = make_workspace();
        store.open(
            id.clone(),
            ws,
            EnforcementTier::DevNone,
            CapabilitySet::default(),
        );

        let handle = store.get(&id).expect("session should be present");
        let session = handle.read().unwrap();
        assert_eq!(session.id, id);
    }

    #[test]
    fn get_unknown_id_returns_none() {
        let store = make_store();
        assert!(store.get(&sid("s_unknown")).is_none());
    }

    #[test]
    fn close_removes_session() {
        let store = make_store();
        let id = sid("s_2");
        store.open(
            id.clone(),
            make_workspace(),
            EnforcementTier::DevNone,
            CapabilitySet::default(),
        );

        store.close(&id).expect("close should succeed");
        assert!(
            store.get(&id).is_none(),
            "session should be absent after close"
        );
    }

    #[test]
    fn close_unknown_session_returns_session_not_found() {
        let store = make_store();
        let result = store.close(&sid("s_ghost"));
        match result {
            Err(Error::SessionNotFound(id)) => {
                assert_eq!(id.as_str(), "s_ghost");
            }
            other => panic!("expected SessionNotFound, got {other:?}"),
        }
    }

    #[test]
    fn opened_session_has_session_opened_audit_event() {
        let store = make_store();
        let id = sid("s_3");
        let handle = store.open(
            id.clone(),
            make_workspace(),
            EnforcementTier::DevNone,
            CapabilitySet::default(),
        );

        let session = handle.read().unwrap();
        let events = session.audit_events();
        assert_eq!(events.len(), 1);
        assert!(
            matches!(events[0].kind, AuditEventKind::SessionOpened),
            "first audit event should be SessionOpened"
        );
    }

    #[test]
    fn closed_session_has_session_closed_audit_event() {
        let store = make_store();
        let id = sid("s_4");
        let handle = store.open(
            id.clone(),
            make_workspace(),
            EnforcementTier::DevNone,
            CapabilitySet::default(),
        );

        store.close(&id).unwrap();

        // The handle we kept before removal should reflect the closed event.
        let session = handle.read().unwrap();
        let events = session.audit_events();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].kind, AuditEventKind::SessionOpened));
        assert!(matches!(events[1].kind, AuditEventKind::SessionClosed));
    }

    #[test]
    fn store_clone_shares_state() {
        let store1 = make_store();
        let store2 = store1.clone();
        let id = sid("s_shared");
        store1.open(
            id.clone(),
            make_workspace(),
            EnforcementTier::DevNone,
            CapabilitySet::default(),
        );

        assert!(
            store2.get(&id).is_some(),
            "cloned store should see sessions opened via original"
        );
    }

    #[test]
    fn session_created_at_is_set() {
        let before = SystemTime::now();
        let store = make_store();
        let handle = store.open(
            sid("s_time"),
            make_workspace(),
            EnforcementTier::DevNone,
            CapabilitySet::default(),
        );
        let after = SystemTime::now();
        let session = handle.read().unwrap();
        assert!(session.created_at >= before);
        assert!(session.created_at <= after);
    }
}
