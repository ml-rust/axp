//! In-memory job store and working-directory resolver.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::SystemTime;

use axp_proto::{JobId, JobPayload, SessionId};

use crate::{
    Error, Result,
    job::{JobStatus, LogBuffer},
    workspace::Workspace,
};

// ── Job ───────────────────────────────────────────────────────────────────────

/// A live job bound to a session, with its runtime state and log buffer.
///
/// `Job` is intentionally not `Clone` — it always lives behind
/// `Arc<RwLock<Job>>` in the [`JobStore`] so all readers share the same
/// instance.
#[derive(Debug)]
pub struct Job {
    /// The unique identifier for this job.
    pub id: JobId,
    /// The session this job belongs to.
    pub session_id: SessionId,
    /// The payload (command or code) this job executes.
    pub payload: JobPayload,
    /// The resolved working directory within the workspace.
    pub cwd: PathBuf,
    /// Current lifecycle status of the job.
    pub status: JobStatus,
    /// Wall-clock time at which this job was created.
    pub created_at: SystemTime,
    /// Wall-clock time at which the process started, if it has started.
    pub started_at: Option<SystemTime>,
    /// Wall-clock time at which the process finished, if it has finished.
    pub finished_at: Option<SystemTime>,
    /// Accumulated stdout/stderr log chunks.
    pub log_buffer: LogBuffer,
}

impl Job {
    /// Create a new job in the [`JobStatus::Pending`] state.
    ///
    /// The caller is responsible for supplying a `cwd` that has already been
    /// validated against the workspace (see [`resolve_cwd`]).
    pub fn new(id: JobId, session_id: SessionId, payload: JobPayload, cwd: PathBuf) -> Self {
        Self {
            id,
            session_id,
            payload,
            cwd,
            status: JobStatus::Pending,
            created_at: SystemTime::now(),
            started_at: None,
            finished_at: None,
            log_buffer: LogBuffer::new(),
        }
    }
}

// ── JobStore ──────────────────────────────────────────────────────────────────

/// Both job indices, held under a single lock so inserts and removals update
/// them atomically.
#[derive(Debug, Default)]
struct Indices {
    /// Jobs keyed by id.
    jobs: HashMap<JobId, Arc<RwLock<Job>>>,
    /// Secondary index: the job ids belonging to each session.
    by_session: HashMap<SessionId, Vec<JobId>>,
}

/// Thread-safe, in-memory store of live [`Job`]s, indexed by id and session.
///
/// Cheap to clone — all clones share the same underlying indices via [`Arc`].
///
/// # Concurrency model
///
/// All operations are **synchronous** (no async, no tokio).  Both indices live
/// under a single `std::sync::RwLock`, so inserts and removals update them
/// atomically and a concurrent reader never sees them disagree.
///
/// # Panic policy
///
/// No method calls `unwrap`, `expect`, or `panic!`.  Poisoned locks are
/// recovered with `unwrap_or_else(|p| p.into_inner())`.  Every critical section
/// performs only infallible operations (`HashMap` insert/remove/get, `Arc`
/// clone), so a panic inside the lock cannot leave the indices in a broken state.
#[derive(Debug, Default, Clone)]
pub struct JobStore {
    inner: Arc<RwLock<Indices>>,
}

impl JobStore {
    /// Create a new, empty `JobStore`.
    pub fn new() -> Self {
        Self::default()
    }

    // ── Poison-recovery helpers ───────────────────────────────────────────────
    //
    // If a thread panics while holding the lock it becomes "poisoned".  We
    // recover the guard instead of propagating the poison because every
    // critical section in this module is infallible (HashMap insert/remove/get,
    // Arc clone) — a panic inside those sections cannot leave the indices in a
    // logically broken state.  Recovering avoids both panicking-in-library-code
    // (which would abort callers) and polluting every return type with a poison
    // error.

