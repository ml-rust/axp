//! Async job execution engine: spawns jobs as child processes and drains their
//! stdout/stderr into each job's log buffer.
//!
//! # Concurrency model
//!
//! Unlike the rest of `axp-core`, this module is **async** — it brings in tokio
//! to spawn child processes and read their piped output without blocking.  The
//! job model itself ([`Job`], [`JobStore`]) stays purely `std::sync`; the engine
//! holds the async machinery (the drainer task and the cancellation senders) so
//! the model never has to.
//!
//! # Locking discipline
//!
//! A `std::sync` lock guard is **never** held across an `.await`.  Every critical
//! section acquires its guard, performs only synchronous work (a clone, a log
//! push, a status write), and drops the guard before any await point.  See
//! [`JobEngine::push_log`] and the terminal-state write in
//! [`JobEngine::run_to_completion`] for the canonical pattern.
//!
//! # Panic policy
//!
//! No `unwrap`/`expect`/`panic!` in non-test code.  Poisoned locks are recovered
//! with `unwrap_or_else(|p| p.into_inner())`; this is correct because every
//! critical section performs only infallible operations and is held briefly with
//! no await inside, so a poisoned guard cannot expose a logically broken state.

use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc, Mutex, RwLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::SystemTime;

use bytes::Bytes;
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;

use axp_proto::{JobId, JobPayload, JobStartRequest, SessionId};

use crate::{
    Error, Result,
    capability::RuntimeCapability,
    job::{
        DEFAULT_LOG_BYTE_CAP, Job, JobStatus, JobStore, LogBuffer, LogEvent, LogStream, Seq,
        resolve_cwd,
    },
    session::SessionStore,
};

/// Read buffer size for draining a child's stdout/stderr pipes.
const DRAIN_BUF_SIZE: usize = 8192;

/// Runs jobs as child processes, capturing their output into each job's log buffer.
///
/// The engine does NOT own a tokio runtime — its async methods run on the ambient
/// runtime (call them from within one). Cancellation senders live here (not on `Job`),
/// keeping the job model free of async machinery.
#[derive(Clone)]
pub struct JobEngine {
    sessions: SessionStore,
    jobs: JobStore,
    next_id: Arc<AtomicU64>,
    cancels: Arc<Mutex<HashMap<JobId, oneshot::Sender<()>>>>,
    /// Per-job log-buffer byte cap. A job whose output exceeds this is killed and
    /// marked `Failed` (output is never silently dropped).
    log_byte_cap: usize,
}

impl JobEngine {
    /// Create a new engine over the given session and job stores.
    ///
    /// Job ids begin at `j_1`; the cancellation table starts empty; the per-job
    /// log-buffer cap defaults to [`DEFAULT_LOG_BYTE_CAP`]. Use
    /// [`with_log_byte_cap`](Self::with_log_byte_cap) to override it.
    pub fn new(sessions: SessionStore, jobs: JobStore) -> Self {
        Self {
            sessions,
            jobs,
            next_id: Arc::new(AtomicU64::new(1)),
            cancels: Arc::new(Mutex::new(HashMap::new())),
            log_byte_cap: DEFAULT_LOG_BYTE_CAP,
        }
    }

    /// Override the per-job log-buffer byte cap (default [`DEFAULT_LOG_BYTE_CAP`]).
    ///
    /// A job whose combined stdout+stderr exceeds this is killed and marked
    /// `Failed { reason: "log buffer overflow" }`.
    pub fn with_log_byte_cap(mut self, cap: usize) -> Self {
        self.log_byte_cap = cap;
        self
    }

    /// Borrow the engine's [`JobStore`] so callers can read job state.
    pub fn jobs(&self) -> &JobStore {
        &self.jobs
    }

