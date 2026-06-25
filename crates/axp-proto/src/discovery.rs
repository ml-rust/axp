//! `axp.index` and `axp.describe` request and response types.
use serde::{Deserialize, Serialize};

use crate::SessionId;

/// One entry in the cheap breadth catalog returned by `axp.index`: name + one-line description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// The capability name.
    pub name: String,
    /// A one-line human-readable description.
    pub desc: String,
}

/// Full on-demand detail for one capability, returned by `axp.describe`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityDetail {
    /// TypeScript-style signature string, e.g. `git_diff(): string`.
    pub signature: String,
    /// JSON Schema (2020-12) for the capability's parameters, carried verbatim.
    pub schema: serde_json::Value,
}

/// `axp.index` request — session-scoped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRequest {
    /// The session to query capabilities for.
    pub session_id: SessionId,
    /// Opaque capability token proving authority over the session (from session.open).
    pub cap_token: String,
}

/// `axp.index` response — the full catalog as name+description pairs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexResponse {
    /// All available capabilities in the session's catalog.
    pub entries: Vec<IndexEntry>,
}

/// `axp.describe` request — fetch full detail for one capability within a session's catalog.
/// `session_id` is REQUIRED: describe resolves a name within the same session-scoped catalog
/// that `index` returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DescribeRequest {
    /// The session whose catalog to query.
    pub session_id: SessionId,
    /// Opaque capability token proving authority over the session (from session.open).
    pub cap_token: String,
    /// The name of the capability to describe.
    pub name: String,
}

/// `axp.describe` response is the capability detail itself (flat `{signature, schema}` on the wire).
pub type DescribeResponse = CapabilityDetail;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_response_round_trips() {
        let resp = IndexResponse {
            entries: vec![IndexEntry {
                name: "git_diff".into(),
                desc: "Show uncommitted changes".into(),
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let resp2: IndexResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, resp2);
    }

    #[test]
    fn index_response_json_shape() {
        let resp = IndexResponse {
            entries: vec![IndexEntry {
                name: "git_diff".into(),
                desc: "Show uncommitted changes".into(),
            }],
        };
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "entries": [{"name": "git_diff", "desc": "Show uncommitted changes"}]
            })
        );
    }

    #[test]
    fn capability_detail_round_trips() {
        let detail = CapabilityDetail {
            signature: "git_diff(): string".into(),
            schema: serde_json::json!({"type": "object"}),
        };
        let json = serde_json::to_string(&detail).unwrap();
        let detail2: CapabilityDetail = serde_json::from_str(&json).unwrap();
        assert_eq!(detail.signature, detail2.signature);
        assert_eq!(detail.schema, detail2.schema);
    }

    #[test]
    fn capability_detail_json_shape() {
        let detail: DescribeResponse = CapabilityDetail {
            signature: "git_diff(): string".into(),
            schema: serde_json::json!({"type": "object"}),
        };
        let value = serde_json::to_value(&detail).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "signature": "git_diff(): string",
                "schema": {"type": "object"}
            })
        );
    }
}
