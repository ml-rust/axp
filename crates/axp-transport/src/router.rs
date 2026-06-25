//! Axum router and JSON-RPC dispatcher.
//!
//! Exposes a single `POST /` endpoint that speaks JSON-RPC 2.0.  Until method
//! handlers are wired (unit U7b), every method returns `-32601 Method not
//! found`.

use axum::{Json, extract::State, response::IntoResponse, routing};

use crate::{
    jsonrpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, METHOD_NOT_FOUND, PARSE_ERROR},
    state::AppState,
};

/// Build the axum [`Router`] for the AXP JSON-RPC endpoint.
///
/// A single `POST /` route is registered.  The `state` is cloned into the
/// router; all handlers share it cheaply via `Arc`-backed fields.
pub fn build_router(state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/", routing::post(rpc_handler))
        .with_state(state)
}

/// Axum handler for `POST /`.
///
/// Parses the raw request bytes as a JSON-RPC 2.0 request and delegates to
/// [`dispatch`].  Parse failures return a JSON-RPC PARSE_ERROR with id `null`
/// (HTTP 200 — protocol errors are in-body per JSON-RPC spec).
async fn rpc_handler(State(state): State<AppState>, body: axum::body::Bytes) -> impl IntoResponse {
    let req: JsonRpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            let resp = JsonRpcResponse::error(
                serde_json::Value::Null,
                JsonRpcError {
                    code: PARSE_ERROR,
                    message: format!("parse error: {e}"),
                    data: None,
                },
            );
            return Json(resp);
        }
    };

    let resp = dispatch(&state, req).await;
    Json(resp)
}

/// Dispatch a parsed JSON-RPC request to the appropriate method handler.
///
/// Currently all known methods fall through to the `_` arm and return
/// `-32601 Method not found`.  Unit U7b replaces the individual arms with real
/// handlers.
///
/// The response id always echoes the request id (`null` when absent).
pub async fn dispatch(state: &AppState, req: JsonRpcRequest) -> JsonRpcResponse {
    let id = req.id.unwrap_or(serde_json::Value::Null);

    // U7b will add arms for: "session.open", "axp.index", "axp.describe",
    // "job.start", "job.attach", "job.status", "job.cancel". Until then every
    // method returns -32601. The match (with only the `_` arm for now) is the
    // dispatch table U7b fills in, hence the temporary single-binding allow.
    #[allow(clippy::match_single_binding)]
    match req.method.as_str() {
        _ => {
            let _ = state; // state will be used by real handlers in U7b
            JsonRpcResponse::error(
                id,
                JsonRpcError {
                    code: METHOD_NOT_FOUND,
                    message: format!("method not found: {}", req.method),
                    data: None,
                },
            )
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock, atomic::AtomicU64};

    use axp_core::{JobEngine, JobStore, ProviderRegistry, SessionStore};
    use serde_json::json;

    use super::*;
    use crate::jsonrpc::METHOD_NOT_FOUND;

    fn make_state() -> AppState {
        let sessions = SessionStore::new();
        let engine = JobEngine::new(sessions.clone(), JobStore::new());
        AppState {
            sessions,
            engine,
            registry: Arc::new(RwLock::new(ProviderRegistry::new())),
            session_counter: Arc::new(AtomicU64::new(1)),
        }
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let state = make_state();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: "axp.index".into(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch(&state, req).await;
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("-32601"), "expected METHOD_NOT_FOUND code: {s}");
        assert!(
            s.contains("axp.index"),
            "expected method name in message: {s}"
        );
    }

    #[tokio::test]
    async fn response_id_echoes_request_id() {
        let state = make_state();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(99)),
            method: "session.open".into(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch(&state, req).await;
        assert_eq!(resp.id, json!(99), "id must be echoed verbatim");
    }

    #[tokio::test]
    async fn null_id_when_request_has_no_id() {
        let state = make_state();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: None,
            method: "job.start".into(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch(&state, req).await;
        assert_eq!(resp.id, serde_json::Value::Null);
    }

    #[tokio::test]
    async fn all_known_methods_return_method_not_found() {
        let state = make_state();
        let methods = [
            "session.open",
            "axp.index",
            "axp.describe",
            "job.start",
            "job.attach",
            "job.status",
            "job.cancel",
            "completely.unknown",
        ];
        for method in methods {
            let req = JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: method.into(),
                params: serde_json::Value::Null,
            };
            let resp = dispatch(&state, req).await;
            let s = serde_json::to_string(&resp).unwrap();
            assert!(
                s.contains(&METHOD_NOT_FOUND.to_string()),
                "method {method}: expected -32601, got: {s}"
            );
        }
    }
}