    fn read(&self) -> RwLockReadGuard<'_, Indices> {
        self.inner.read().unwrap_or_else(|p| p.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Indices> {
        self.inner.write().unwrap_or_else(|p| p.into_inner())
    }

    /// Insert a job into the store and return a shared handle to it.
    ///
    /// The job is indexed both by its [`JobId`] and under its [`SessionId`]; both
    /// indices are updated under a single lock so a concurrent reader never sees
    /// them disagree.
    pub fn insert(&self, job: Job) -> Arc<RwLock<Job>> {
        let id = job.id.clone();
        let session_id = job.session_id.clone();
        let handle = Arc::new(RwLock::new(job));
        let mut idx = self.write();
        idx.jobs.insert(id.clone(), Arc::clone(&handle));
        idx.by_session.entry(session_id).or_default().push(id);
        handle
    }

    /// Look up a job by its [`JobId`], returning a shared handle or `None`.
    pub fn get(&self, id: &JobId) -> Option<Arc<RwLock<Job>>> {
        self.read().jobs.get(id).map(Arc::clone)
    }

    /// Return all job ids that belong to `sid`, or an empty vec if unknown.
    pub fn list_for_session(&self, sid: &SessionId) -> Vec<JobId> {
        self.read().by_session.get(sid).cloned().unwrap_or_default()
    }

    /// Remove the job with `id` from the store, returning its handle if present.
    ///
    /// Also removes the id from the session index, atomically with the primary
    /// removal.  Returns `None` if no job with `id` exists.
    ///
    /// Lock ordering: the store lock is acquired first, then the individual
    /// job's lock (to read its `session_id`).  This ordering is never reversed
    /// elsewhere (other code locks a job WITHOUT holding the store lock), so the
    /// nested acquisition cannot deadlock.
    pub fn remove(&self, id: &JobId) -> Option<Arc<RwLock<Job>>> {
        let mut idx = self.write();
        let handle = idx.jobs.remove(id)?;
        let session_id = handle
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .session_id
            .clone();
        if let Some(ids) = idx.by_session.get_mut(&session_id) {
            ids.retain(|jid| jid != id);
        }
        Some(handle)
    }
}

// ── resolve_cwd ───────────────────────────────────────────────────────────────

/// Resolve an optional relative working directory against a [`Workspace`] root.
///
/// - `None` → workspace root.
/// - `Some(r)` → `workspace.root().join(r)`, then canonicalized.
///
/// Returns [`Error::WorkspaceViolation`] if:
/// - the path does not exist (canonicalize fails), or
/// - the canonical path falls outside the workspace root.
pub fn resolve_cwd(rel: Option<&str>, workspace: &Workspace) -> Result<PathBuf> {
    let base = match rel {
        None => workspace.root().to_path_buf(),
        Some(r) => workspace.root().join(r),
    };
    let canonical =
        std::fs::canonicalize(&base).map_err(|_| Error::WorkspaceViolation { path: base })?;
    if !workspace.contains(&canonical) {
        return Err(Error::WorkspaceViolation { path: canonical });
    }
    Ok(canonical)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axp_proto::JobPayload;

    fn make_workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = Workspace::new(dir.path()).expect("workspace");
        (dir, ws)
    }

    fn make_job(id: &str, session: &str, ws: &Workspace) -> Job {
        Job::new(
            JobId(id.to_owned()),
            SessionId(session.to_owned()),
            JobPayload::Command {
                command: "echo hi".into(),
            },
            ws.root().to_path_buf(),
        )
    }

    // ── resolve_cwd ──────────────────────────────────────────────────────────

    #[test]
    fn resolve_cwd_none_returns_canonical_root() {
        let (dir, ws) = make_workspace();
        let result = resolve_cwd(None, &ws).unwrap();
        let expected = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn resolve_cwd_existing_subdir_is_within_workspace() {
        let (dir, ws) = make_workspace();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let result = resolve_cwd(Some("sub"), &ws).unwrap();
        assert!(ws.contains(&result));
    }

    #[test]
    fn resolve_cwd_nonexistent_subdir_returns_violation() {
        let (_dir, ws) = make_workspace();
        let result = resolve_cwd(Some("nonexistent"), &ws);
        assert!(
            matches!(result, Err(Error::WorkspaceViolation { .. })),
            "expected WorkspaceViolation, got {result:?}"
        );
    }

    #[test]
    fn resolve_cwd_absolute_escape_returns_violation() {
        let (_dir, ws) = make_workspace();
        // Joining an absolute path replaces the base entirely via std::path::Path::join.
        let result = resolve_cwd(Some("/etc"), &ws);
        assert!(
            matches!(result, Err(Error::WorkspaceViolation { .. })),
            "expected WorkspaceViolation for /etc escape, got {result:?}"
        );
    }

    // ── JobStore ──────────────────────────────────────────────────────────────

    #[test]
    fn insert_and_get_returns_handle() {
        let (_dir, ws) = make_workspace();
        let store = JobStore::new();
        let job = make_job("j_1", "s_1", &ws);
        let handle = store.insert(job);
        let got = store
            .get(&JobId("j_1".into()))
            .expect("job must be present");
        assert_eq!(handle.read().unwrap().id, got.read().unwrap().id);
    }

    #[test]
    fn get_unknown_id_returns_none() {
        let store = JobStore::new();
        assert!(store.get(&JobId("j_unknown".into())).is_none());
    }

    #[test]
    fn list_for_session_groups_jobs_under_session() {
        let (_dir, ws) = make_workspace();
        let store = JobStore::new();
        store.insert(make_job("j_1", "s_1", &ws));
        store.insert(make_job("j_2", "s_1", &ws));
        store.insert(make_job("j_3", "s_2", &ws));

        let mut ids = store.list_for_session(&SessionId("s_1".into()));
        ids.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(ids, vec![JobId("j_1".into()), JobId("j_2".into())]);
    }

    #[test]
    fn list_for_session_unknown_session_returns_empty() {
        let store = JobStore::new();
        let ids = store.list_for_session(&SessionId("s_unknown".into()));
        assert!(ids.is_empty());
    }

    #[test]
    fn remove_returns_handle_and_subsequent_get_is_none() {
        let (_dir, ws) = make_workspace();
        let store = JobStore::new();
        store.insert(make_job("j_1", "s_1", &ws));

        let removed = store.remove(&JobId("j_1".into()));
        assert!(removed.is_some());
        assert!(store.get(&JobId("j_1".into())).is_none());
    }

    #[test]
    fn remove_updates_session_index() {
        let (_dir, ws) = make_workspace();
        let store = JobStore::new();
        store.insert(make_job("j_1", "s_1", &ws));
        store.insert(make_job("j_2", "s_1", &ws));

        store.remove(&JobId("j_1".into()));
        let ids = store.list_for_session(&SessionId("s_1".into()));
        assert!(!ids.contains(&JobId("j_1".into())));
        assert!(ids.contains(&JobId("j_2".into())));
    }
}
