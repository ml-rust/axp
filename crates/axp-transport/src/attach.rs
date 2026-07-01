//! Resumable Server-Sent-Events (SSE) handler for `job.attach`.
//!
//! The JSON-RPC `POST /` endpoint cannot stream, so attach is served over a
//! dedicated `GET /job/attach` route that emits the job's log frames as an SSE
//! event stream (`text/event-stream`).
//!
//! Resumption follows standard SSE semantics: each event's `id:` carries the
//! frame's monotonic sequence number, and a reconnecting client may send a
//! `Last-Event-ID` header (or the `from_offset` query param) to resume from a
//! given sequence number. `Last-Event-ID` takes precedence over `from_offset`.

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use serde::Deserialize;

use crate::{
    TransportError,
    jsonrpc::{DENIED, INVALID_PARAMS, NOT_FOUND, UNAUTHORIZED},
    state::AppState,
};

// ── Send-safety assertion ──────────────────────────────────────────────────────

// The `JobLogStream` is moved into the SSE body stream and driven on axum's
// multi-thread runtime, so it MUST be `Send`. This compile-time check fails the
// build loudly if that invariant is ever broken upstream.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<axp_core::JobLogStream>();
};

// ── Query params ────────────────────────────────────────────────────────────────

/// Query parameters for `GET /job/attach`.
///
/// `from_offset` is the fallback resume point; the `Last-Event-ID` header, when
/// present and parseable, overrides it.
#[derive(Debug, Deserialize)]
pub(crate) struct AttachParams {
    /// Session that owns the job (must match the job's owner).
    session_id: String,
    /// Opaque capability token proving authority over the session (from
    /// session.open). REQUIRED: a missing `cap_token` query param makes the
    /// `Query` extractor reject the request with HTTP 400.
    cap_token: String,
    /// Job to attach to.
    job_id: String,
    /// Resume from this sequence number. Defaults to `0` (from the beginning).
    #[serde(default)]
    from_offset: u64,
}

// ── Error type ──────────────────────────────────────────────────────────────────

/// Error wrapper for the SSE GET endpoint.
///
/// Unlike the JSON-RPC handlers (which return protocol errors in-body with HTTP
/// 200), an HTTP streaming endpoint should signal failure with a real HTTP
/// status. This wraps a [`TransportError`], maps its JSON-RPC code to an HTTP
/// status, and renders an `{ "error": { code, message } }` JSON body.
pub(crate) struct AttachError(TransportError);

impl From<axp_core::Error> for AttachError {
    fn from(e: axp_core::Error) -> Self {
        AttachError(TransportError::Runtime(e))
    }
}

impl From<TransportError> for AttachError {
    fn from(e: TransportError) -> Self {
        AttachError(e)
    }
}

