//! `job.start` request and response types.
use serde::{Deserialize, Serialize};

use crate::{Capability, JobId, SessionId};

/// The work a job runs: a shell command, or a code submission (code-mode).
/// Internally tagged on `kind` so the receiver knows the execution path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobPayload {
    /// A shell command string.
    Command {
        /// The shell command to execute.
        command: String,
    },
    /// A code submission executed inside the sandbox (code-mode). `lang` optionally selects
    /// the runtime; absent means the server's default runtime.
    Code {
        /// The source code to execute.
        code: String,
        /// The runtime language identifier (e.g. `"python"`, `"javascript"`); absent means the server default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lang: Option<String>,
    },
}

/// `job.start` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStartRequest {
    /// The session this job runs within.
    pub session_id: SessionId,
    /// The work to run; its `kind`/fields are flattened into this object on the wire.
    #[serde(flatten)]
    pub payload: JobPayload,
    /// Working directory within the workspace; defaults to the workspace root if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Capability grants scoped to this job (a subset/attenuation of the session's grants).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<Capability>,
}

/// `job.start` response. Streaming logs/exit follow out-of-band keyed by this id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStartResponse {
    /// The identifier assigned to the newly started job.
    pub job_id: JobId,
}

/// Wire-format job status. Internally tagged on `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JobStatusProto {
    /// Created, not yet started.
    Pending,
    /// Process is running.
    Running,
    /// Process exited with this code.
    Exited { code: i32 },
    /// Process was killed (signal/cancel).
    Killed,
    /// Job failed before/around execution (capability denial, spawn error, buffer overflow, …).
    Failed { reason: String },
}

/// Which standard stream a log frame came from (wire form).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStreamProto {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// A single log frame streamed to an attached client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEventFrame {
    /// The job this frame belongs to.
    pub job_id: JobId,
    /// Monotonic sequence number of this frame within the job's log stream.
    pub seq: u64,
    /// Which standard stream the bytes came from.
    pub stream: LogStreamProto,
    /// Raw log bytes for this chunk.
    pub data: Vec<u8>,
    /// Milliseconds since the Unix epoch when the chunk was captured.
    pub ts_millis: u64,
}

/// `job.attach` request: reattach to a job's log stream from a given offset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobAttachRequest {
    /// The session the caller is operating within (must own the job).
    pub session_id: SessionId,
    /// The job to attach to.
    pub job_id: JobId,
    /// Resume from this sequence number (0 = from the beginning). `Last-Event-ID` semantics.
    #[serde(default)]
    pub from_offset: u64,
}

/// `job.status` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStatusRequest {
    /// The session the caller is operating within (must own the job).
    pub session_id: SessionId,
    /// The job to report on.
    pub job_id: JobId,
}

/// `job.status` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStatusResponse {
    /// The job this status describes.
    pub job_id: JobId,
    /// Current lifecycle status of the job.
    pub status: JobStatusProto,
    /// Current number of buffered log events (useful for offset negotiation).
    pub seq: u64,
}

/// `job.cancel` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobCancelRequest {
    /// The session the caller is operating within (must own the job).
    pub session_id: SessionId,
    /// The job to cancel.
    pub job_id: JobId,
}

