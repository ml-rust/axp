//! Session lifecycle request, response, and audit event types.
use serde::{Deserialize, Serialize};

use crate::{Capability, EnforcementTier, JobId, JobStatusProto, SessionId};

/// `session.open` request: open an isolated session over a workspace at a declared tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOpenRequest {
    /// Filesystem path to the isolated workspace root; default-deny outside it.
    pub workspace: String,
    /// Requested sandbox enforcement tier.
    pub sandbox_tier: EnforcementTier,
    /// Requested capability grants (least-privilege, attenuable).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<Capability>,
}

/// `session.open` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOpenResponse {
    /// The assigned session identifier.
    pub session_id: SessionId,
    /// The enforcement tier actually granted (may be stronger-or-equal to requested; never silently weaker).
    pub granted_tier: EnforcementTier,
    /// Opaque object-capability token for subsequent calls.
    pub cap_token: String,
}

/// `session.close` request: close a live session and revoke its capability token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCloseRequest {
    /// The session to close.
    pub session_id: SessionId,
    /// Opaque capability token proving authority over the session.
    pub cap_token: String,
}

/// `session.close` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCloseResponse {
    /// True when the session was closed.
    pub ok: bool,
}

/// `session.audit` request: read audit events for a live session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAuditRequest {
    /// The live session to read audit events from.
    pub session_id: SessionId,
    /// Opaque capability token proving authority over the session.
    pub cap_token: String,
}

/// `session.audit` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAuditResponse {
    /// Ordered audit events recorded for the live session.
    pub events: Vec<SessionAuditEvent>,
}

/// A serializable audit event recorded for a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAuditEvent {
    /// Milliseconds since the Unix epoch when the event was recorded.
    pub ts_millis: u64,
    /// Event-specific payload.
    #[serde(flatten)]
    pub kind: SessionAuditEventKind,
}

/// Serializable audit event payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SessionAuditEventKind {
    /// The session was opened.
    SessionOpened,
    /// The session was closed.
    SessionClosed,
    /// A job was started in this session.
    JobStarted {
        /// The id of the job that started.
        job_id: JobId,
    },
    /// A job reached a terminal status in this session.
    JobFinished {
        /// The id of the job that finished.
        job_id: JobId,
        /// The terminal status reached by the job.
        status: JobStatusProto,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_open_request_round_trips() {
        let req = SessionOpenRequest {
            workspace: "/proj".into(),
            sandbox_tier: EnforcementTier::KernelLsm,
            capabilities: vec![
                Capability("fs.read(/proj)".into()),
                Capability("proc.spawn".into()),
            ],
        };
        let json = serde_json::to_string(&req).unwrap();
        let req2: SessionOpenRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, req2);
    }

    #[test]
    fn session_open_request_json_shape() {
        let req = SessionOpenRequest {
            workspace: "/proj".into(),
            sandbox_tier: EnforcementTier::KernelLsm,
            capabilities: vec![
                Capability("fs.read(/proj)".into()),
                Capability("proc.spawn".into()),
            ],
        };
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "workspace": "/proj",
                "sandbox_tier": "kernel-lsm",
                "capabilities": ["fs.read(/proj)", "proc.spawn"]
            })
        );
    }

    #[test]
    fn empty_capabilities_omitted_from_json() {
        let req = SessionOpenRequest {
            workspace: "/proj".into(),
            sandbox_tier: EnforcementTier::KernelLsm,
            capabilities: vec![],
        };
        let value = serde_json::to_value(&req).unwrap();
        assert!(!value.as_object().unwrap().contains_key("capabilities"));
    }

    #[test]
    fn session_open_response_round_trips() {
        let resp = SessionOpenResponse {
            session_id: SessionId("s_91".into()),
            granted_tier: EnforcementTier::KernelLsm,
            cap_token: "tok_abc".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let resp2: SessionOpenResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, resp2);
    }

    #[test]
    fn session_close_request_round_trips() {
        let req = SessionCloseRequest {
            session_id: SessionId("s_91".into()),
            cap_token: "tok_abc".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let req2: SessionCloseRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, req2);
    }

    #[test]
    fn session_close_response_json_shape() {
        let resp = SessionCloseResponse { ok: true };
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(value, serde_json::json!({ "ok": true }));
    }

    #[test]
    fn session_audit_request_round_trips() {
        let req = SessionAuditRequest {
            session_id: SessionId("s_91".into()),
            cap_token: "tok_abc".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let req2: SessionAuditRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, req2);
    }

    #[test]
    fn session_audit_event_json_shape() {
        let event = SessionAuditEvent {
            ts_millis: 1_700_000_000_000,
            kind: SessionAuditEventKind::JobFinished {
                job_id: JobId("j_1".into()),
                status: JobStatusProto::Exited { code: 0 },
            },
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "ts_millis": 1_700_000_000_000u64,
                "event": "job_finished",
                "job_id": "j_1",
                "status": {
                    "status": "exited",
                    "code": 0
                }
            })
        );
        let event2: SessionAuditEvent = serde_json::from_value(value).unwrap();
        assert_eq!(event, event2);
    }

    #[test]
    fn session_audit_response_round_trips() {
        let resp = SessionAuditResponse {
            events: vec![SessionAuditEvent {
                ts_millis: 1,
                kind: SessionAuditEventKind::SessionOpened,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let resp2: SessionAuditResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, resp2);
    }
}
