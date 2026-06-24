//! `session.open` request and response types.
use serde::{Deserialize, Serialize};

use crate::{Capability, EnforcementTier, SessionId};

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
}