    /// Allocate the next monotonic [`JobId`] (`j_1`, `j_2`, …).
    fn next_job_id(&self) -> JobId {
        JobId(format!(
            "j_{}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Acquire the cancellation table, recovering from a poisoned lock.
    ///
    /// The guard is held only briefly to insert/remove/send a `oneshot` sender;
    /// no `.await` ever happens while it is held, so poison recovery cannot expose
    /// a logically broken state.
    fn cancels(&self) -> std::sync::MutexGuard<'_, HashMap<JobId, oneshot::Sender<()>>> {
        self.cancels.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Seam for the kernel-sandbox unit. A no-op today; the sandbox unit will configure
    /// Landlock/seccomp/Seatbelt/AppContainer on the command before it is spawned.
    fn apply_sandbox(_cmd: &mut tokio::process::Command, _cwd: &std::path::Path) -> Result<()> {
        Ok(())
    }

    /// Start a job: validate, spawn the child process, and launch the drainer.
    ///
    /// All synchronous validation (session lookup, capability checks, cwd
    /// resolution, job-level attenuation) happens BEFORE the process is spawned.
    /// A [`Job`] is created only after a successful spawn — a job exists iff it
    /// started.  The actual draining/reaping runs on a detached tokio task.
    ///
    /// # Errors
    ///
    /// - [`Error::SessionNotFound`] if the session id is unknown.
    /// - [`Error::CapabilityDenied`] if the session lacks `proc.spawn` for a
    ///   command payload, or lacks any requested job-level capability.
    /// - [`Error::NotImplemented`] for code-mode payloads (not yet supported).
    /// - [`Error::WorkspaceViolation`] if `cwd` resolves outside the workspace.
    /// - [`Error::CapabilityParse`] if a requested capability is malformed.
    /// - [`Error::JobSpawn`] if the child process fails to spawn.
    pub async fn start(&self, req: &JobStartRequest) -> Result<JobId> {
        // 1. Resolve the session and clone out the bits we need under a short read
        //    lock, dropping the guard before any await.
        let session = self
            .sessions
            .get(&req.session_id)
            .ok_or_else(|| Error::SessionNotFound(req.session_id.clone()))?;
        let (workspace, capabilities) = {
            let s = session.read().unwrap_or_else(|p| p.into_inner());
            (s.workspace.clone(), s.capabilities.clone())
        };

        // 2. Determine the command string from the payload, enforcing the
        //    payload-level capability requirement.
        let command = match &req.payload {
            JobPayload::Command { command } => {
                if !capabilities.permits(&RuntimeCapability::ProcSpawn) {
                    return Err(Error::CapabilityDenied {
                        required: "proc.spawn".into(),
                    });
                }
                command
            }
            JobPayload::Code { .. } => {
                return Err(Error::NotImplemented("code-mode execution"));
            }
        };

        // 3. Resolve the working directory against the workspace.
        let cwd = resolve_cwd(req.cwd.as_deref(), &workspace)?;

        // 4. Job-level capability attenuation: every requested capability must be
        //    permitted by the session's grants.
        for wire_cap in &req.capabilities {
            let rc = RuntimeCapability::parse(wire_cap)?;
            if !capabilities.permits(&rc) {
                return Err(Error::CapabilityDenied {
                    required: wire_cap.0.clone(),
                });
            }
        }

        // 5. Build the command.
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);

        // 6. Sandbox seam (no-op today).
        Self::apply_sandbox(&mut cmd, &cwd)?;

        // 7. Spawn. Only after a successful spawn do we create a Job.
        let mut child = cmd.spawn().map_err(|e| Error::JobSpawn {
            reason: e.to_string(),
        })?;

        // 8. Create the job in the Running state and insert it.
        let id = self.next_job_id();
        let mut job = Job::new(id.clone(), req.session_id.clone(), req.payload.clone(), cwd);
        job.log_buffer = LogBuffer::with_cap(self.log_byte_cap);
        job.status = JobStatus::Running;
        job.started_at = Some(SystemTime::now());
        let handle = self.jobs.insert(job);

        // 9. Register the cancellation channel.
        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.cancels().insert(id.clone(), cancel_tx);

        // 10. Take the child's piped stdout/stderr.
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // 11. Spawn the drainer on the ambient runtime.
        let engine = self.clone();
        let jid = id.clone();
        tokio::spawn(async move {
            engine
                .run_to_completion(jid, handle, child, stdout, stderr, cancel_rx)
                .await;
        });

        // 12. Return the new job id.
        Ok(id)
    }

    /// Drive a spawned job to completion: drain its output, honour cancellation,
    /// reap the process, and record the terminal status.
    ///
    /// This runs on a detached tokio task.  It concurrently reads both pipes and
    /// listens for a cancellation signal via `tokio::select!`; on cancellation it
    /// best-effort kills the child but keeps draining until both pipes hit EOF so
    /// no buffered output is lost.  If the log buffer overflows, the job is killed
    /// and marked `Failed`.
    async fn run_to_completion(
        &self,
        job_id: JobId,
        handle: Arc<RwLock<Job>>,
        mut child: tokio::process::Child,
        stdout: Option<tokio::process::ChildStdout>,
        stderr: Option<tokio::process::ChildStderr>,
        mut cancel_rx: oneshot::Receiver<()>,
    ) {
        let mut stdout = stdout;
        let mut stderr = stderr;

        // A pipe that was never piped is "done" up front; we never poll a None pipe.
        let mut out_done = stdout.is_none();
        let mut err_done = stderr.is_none();

        let mut cancelled = false;
        let mut overflowed = false;

        let mut buf_out = [0u8; DRAIN_BUF_SIZE];
        let mut buf_err = [0u8; DRAIN_BUF_SIZE];

        loop {
            tokio::select! {
                biased;

                res = (&mut cancel_rx), if !cancelled => {
                    // Either an explicit cancel or the sender was dropped; treat
                    // both as a one-shot trigger and stop listening afterwards.
                    let _ = res;
                    cancelled = true;
                    let _ = child.start_kill();
                }

                n = read_into(&mut stdout, &mut buf_out), if !out_done => {
                    match n {
                        Ok(0) | Err(_) => out_done = true,
                        Ok(n) => {
                            if self.push_log(&handle, LogStream::Stdout, &buf_out[..n]).is_err() {
                                overflowed = true;
                                let _ = child.start_kill();
                                out_done = true;
                                err_done = true;
                            }
                        }
                    }
                }

                n = read_into(&mut stderr, &mut buf_err), if !err_done => {
                    match n {
                        Ok(0) | Err(_) => err_done = true,
                        Ok(n) => {
                            if self.push_log(&handle, LogStream::Stderr, &buf_err[..n]).is_err() {
                                overflowed = true;
                                let _ = child.start_kill();
                                out_done = true;
                                err_done = true;
                            }
                        }
                    }
                }
            }

            if out_done && err_done {
                break;
            }
        }

        // Reap the process.
        let status = child.wait().await;

        // Compute the terminal status.
        let final_status = if overflowed {
            JobStatus::Failed {
                reason: "log buffer overflow".into(),
            }
        } else if cancelled {
            JobStatus::Killed
        } else {
            match status {
                Ok(es) => match es.code() {
                    Some(c) => JobStatus::Exited { code: c },
                    None => JobStatus::Killed,
                },
                Err(e) => JobStatus::Failed {
                    reason: e.to_string(),
                },
            }
        };

        // Write the terminal state under a short lock with no await inside.
        // The drainer's final state write does NOT push a log event, so it would
        // not otherwise wake live subscribers. Capture the buffer's notify and
        // fire it AFTER the terminal status is written so any `JobLogStream`
        // waiting on a quiet-then-finished job observes the terminal state and
        // returns `None` instead of hanging forever.
        let notify = {
            let mut job = handle.write().unwrap_or_else(|p| p.into_inner());
            job.status = final_status;
            job.finished_at = Some(SystemTime::now());
            job.log_buffer.subscribe()
        };
        notify.notify_waiters();

        // Drop the cancellation sender; the job is finished.
        self.cancels().remove(&job_id);
    }

    /// Push a chunk of output into a job's log buffer.
    ///
    /// The write lock is acquired and dropped entirely within this synchronous
    /// function — there is no `.await` inside — so the locking discipline holds.
    ///
    /// # Errors
    ///
    /// Propagates [`Error::LogBufferOverflow`] from the underlying buffer.
    fn push_log(&self, handle: &Arc<RwLock<Job>>, stream: LogStream, bytes: &[u8]) -> Result<()> {
        let mut job = handle.write().unwrap_or_else(|p| p.into_inner());
        job.log_buffer
            .push(stream, Bytes::copy_from_slice(bytes), SystemTime::now())?;
        Ok(())
    }

    /// Request cancellation of a running job (best-effort SIGKILL via the drainer).
    ///
    /// Returns `Err(JobNotFound)` if the job is unknown or already finished.
    pub fn cancel(&self, job_id: &JobId) -> Result<()> {
        match self.cancels().remove(job_id) {
            Some(tx) => {
                let _ = tx.send(());
                Ok(())
            }
            None => Err(Error::JobNotFound(job_id.clone())),
        }
    }

    /// Look up a job and verify it belongs to `session_id`.
    ///
    /// Returns [`Error::JobNotFound`] for both an unknown job AND a job owned by a
    /// different session — we deliberately do not reveal a job's existence to the
    /// wrong session.
    fn lookup_owned(&self, session_id: &SessionId, job_id: &JobId) -> Result<Arc<RwLock<Job>>> {
        let handle = self
            .jobs
            .get(job_id)
            .ok_or_else(|| Error::JobNotFound(job_id.clone()))?;
        let owned = {
            let j = handle.read().unwrap_or_else(|p| p.into_inner());
            j.session_id == *session_id
        };
        if owned {
            Ok(handle)
        } else {
            Err(Error::JobNotFound(job_id.clone()))
        }
    }

    /// Return the current status and buffered-event count for a job the caller owns.
    ///
    /// # Errors
    ///
    /// [`Error::JobNotFound`] if the job is unknown or owned by another session.
    pub fn status(
        &self,
        req: &axp_proto::JobStatusRequest,
    ) -> Result<axp_proto::JobStatusResponse> {
        let handle = self.lookup_owned(&req.session_id, &req.job_id)?;
        let j = handle.read().unwrap_or_else(|p| p.into_inner());
        Ok(axp_proto::JobStatusResponse {
            job_id: req.job_id.clone(),
            status: j.status.clone().into(),
            seq: j.log_buffer.len() as u64,
        })
    }

    /// Attach to a job's log stream from `from_offset`.
    ///
    /// The returned [`JobLogStream`] replays buffered events at/after the offset,
    /// then yields new events live until the job is terminal and fully drained.
    /// This call is synchronous — it only builds the stream; the awaiting happens
    /// in [`JobLogStream::next`].
    ///
    /// # Errors
    ///
    /// [`Error::JobNotFound`] if the job is unknown or owned by another session.
    pub fn attach(&self, req: &axp_proto::JobAttachRequest) -> Result<JobLogStream> {
        let handle = self.lookup_owned(&req.session_id, &req.job_id)?;
        let notify = {
            let j = handle.read().unwrap_or_else(|p| p.into_inner());
            j.log_buffer.subscribe()
        };
        Ok(JobLogStream {
            job_id: req.job_id.clone(),
            handle,
            notify,
            cursor: req.from_offset,
            pending: VecDeque::new(),
        })
    }
}

/// Convert a runtime [`LogStream`] to its wire form.
fn stream_to_proto(s: LogStream) -> axp_proto::LogStreamProto {
    match s {
        LogStream::Stdout => axp_proto::LogStreamProto::Stdout,
        LogStream::Stderr => axp_proto::LogStreamProto::Stderr,
    }
}

/// Convert a buffered [`LogEvent`] into a wire [`axp_proto::LogEventFrame`].
fn event_to_frame(job_id: &JobId, ev: &LogEvent) -> axp_proto::LogEventFrame {
    let ts_millis = ev
        .timestamp
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    axp_proto::LogEventFrame {
        job_id: job_id.clone(),
        seq: ev.seq,
        stream: stream_to_proto(ev.stream),
        data: ev.data.to_vec(),
        ts_millis,
    }
}

/// A reattachable, replay-then-live view over a single job's log stream.
///
/// Built by [`JobEngine::attach`]. It first replays all buffered events at or
/// after the attach offset, then blocks on the buffer's wake signal to deliver
/// new events as they are pushed, ending once the job is terminal and every
/// buffered event has been delivered.
pub struct JobLogStream {
    job_id: JobId,
    handle: Arc<RwLock<Job>>,
    notify: Arc<tokio::sync::Notify>,
    cursor: Seq,
    pending: VecDeque<axp_proto::LogEventFrame>,
}

impl JobLogStream {
    /// Yield the next log frame, or `None` once the job is terminal and all
    /// buffered output has been delivered.
    pub async fn next(&mut self) -> Option<axp_proto::LogEventFrame> {
        loop {
            if let Some(frame) = self.pending.pop_front() {
                return Some(frame);
            }

            // RACE-FREE WAIT: register interest BEFORE reading state, so a push
            // that happens between our drain and our await is not missed.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let terminal = {
                let job = self.handle.read().unwrap_or_else(|p| p.into_inner());
                let new = job.log_buffer.since(self.cursor);
                for ev in new {
                    self.pending.push_back(event_to_frame(&self.job_id, ev));
                    self.cursor = ev.seq + 1;
                }
                job.status.is_terminal()
            }; // <-- lock dropped here, BEFORE any await

            if !self.pending.is_empty() {
                continue; // got new events; loop pops them
            }
            if terminal {
                return None; // terminal AND nothing buffered → end
            }
            notified.await; // wait for the next push (or terminal-transition push)
        }
    }
}

/// Read from an optional pipe into `buf`.
///
/// Returns `Ok(0)` if the pipe is `None` (so the caller treats it as EOF);
/// otherwise reads from the underlying stream.  This keeps the `select!` arms
/// uniform without ever polling a `None` pipe (the `if !*_done` guard plus the
/// up-front "None is done" marking ensure that arm is disabled when the pipe is
/// absent).
async fn read_into<R>(s: &mut Option<R>, buf: &mut [u8]) -> std::io::Result<usize>
where
    R: AsyncReadExt + Unpin,
{
    match s {
        Some(r) => r.read(buf).await,
        None => Ok(0),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use axp_proto::{Capability, EnforcementTier, SessionId};

    use crate::{capability::CapabilitySet, session::SessionStore, workspace::Workspace};

    /// A test harness bundling the stores, an engine, and the workspace tempdir
    /// (kept alive for the test's duration).
    struct Harness {
        engine: JobEngine,
        session_id: SessionId,
        _dir: tempfile::TempDir,
    }

    /// Build a harness: a session over a fresh tempdir workspace with the given
    /// capabilities, plus a job store and engine.
    fn harness_with(caps: CapabilitySet) -> Harness {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = Workspace::new(dir.path()).expect("workspace");
        let sessions = SessionStore::new();
        let session_id = SessionId("s_test".into());
        sessions.open(session_id.clone(), ws, EnforcementTier::DevNone, caps);
        let jobs = JobStore::new();
        let engine = JobEngine::new(sessions, jobs);
        Harness {
            engine,
            session_id,
            _dir: dir,
        }
    }

    /// A harness whose session holds the `proc.spawn` capability.
    fn harness() -> Harness {
        harness_with(CapabilitySet::new(vec![RuntimeCapability::ProcSpawn]))
    }

    fn command_req(session_id: &SessionId, command: &str) -> JobStartRequest {
        JobStartRequest {
            session_id: session_id.clone(),
            payload: JobPayload::Command {
                command: command.into(),
            },
            cwd: None,
            capabilities: vec![],
        }
    }

    /// Poll a job's status until it reaches a terminal state, with a bounded
    /// timeout so a hung test fails rather than wedging.
    async fn poll_terminal(engine: &JobEngine, id: &JobId) -> JobStatus {
        for _ in 0..500 {
            if let Some(handle) = engine.jobs().get(id) {
                let status = handle.read().unwrap().status.clone();
                if status.is_terminal() {
                    return status;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job {id:?} did not reach a terminal state within timeout");
    }

    /// Concatenate all log bytes for a given stream of a job.
    fn collected_bytes(engine: &JobEngine, id: &JobId, stream: LogStream) -> Vec<u8> {
        let handle = engine.jobs().get(id).expect("job present");
        let job = handle.read().unwrap();
        let mut out = Vec::new();
        for ev in job.log_buffer.since(0) {
            if ev.stream == stream {
                out.extend_from_slice(&ev.data);
            }
        }
        out
    }

    #[tokio::test]
    async fn command_echo_captures_stdout() {
        let h = harness();
        let req = command_req(&h.session_id, "echo hello");
        let id = h.engine.start(&req).await.expect("start");
        let status = poll_terminal(&h.engine, &id).await;
        assert_eq!(status, JobStatus::Exited { code: 0 });
        let out = collected_bytes(&h.engine, &id, LogStream::Stdout);
        assert!(
            out.windows(5).any(|w| w == b"hello"),
            "stdout must contain `hello`, got {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[tokio::test]
    async fn command_writes_stderr() {
        let h = harness();
        let req = command_req(&h.session_id, "echo oops 1>&2");
        let id = h.engine.start(&req).await.expect("start");
        poll_terminal(&h.engine, &id).await;
        let err = collected_bytes(&h.engine, &id, LogStream::Stderr);
        assert!(
            err.windows(4).any(|w| w == b"oops"),
            "stderr must contain `oops`, got {:?}",
            String::from_utf8_lossy(&err)
        );
    }

    #[tokio::test]
    async fn command_nonzero_exit() {
        let h = harness();
        let req = command_req(&h.session_id, "exit 3");
        let id = h.engine.start(&req).await.expect("start");
        let status = poll_terminal(&h.engine, &id).await;
        assert_eq!(status, JobStatus::Exited { code: 3 });
    }

    #[tokio::test]
    async fn command_denied_without_proc_spawn() {
        let h = harness_with(CapabilitySet::default());
        let req = command_req(&h.session_id, "echo nope");
        let result = h.engine.start(&req).await;
        match result {
            Err(Error::CapabilityDenied { required }) => {
                assert_eq!(required, "proc.spawn");
            }
            other => panic!("expected CapabilityDenied, got {other:?}"),
        }
        // No job must have been created.
        assert!(
            h.engine.jobs().get(&JobId("j_1".into())).is_none(),
            "no job should exist after a denied start"
        );
    }

    #[tokio::test]
    async fn command_cwd_outside_workspace() {
        let h = harness();
        let mut req = command_req(&h.session_id, "echo hi");
        req.cwd = Some("/etc".into());
        let result = h.engine.start(&req).await;
        assert!(
            matches!(result, Err(Error::WorkspaceViolation { .. })),
            "expected WorkspaceViolation, got {result:?}"
        );
    }

    #[tokio::test]
    async fn code_payload_not_implemented() {
        let h = harness();
        let req = JobStartRequest {
            session_id: h.session_id.clone(),
            payload: JobPayload::Code {
                code: "x".into(),
                lang: None,
            },
            cwd: None,
            capabilities: vec![],
        };
        let result = h.engine.start(&req).await;
        assert!(
            matches!(result, Err(Error::NotImplemented(_))),
            "expected NotImplemented, got {result:?}"
        );
    }

    #[tokio::test]
    async fn cancel_running_job_marks_killed() {
        let h = harness();
        let req = command_req(&h.session_id, "sleep 30");
        let id = h.engine.start(&req).await.expect("start");
        // Cancel immediately so the test stays fast.
        h.engine.cancel(&id).expect("cancel a running job");
        let status = poll_terminal(&h.engine, &id).await;
        assert_eq!(status, JobStatus::Killed);
    }

    #[tokio::test]
    async fn unknown_session_returns_not_found() {
        let h = harness();
        let req = command_req(&SessionId("s_ghost".into()), "echo hi");
        let result = h.engine.start(&req).await;
        assert!(
            matches!(result, Err(Error::SessionNotFound(_))),
            "expected SessionNotFound, got {result:?}"
        );
    }

    #[tokio::test]
    async fn job_level_capability_attenuation_denied() {
        // Session has proc.spawn but NOT the requested fs.read capability.
        let h = harness();
        let mut req = command_req(&h.session_id, "echo hi");
        req.capabilities = vec![Capability("fs.read(/proj)".into())];
        let result = h.engine.start(&req).await;
        match result {
            Err(Error::CapabilityDenied { required }) => {
                assert_eq!(required, "fs.read(/proj)");
            }
            other => panic!("expected CapabilityDenied for job-level cap, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn output_exceeding_log_cap_kills_job_and_marks_failed() {
        // A tiny log cap forces the very first output chunk to overflow.
        let mut h = harness();
        h.engine = h.engine.with_log_byte_cap(8);
        let req = command_req(&h.session_id, "echo this output exceeds eight bytes");
        let id = h.engine.start(&req).await.expect("start");
        let status = poll_terminal(&h.engine, &id).await;
        assert!(
            matches!(status, JobStatus::Failed { ref reason } if reason.contains("overflow")),
            "expected Failed(log buffer overflow), got {status:?}"
        );
    }

    // ── attach / status (U5c) ─────────────────────────────────────────────────

    use axp_proto::{JobAttachRequest, JobStatusProto, JobStatusRequest, LogStreamProto};

    fn attach_req(session_id: &SessionId, job_id: &JobId, from_offset: u64) -> JobAttachRequest {
        JobAttachRequest {
            session_id: session_id.clone(),
            job_id: job_id.clone(),
            from_offset,
        }
    }

    fn status_req(session_id: &SessionId, job_id: &JobId) -> JobStatusRequest {
        JobStatusRequest {
            session_id: session_id.clone(),
            job_id: job_id.clone(),
        }
    }

    /// Concatenate the stdout bytes across a slice of collected frames.
    fn stdout_of(frames: &[axp_proto::LogEventFrame]) -> Vec<u8> {
        let mut out = Vec::new();
        for f in frames {
            if f.stream == LogStreamProto::Stdout {
                out.extend_from_slice(&f.data);
            }
        }
        out
    }

    #[tokio::test]
    async fn attach_from_zero_replays_all_after_completion() {
        let h = harness();
        let req = command_req(&h.session_id, "echo alpha");
        let id = h.engine.start(&req).await.expect("start");
        poll_terminal(&h.engine, &id).await;

        let mut stream = h
            .engine
            .attach(&attach_req(&h.session_id, &id, 0))
            .expect("attach");
        let mut frames = Vec::new();
        while let Some(frame) = stream.next().await {
            frames.push(frame);
        }

        // Stream ended (the while loop above only exits on None).
        assert!(!frames.is_empty(), "expected at least one frame");
        // Seqs are contiguous from 0.
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.seq, i as u64, "seqs must be contiguous from 0");
        }
        let out = stdout_of(&frames);
        assert!(
            out.windows(5).any(|w| w == b"alpha"),
            "stdout must contain `alpha`, got {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[tokio::test]
    async fn attach_with_offset_skips_prior() {
        let h = harness();
        // The engine captures RAW chunks, not lines — so to produce several distinct
        // log events we space the writes with brief sleeps, forcing separate reads.
        let req = command_req(
            &h.session_id,
            "echo a; sleep 0.1; echo b; sleep 0.1; echo c",
        );
        let id = h.engine.start(&req).await.expect("start");
        poll_terminal(&h.engine, &id).await;

        // There must be at least 2 events to make offset 1 meaningful.
        let st = h
            .engine
            .status(&status_req(&h.session_id, &id))
            .expect("status");
        assert!(st.seq >= 1, "expected buffered events, got seq={}", st.seq);

        let mut stream = h
            .engine
            .attach(&attach_req(&h.session_id, &id, 1))
            .expect("attach");
        let mut frames = Vec::new();
        while let Some(frame) = stream.next().await {
            frames.push(frame);
        }
        assert!(!frames.is_empty(), "expected frames from offset 1");
        assert_eq!(frames[0].seq, 1, "first delivered frame must have seq == 1");
        assert!(
            frames.iter().all(|f| f.seq >= 1),
            "no frame with seq 0 should be delivered"
        );
    }

    #[tokio::test]
    async fn attach_live_receives_later_output() {
        let h = harness();
        let req = command_req(&h.session_id, "echo one; sleep 0.2; echo two");
        let id = h.engine.start(&req).await.expect("start");

        // Attach immediately (the job is still running) from offset 0.
        let mut stream = h
            .engine
            .attach(&attach_req(&h.session_id, &id, 0))
            .expect("attach");

        // Guard the whole collection: a hang fails the test rather than wedging.
        let frames = tokio::time::timeout(Duration::from_secs(5), async {
            let mut frames = Vec::new();
            while let Some(frame) = stream.next().await {
                frames.push(frame);
            }
            frames
        })
        .await
        .expect("collecting live frames must not time out");

        let out = stdout_of(&frames);
        assert!(
            out.windows(3).any(|w| w == b"one"),
            "stdout must contain `one`, got {:?}",
            String::from_utf8_lossy(&out)
        );
        assert!(
            out.windows(3).any(|w| w == b"two"),
            "stdout must contain `two`, got {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[tokio::test]
    async fn status_after_exit_reports_exited() {
        let h = harness();
        let req = command_req(&h.session_id, "echo hi");
        let id = h.engine.start(&req).await.expect("start");
        poll_terminal(&h.engine, &id).await;

        let st = h
            .engine
            .status(&status_req(&h.session_id, &id))
            .expect("status");
        assert_eq!(st.status, JobStatusProto::Exited { code: 0 });
        assert!(st.seq >= 1, "expected at least one buffered event");
    }

    #[tokio::test]
    async fn attach_wrong_session_returns_not_found() {
        let h = harness();
        let req = command_req(&h.session_id, "echo hi");
        let id = h.engine.start(&req).await.expect("start");
        poll_terminal(&h.engine, &id).await;

        let other = SessionId("s_other".into());
        let attach = h.engine.attach(&attach_req(&other, &id, 0));
        assert!(
            matches!(attach.as_ref().err(), Some(Error::JobNotFound(_))),
            "expected JobNotFound for wrong-session attach, got {:?}",
            attach.map(|_| "ok")
        );
        let status = h.engine.status(&status_req(&other, &id));
        assert!(
            matches!(status.as_ref().err(), Some(Error::JobNotFound(_))),
            "expected JobNotFound for wrong-session status, got {:?}",
            status.map(|_| "ok")
        );
    }

    #[tokio::test]
    async fn status_unknown_job_returns_not_found() {
        let h = harness();
        let unknown = JobId("j_unknown".into());
        let status = h.engine.status(&status_req(&h.session_id, &unknown));
        assert!(
            matches!(status.as_ref().err(), Some(Error::JobNotFound(_))),
            "expected JobNotFound for unknown job, got {:?}",
            status.map(|_| "ok")
        );
    }
}
