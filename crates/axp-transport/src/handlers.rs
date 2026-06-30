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

/// Unified authentication gate for every authenticated method.
///
/// Returns `Ok(())` only when `id` names a known session AND `presented` matches
/// that session's capability token. Both the unknown-session case and the
/// wrong-token case return the SAME [`TransportError::Unauthorized`] with the
/// same generic message, so an attacker cannot use the error to learn whether a
/// given session id exists (no existence oracle). The underlying
/// [`SessionStore::authorize`](axp_core::SessionStore::authorize) comparison is
/// constant-time.
fn require_authorized_session(
    state: &AppState,
    id: &SessionId,
    presented: &str,
) -> Result<(), TransportError> {
    if !state.sessions.authorize(id, presented) {
        return Err(TransportError::Unauthorized);
    }
    Ok(())
}

fn authenticated_session_capabilities(
    state: &AppState,
    id: &SessionId,
    presented: &str,
) -> Result<CapabilitySet, TransportError> {
    require_authorized_session(state, id, presented)?;
    let session = state.sessions.get(id).ok_or(TransportError::Unauthorized)?;
    let capabilities = session
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .capabilities
        .clone();
    Ok(capabilities)
}

// ── Method handlers ────────────────────────────────────────────────────────────

/// Handle `session.open`: open an isolated workspace session.
///
/// Parses the workspace path and capability grants from the request, mints a
/// new session id and a fresh sparse-capability [`CapToken`](axp_core::CapToken),
/// stores the token on the session, and returns a [`SessionOpenResponse`]
/// carrying the raw token, the assigned id, and the granted tier.
///
/// The `cap_token` is the unforgeable credential the client must present on
/// subsequent calls; the `session_id` remains a non-secret addressing handle.
///
/// # Note
///
/// This handler mints, stores, and returns the token, but does **not** yet
/// enforce it on subsequent RPC calls — that enforcement is a follow-up unit.
pub(crate) async fn session_open(
    state: &AppState,
    params: serde_json::Value,
) -> Result<serde_json::Value, TransportError> {
    let req = parse_params::<SessionOpenRequest>(params)?;
    let workspace = Workspace::new(&req.workspace)?;
    let caps = CapabilitySet::from_wire(&req.capabilities)?;
    let id = state.next_session_id();
    // Mint a high-entropy sparse capability; `?` maps axp_core::Error::Entropy
    // into TransportError via the `Runtime` `#[from]` conversion.
    let cap_token = axp_core::CapToken::generate()?;
    let resp = SessionOpenResponse {
        cap_token: cap_token.expose().to_owned(),
        session_id: id.clone(),
        granted_tier: req.sandbox_tier,
    };
    state
        .sessions
        .open(id, workspace, req.sandbox_tier, caps, cap_token);
    to_value(&resp)
}

/// Handle `axp.index`: return the full capability catalog for a session.
///
/// The caller is authenticated first (unknown session or invalid capability
/// token → `UNAUTHORIZED`, indistinguishably). The returned catalog is scoped
/// to the session grants.
pub(crate) async fn index(
    state: &AppState,
    params: serde_json::Value,
) -> Result<serde_json::Value, TransportError> {
    let req = parse_params::<IndexRequest>(params)?;
    let capabilities = authenticated_session_capabilities(state, &req.session_id, &req.cap_token)?;
    let reg = state.registry.read().unwrap_or_else(|p| p.into_inner());
    let resp = reg.index_for_capabilities(&capabilities)?;
    to_value(&resp)
}

/// Handle `axp.describe`: return full detail for one capability by name.
///
/// The caller is authenticated first (unknown session or invalid capability
/// token → `UNAUTHORIZED`, indistinguishably). The capability name is then
/// resolved within the session-scoped catalog (unknown or ungranted name →
/// `NOT_FOUND`).
pub(crate) async fn describe(
    state: &AppState,
    params: serde_json::Value,
) -> Result<serde_json::Value, TransportError> {
    let req = parse_params::<DescribeRequest>(params)?;
    let capabilities = authenticated_session_capabilities(state, &req.session_id, &req.cap_token)?;
    let reg = state.registry.read().unwrap_or_else(|p| p.into_inner());
    let detail = reg.describe_for_capabilities(&req.name, &capabilities)?;
    to_value(&detail)
}

/// Handle `job.start`: start a new job in an existing session.
///
/// The caller is authenticated first (unknown session or invalid capability
/// token → `UNAUTHORIZED`, indistinguishably) BEFORE any engine delegation.
/// On success, delegates to [`JobEngine::start`]; the engine performs all
/// capability/cwd validation.
pub(crate) async fn job_start(
    state: &AppState,
    params: serde_json::Value,
) -> Result<serde_json::Value, TransportError> {
    let req = parse_params::<JobStartRequest>(params)?;
    require_authorized_session(state, &req.session_id, &req.cap_token)?;
    let job_id = state.engine.start(&req).await?;
    to_value(&axp_proto::JobStartResponse { job_id })
}

/// Handle `job.status`: return the current status of a job.
///
/// The caller is authenticated first (unknown session or invalid capability
/// token → `UNAUTHORIZED`, indistinguishably) BEFORE any engine delegation.
/// Delegates to [`JobEngine::status`]; returns `NOT_FOUND` if the job is
/// unknown or not owned by the specified session.
pub(crate) async fn job_status(
    state: &AppState,
    params: serde_json::Value,
) -> Result<serde_json::Value, TransportError> {
    let req = parse_params::<JobStatusRequest>(params)?;
    require_authorized_session(state, &req.session_id, &req.cap_token)?;
    let resp = state.engine.status(&req)?;
    to_value(&resp)
}

