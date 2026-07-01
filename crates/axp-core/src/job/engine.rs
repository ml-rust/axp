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
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, SystemTime};

use axp_codemode::{
    CapabilityInvokeHandler, CodeModeInterruptHandle, CodeModeRunner, RunnerConfig,
};
use base64::{Engine as _, engine::general_purpose};
use bytes::Bytes;
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;

use axp_proto::{
    EnforcementTier, JobCancelRequest, JobCancelResponse, JobId, JobPayload, JobStartRequest,
    SessionId,
};
use axp_sandbox::{SandboxError, SandboxPolicy};

use super::log::{AppendLogEvent, DEFAULT_LOG_BYTE_CAP, JobReplayLog};
use crate::{
    Error, Result,
    capability::{CapabilitySet, RuntimeCapability},
    job::{Job, JobStatus, JobStore, LogBuffer, LogEvent, LogStream, Seq, resolve_cwd},
    provider::ResolvedCommand,
    registry::ProviderRegistry,
    session::{AuditEventKind, SessionStore},
    workspace::Workspace,
};

/// Read buffer size for draining a child's stdout/stderr pipes.
const DRAIN_BUF_SIZE: usize = 8192;

/// The per-job identity handed to the drainer task: the bits it needs to push
/// logs, record audit events, and clean up its cancellation entry.
struct JobTask {
    job_id: JobId,
    session_id: SessionId,
    handle: Arc<RwLock<Job>>,
}

struct CancelEntry {
    signal: oneshot::Sender<()>,
    code_interrupt: Option<CodeModeInterruptHandle>,
    host_capability_cancel: Option<Arc<HostCapabilityCancelState>>,
}

#[derive(Debug, Default)]
struct HostCapabilityCancelState {
    cancelled: AtomicBool,
}

struct CodeCapabilityContext {
    registry: Arc<RwLock<ProviderRegistry>>,
    capabilities: CapabilitySet,
    workspace: Workspace,
    tier: EnforcementTier,
    cwd: std::path::PathBuf,
    stdout_byte_cap: usize,
    cancel: Arc<HostCapabilityCancelState>,
}

impl HostCapabilityCancelState {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// What a job actually runs: either a user shell string or a resolved capability argv.
///
/// Both variants flow through one uniform command-build path in [`JobEngine::start`];
/// the distinction is only *how* the program/args are derived. The `Argv` path is
/// shell-injection-safe: the program and arguments are passed directly to
/// `Command::new(program).args(args)`, never interpolated into a `sh -c` string.
enum Runnable<'a> {
    /// A user shell string, run via `sh -c` (from a [`JobPayload::Command`]).
    Shell(&'a str),
    /// A resolved capability, run as argv via `Command::new(program).args(args)`.
    Argv(ResolvedCommand),
}

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
    cancels: Arc<Mutex<HashMap<JobId, CancelEntry>>>,
    /// Per-job log-buffer byte cap. A job whose output exceeds this is killed and
    /// marked `Failed` (output is never silently dropped).
    log_byte_cap: usize,
    /// Capability provider registry, shared (typically with `AppState.registry`).
    /// Read under a short-lived guard to resolve a [`JobPayload::Capability`] to an
    /// argv command; the guard is never held across an `.await`.
    registry: Arc<RwLock<ProviderRegistry>>,
}

