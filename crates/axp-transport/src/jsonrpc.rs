//! JSON-RPC 2.0 envelope types: request, response, and error.
//!
//! These types cover the wire format only — no business logic here.

use serde::{Deserialize, Serialize};

// ── Standard JSON-RPC error codes ─────────────────────────────────────────────

/// JSON-RPC 2.0 standard: Parse error (invalid JSON received).
pub const PARSE_ERROR: i64 = -32700;
/// JSON-RPC 2.0 standard: Invalid Request (JSON was valid but not a valid
/// Request object).
pub const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC 2.0 standard: Method not found.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC 2.0 standard: Invalid params.
pub const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC 2.0 standard: Internal error.
pub const INTERNAL_ERROR: i64 = -32603;

// ── AXP application-range error codes ─────────────────────────────────────────

/// AXP application error: resource not found (session, job, or capability).
pub const NOT_FOUND: i64 = -32001;
/// AXP application error: capability access denied.
pub const DENIED: i64 = -32002;
/// AXP application error: method or feature not yet implemented.
pub const NOT_IMPLEMENTED: i64 = -32003;
/// AXP application error: authentication/authorization failed (unknown session
/// or invalid capability token).
pub const UNAUTHORIZED: i64 = -32004;

// ── JsonRpcRequest ─────────────────────────────────────────────────────────────

fn default_jsonrpc() -> String {
    "2.0".to_owned()
}

/// An inbound JSON-RPC 2.0 request object.
///
/// The `jsonrpc` and `id` fields default gracefully when absent so that
/// malformed-but-parseable requests can still produce a well-formed error
/// response with the correct echoed id.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    /// The JSON-RPC protocol version string. Should be `"2.0"`.
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,

    /// Client-supplied correlation id. `None` when the field is absent
    /// (i.e. a notification; AXP does not use notifications but we accept
    /// them without crashing).
    #[serde(default)]
    pub id: Option<serde_json::Value>,

    /// The method name to dispatch.
    pub method: String,

    /// Method parameters. Defaults to JSON `null` when absent.
    #[serde(default)]
    pub params: serde_json::Value,
}

// ── JsonRpcError ───────────────────────────────────────────────────────────────

/// A JSON-RPC 2.0 error object, embedded in a `JsonRpcResponse` on failure.
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    /// Numeric error code. See the `PARSE_ERROR`, `METHOD_NOT_FOUND`, etc.
    /// constants in this module.
    pub code: i64,

    /// Human-readable error message.
    pub message: String,

    /// Optional structured additional data. Omitted from serialization when
    /// `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// ── JsonRpcBody (private tagged-union) ────────────────────────────────────────

/// The success/failure body flattened into a [`JsonRpcResponse`].
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum JsonRpcBody {
    /// Successful response — carries a `result` field.
    Success { result: serde_json::Value },
    /// Error response — carries an `error` field.
    Failure { error: JsonRpcError },
}

// ── JsonRpcResponse ────────────────────────────────────────────────────────────

/// An outbound JSON-RPC 2.0 response object.
///
/// The `jsonrpc` field is always `"2.0"`.  The `id` mirrors the request's id
/// (or `null` when the request had no id).  Exactly one of `result` or `error`
/// appears in the serialized form, enforced by the flattened [`JsonRpcBody`].
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    /// Always `"2.0"`.
    pub jsonrpc: &'static str,

    /// Echoed correlation id from the request, or `null` for notifications /
    /// parse failures.
    pub id: serde_json::Value,

    /// Flattened — serializes as either `"result": ...` or `"error": {...}`.
    #[serde(flatten)]
    body: JsonRpcBody,
}

impl JsonRpcResponse {
    /// Build a success response carrying the given `result` value.
    pub fn success(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            body: JsonRpcBody::Success { result },
        }
    }

    /// Build an error response carrying the given [`JsonRpcError`].
    pub fn error(id: serde_json::Value, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            body: JsonRpcBody::Failure { error },
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn request_round_trip_full() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"axp.index","params":{}}"#;
        let req: JsonRpcRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.id, Some(json!(1)));
        assert_eq!(req.method, "axp.index");
        assert_eq!(req.params, json!({}));
    }

    #[test]
    fn request_defaults_applied_when_fields_absent() {
        // No `id` and no `params` — both should default gracefully.
        let raw = r#"{"method":"ping"}"#;
        let req: JsonRpcRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.id, None);
        assert_eq!(req.method, "ping");
        assert_eq!(req.params, serde_json::Value::Null);
    }

    #[test]
    fn success_response_serializes_correctly() {
        let resp = JsonRpcResponse::success(json!(1), json!({"ok": true}));
        let s = serde_json::to_string(&resp).unwrap();
        assert!(
            s.contains(r#""jsonrpc":"2.0""#),
            "missing jsonrpc field: {s}"
        );
        assert!(s.contains(r#""result""#), "missing result field: {s}");
        assert!(
            !s.contains(r#""error""#),
            "must not contain error field: {s}"
        );
    }

    #[test]
    fn error_response_serializes_correctly() {
        let err = JsonRpcError {
            code: METHOD_NOT_FOUND,
            message: "method not found: foo".into(),
            data: None,
        };
        let resp = JsonRpcResponse::error(json!(42), err);
        let s = serde_json::to_string(&resp).unwrap();
        assert!(
            s.contains(r#""jsonrpc":"2.0""#),
            "missing jsonrpc field: {s}"
        );
        assert!(s.contains(r#""error""#), "missing error field: {s}");
        assert!(s.contains("-32601"), "missing code: {s}");
        assert!(
            !s.contains(r#""result""#),
            "must not contain result field: {s}"
        );
    }

    #[test]
    fn error_data_none_is_omitted() {
        let err = JsonRpcError {
            code: INTERNAL_ERROR,
            message: "boom".into(),
            data: None,
        };
        let s = serde_json::to_string(&err).unwrap();
        assert!(!s.contains("data"), "data:None should be omitted: {s}");
    }
}
