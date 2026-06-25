//! JSON-RPC method handlers for the AXP transport layer.
//!
//! Each public function corresponds to one JSON-RPC method.  All handlers share
//! the same async signature `(state: &AppState, params: serde_json::Value) ->
//! Result<serde_json::Value, TransportError>` so that `dispatch` can call them
//! uniformly from a `match` arm.
//!
//! The two private helpers [`parse_params`] and [`to_value`] are used by every
//! handler to decode incoming parameters and encode outgoing results; do not
//! duplicate their logic inline.

use axp_core::{CapabilitySet, Workspace};
use axp_proto::{
    DescribeRequest, IndexRequest, JobCancelRequest, JobStartRequest, JobStatusRequest, SessionId,
    SessionOpenRequest, SessionOpenResponse,
};
use serde::{Serialize, de::DeserializeOwned};

use crate::{TransportError, state::AppState};

// ── Private helpers ────────────────────────────────────────────────────────────

/// Decode JSON-RPC `params` into a typed request, mapping failure to
/// [`TransportError::InvalidParams`].
fn parse_params<T: DeserializeOwned>(params: serde_json::Value) -> Result<T, TransportError> {
    serde_json::from_value(params).map_err(|e| TransportError::InvalidParams(e.to_string()))
}

/// Serialize a typed response to a [`serde_json::Value`], mapping failure to
/// [`TransportError::Internal`].
fn to_value<T: Serialize>(v: &T) -> Result<serde_json::Value, TransportError> {
    serde_json::to_value(v).map_err(|e| TransportError::Internal(e.to_string()))
}

/// Return `Ok(())` if `id` is a known session, otherwise `Err(SessionNotFound)`.
fn require_session(state: &AppState, id: &SessionId) -> Result<(), TransportError> {
    if state.sessions.get(id).is_none() {
        return Err(axp_core::Error::SessionNotFound(id.clone()).into());
    }
    Ok(())
}

// ── Method handlers ────────────────────────────────────────────────────────────

/// Handle `session.open`: open an isolated workspace session.
///
/// Parses the workspace path and capability grants from the request, mints a
/// new session id, opens the session in the store, and returns a
/// [`SessionOpenResponse`] with the assigned id and granted tier.
///
/// # TODO
///
/// Replace `cap_token` with a real object-capability token and add
/// cryptographic validation of tokens on subsequent calls.
pub(crate) async fn session_open(
    state: &AppState,
    params: serde_json::Value,
) -> Result<serde_json::Value, TransportError> {
    let req = parse_params::<SessionOpenRequest>(params)?;
    let workspace = Workspace::new(&req.workspace)?;
    let caps = CapabilitySet::from_wire(&req.capabilities)?;
    let id = state.next_session_id();
    // TODO(auth): real object-capability token + validation (post-MVP)
    let resp = SessionOpenResponse {
        cap_token: id.0.clone(),
        session_id: id.clone(),
        granted_tier: req.sandbox_tier,
    };
    state.sessions.open(id, workspace, req.sandbox_tier, caps);
    to_value(&resp)
}

/// Handle `axp.index`: return the full capability catalog for a session.
///
/// The session id is validated (unknown → `NOT_FOUND`) but the registry is
/// global for now; it is not yet scoped per session.
pub(crate) async fn index(
    state: &AppState,
    params: serde_json::Value,
) -> Result<serde_json::Value, TransportError> {
    let req = parse_params::<IndexRequest>(params)?;
    require_session(state, &req.session_id)?;
    // NOTE: registry is global for now; session_id is validated but not yet session-scoped.
    let reg = state.registry.read().unwrap_or_else(|p| p.into_inner());
    let resp = reg.index()?;
    to_value(&resp)
}

/// Handle `axp.describe`: return full detail for one capability by name.
///
/// The session id is validated (unknown → `NOT_FOUND`) and the capability name
/// is resolved from the global registry (unknown name → `NOT_FOUND`).
pub(crate) async fn describe(
    state: &AppState,
    params: serde_json::Value,
) -> Result<serde_json::Value, TransportError> {
    let req = parse_params::<DescribeRequest>(params)?;
    require_session(state, &req.session_id)?;
    // NOTE: registry is global for now; session_id is validated but not yet session-scoped.
    let reg = state.registry.read().unwrap_or_else(|p| p.into_inner());
    let detail = reg.describe(&req.name)?;
    to_value(&detail)
}

/// Handle `job.start`: start a new job in an existing session.
///
/// Delegates to [`JobEngine::start`] after decoding the request; the engine
/// performs all session/capability/cwd validation.
pub(crate) async fn job_start(
    state: &AppState,
    params: serde_json::Value,
) -> Result<serde_json::Value, TransportError> {
    let req = parse_params::<JobStartRequest>(params)?;
    let job_id = state.engine.start(&req).await?;
    to_value(&axp_proto::JobStartResponse { job_id })
}

/// Handle `job.status`: return the current status of a job.
///
/// Delegates to [`JobEngine::status`]; returns `NOT_FOUND` if the job is
/// unknown or not owned by the specified session.
pub(crate) async fn job_status(
    state: &AppState,
    params: serde_json::Value,
) -> Result<serde_json::Value, TransportError> {
    let req = parse_params::<JobStatusRequest>(params)?;
    let resp = state.engine.status(&req)?;
    to_value(&resp)
}

