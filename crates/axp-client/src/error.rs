//! Error types for the AXP HTTP client.

/// Result alias for AXP client operations.
pub type Result<T> = std::result::Result<T, Error>;

/// JSON-RPC error object returned by the server.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[non_exhaustive]
pub struct RpcError {
    /// Numeric JSON-RPC error code.
    pub code: i64,
    /// Human-readable server error message.
    pub message: String,
    /// Optional structured error data.
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

/// Error returned by the AXP HTTP client.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The configured server URL is not usable.
    #[error("invalid base URL: {0}")]
    InvalidBaseUrl(String),
    /// URL parsing or URL joining failed.
    #[error("url error: {0}")]
    Url(String),
    /// HTTP request or response handling failed.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    /// The server returned a non-success HTTP status without a JSON-RPC body.
    #[error("http status {0}")]
    HttpStatus(u16),
    /// JSON encoding or decoding failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// The server returned a JSON-RPC error response.
    #[error("json-rpc error {code}: {message}")]
    Rpc {
        /// Numeric JSON-RPC error code.
        code: i64,
        /// Human-readable server error message.
        message: String,
        /// Optional structured error data.
        data: Option<serde_json::Value>,
    },
    /// The server response did not match the JSON-RPC 2.0 response contract.
    #[error("invalid JSON-RPC response: {0}")]
    InvalidRpcResponse(String),
}

impl From<RpcError> for Error {
    fn from(error: RpcError) -> Self {
        Error::Rpc {
            code: error.code,
            message: error.message,
            data: error.data,
        }
    }
}
