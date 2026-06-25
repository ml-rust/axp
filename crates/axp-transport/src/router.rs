//! Axum router and JSON-RPC dispatcher.
//!
//! Exposes a `POST /` endpoint that speaks JSON-RPC 2.0 and a separate
//! `GET /job/attach` endpoint that streams a job's log frames as SSE.  The
//! `dispatch` function routes each JSON-RPC method to its handler in
//! [`crate::handlers`].

use axum::{Json, extract::State, response::IntoResponse, routing};

use crate::{
    handlers,
    jsonrpc::{
        INVALID_REQUEST, JsonRpcError, JsonRpcRequest, JsonRpcResponse, METHOD_NOT_FOUND,
        PARSE_ERROR,
    },
    state::AppState,
};

/// Build the axum [`Router`] for the AXP endpoints.
///
/// Registers `POST /` for JSON-RPC 2.0 and `GET /job/attach` for the resumable
/// SSE log stream.  The `state` is cloned into the router; all handlers share it
/// cheaply via `Arc`-backed fields.
pub fn build_router(state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/", routing::post(rpc_handler))
        .route("/job/attach", routing::get(crate::attach::attach_sse))
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
/// Each known method delegates to its handler in [`crate::handlers`].  The
/// result of the handler (`Ok(Value)` or `Err(TransportError)`) is converted
/// into a [`JsonRpcResponse`] with the echoed request id.
///
/// `job.attach` is not dispatched here — it is a streaming endpoint served over
/// `GET /job/attach` (SSE).  The JSON-RPC arm returns `INVALID_REQUEST` directing
/// clients to that endpoint.
///
/// The response id always echoes the request id (`null` when absent).
pub async fn dispatch(state: &AppState, req: JsonRpcRequest) -> JsonRpcResponse {
    let id = req.id.unwrap_or(serde_json::Value::Null);

    let result = match req.method.as_str() {
        "session.open" => handlers::session_open(state, req.params).await,
        "axp.index" => handlers::index(state, req.params).await,
        "axp.describe" => handlers::describe(state, req.params).await,
        "job.start" => handlers::job_start(state, req.params).await,
        "job.status" => handlers::job_status(state, req.params).await,
        "job.cancel" => handlers::job_cancel(state, req.params).await,
        "job.attach" => {
            return JsonRpcResponse::error(
                id,
                JsonRpcError {
                    code: INVALID_REQUEST,
                    message: "job.attach is a streaming endpoint; connect with GET /job/attach (text/event-stream)".into(),
                    data: None,
                },
            );
        }
        _ => {
            return JsonRpcResponse::error(
                id,
                JsonRpcError {
                    code: METHOD_NOT_FOUND,
                    message: format!("method not found: {}", req.method),
                    data: None,
                },
            );
        }
    };

    match result {
        Ok(v) => JsonRpcResponse::success(id, v),
        Err(te) => JsonRpcResponse::error(id, te.to_jsonrpc_error()),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock, atomic::AtomicU64};

    use axp_core::{JobEngine, JobStore, ProviderRegistry, SessionStore};
    use serde_json::json;

    use super::*;
    use crate::jsonrpc::{INVALID_REQUEST, METHOD_NOT_FOUND};

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
    async fn completely_unknown_method_returns_method_not_found() {
        let state = make_state();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: "completely.unknown".into(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch(&state, req).await;
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("-32601"), "expected METHOD_NOT_FOUND code: {s}");
        assert!(
            s.contains("completely.unknown"),
            "expected method name in message: {s}"
        );
    }

    #[tokio::test]
    async fn response_id_echoes_request_id() {
        let state = make_state();
        // Use a completely unknown method so the id-echo invariant is easy to
        // check without worrying about params decoding.
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(99)),
            method: "no.such.method".into(),
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
            method: "no.such.method".into(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch(&state, req).await;
        assert_eq!(resp.id, serde_json::Value::Null);
    }

    #[tokio::test]
    async fn job_attach_directs_to_streaming_endpoint() {
        let state = make_state();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: "job.attach".into(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch(&state, req).await;
        let s = serde_json::to_string(&resp).unwrap();
        assert!(
            s.contains(&INVALID_REQUEST.to_string()),
            "expected INVALID_REQUEST for job.attach: {s}"
        );
        assert!(
            s.contains("GET /job/attach"),
            "expected message directing to the streaming endpoint: {s}"
        );
        // The dispatcher must not treat job.attach as an unknown method.
        assert!(
            !s.contains(&METHOD_NOT_FOUND.to_string()),
            "job.attach must not be METHOD_NOT_FOUND: {s}"
        );
    }
}