impl axum::response::IntoResponse for AttachError {
    fn into_response(self) -> Response {
        let err = self.0.to_jsonrpc_error();
        let status = match err.code {
            NOT_FOUND => StatusCode::NOT_FOUND,
            DENIED => StatusCode::FORBIDDEN,
            UNAUTHORIZED => StatusCode::UNAUTHORIZED,
            INVALID_PARAMS => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (
            status,
            axum::Json(serde_json::json!({
                "error": { "code": err.code, "message": err.message }
            })),
        )
            .into_response()
    }
}

// ── Handler ─────────────────────────────────────────────────────────────────────

/// Axum handler for `GET /job/attach`.
///
/// Builds the [`axp_core::JobLogStream`] for the requested job and streams its
/// frames as SSE. The resume offset is taken from the `Last-Event-ID` header if
/// present and parseable, otherwise from the `from_offset` query param.
///
/// Each SSE event sets `id:` to the frame's `seq` and `data:` to the frame's
/// JSON encoding. The stream ends naturally once the job is terminal and its log
/// buffer is fully drained, so attaching to an already-finished job replays the
/// buffer and then closes.
///
/// Returns [`AttachError`] (mapped to an HTTP status) when the job is unknown or
/// not owned by the session.
pub(crate) async fn attach_sse(
    State(state): State<AppState>,
    Query(params): Query<AttachParams>,
    headers: HeaderMap,
) -> Result<Response, AttachError> {
    // Build the typed request first so `session_id` can be borrowed for the
    // auth check and then moved into the engine call — avoids an extra clone.
    let from_offset = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(params.from_offset);

    let req = axp_proto::JobAttachRequest {
        session_id: axp_proto::SessionId(params.session_id),
        cap_token: params.cap_token,
        job_id: axp_proto::JobId(params.job_id),
        from_offset,
    };

    // Authenticate BEFORE touching the engine. The capability token arrives as
    // a query param (not a header) because the browser `EventSource` API cannot
    // set custom headers, so a query-param token is the standard auth mechanism
    // for SSE. Unknown session and bad token are rejected identically
    // (UNAUTHORIZED) — no existence oracle.
    if !state.sessions.authorize(&req.session_id, &req.cap_token) {
        return Err(AttachError::from(TransportError::Unauthorized));
    }

    let mut stream = state.engine.attach(&req)?;

    let body = async_stream::stream! {
        while let Some(frame) = stream.next().await {
            // A frame that fails to serialize is dropped rather than tearing down
            // the stream; LogEventFrame is plain data, so this is effectively
            // unreachable.
            if let Ok(data) = serde_json::to_string(&frame) {
                yield Ok::<Event, std::convert::Infallible>(
                    Event::default().id(frame.seq.to_string()).data(data),
                );
            }
        }
    };

    Ok(Sse::new(body)
        .keep_alive(KeepAlive::default())
        .into_response())
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axp_core::{CapToken, CapabilitySet, RuntimeCapability, Workspace};
    use axp_proto::{EnforcementTier, JobId, JobPayload, JobStartRequest, SessionId};
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use axum::response::Response;
    use tower::ServiceExt;

    use crate::{router::build_router, state::AppState};

    /// Poll a job to a terminal state so the SSE stream is finite (bounded ~5s).
    async fn poll_terminal(state: &AppState, id: &JobId) {
        for _ in 0..500 {
            if let Some(handle) = state.engine.jobs().get(id) {
                let terminal = handle
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .status
                    .is_terminal();
                if terminal {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job did not reach a terminal state within timeout");
    }

    /// Build a `dev-none` session with `proc.spawn` over a tempdir, start an
    /// `echo hello` job, poll it to terminal, and return
    /// `(state, sid, token, jid, dir)`. The `token` is the capability token the
    /// session was opened with — the same value every authenticated call (and
    /// the SSE attach endpoint) must present.
    async fn finished_job() -> (AppState, SessionId, String, JobId, tempfile::TempDir) {
        let state = AppState::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = Workspace::new(dir.path()).expect("workspace");
        let sid = SessionId("s_attach".into());
        // Mint the token here and keep a copy of its exposed string BEFORE moving
        // the `CapToken` into `open()`, so the test can present the matching token.
        let cap_token = CapToken::generate().expect("entropy");
        let token = cap_token.expose().to_owned();
        state.sessions.open(
            sid.clone(),
            ws,
            EnforcementTier::DevNone,
            CapabilitySet::new(vec![RuntimeCapability::ProcSpawn]),
            cap_token,
        );
        let req = JobStartRequest {
            session_id: sid.clone(),
            cap_token: token.clone(),
            payload: JobPayload::Command {
                command: "echo hello".into(),
            },
            cwd: None,
            capabilities: vec![],
        };
        let jid = state.engine.start(&req).await.expect("start");
        poll_terminal(&state, &jid).await;
        (state, sid, token, jid, dir)
    }

    /// Parse an SSE response body, decode each `data:` line back into a
    /// `LogEventFrame`, and concatenate the raw log bytes. This validates the
    /// real wire contract (frame `data` is a `Vec<u8>` serialized as a JSON
    /// number array), not an incidental text substring.
    fn collected_log_bytes(body: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for line in body.lines() {
            if let Some(json) = line.strip_prefix("data:") {
                if let Ok(frame) = serde_json::from_str::<axp_proto::LogEventFrame>(json.trim()) {
                    out.extend_from_slice(&frame.data);
                }
            }
        }
        out
    }

    fn assert_non_sse_response(resp: &Response) {
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            !ctype.contains("text/event-stream"),
            "expected a non-SSE response, got content-type: {ctype}"
        );
    }

    fn assert_bad_request_without_sse(resp: &Response, context: &str) {
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "{context} must return HTTP 400"
        );
        assert_non_sse_response(resp);
    }

    #[tokio::test]
    async fn attach_streams_terminal_job_as_sse() {
        let (state, sid, token, jid, _dir) = finished_job().await;
        let router = build_router(state);
        let uri = format!(
            "/job/attach?session_id={}&cap_token={}&job_id={}",
            sid.0, token, jid.0
        );
        let resp = router
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(resp.status(), StatusCode::OK);
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ctype.contains("text/event-stream"),
            "expected SSE content-type, got: {ctype}"
        );

        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("id:"), "expected SSE id field: {body}");
        assert!(body.contains("data:"), "expected SSE data field: {body}");
        // Decode the frames and assert the real log payload round-trips. `echo
        // hello` produces "hello\n".
        let payload = collected_log_bytes(&body);
        assert!(
            payload.windows(5).any(|w| w == b"hello"),
            "expected decoded log payload to contain `hello`, got bytes {payload:?} from body: {body}"
        );
    }