/// Handle `job.cancel`: cancel a running job.
///
/// Delegates to [`JobEngine::cancel`]; returns `NOT_FOUND` if the job is
/// unknown or not owned by the specified session.  Returns `ok: false` (not an
/// error) when the job has already finished.
pub(crate) async fn job_cancel(
    state: &AppState,
    params: serde_json::Value,
) -> Result<serde_json::Value, TransportError> {
    let req = parse_params::<JobCancelRequest>(params)?;
    let resp = state.engine.cancel(&req)?;
    to_value(&resp)
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use axp_proto::SessionId;
    use serde_json::{Value, json};

    use crate::{
        AppState,
        jsonrpc::{
            INVALID_PARAMS, INVALID_REQUEST, JsonRpcRequest, JsonRpcResponse, METHOD_NOT_FOUND,
            NOT_FOUND,
        },
        router::dispatch,
    };

    /// Call `dispatch` and return the JSON-serialized response as a `Value`.
    async fn call(state: &AppState, method: &str, params: Value) -> Value {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params,
        };
        let resp: JsonRpcResponse = dispatch(state, req).await;
        serde_json::to_value(&resp).expect("JsonRpcResponse must serialize")
    }

    /// Open a session over a real tempdir and return `(state, session_id, _dir)`.
    /// Keep `_dir` alive for the duration of the test so the path remains valid.
    async fn open_session(state: &AppState, dir: &tempfile::TempDir) -> SessionId {
        let ws = dir.path().to_string_lossy().into_owned();
        let v = call(
            state,
            "session.open",
            json!({
                "workspace": ws,
                "sandbox_tier": "dev-none",
                "capabilities": ["proc.spawn"]
            }),
        )
        .await;
        let id_str = v["result"]["session_id"]
            .as_str()
            .expect("session_id must be a string")
            .to_owned();
        SessionId(id_str)
    }

    // ── session.open ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn session_open_returns_session_id_with_s_prefix() {
        let state = AppState::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let v = call(
            &state,
            "session.open",
            json!({
                "workspace": dir.path().to_string_lossy(),
                "sandbox_tier": "dev-none",
                "capabilities": ["proc.spawn"]
            }),
        )
        .await;
        let id = v["result"]["session_id"].as_str().expect("session_id");
        assert!(id.starts_with("s_"), "expected s_ prefix, got: {id}");
        let _dir = dir; // keep alive
    }

    // ── axp.index ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn index_valid_session_returns_builtin_entries() {
        let state = AppState::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let sid = open_session(&state, &dir).await;

        let v = call(&state, "axp.index", json!({ "session_id": sid.0 })).await;
        let entries = v["result"]["entries"]
            .as_array()
            .expect("entries must be an array");
        let names: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();
        assert!(
            names.contains(&"git_diff"),
            "expected git_diff in index: {names:?}"
        );
        assert!(
            names.contains(&"git_log"),
            "expected git_log in index: {names:?}"
        );
        let _dir = dir; // keep alive
    }

    #[tokio::test]
    async fn index_unknown_session_returns_not_found() {
        let state = AppState::new();
        let v = call(&state, "axp.index", json!({ "session_id": "s_unknown" })).await;
        assert_eq!(
            v["error"]["code"], NOT_FOUND,
            "expected NOT_FOUND for unknown session: {v}"
        );
    }

    // ── axp.describe ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn describe_unknown_capability_returns_not_found() {
        let state = AppState::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let sid = open_session(&state, &dir).await;

        let v = call(
            &state,
            "axp.describe",
            json!({ "session_id": sid.0, "name": "nonexistent_cap" }),
        )
        .await;
        assert_eq!(
            v["error"]["code"], NOT_FOUND,
            "expected NOT_FOUND for unknown capability: {v}"
        );
        let _dir = dir; // keep alive
    }

    // ── job.start ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn job_start_returns_job_id() {
        let state = AppState::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let sid = open_session(&state, &dir).await;

        let v = call(
            &state,
            "job.start",
            json!({
                "session_id": sid.0,
                "kind": "command",
                "command": "echo hi"
            }),
        )
        .await;
        let job_id = v["result"]["job_id"]
            .as_str()
            .expect("job_id must be string");
        assert!(
            job_id.starts_with("j_"),
            "expected j_ prefix, got: {job_id}"
        );
        let _dir = dir; // keep alive
    }

    #[tokio::test]
    async fn job_start_malformed_params_returns_invalid_params() {
        let state = AppState::new();
        let v = call(
            &state,
            "job.start",
            json!({ "session_id": "s_1" /* missing kind */ }),
        )
        .await;
        assert_eq!(
            v["error"]["code"], INVALID_PARAMS,
            "expected INVALID_PARAMS for missing kind: {v}"
        );
    }

    // ── unknown / deferred methods ─────────────────────────────────────────────

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let state = AppState::new();
        let v = call(&state, "frobnicate", json!(null)).await;
        assert_eq!(
            v["error"]["code"], METHOD_NOT_FOUND,
            "expected METHOD_NOT_FOUND for unknown method: {v}"
        );
    }

    #[tokio::test]
    async fn job_attach_directs_to_streaming_endpoint() {
        let state = AppState::new();
        let v = call(&state, "job.attach", json!(null)).await;
        assert_eq!(
            v["error"]["code"], INVALID_REQUEST,
            "expected INVALID_REQUEST for job.attach (served over GET /job/attach): {v}"
        );
    }
}