impl JobEngine {
    /// Create a new engine over the given session and job stores, sharing the
    /// given capability provider `registry` (used to resolve capability payloads).
    ///
    /// Job ids begin at `j_1`; the cancellation table starts empty; the per-job
    /// log-buffer cap defaults to [`DEFAULT_LOG_BYTE_CAP`]. Use
    /// [`with_log_byte_cap`](Self::with_log_byte_cap) to override it.
    pub fn new(
        sessions: SessionStore,
        jobs: JobStore,
        registry: Arc<RwLock<ProviderRegistry>>,
    ) -> Self {
        Self {
            sessions,
            jobs,
            next_id: Arc::new(AtomicU64::new(1)),
            cancels: Arc::new(Mutex::new(HashMap::new())),
            log_byte_cap: DEFAULT_LOG_BYTE_CAP,
            registry,
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
    fn cancels(&self) -> std::sync::MutexGuard<'_, HashMap<JobId, CancelEntry>> {
        self.cancels.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Record an audit event on the owning session, if it still exists.
    ///
    /// A closed or removed session is silently skipped — jobs can outlive their
    /// session and the drainer may fire the `JobFinished` event after the session
    /// is gone.  This call is synchronous (no `.await`) and drops its lock guard
    /// before returning, preserving the no-lock-across-await discipline.
    fn record_session_audit(&self, session_id: &SessionId, kind: AuditEventKind) {
        if let Some(s) = self.sessions.get(session_id) {
            s.write()
                .unwrap_or_else(|p| p.into_inner())
                .record_audit(kind);
        }
    }

    /// Start a job: validate, spawn the work, and launch its completion task.
    ///
    /// All synchronous validation (session lookup, capability checks, cwd
    /// resolution, job-level attenuation) happens BEFORE the process is spawned.
    /// A [`Job`] is created only after work has enough validated state to run —
    /// a job exists iff it started. The actual completion runs on a detached
    /// tokio task.
    ///
    /// # Errors
    ///
    /// - [`Error::SessionNotFound`] if the session id is unknown.
    /// - [`Error::CapabilityDenied`] if the session lacks `proc.spawn` for a
    ///   command payload, lacks `tool:<name>` (or `proc.spawn`) for a capability
    ///   payload, or lacks any requested job-level capability.
    /// - [`Error::InvalidCodePayload`] if a code-mode payload has an unsupported
    ///   language or invalid base64.
    /// - [`Error::CapabilityNotFound`] if a capability payload names an unknown
    ///   capability.
    /// - [`Error::WorkspaceViolation`] if `cwd` resolves outside the workspace.
    /// - [`Error::CapabilityParse`] if a requested capability is malformed, or a
    ///   capability payload's params fail to bind.
    /// - [`Error::JobSpawn`] if the child process fails to spawn.
    pub async fn start(&self, req: &JobStartRequest) -> Result<JobId> {
        // 1. Resolve the session and clone out the bits we need under a short read
        //    lock, dropping the guard before any await.
        let session = self
            .sessions
            .get(&req.session_id)
            .ok_or_else(|| Error::SessionNotFound(req.session_id.clone()))?;
        let (workspace, capabilities, tier) = {
            let s = session.read().unwrap_or_else(|p| p.into_inner());
            (s.workspace.clone(), s.capabilities.clone(), s.tier)
        };

        // 2. Determine what to run from the payload, enforcing the payload-level
        //    capability requirement and (for capabilities) resolving via the registry.
        let runnable = match &req.payload {
            JobPayload::Command { command } => {
                if !capabilities.permits(&RuntimeCapability::ProcSpawn) {
                    return Err(Error::CapabilityDenied {
                        required: "proc.spawn".into(),
                    });
                }
                Runnable::Shell(command)
            }
            JobPayload::Code { code, lang } => {
                let bytes = decode_code_payload(code, lang)?;

                // Resolve cwd and job-level capability attenuation before creating
                // a job, matching the command/capability validation ordering.
                let cwd = resolve_cwd(req.cwd.as_deref(), &workspace)?;
                self.validate_job_capabilities(&req.capabilities, &capabilities)?;

                return self
                    .start_code_job(req, cwd, bytes, capabilities, workspace, tier)
                    .await;
            }
            JobPayload::Capability { name, params } => {
                // A capability invocation requires the narrow `tool:<name>` grant OR
                // the broad proc.spawn (which subsumes it).
                let tool_grant = RuntimeCapability::Tool(name.clone());
                if !capabilities.permits(&tool_grant)
                    && !capabilities.permits(&RuntimeCapability::ProcSpawn)
                {
                    return Err(Error::CapabilityDenied {
                        required: format!("tool:{name}"),
                    });
                }
                // Resolve to an argv command. The registry read guard is dropped at
                // the end of this block (NEVER held across the later `.await`);
                // `resolved` is owned.
                let resolved = {
                    let reg = self.registry.read().unwrap_or_else(|p| p.into_inner());
                    reg.resolve(name, params)?
                };
                Runnable::Argv(resolved)
            }
        };

        // 3. Resolve the working directory against the workspace.
        let cwd = resolve_cwd(req.cwd.as_deref(), &workspace)?;

        // 4. Job-level capability attenuation: every requested capability must be
        //    permitted by the session's grants.
        self.validate_job_capabilities(&req.capabilities, &capabilities)?;

        // 5. Build the command. Shell payloads run via `sh -c`; capability payloads
        //    run as a resolved argv (no shell), which is shell-injection-safe.
        let mut cmd = match &runnable {
            Runnable::Shell(s) => {
                let mut c = tokio::process::Command::new("sh");
                c.arg("-c").arg(s);
                c
            }
            Runnable::Argv(r) => {
                let mut c = tokio::process::Command::new(&r.program);
                c.args(&r.args);
                c
            }
        };
        cmd.current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);

        // 6. Build and apply the sandbox policy for this job's tier + capabilities.
        let mut policy = build_sandbox_policy(tier, &workspace, &capabilities);
        if matches!(runnable, Runnable::Argv(_)) {
            // Running a named tool inherently spawns its one resolved program; the
            // `tool:<name>` grant authorizes that single spawn even without the broad
            // proc.spawn grant. (Currently advisory — proc-spawn seccomp gating is a
            // later unit — but set correctly so enforcement stays correct when added.)
            policy.allow_proc_spawn = true;
        }
        policy.apply(&mut cmd).map_err(|e| match e {
            SandboxError::Unavailable { .. } => Error::SandboxUnavailable { source: e },
            _ => Error::SandboxApply { source: e },
        })?;

        // 7. Spawn. Only after a successful spawn do we create a Job.
        let mut child = cmd.spawn().map_err(|e| Error::JobSpawn {
            reason: e.to_string(),
        })?;

        // 8. Create the job in the Running state and insert it.
        let (id, handle) = self.insert_running_job(req, cwd);

        // 9. Register the cancellation channel.
        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.cancels().insert(
            id.clone(),
            CancelEntry {
                signal: cancel_tx,
                code_interrupt: None,
                host_capability_cancel: None,
            },
        );

        // 9a. Record the JobStarted audit event now that the job truly exists.
        self.record_session_audit(
            &req.session_id,
            AuditEventKind::JobStarted { job_id: id.clone() },
        );

        // 10. Take the child's piped stdout/stderr.
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // 11. Spawn the drainer on the ambient runtime.
        let engine = self.clone();
        let task = JobTask {
            job_id: id.clone(),
            session_id: req.session_id.clone(),
            handle,
        };
        tokio::spawn(async move {
            engine
                .run_to_completion(task, child, stdout, stderr, cancel_rx)
                .await;
        });

        // 12. Return the new job id.
        Ok(id)
    }

    fn validate_job_capabilities(
        &self,
        requested: &[axp_proto::Capability],
        capabilities: &CapabilitySet,
    ) -> Result<()> {
        for wire_cap in requested {
            let rc = RuntimeCapability::parse(wire_cap)?;
            if !capabilities.permits(&rc) {
                return Err(Error::CapabilityDenied {
                    required: wire_cap.0.clone(),
                });
            }
        }
        Ok(())
    }

    fn insert_running_job(
        &self,
        req: &JobStartRequest,
        cwd: std::path::PathBuf,
    ) -> (JobId, Arc<RwLock<Job>>) {
        let id = self.next_job_id();
        let mut job = Job::new(id.clone(), req.session_id.clone(), req.payload.clone(), cwd);
        job.log_buffer = LogBuffer::with_cap(self.log_byte_cap);
        job.status = JobStatus::Running;
        job.started_at = Some(SystemTime::now());
        let handle = self.jobs.insert(job);
        (id, handle)
    }

    async fn start_code_job(
        &self,
        req: &JobStartRequest,
        cwd: std::path::PathBuf,
        bytes: Vec<u8>,
        capabilities: CapabilitySet,
        workspace: Workspace,
        tier: EnforcementTier,
    ) -> Result<JobId> {
        let registry = self.registry.clone();
        let handler_cwd = cwd.clone();
        let stdout_byte_cap = self.log_byte_cap;
        let host_cancel = Arc::new(HostCapabilityCancelState::default());
        let handler_cancel = host_cancel.clone();
        let capability_context = CodeCapabilityContext {
            registry,
            capabilities,
            workspace,
            tier,
            cwd: handler_cwd,
            stdout_byte_cap,
            cancel: handler_cancel,
        };
        let capability_invoke: CapabilityInvokeHandler = Arc::new(move |name, params_json| {
            invoke_code_capability(&capability_context, name, params_json)
                .map_err(|e| e.to_string())
        });
        let runner = CodeModeRunner::with_config(RunnerConfig {
            capability_invoke: Some(capability_invoke),
            ..RunnerConfig::default()
        })
        .map_err(|e| Error::JobSpawn {
            reason: e.to_string(),
        })?;
        let interrupt = runner.interrupt_handle();
        let (id, handle) = self.insert_running_job(req, cwd);

        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.cancels().insert(
            id.clone(),
            CancelEntry {
                signal: cancel_tx,
                code_interrupt: Some(interrupt.clone()),
                host_capability_cancel: Some(host_cancel),
            },
        );

        self.record_session_audit(
            &req.session_id,
            AuditEventKind::JobStarted { job_id: id.clone() },
        );

        let engine = self.clone();
        let task = JobTask {
            job_id: id.clone(),
            session_id: req.session_id.clone(),
            handle,
        };
        tokio::spawn(async move {
            engine
                .run_code_to_completion(task, runner, interrupt, bytes, cancel_rx)
                .await;
        });

        Ok(id)
    }

    async fn run_code_to_completion(
        &self,
        task: JobTask,
        runner: CodeModeRunner,
        interrupt: CodeModeInterruptHandle,
        bytes: Vec<u8>,
        mut cancel_rx: oneshot::Receiver<()>,
    ) {
        let JobTask {
            job_id,
            session_id,
            handle,
        } = task;

        let run_interrupt = interrupt.clone();
        let mut run = tokio::task::spawn_blocking(move || {
            runner
                .run_component_with_interrupt(&bytes, &run_interrupt)
                .map_err(|e| e.to_string())
        });

        let final_status = tokio::select! {
            biased;

            res = (&mut cancel_rx) => {
                let _ = res;
                interrupt.interrupt();
                let _ = run.await;
                JobStatus::Killed
            }

            joined = &mut run => {
                match joined {
                    Ok(Ok(output)) => {
                        match output.result {
                            Some(result) => match self.push_text_result(
                                &handle,
                                LogStream::Stdout,
                                &result,
                            ) {
                                Ok(()) => JobStatus::Exited { code: 0 },
                                Err(e) => JobStatus::Failed {
                                    reason: e.to_string(),
                                },
                            },
                            None => {
                                JobStatus::Exited { code: 0 }
                            }
                        }
                    }
                    Ok(Err(reason)) => {
                        let _ = self.push_text_result(&handle, LogStream::Stderr, &reason);
                        JobStatus::Failed { reason }
                    }
                    Err(e) => {
                        let reason = e.to_string();
                        let _ = self.push_text_result(&handle, LogStream::Stderr, &reason);
                        JobStatus::Failed { reason }
                    }
                }
            }
        };

        self.finish_job(job_id, session_id, handle, final_status);
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
        task: JobTask,
        mut child: tokio::process::Child,
        stdout: Option<tokio::process::ChildStdout>,
        stderr: Option<tokio::process::ChildStderr>,
        mut cancel_rx: oneshot::Receiver<()>,
    ) {
        let JobTask {
            job_id,
            session_id,
            handle,
        } = task;
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
                            if self
                                .push_log(
                                    &handle,
                                    LogStream::Stdout,
                                    Bytes::copy_from_slice(&buf_out[..n]),
                                )
                                .is_err()
                            {
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
                            if self
                                .push_log(
                                    &handle,
                                    LogStream::Stderr,
                                    Bytes::copy_from_slice(&buf_err[..n]),
                                )
                                .is_err()
                            {
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

        self.finish_job(job_id, session_id, handle, final_status);
    }

    /// Push a chunk of output into a job's log buffer.
    ///
    /// The write lock is acquired and dropped entirely within this synchronous
    /// function — there is no `.await` inside — so the locking discipline holds.
    ///
    /// # Errors
    ///
    /// Propagates [`Error::LogBufferOverflow`] from the underlying buffer.
    fn push_log(&self, handle: &Arc<RwLock<Job>>, stream: LogStream, data: Bytes) -> Result<()> {
        let mut job = handle.write().unwrap_or_else(|p| p.into_inner());
        job.log_buffer.append(AppendLogEvent {
            stream,
            data,
            timestamp: SystemTime::now(),
        })?;
        Ok(())
    }

    fn push_text_result(
        &self,
        handle: &Arc<RwLock<Job>>,
        stream: LogStream,
        text: &str,
    ) -> Result<()> {
        let mut bytes = text.as_bytes().to_vec();
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        self.push_log(handle, stream, Bytes::from(bytes))
    }

    fn finish_job(
        &self,
        job_id: JobId,
        session_id: SessionId,
        handle: Arc<RwLock<Job>>,
        final_status: JobStatus,
    ) {
        // Write the terminal state under a short lock with no await inside.
        // The final state write does NOT push a log event, so it would not
        // otherwise wake live subscribers. Capture the buffer's notify and fire
        // it AFTER the terminal status is written so any `JobLogStream` waiting
        // on a quiet-then-finished job observes the terminal state and returns
        // `None` instead of hanging forever.
        let notify = {
            let mut job = handle.write().unwrap_or_else(|p| p.into_inner());
            job.status = final_status.clone();
            job.finished_at = Some(SystemTime::now());
            job.log_buffer.subscribe()
        };
        notify.notify_waiters();

        // Record the JobFinished audit event. This is a synchronous call that
        // acquires and drops its lock guard internally — no .await is held.
        self.record_session_audit(
            &session_id,
            AuditEventKind::JobFinished {
                job_id: job_id.clone(),
                status: final_status.into(),
            },
        );

        // Drop the cancellation sender; the job is finished.
        self.cancels().remove(&job_id);
    }

    /// Send the cancellation signal for a job (best-effort SIGKILL via the drainer).
    ///
    /// Returns `Err(JobNotFound)` if the job's cancel sender is absent — i.e. the
    /// job is unknown to the cancel table, already signalled, or already finished.
    ///
    /// This is a private helper; callers must perform ownership checks before
    /// invoking it.
    fn kill_job(&self, job_id: &JobId) -> Result<()> {
        match self.cancels().remove(job_id) {
            Some(entry) => {
                let _ = entry.signal.send(());
                if let Some(interrupt) = entry.code_interrupt {
                    interrupt.interrupt();
                }
                if let Some(host_cancel) = entry.host_capability_cancel {
                    host_cancel.cancel();
                }
                Ok(())
            }
            None => Err(Error::JobNotFound(job_id.clone())),
        }
    }

    /// Cancel a running job the caller owns (best-effort SIGKILL via the drainer).
    ///
    /// Returns `ok: true` if a running job was signalled, `ok: false` if the job
    /// exists and is owned by the caller but is already signalled or finished.
    ///
    /// # Errors
    ///
    /// [`Error::JobNotFound`] if the job is unknown or owned by a different session.
    pub fn cancel(&self, req: &JobCancelRequest) -> Result<JobCancelResponse> {
        // Ownership check first: unknown or wrong-session → JobNotFound.
        self.lookup_owned(&req.session_id, &req.job_id)?;
        match self.kill_job(&req.job_id) {
            Ok(()) => Ok(JobCancelResponse { ok: true }),
            // Already signalled or finished — owned, but nothing new to cancel.
            Err(Error::JobNotFound(_)) => Ok(JobCancelResponse { ok: false }),
            Err(e) => Err(e),
        }
    }

    /// Cancel all of a session's still-running jobs.
    ///
    /// Intended to be called by the session-close path BEFORE
    /// `SessionStore::close` removes the session.
    ///
    /// Returns the number of jobs signalled. Jobs already finished are skipped.
    /// This deliberately does not call `SessionStore::close` itself — wiring that
    /// here would couple the engine to session lifecycle and risk a circular
    /// dependency; the close handler is responsible for ordering.
    pub fn cancel_for_session(&self, session_id: &SessionId) -> usize {
        let mut n = 0;
        for job_id in self.jobs.list_for_session(session_id) {
            if self.kill_job(&job_id).is_ok() {
                n += 1;
            }
        }
        n
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

/// Build a [`SandboxPolicy`](axp_sandbox::SandboxPolicy) from a job's enforcement
/// tier, its workspace, and the session's granted capabilities.
///
/// The workspace root is always readable. Verbs are orthogonal: `fs.read` grants
/// add read paths, `fs.write` grants add write paths. `proc.spawn`/`net.connect`
/// grants set the coarse spawn/network flags (domain-level egress filtering is a
/// later unit). `pty.open`/`tool` grants are not filesystem/sandbox concerns here.
fn build_sandbox_policy(
    tier: EnforcementTier,
    workspace: &Workspace,
    capabilities: &CapabilitySet,
) -> SandboxPolicy {
    let mut fs_read = vec![workspace.root().to_path_buf()];
    let mut fs_write = Vec::new();
    let mut allow_proc_spawn = false;
    let mut allow_network = false;
    for cap in capabilities.grants() {
        match cap {
            RuntimeCapability::FsRead(p) => fs_read.push(p.clone()),
            RuntimeCapability::FsWrite(p) => fs_write.push(p.clone()),
            RuntimeCapability::NetConnect(_) => allow_network = true,
            RuntimeCapability::ProcSpawn => allow_proc_spawn = true,
            RuntimeCapability::PtyOpen | RuntimeCapability::Tool(_) => {}
        }
    }
    SandboxPolicy::from_parts(tier, fs_read, fs_write, allow_proc_spawn, allow_network)
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
                let replay = job.log_buffer.replay_from(self.cursor);
                for ev in &replay {
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

fn decode_code_payload(code: &str, lang: &Option<String>) -> Result<Vec<u8>> {
    match lang {
        Some(lang) if lang != "wasm-component" => {
            return Err(Error::InvalidCodePayload {
                reason: format!("unsupported language `{lang}`"),
            });
        }
        Some(_) | None => {}
    }

    general_purpose::STANDARD
        .decode(code)
        .map_err(|e| Error::InvalidCodePayload {
            reason: format!("invalid base64: {e}"),
        })
}

fn invoke_code_capability(
    context: &CodeCapabilityContext,
    name: &str,
    params_json: &str,
) -> Result<String> {
    let tool_grant = RuntimeCapability::Tool(name.to_owned());
    if !context.capabilities.permits(&tool_grant)
        && !context.capabilities.permits(&RuntimeCapability::ProcSpawn)
    {
        return Err(Error::CapabilityDenied {
            required: format!("tool:{name}"),
        });
    }

    let params = serde_json::from_str(params_json).map_err(|e| Error::CapabilityParse {
        raw: params_json.to_owned(),
        reason: format!("invalid params JSON: {e}"),
    })?;

    let resolved = {
        let reg = context.registry.read().unwrap_or_else(|p| p.into_inner());
        reg.resolve(name, &params)?
    };

    let mut policy = build_sandbox_policy(context.tier, &context.workspace, &context.capabilities);
    policy.allow_proc_spawn = true;

    run_resolved_command_sync(
        resolved,
        &context.cwd,
        context.stdout_byte_cap,
        policy,
        &context.cancel,
    )
}

fn run_resolved_command_sync(
    resolved: ResolvedCommand,
    cwd: &std::path::Path,
    stdout_byte_cap: usize,
    policy: SandboxPolicy,
    cancel: &HostCapabilityCancelState,
) -> Result<String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|e| Error::JobSpawn {
            reason: format!("failed to build capability runtime: {e}"),
        })?;

    runtime.block_on(run_resolved_command(
        resolved,
        cwd,
        stdout_byte_cap,
        policy,
        cancel,
    ))
}

async fn run_resolved_command(
    resolved: ResolvedCommand,
    cwd: &std::path::Path,
    stdout_byte_cap: usize,
    policy: SandboxPolicy,
    cancel: &HostCapabilityCancelState,
) -> Result<String> {
    if cancel.is_cancelled() {
        return Err(Error::JobSpawn {
            reason: "capability command cancelled".into(),
        });
    }

    let mut cmd = tokio::process::Command::new(&resolved.program);
    cmd.args(&resolved.args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    cmd.kill_on_drop(true);

    policy.apply(&mut cmd).map_err(|e| match e {
        SandboxError::Unavailable { .. } => Error::SandboxUnavailable { source: e },
        _ => Error::SandboxApply { source: e },
    })?;

    let mut child = cmd.spawn().map_err(|e| Error::JobSpawn {
        reason: e.to_string(),
    })?;
    let mut stdout = child.stdout.take().ok_or_else(|| Error::JobSpawn {
        reason: "capability stdout pipe was not available".into(),
    })?;

    let mut bytes = Vec::new();
    let mut buf = [0u8; DRAIN_BUF_SIZE];
    let mut stdout_done = false;
    let mut killed = false;
    let mut status = None;
    let mut poll_cancel = tokio::time::interval(Duration::from_millis(10));

    loop {
        tokio::select! {
            _ = poll_cancel.tick() => {
                if !killed && cancel.is_cancelled() {
                    killed = true;
                    let _ = child.start_kill();
                }

                match child.try_wait() {
                    Ok(Some(es)) => status = Some(es),
                    Ok(None) => {}
                    Err(e) => {
                        let _ = child.start_kill();
                        return Err(Error::JobSpawn {
                            reason: e.to_string(),
                        });
                    }
                }
            }

            n = stdout.read(&mut buf), if !stdout_done => {
                match n {
                    Ok(0) => stdout_done = true,
                    Ok(n) => {
                        if bytes.len().saturating_add(n) > stdout_byte_cap {
                            let _ = child.start_kill();
                            return Err(Error::LogBufferOverflow {
                                cap: stdout_byte_cap,
                            });
                        }
                        bytes.extend_from_slice(&buf[..n]);
                    }
                    Err(e) => {
                        let _ = child.start_kill();
                        return Err(Error::JobSpawn {
                            reason: e.to_string(),
                        });
                    }
                }
            }
        }

        if status.is_some() && stdout_done {
            break;
        }
    }

    let status = status.ok_or_else(|| Error::JobSpawn {
        reason: "capability command status was not available".into(),
    })?;
    if killed || cancel.is_cancelled() {
        return Err(Error::JobSpawn {
            reason: "capability command cancelled".into(),
        });
    }
    if !status.success() {
        return Err(Error::JobSpawn {
            reason: format!("capability command exited with {status}"),
        });
    }

    String::from_utf8(bytes).map_err(|e| Error::JobSpawn {
        reason: format!("capability stdout was not valid UTF-8: {e}"),
    })
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

    fn fresh_token() -> crate::CapToken {
        crate::CapToken::generate().expect("entropy")
    }

    /// Build a harness: a session over a fresh tempdir workspace with the given
    /// capabilities, plus a job store and engine.
    fn harness_with(caps: CapabilitySet) -> Harness {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = Workspace::new(dir.path()).expect("workspace");
        let sessions = SessionStore::new();
        let session_id = SessionId("s_test".into());
        sessions.open(
            session_id.clone(),
            ws,
            EnforcementTier::DevNone,
            caps,
            fresh_token(),
        );
        let jobs = JobStore::new();
        let engine = JobEngine::new(
            sessions,
            jobs,
            std::sync::Arc::new(std::sync::RwLock::new(crate::ProviderRegistry::new())),
        );
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
            // The engine never reads `cap_token` (authentication is enforced at
            // the transport layer); a placeholder keeps these unit tests focused
            // on engine behavior.
            cap_token: "ct_test".into(),
            payload: JobPayload::Command {
                command: command.into(),
            },
            cwd: None,
            capabilities: vec![],
        }
    }

    const NOOP_COMPONENT: &str = r#"
        (component
          (core module $m
            (func (export "run")))
          (core instance $i (instantiate $m))
          (func (export "run") (canon lift (core func $i "run"))))
    "#;

    const LOOP_COMPONENT: &str = r#"
        (component
          (core module $m
            (func $run (export "run")
              (loop $again
                br $again)))
          (core instance $i (instantiate $m))
          (func (export "run") (canon lift (core func $i "run"))))
    "#;

    fn capability_invoke_component(name: &str, params_json: &str) -> String {
        format!(
            r#"
        (component
          (import "axp:capability/invoke"
            (func $invoke (param "name" string) (param "params-json" string) (result string)))
          (core module $memory
            (memory (export "memory") 1)
            (global $next (mut i32) (i32.const 256))
            (data (i32.const 32) "{name}")
            (data (i32.const 64) "{params_json}")
            (func (export "cabi_realloc")
              (param i32 i32 i32 i32)
              (result i32)
              (local $ptr i32)
              global.get $next
              local.set $ptr
              global.get $next
              local.get 3
              i32.add
              global.set $next
              local.get $ptr))
          (core instance $memory-instance (instantiate $memory))
          (alias core export $memory-instance "memory" (core memory $memory-export))
          (alias core export $memory-instance "cabi_realloc" (core func $realloc))
          (core func $invoke-lowered
            (canon lower
              (func $invoke)
              (memory $memory-export)
              (realloc $realloc)
              string-encoding=utf8))
          (core instance $imports
            (export "axp:capability/invoke" (func $invoke-lowered)))
          (core instance $env
            (export "memory" (memory $memory-export)))
          (core module $m
            (import "" "axp:capability/invoke"
              (func $invoke-core (param i32 i32 i32 i32 i32)))
            (import "env" "memory" (memory 1))
            (func (export "run") (result i32)
              i32.const 32
              i32.const {name_len}
              i32.const 64
              i32.const {params_len}
              i32.const 0
              call $invoke-core
              i32.const 0))
          (core instance $i
            (instantiate $m
              (with "" (instance $imports))
              (with "env" (instance $env))))
          (alias core export $i "run" (core func $run))
          (func (export "run")
            (result string)
            (canon lift
              (core func $run)
              (memory $memory-export)
              (realloc $realloc)
              string-encoding=utf8)))
    "#,
            name_len = name.len(),
            params_len = params_json.len(),
        )
    }

    fn code_req(session_id: &SessionId, code: String, lang: Option<String>) -> JobStartRequest {
        JobStartRequest {
            session_id: session_id.clone(),
            cap_token: "ct_test".into(),
            payload: JobPayload::Code { code, lang },
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
    async fn code_payload_runs_without_proc_spawn() {
        let h = harness_with(CapabilitySet::default());
        let req = code_req(
            &h.session_id,
            general_purpose::STANDARD.encode(NOOP_COMPONENT),
            None,
        );
        let id = h.engine.start(&req).await.expect("start");
        let status = poll_terminal(&h.engine, &id).await;
        assert_eq!(status, JobStatus::Exited { code: 0 });
    }

    #[tokio::test]
    async fn code_payload_invalid_base64_is_rejected_without_job() {
        let h = harness_with(CapabilitySet::default());
        let req = code_req(&h.session_id, "not base64!".into(), None);
        let result = h.engine.start(&req).await;
        assert!(
            matches!(result, Err(Error::InvalidCodePayload { .. })),
            "expected InvalidCodePayload, got {result:?}"
        );
        assert!(
            h.engine.jobs().get(&JobId("j_1".into())).is_none(),
            "no job should exist after invalid code payload"
        );
    }

    #[tokio::test]
    async fn code_payload_unknown_lang_is_rejected() {
        let h = harness_with(CapabilitySet::default());
        let req = code_req(
            &h.session_id,
            general_purpose::STANDARD.encode(NOOP_COMPONENT),
            Some("python".into()),
        );
        let result = h.engine.start(&req).await;
        assert!(
            matches!(result, Err(Error::InvalidCodePayload { .. })),
            "expected InvalidCodePayload, got {result:?}"
        );
    }

    #[tokio::test]
    async fn code_payload_runner_failure_marks_failed() {
        let h = harness_with(CapabilitySet::default());
        let req = code_req(
            &h.session_id,
            general_purpose::STANDARD.encode("not a component"),
            Some("wasm-component".into()),
        );
        let id = h.engine.start(&req).await.expect("start");
        let status = poll_terminal(&h.engine, &id).await;
        assert!(
            matches!(status, JobStatus::Failed { .. }),
            "expected Failed, got {status:?}"
        );
        let err = collected_bytes(&h.engine, &id, LogStream::Stderr);
        assert!(!err.is_empty(), "stderr should contain the runner error");
    }

    #[tokio::test]
    async fn code_payload_cancel_loop_marks_killed() {
        let h = harness_with(CapabilitySet::default());
        let req = code_req(
            &h.session_id,
            general_purpose::STANDARD.encode(LOOP_COMPONENT),
            Some("wasm-component".into()),
        );
        let id = h.engine.start(&req).await.expect("start");
        let cancel_req = axp_proto::JobCancelRequest {
            session_id: h.session_id.clone(),
            cap_token: "ct_test".into(),
            job_id: id.clone(),
        };

        let resp = h.engine.cancel(&cancel_req).expect("cancel");
        assert!(
            resp.ok,
            "cancel should return ok=true for a running code job"
        );
        let status = poll_terminal(&h.engine, &id).await;
        assert_eq!(status, JobStatus::Killed);
    }

    // ── capability invocation ────────────────────────────────────────────────

    /// Build a harness whose registry contains a single host-independent
    /// capability `say_hi` (program `echo`, fixed literal arg `hi`), and whose
    /// session is opened with `caps` over a fresh tempdir at the `DevNone` tier.
    fn capability_harness(caps: CapabilitySet) -> Harness {
        use crate::{CapabilityArg, CapabilityDescriptor, ExecutionSpec, NativeProvider};

        let dir = tempfile::tempdir().expect("tempdir");
        let ws = Workspace::new(dir.path()).expect("workspace");
        let sessions = SessionStore::new();
        let session_id = SessionId("s_cap".into());
        sessions.open(
            session_id.clone(),
            ws,
            EnforcementTier::DevNone,
            caps,
            fresh_token(),
        );

        let provider = NativeProvider::new(
            "test",
            vec![CapabilityDescriptor {
                name: "say_hi".into(),
                desc: "Print a fixed greeting line for tests".into(),
                signature: "say_hi(): string".into(),
                schema: serde_json::json!({}),
                exec: ExecutionSpec {
                    program: "echo".into(),
                    args_template: vec![CapabilityArg::Literal("hi".into())],
                },
            }],
        )
        .expect("native provider");
        let mut registry = crate::ProviderRegistry::new();
        registry.register(Box::new(provider)).expect("register");

        let engine = JobEngine::new(
            sessions,
            JobStore::new(),
            std::sync::Arc::new(std::sync::RwLock::new(registry)),
        );
        Harness {
            engine,
            session_id,
            _dir: dir,
        }
    }

    fn sleepy_capability_harness(caps: CapabilitySet) -> Harness {
        use crate::{CapabilityArg, CapabilityDescriptor, ExecutionSpec, NativeProvider};

        let dir = tempfile::tempdir().expect("tempdir");
        let ws = Workspace::new(dir.path()).expect("workspace");
        let sessions = SessionStore::new();
        let session_id = SessionId("s_sleepy_cap".into());
        sessions.open(
            session_id.clone(),
            ws,
            EnforcementTier::DevNone,
            caps,
            fresh_token(),
        );

        let provider = NativeProvider::new(
            "test",
            vec![CapabilityDescriptor {
                name: "sleepy".into(),
                desc: "Sleep long enough for cancellation tests".into(),
                signature: "sleepy(): string".into(),
                schema: serde_json::json!({}),
                exec: ExecutionSpec {
                    program: "sleep".into(),
                    args_template: vec![CapabilityArg::Literal("30".into())],
                },
            }],
        )
        .expect("native provider");
        let mut registry = crate::ProviderRegistry::new();
        registry.register(Box::new(provider)).expect("register");

        let engine = JobEngine::new(
            sessions,
            JobStore::new(),
            std::sync::Arc::new(std::sync::RwLock::new(registry)),
        );
        Harness {
            engine,
            session_id,
            _dir: dir,
        }
    }

    fn capability_req(session_id: &SessionId, name: &str) -> JobStartRequest {
        JobStartRequest {
            session_id: session_id.clone(),
            cap_token: "ct_test".into(),
            payload: JobPayload::Capability {
                name: name.into(),
                params: serde_json::json!({}),
            },
            cwd: None,
            capabilities: vec![],
        }
    }

    fn capability_code_req(
        session_id: &SessionId,
        name: &str,
        params_json: &str,
    ) -> JobStartRequest {
        code_req(
            session_id,
            general_purpose::STANDARD.encode(capability_invoke_component(name, params_json)),
            Some("wasm-component".into()),
        )
    }

    #[tokio::test]
    async fn code_payload_invokes_registered_capability_with_tool_grant() {
        let h = capability_harness(CapabilitySet::new(vec![RuntimeCapability::Tool(
            "say_hi".into(),
        )]));
        let req = capability_code_req(&h.session_id, "say_hi", "{}");
        let id = h.engine.start(&req).await.expect("start");
        let status = poll_terminal(&h.engine, &id).await;
        assert_eq!(status, JobStatus::Exited { code: 0 });
        let out = collected_bytes(&h.engine, &id, LogStream::Stdout);
        assert!(
            out.windows(2).any(|w| w == b"hi"),
            "stdout must contain `hi`, got {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[tokio::test]
    async fn code_payload_cancel_inside_host_capability_marks_killed() {
        let h = sleepy_capability_harness(CapabilitySet::new(vec![RuntimeCapability::Tool(
            "sleepy".into(),
        )]));
        let req = capability_code_req(&h.session_id, "sleepy", "{}");
        let id = h.engine.start(&req).await.expect("start");
        let cancel_req = axp_proto::JobCancelRequest {
            session_id: h.session_id.clone(),
            cap_token: "ct_test".into(),
            job_id: id.clone(),
        };

        let resp = h.engine.cancel(&cancel_req).expect("cancel");
        assert!(
            resp.ok,
            "cancel should return ok=true for a running code job"
        );
        let status = poll_terminal(&h.engine, &id).await;
        assert_eq!(status, JobStatus::Killed);
    }

    #[tokio::test]
    async fn code_payload_capability_missing_grant_fails_job() {
        let h = capability_harness(CapabilitySet::new(vec![RuntimeCapability::Tool(
            "other".into(),
        )]));
        let req = capability_code_req(&h.session_id, "say_hi", "{}");
        let id = h.engine.start(&req).await.expect("start");
        let status = poll_terminal(&h.engine, &id).await;
        assert!(
            matches!(status, JobStatus::Failed { .. }),
            "expected Failed without the tool grant, got {status:?}"
        );
    }

    #[tokio::test]
    async fn code_payload_capability_malformed_params_json_fails_job() {
        let h = capability_harness(CapabilitySet::new(vec![RuntimeCapability::Tool(
            "say_hi".into(),
        )]));
        let req = capability_code_req(&h.session_id, "say_hi", "{");
        let id = h.engine.start(&req).await.expect("start");
        let status = poll_terminal(&h.engine, &id).await;
        assert!(
            matches!(status, JobStatus::Failed { .. }),
            "expected Failed for malformed params JSON, got {status:?}"
        );
    }

    #[tokio::test]
    async fn code_payload_capability_unknown_name_fails_job() {
        let h = capability_harness(CapabilitySet::new(vec![RuntimeCapability::Tool(
            "ghost".into(),
        )]));
        let req = capability_code_req(&h.session_id, "ghost", "{}");
        let id = h.engine.start(&req).await.expect("start");
        let status = poll_terminal(&h.engine, &id).await;
        assert!(
            matches!(status, JobStatus::Failed { .. }),
            "expected Failed for unknown capability, got {status:?}"
        );
    }

    #[tokio::test]
    async fn capability_invocation_runs_and_captures_stdout() {
        // The `tool:say_hi` grant ALONE (no proc.spawn) must authorize the tool.
        let h = capability_harness(CapabilitySet::new(vec![RuntimeCapability::Tool(
            "say_hi".into(),
        )]));
        let req = capability_req(&h.session_id, "say_hi");
        let id = h.engine.start(&req).await.expect("start");
        let status = poll_terminal(&h.engine, &id).await;
        assert_eq!(status, JobStatus::Exited { code: 0 });
        let out = collected_bytes(&h.engine, &id, LogStream::Stdout);
        assert!(
            out.windows(2).any(|w| w == b"hi"),
            "stdout must contain `hi`, got {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[tokio::test]
    async fn capability_without_grant_is_denied() {
        // Session holds only an unrelated tool grant — not `tool:say_hi`.
        let h = capability_harness(CapabilitySet::new(vec![RuntimeCapability::Tool(
            "other".into(),
        )]));
        let req = capability_req(&h.session_id, "say_hi");
        let result = h.engine.start(&req).await;
        assert!(
            matches!(result, Err(Error::CapabilityDenied { .. })),
            "expected CapabilityDenied without the tool grant, got {result:?}"
        );
    }

    #[tokio::test]
    async fn capability_unknown_name_is_not_found() {
        // Grant passes (`tool:ghost`), but the registry has no such capability.
        let h = capability_harness(CapabilitySet::new(vec![RuntimeCapability::Tool(
            "ghost".into(),
        )]));
        let req = capability_req(&h.session_id, "ghost");
        let result = h.engine.start(&req).await;
        assert!(
            matches!(result, Err(Error::CapabilityNotFound { .. })),
            "expected CapabilityNotFound for unknown capability, got {result:?}"
        );
    }

    #[tokio::test]
    async fn cancel_running_job_marks_killed() {
        let h = harness();
        let req = command_req(&h.session_id, "sleep 30");
        let id = h.engine.start(&req).await.expect("start");
        // Cancel immediately so the test stays fast.
        let cancel_req = axp_proto::JobCancelRequest {
            session_id: h.session_id.clone(),
            cap_token: "ct_test".into(),
            job_id: id.clone(),
        };
        let resp = h.engine.cancel(&cancel_req).expect("cancel");
        assert!(resp.ok, "cancel should return ok=true for a running job");
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

    // ── attach / status ───────────────────────────────────────────────────────

    use axp_proto::{JobAttachRequest, JobStatusProto, JobStatusRequest, LogStreamProto};

    fn attach_req(session_id: &SessionId, job_id: &JobId, from_offset: u64) -> JobAttachRequest {
        JobAttachRequest {
            session_id: session_id.clone(),
            cap_token: "ct_test".into(),
            job_id: job_id.clone(),
            from_offset,
        }
    }

    fn status_req(session_id: &SessionId, job_id: &JobId) -> JobStatusRequest {
        JobStatusRequest {
            session_id: session_id.clone(),
            cap_token: "ct_test".into(),
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

    // ── cancel ownership / session-close / audit ─────────────────────────────

    #[tokio::test]
    async fn cancel_wrong_session_returns_not_found() {
        let h = harness();
        let req = command_req(&h.session_id, "sleep 30");
        let id = h.engine.start(&req).await.expect("start");

        // Use a completely different session id — ownership check must reject it.
        let wrong = SessionId("s_wrong".into());
        let cancel_req = axp_proto::JobCancelRequest {
            session_id: wrong,
            cap_token: "ct_test".into(),
            job_id: id.clone(),
        };
        let result = h.engine.cancel(&cancel_req);
        assert!(
            matches!(result, Err(Error::JobNotFound(_))),
            "expected JobNotFound for wrong-session cancel, got {result:?}"
        );

        // Clean up: cancel the real job so the tokio task doesn't linger.
        let real_req = axp_proto::JobCancelRequest {
            session_id: h.session_id.clone(),
            cap_token: "ct_test".into(),
            job_id: id.clone(),
        };
        let _ = h.engine.cancel(&real_req);
        poll_terminal(&h.engine, &id).await;
    }

    #[tokio::test]
    async fn cancel_finished_job_returns_ok_false() {
        let h = harness();
        let req = command_req(&h.session_id, "echo hi");
        let id = h.engine.start(&req).await.expect("start");
        poll_terminal(&h.engine, &id).await;

        // Job is finished — cancel should return ok=false (owned but already done).
        let cancel_req = axp_proto::JobCancelRequest {
            session_id: h.session_id.clone(),
            cap_token: "ct_test".into(),
            job_id: id.clone(),
        };
        let resp = h
            .engine
            .cancel(&cancel_req)
            .expect("cancel of finished job");
        assert!(!resp.ok, "cancel of a finished job should return ok=false");
    }

    #[tokio::test]
    async fn cancel_for_session_kills_all_running() {
        let h = harness();
        // Start 3 long-running jobs.
        let id1 = h
            .engine
            .start(&command_req(&h.session_id, "sleep 30"))
            .await
            .expect("start 1");
        let id2 = h
            .engine
            .start(&command_req(&h.session_id, "sleep 30"))
            .await
            .expect("start 2");
        let id3 = h
            .engine
            .start(&command_req(&h.session_id, "sleep 30"))
            .await
            .expect("start 3");

        let n = h.engine.cancel_for_session(&h.session_id);
        assert_eq!(n, 3, "cancel_for_session must signal all 3 running jobs");

        // All three must reach a terminal Killed state.
        let s1 = poll_terminal(&h.engine, &id1).await;
        let s2 = poll_terminal(&h.engine, &id2).await;
        let s3 = poll_terminal(&h.engine, &id3).await;
        assert_eq!(s1, JobStatus::Killed, "job 1 should be Killed");
        assert_eq!(s2, JobStatus::Killed, "job 2 should be Killed");
        assert_eq!(s3, JobStatus::Killed, "job 3 should be Killed");
    }

    #[tokio::test]
    async fn audit_records_job_started_and_finished() {
        use crate::session::AuditEventKind;

        // Build the stores and engine manually so we retain a SessionStore handle
        // to read audit events after the run (the Harness helper doesn't expose it).
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = Workspace::new(dir.path()).expect("workspace");
        let sessions = SessionStore::new();
        let session_id = SessionId("s_audit".into());
        sessions.open(
            session_id.clone(),
            ws,
            EnforcementTier::DevNone,
            CapabilitySet::new(vec![RuntimeCapability::ProcSpawn]),
            fresh_token(),
        );
        let jobs = JobStore::new();
        let engine = JobEngine::new(
            sessions.clone(),
            jobs,
            std::sync::Arc::new(std::sync::RwLock::new(crate::ProviderRegistry::new())),
        );

        let req = command_req(&session_id, "echo hi");
        let id = engine.start(&req).await.expect("start");
        poll_terminal(&engine, &id).await;

        // Read the session's audit log via the SessionStore we retained.
        let session_arc = sessions.get(&session_id).expect("session must exist");
        let session = session_arc.read().unwrap_or_else(|p| p.into_inner());
        let events = session.audit_events();

        // There must be a JobStarted event for this job id.
        let started = events
            .iter()
            .any(|e| matches!(&e.kind, AuditEventKind::JobStarted { job_id } if job_id == &id));
        assert!(started, "expected a JobStarted audit event for {id:?}");

        // There must be a JobFinished event for this job id with Exited { code: 0 }.
        let finished = events.iter().any(|e| {
            matches!(&e.kind, AuditEventKind::JobFinished { job_id, status }
                if job_id == &id && *status == JobStatusProto::Exited { code: 0 })
        });
        assert!(
            finished,
            "expected a JobFinished(Exited{{code:0}}) audit event for {id:?}"
        );
    }

    // ── sandbox wiring ────────────────────────────────────────────────────────

    /// E2E confinement test: a `KernelLsm` job cannot read a file that lives
    /// outside the workspace tempdir.  A file in a SEPARATE tempdir is the
    /// secret — `/etc` is in the sandbox baseline read-allowlist so is NOT a
    /// reliable oracle.  Only runs on Linux with Landlock available.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn kernel_lsm_confines_job_to_workspace() {
        if !axp_sandbox::landlock_available() {
            eprintln!("skipping: Landlock unavailable");
            return;
        }
        let dir = tempfile::tempdir().expect("workspace tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "TOPSECRET").expect("write secret");

        let ws = Workspace::new(dir.path()).expect("workspace");
        let sessions = SessionStore::new();
        let session_id = SessionId("s_lsm".into());
        sessions.open(
            session_id.clone(),
            ws,
            EnforcementTier::KernelLsm,
            CapabilitySet::new(vec![RuntimeCapability::ProcSpawn]),
            fresh_token(),
        );
        let engine = JobEngine::new(
            sessions,
            JobStore::new(),
            std::sync::Arc::new(std::sync::RwLock::new(crate::ProviderRegistry::new())),
        );

        // Negative: secret outside the workspace must NOT be readable.
        let req = command_req(&session_id, &format!("cat {}", secret.display()));
        let id = engine.start(&req).await.expect("start under KernelLsm");
        poll_terminal(&engine, &id).await;

        let out = collected_bytes(&engine, &id, LogStream::Stdout);
        assert!(
            !out.windows(b"TOPSECRET".len()).any(|w| w == b"TOPSECRET"),
            "secret outside the workspace leaked under KernelLsm: {:?}",
            String::from_utf8_lossy(&out)
        );

        // Positive: a file inside the workspace IS readable (workspace-root-always-readable).
        let ok_file = dir.path().join("ok.txt");
        std::fs::write(&ok_file, "OK").expect("write ok file");
        let req2 = command_req(&session_id, &format!("cat {}", ok_file.display()));
        let id2 = engine.start(&req2).await.expect("start workspace read");
        poll_terminal(&engine, &id2).await;

        let out2 = collected_bytes(&engine, &id2, LogStream::Stdout);
        assert!(
            out2.windows(b"OK".len()).any(|w| w == b"OK"),
            "expected to read workspace file under KernelLsm, got {:?}",
            String::from_utf8_lossy(&out2)
        );
        let handle2 = engine.jobs().get(&id2).expect("job2 present");
        let status2 = handle2.read().unwrap().status.clone();
        assert_eq!(
            status2,
            JobStatus::Exited { code: 0 },
            "workspace cat should exit 0 under KernelLsm"
        );
    }

    /// On non-Linux hosts, `KernelLsm` must FAIL job start with
    /// `SandboxUnavailable` — never run unconfined.
    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn kernel_lsm_on_non_linux_fails_job_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = Workspace::new(dir.path()).expect("workspace");
        let sessions = SessionStore::new();
        let session_id = SessionId("s_lsm_nl".into());
        sessions.open(
            session_id.clone(),
            ws,
            EnforcementTier::KernelLsm,
            CapabilitySet::new(vec![RuntimeCapability::ProcSpawn]),
            fresh_token(),
        );
        let engine = JobEngine::new(
            sessions,
            JobStore::new(),
            std::sync::Arc::new(std::sync::RwLock::new(crate::ProviderRegistry::new())),
        );
        let req = command_req(&session_id, "echo hi");
        let result = engine.start(&req).await;
        assert!(
            matches!(result, Err(Error::SandboxUnavailable { .. })),
            "expected SandboxUnavailable on non-Linux, got {result:?}"
        );
    }
}