/// Handle `job.cancel`: cancel a running job.
///
/// The caller is authenticated first (unknown session or invalid capability
/// token → `UNAUTHORIZED`, indistinguishably) BEFORE any engine delegation.
/// Delegates to [`JobEngine::cancel`]; returns `NOT_FOUND` if the job is
/// unknown or not owned by the specified session.  Returns `ok: false` (not an
/// error) when the job has already finished.
pub(crate) async fn job_cancel(
    state: &AppState,
    params: serde_json::Value,
) -> Result<serde_json::Value, TransportError> {
    let req = parse_params::<JobCancelRequest>(params)?;
    require_authorized_session(state, &req.session_id, &req.cap_token)?;
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
            NOT_FOUND, UNAUTHORIZED,
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

    /// Open a session over a real tempdir and return `(session_id, cap_token)`.
    /// Keep the caller's `dir` alive for the duration of the test so the path
    /// remains valid. The returned `cap_token` is the credential every
    /// authenticated call must present.
    async fn open_session(state: &AppState, dir: &tempfile::TempDir) -> (SessionId, String) {
        open_session_with_capabilities(state, dir, &["proc.spawn"]).await
    }

    async fn open_session_with_capabilities(
        state: &AppState,
        dir: &tempfile::TempDir,
        capabilities: &[&str],
    ) -> (SessionId, String) {
        let ws = dir.path().to_string_lossy().into_owned();
        let v = call(
            state,
            "session.open",
            json!({
                "workspace": ws,
                "sandbox_tier": "dev-none",
                "capabilities": capabilities
            }),
        )
        .await;
        let id_str = v["result"]["session_id"]
            .as_str()
            .expect("session_id must be a string")
            .to_owned();
        let token = v["result"]["cap_token"]
            .as_str()
            .expect("cap_token must be a string")
            .to_owned();
        (SessionId(id_str), token)
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
        let (sid, token) = open_session(&state, &dir).await;

        let v = call(
            &state,
            "axp.index",
            json!({ "session_id": sid.0, "cap_token": token }),
        )
        .await;
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
    async fn index_unknown_session_returns_unauthorized() {
        // Unknown session at an authenticated endpoint is deliberately
        // indistinguishable from a bad token: both yield UNAUTHORIZED (no
        // existence oracle).
        let state = AppState::new();
        let v = call(
            &state,
            "axp.index",
            json!({ "session_id": "s_unknown", "cap_token": "ct_whatever" }),
        )
        .await;
        assert_eq!(
            v["error"]["code"], UNAUTHORIZED,
            "expected UNAUTHORIZED for unknown session: {v}"
        );
    }

    #[tokio::test]
    async fn index_tool_grant_returns_only_matching_capability() {
        let state = AppState::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let (sid, token) = open_session_with_capabilities(&state, &dir, &["tool:git_diff"]).await;

        let v = call(
            &state,
            "axp.index",
            json!({ "session_id": sid.0, "cap_token": token }),
        )
        .await;
        let entries = v["result"]["entries"]
            .as_array()
            .expect("entries must be an array");
        let names: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();
        assert_eq!(
            names,
            vec!["git_diff"],
            "unexpected index entries: {names:?}"
        );
        let _dir = dir; // keep alive
    }

    // ── axp.describe ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn describe_unknown_capability_returns_not_found() {
        let state = AppState::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let (sid, token) = open_session(&state, &dir).await;

        let v = call(
            &state,
            "axp.describe",
            json!({ "session_id": sid.0, "cap_token": token, "name": "nonexistent_cap" }),
        )
        .await;
        assert_eq!(
            v["error"]["code"], NOT_FOUND,
            "expected NOT_FOUND for unknown capability: {v}"
        );
        let _dir = dir; // keep alive
    }

    #[tokio::test]
    async fn describe_bad_token_returns_unauthorized() {
        let state = AppState::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let (sid, _token) = open_session(&state, &dir).await;

        let v = call(
            &state,
            "axp.describe",
            json!({ "session_id": sid.0, "cap_token": "ct_wrong", "name": "git_diff" }),
        )
        .await;
        assert_eq!(
            v["error"]["code"], UNAUTHORIZED,
            "expected UNAUTHORIZED for bad token: {v}"
        );
        let _dir = dir; // keep alive
    }

    #[tokio::test]
    async fn describe_tool_grant_returns_matching_capability_detail() {
        let state = AppState::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let (sid, token) = open_session_with_capabilities(&state, &dir, &["tool:git_diff"]).await;

        let v = call(
            &state,
            "axp.describe",
            json!({ "session_id": sid.0, "cap_token": token, "name": "git_diff" }),
        )
        .await;
        assert_eq!(
            v["result"]["signature"], "git_diff(): string",
            "expected git_diff detail: {v}"
        );
        let _dir = dir; // keep alive
    }

    #[tokio::test]
    async fn describe_tool_grant_hides_other_capabilities() {
        let state = AppState::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let (sid, token) = open_session_with_capabilities(&state, &dir, &["tool:git_diff"]).await;

        let v = call(
            &state,
            "axp.describe",
            json!({ "session_id": sid.0, "cap_token": token, "name": "git_log" }),
        )
        .await;
        assert_eq!(
            v["error"]["code"], NOT_FOUND,
            "expected NOT_FOUND for ungranted capability: {v}"
        );
        let _dir = dir; // keep alive
    }

    // ── job.start ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn job_start_returns_job_id() {
        let state = AppState::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let (sid, token) = open_session(&state, &dir).await;

        let v = call(
            &state,
            "job.start",
            json!({
                "session_id": sid.0,
                "cap_token": token,
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
