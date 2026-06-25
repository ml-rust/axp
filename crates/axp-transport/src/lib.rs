//! `axp-transport` — JSON-RPC-2.0-over-HTTP transport for AXP.
//!
//! This crate provides the wire-protocol layer: envelope types, error mapping,
//! shared application state, the axum router, the six core JSON-RPC method
//! handlers, and a resumable SSE endpoint for streaming `job.attach`.  A server
//! run-loop is not yet wired.

mod attach;
mod error;
mod handlers;
mod jsonrpc;
mod router;
mod state;

pub use error::TransportError;
pub use jsonrpc::{
    DENIED, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, JsonRpcError, JsonRpcRequest,
    JsonRpcResponse, METHOD_NOT_FOUND, NOT_FOUND, NOT_IMPLEMENTED, PARSE_ERROR,
};
pub use router::{build_router, dispatch};
pub use state::AppState;