/// `job.cancel` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobCancelResponse {
    /// True if a running job was signalled; false if it was already finished.
    pub ok: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_payload_flattens_into_request() {
        let req = JobStartRequest {
            session_id: SessionId("s_91".into()),
            payload: JobPayload::Command {
                command: "git diff".into(),
            },
            cwd: None,
            capabilities: vec![],
        };
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "session_id": "s_91",
                "kind": "command",
                "command": "git diff"
            })
        );
    }

    #[test]
    fn command_payload_round_trips() {
        let req = JobStartRequest {
            session_id: SessionId("s_91".into()),
            payload: JobPayload::Command {
                command: "git diff".into(),
            },
            cwd: None,
            capabilities: vec![],
        };
        let value = serde_json::to_value(&req).unwrap();
        let req2: JobStartRequest = serde_json::from_value(value).unwrap();
        assert_eq!(req, req2);
    }

    #[test]
    fn code_payload_with_lang_round_trips() {
        let req = JobStartRequest {
            session_id: SessionId("s_1".into()),
            payload: JobPayload::Code {
                code: "print(1)".into(),
                lang: Some("python".into()),
            },
            cwd: Some("sub".into()),
            capabilities: vec![],
        };
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["kind"], "code");
        assert_eq!(value["lang"], "python");
        assert_eq!(value["cwd"], "sub");
        let req2: JobStartRequest = serde_json::from_value(value).unwrap();
        assert_eq!(req, req2);
    }

    #[test]
    fn code_payload_without_lang_omits_field() {
        let req = JobStartRequest {
            session_id: SessionId("s_1".into()),
            payload: JobPayload::Code {
                code: "print(1)".into(),
                lang: None,
            },
            cwd: None,
            capabilities: vec![],
        };
        let value = serde_json::to_value(&req).unwrap();
        assert!(!value.as_object().unwrap().contains_key("lang"));
    }

    #[test]
    fn job_start_response_round_trips() {
        let resp = JobStartResponse {
            job_id: JobId("j_5".into()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let resp2: JobStartResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, resp2);
    }

    #[test]
    fn job_status_proto_pending_serializes() {
        let val = serde_json::to_value(JobStatusProto::Pending).unwrap();
        assert_eq!(val, serde_json::json!({"status": "pending"}));
    }

    #[test]
    fn job_status_proto_exited_serializes() {
        let val = serde_json::to_value(JobStatusProto::Exited { code: 0 }).unwrap();
        assert_eq!(val, serde_json::json!({"status": "exited", "code": 0}));
    }

    #[test]
    fn job_status_proto_failed_serializes() {
        let val = serde_json::to_value(JobStatusProto::Failed { reason: "x".into() }).unwrap();
        assert_eq!(val, serde_json::json!({"status": "failed", "reason": "x"}));
    }

    #[test]
    fn job_status_proto_pending_round_trips() {
        let orig = JobStatusProto::Pending;
        let val = serde_json::to_value(&orig).unwrap();
        let back: JobStatusProto = serde_json::from_value(val).unwrap();
        assert_eq!(orig, back);
    }

    #[test]
    fn job_status_proto_exited_round_trips() {
        let orig = JobStatusProto::Exited { code: 0 };
        let val = serde_json::to_value(&orig).unwrap();
        let back: JobStatusProto = serde_json::from_value(val).unwrap();
        assert_eq!(orig, back);
    }

    #[test]
    fn job_status_proto_failed_round_trips() {
        let orig = JobStatusProto::Failed { reason: "x".into() };
        let val = serde_json::to_value(&orig).unwrap();
        let back: JobStatusProto = serde_json::from_value(val).unwrap();
        assert_eq!(orig, back);
    }

    #[test]
    fn log_event_frame_round_trips_with_snake_case_stream() {
        let frame = LogEventFrame {
            job_id: JobId("j_7".into()),
            seq: 3,
            stream: LogStreamProto::Stdout,
            data: b"hello".to_vec(),
            ts_millis: 1_700_000_000_000,
        };
        let value = serde_json::to_value(&frame).unwrap();
        assert_eq!(value["stream"], "stdout");
        let frame2: LogEventFrame = serde_json::from_value(value).unwrap();
        assert_eq!(frame, frame2);
    }

    #[test]
    fn job_attach_request_from_offset_defaults_to_zero() {
        let json = serde_json::json!({
            "session_id": "s_1",
            "job_id": "j_1"
        });
        let req: JobAttachRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.from_offset, 0);
        // And a full round trip preserves an explicit offset.
        let req2 = JobAttachRequest {
            session_id: SessionId("s_1".into()),
            job_id: JobId("j_1".into()),
            from_offset: 42,
        };
        let back: JobAttachRequest =
            serde_json::from_value(serde_json::to_value(&req2).unwrap()).unwrap();
        assert_eq!(req2, back);
    }

    #[test]
    fn job_status_response_round_trips() {
        let resp = JobStatusResponse {
            job_id: JobId("j_9".into()),
            status: JobStatusProto::Exited { code: 0 },
            seq: 5,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let resp2: JobStatusResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, resp2);
    }

    #[test]
    fn job_cancel_request_round_trips() {
        let req = JobCancelRequest {
            session_id: SessionId("s_1".into()),
            job_id: JobId("j_1".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let req2: JobCancelRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, req2);
    }

    #[test]
    fn job_cancel_response_ok_true_round_trips() {
        let resp = JobCancelResponse { ok: true };
        let json = serde_json::to_string(&resp).unwrap();
        let resp2: JobCancelResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, resp2);
    }

    #[test]
    fn job_cancel_response_ok_false_round_trips() {
        let resp = JobCancelResponse { ok: false };
        let json = serde_json::to_string(&resp).unwrap();
        let resp2: JobCancelResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, resp2);
    }
}