    #[tokio::test]
    async fn attach_resume_past_last_seq_omits_earlier_frames() {
        let (state, sid, token, jid, _dir) = finished_job().await;
        let router = build_router(state);
        // Resume from a very high offset: every existing frame is before it, so
        // the replayed body must not contain the `hello` payload.
        let uri = format!(
            "/job/attach?session_id={}&cap_token={}&job_id={}&from_offset=1000000",
            sid.0, token, jid.0
        );
        let resp = router
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        let body = String::from_utf8_lossy(&bytes);
        // No frames at or after the (huge) offset, so no log bytes replay.
        assert!(
            collected_log_bytes(&body).is_empty(),
            "resume past last seq must not replay earlier frames: {body}"
        );
    }

    #[tokio::test]
    async fn attach_last_event_id_header_overrides_from_offset() {
        let (state, sid, token, jid, _dir) = finished_job().await;
        let router = build_router(state);
        // from_offset=0 would replay everything, but Last-Event-ID past the end
        // must win and suppress the replay.
        let uri = format!(
            "/job/attach?session_id={}&cap_token={}&job_id={}&from_offset=0",
            sid.0, token, jid.0
        );
        let resp = router
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("last-event-id", "1000000")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        let body = String::from_utf8_lossy(&bytes);
        // Header offset is past the end, so despite from_offset=0 nothing replays.
        assert!(
            collected_log_bytes(&body).is_empty(),
            "Last-Event-ID must override from_offset and suppress replay: {body}"
        );
    }

    #[tokio::test]
    async fn attach_malformed_last_event_id_falls_back_to_from_offset() {
        let (state, sid, token, jid, _dir) = finished_job().await;
        let router = build_router(state);
        let uri = format!(
            "/job/attach?session_id={}&cap_token={}&job_id={}&from_offset=0",
            sid.0, token, jid.0
        );
        let resp = router
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("last-event-id", "not-a-number")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        let body = String::from_utf8_lossy(&bytes);
        let payload = collected_log_bytes(&body);
        assert!(
            payload.windows(5).any(|w| w == b"hello"),
            "malformed Last-Event-ID must fall back to from_offset and replay `hello`: {body}"
        );
    }

    #[tokio::test]
    async fn attach_unknown_job_returns_not_found() {
        let (state, sid, token, _jid, _dir) = finished_job().await;
        let router = build_router(state);
        let uri = format!(
            "/job/attach?session_id={}&cap_token={}&job_id=j_nope",
            sid.0, token
        );
        let resp = router
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn attach_missing_cap_token_returns_bad_request_without_sse() {
        let (state, sid, _token, jid, _dir) = finished_job().await;
        let router = build_router(state);
        let uri = format!("/job/attach?session_id={}&job_id={}", sid.0, jid.0);
        let resp = router
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_bad_request_without_sse(&resp, "missing cap_token");
    }

    #[tokio::test]
    async fn attach_malformed_from_offset_returns_bad_request_without_sse() {
        let (state, sid, token, jid, _dir) = finished_job().await;
        let router = build_router(state);
        let uri = format!(
            "/job/attach?session_id={}&cap_token={}&job_id={}&from_offset=not-a-number",
            sid.0, token, jid.0
        );
        let resp = router
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_bad_request_without_sse(&resp, "malformed from_offset");
    }

    #[tokio::test]
    async fn attach_missing_required_query_fields_returns_bad_request_without_sse() {
        let (state, sid, token, jid, _dir) = finished_job().await;
        let router = build_router(state);
        let cases = [
            (
                "missing session_id",
                format!("/job/attach?cap_token={}&job_id={}", token, jid.0),
            ),
            (
                "missing job_id",
                format!("/job/attach?session_id={}&cap_token={}", sid.0, token),
            ),
        ];

        for (case, uri) in cases {
            let resp = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");

            assert_bad_request_without_sse(&resp, case);
        }
    }

    #[tokio::test]
    async fn attach_bad_token_does_not_stream_logs() {
        // A valid session+job but a WRONG cap_token must be rejected before the
        // log stream is built: the response must not be a 200 SSE stream.
        let (state, sid, _token, jid, _dir) = finished_job().await;
        let router = build_router(state);
        let uri = format!(
            "/job/attach?session_id={}&cap_token=ct_wrong_token&job_id={}",
            sid.0, jid.0
        );
        let resp = router
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_ne!(
            resp.status(),
            StatusCode::OK,
            "bad token must not yield a 200 log stream"
        );
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "bad token must map to HTTP 401 Unauthorized"
        );
        assert_non_sse_response(&resp);
    }
}
