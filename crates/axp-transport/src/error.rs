//! Transport-layer error type and mapping to JSON-RPC error objects.

use crate::jsonrpc::{
    DENIED, INTERNAL_ERROR, INVALID_PARAMS, JsonRpcError, NOT_FOUND, NOT_IMPLEMENTED, PARSE_ERROR,
    UNAUTHORIZED,
};

/// Failure modes for the transport layer.
///
/// Marked `#[non_exhaustive]` to allow future variants without a breaking
/// change to downstream `match` expressions.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransportError {
    /// A runtime error propagated from `axp-core`.
    #[error(transparent)]
    Runtime(#[from] axp_core::Error),

    /// The request body was not valid JSON or not a valid JSON-RPC object.
    #[error("parse error: {0}")]
    Parse(String),

    /// A method parameter could not be decoded or failed a structural check.
    #[error("invalid params: {0}")]
    InvalidParams(String),

    /// Authentication/authorization failed: the session is unknown or the
    /// presented capability token is invalid.
    ///
    /// The message is intentionally generic ("unauthorized") and identical for
    /// both cases so that an attacker cannot use the error to discover whether a
    /// given session id exists (no existence oracle).
    #[error("unauthorized")]
    Unauthorized,

    /// An unexpected internal error occurred in the transport layer.
    #[error("internal error: {0}")]
    Internal(String),
}

impl TransportError {
    /// Map this error to the JSON-RPC error object that should be sent to the
    /// client.
    ///
    /// The `message` is the `Display` representation of `self`.  The `data`
    /// field is always `None`; callers that need structured data should build
    /// a [`JsonRpcError`] directly.
    pub fn to_jsonrpc_error(&self) -> JsonRpcError {
        use axp_core::Error as CoreError;

        let code = match self {
            TransportError::Parse(_) => PARSE_ERROR,

            TransportError::InvalidParams(_) => INVALID_PARAMS,

            TransportError::Unauthorized => UNAUTHORIZED,

            TransportError::Internal(_) => INTERNAL_ERROR,

            TransportError::Runtime(core_err) => match core_err {
                CoreError::SessionNotFound(_)
                | CoreError::JobNotFound(_)
                | CoreError::CapabilityNotFound { .. } => NOT_FOUND,

                CoreError::CapabilityDenied { .. } => DENIED,

                CoreError::NotImplemented(_) => NOT_IMPLEMENTED,

                CoreError::InvalidWorkspace { .. }
                | CoreError::CapabilityParse { .. }
                | CoreError::WorkspaceViolation { .. } => INVALID_PARAMS,

                // JobSpawn, LogBufferOverflow, SandboxUnavailable, SandboxApply,
                // DuplicateProvider, DuplicateCapability, DescriptionQuality,
                // and any future variants added under #[non_exhaustive].
                _ => INTERNAL_ERROR,
            },
        };

        JsonRpcError {
            code,
            message: self.to_string(),
            data: None,
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use axp_proto::{JobId, SessionId};

    use super::*;
    use crate::jsonrpc::{
        DENIED, INTERNAL_ERROR, INVALID_PARAMS, NOT_FOUND, NOT_IMPLEMENTED, PARSE_ERROR,
        UNAUTHORIZED,
    };

    #[test]
    fn session_not_found_maps_to_not_found() {
        let err = TransportError::Runtime(axp_core::Error::SessionNotFound(SessionId("x".into())));
        assert_eq!(err.to_jsonrpc_error().code, NOT_FOUND);
    }

    #[test]
    fn job_not_found_maps_to_not_found() {
        let err = TransportError::Runtime(axp_core::Error::JobNotFound(JobId("j_1".into())));
        assert_eq!(err.to_jsonrpc_error().code, NOT_FOUND);
    }

    #[test]
    fn capability_not_found_maps_to_not_found() {
        let err =
            TransportError::Runtime(axp_core::Error::CapabilityNotFound { name: "foo".into() });
        assert_eq!(err.to_jsonrpc_error().code, NOT_FOUND);
    }

    #[test]
    fn capability_denied_maps_to_denied() {
        let err = TransportError::Runtime(axp_core::Error::CapabilityDenied {
            required: "fs:read".into(),
        });
        assert_eq!(err.to_jsonrpc_error().code, DENIED);
    }

    #[test]
    fn not_implemented_maps_correctly() {
        let err = TransportError::Runtime(axp_core::Error::NotImplemented("foo::bar"));
        assert_eq!(err.to_jsonrpc_error().code, NOT_IMPLEMENTED);
    }

    #[test]
    fn log_buffer_overflow_maps_to_internal_error() {
        let err = TransportError::Runtime(axp_core::Error::LogBufferOverflow { cap: 1 });
        assert_eq!(err.to_jsonrpc_error().code, INTERNAL_ERROR);
    }

    #[test]
    fn job_spawn_maps_to_internal_error() {
        let err = TransportError::Runtime(axp_core::Error::JobSpawn {
            reason: "no such binary".into(),
        });
        assert_eq!(err.to_jsonrpc_error().code, INTERNAL_ERROR);
    }

    #[test]
    fn invalid_workspace_maps_to_invalid_params() {
        let err = TransportError::Runtime(axp_core::Error::InvalidWorkspace {
            path: "/bad".into(),
            reason: "not a dir".into(),
        });
        assert_eq!(err.to_jsonrpc_error().code, INVALID_PARAMS);
    }

    #[test]
    fn unauthorized_maps_to_unauthorized_code() {
        let err = TransportError::Unauthorized;
        assert_eq!(err.to_jsonrpc_error().code, UNAUTHORIZED);
    }

    #[test]
    fn unauthorized_message_is_generic() {
        // Must not reveal whether the session exists or the token was wrong.
        let err = TransportError::Unauthorized;
        assert_eq!(err.to_jsonrpc_error().message, "unauthorized");
    }

    #[test]
    fn parse_error_carries_parse_error_code() {
        let err = TransportError::Parse("unexpected token".into());
        assert_eq!(err.to_jsonrpc_error().code, PARSE_ERROR);
    }

    #[test]
    fn message_comes_from_display() {
        let err =
            TransportError::Runtime(axp_core::Error::SessionNotFound(SessionId("abc".into())));
        let je = err.to_jsonrpc_error();
        assert!(
            je.message.contains("abc"),
            "expected id in message: {}",
            je.message
        );
    }

    #[test]
    fn data_is_always_none() {
        let err = TransportError::Runtime(axp_core::Error::SessionNotFound(SessionId("x".into())));
        assert!(err.to_jsonrpc_error().data.is_none());
    }
}
