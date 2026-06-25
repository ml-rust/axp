//! Real-HTTP end-to-end integration test for `axp-transport`.
//!
//! Boots the AXP server on an ephemeral TCP port, then drives the full
//! session.open → job.start → job.status (poll to terminal) → job/attach (SSE,
//! finite replay) → job.cancel flow with a `reqwest` client over actual TCP.
//!
//! Only the crate's public API is used: `axp_transport::{AppState, serve}`.

use std::time::Duration;

/// Terminal `JobStatusProto` discriminant values; shared by the poll helpers.
const TERMINAL_STATUSES: [&str; 3] = ["exited", "killed", "failed"];

// ── Shared helpers ─────────────────────────────────────────────────────────────

/// Bind an ephemeral port, spawn the AXP server as a background task, and
/// return `(base_url, TempDir)`.  The `TempDir` is returned to keep the
/// temporary workspace path alive for the duration of the test.
async fn spawn_server() -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let state = axp_transport::AppState::new();
    tokio::spawn(async move {
        let _ = axp_transport::serve(listener, state).await;
    });
    (format!("http://{addr}"), dir)
}

/// POST a JSON-RPC 2.0 request to `{base}/` and return the parsed response body.
///
/// Uses a fixed `id` of `1`.  The caller is responsible for asserting the
/// presence/absence of `result` vs `error` fields.
async fn rpc(
    client: &reqwest::Client,
    base: &str,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    client
        .post(format!("{base}/"))
        .json(&body)
        .send()
        .await
        .expect("rpc POST")
        .json::<serde_json::Value>()
        .await
        .expect("rpc JSON decode")
}

/// Parse an SSE response body string: for each `data:` line, deserialize it as
/// a [`axp_proto::LogEventFrame`] and concatenate the raw log bytes.
///
/// Mirrors the helper used in `src/attach.rs` unit tests.  The `data` field of
/// a `LogEventFrame` is a `Vec<u8>` serialized as a JSON number array, so this
/// exercises the real wire contract.
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

/// Open a session, start a short-lived job, poll it to a terminal state, and
/// return `(session_id, job_id)`.  The caller must hold the `TempDir` alive.
async fn started_finished_job(
    client: &reqwest::Client,
    base: &str,
    workspace: &str,
) -> (String, String) {
    // session.open
    let open = rpc(
        client,
        base,
        "session.open",
        serde_json::json!({
            "workspace": workspace,
            "sandbox_tier": "dev-none",
            "capabilities": ["proc.spawn"],
        }),
    )
    .await;
    assert!(
        open.get("error").is_none(),
        "session.open returned error: {open}"
    );
    let sid = open["result"]["session_id"]
        .as_str()
        .expect("session_id string")
        .to_owned();

    // job.start — `printf` for portability; `sh -c` is the engine's shell.
    let start = rpc(
        client,
        base,
        "job.start",
        serde_json::json!({
            "session_id": sid,
            "kind": "command",
            "command": "printf 'alpha\\nbeta\\ngamma\\n'",
        }),
    )
    .await;
    assert!(
        start.get("error").is_none(),
        "job.start returned error: {start}"
    );
    let jid = start["result"]["job_id"]
        .as_str()
        .expect("job_id string")
        .to_owned();

    // Poll job.status until terminal.
    let mut became_terminal = false;
    for _ in 0..100 {
        let status_resp = rpc(
            client,
            base,
            "job.status",
            serde_json::json!({"session_id": sid, "job_id": jid}),
        )
        .await;
        assert!(
            status_resp.get("error").is_none(),
            "job.status returned error: {status_resp}"
        );
        // `JobStatusResponse.status` is a `JobStatusProto` embedded via
        // internal tag: `{"status":"exited","code":0}` etc.
        let discriminant = status_resp["result"]["status"]["status"]
            .as_str()
            .unwrap_or("");
        if TERMINAL_STATUSES.contains(&discriminant) {
            became_terminal = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        became_terminal,
        "job {jid} did not reach a terminal state within the polling timeout"
    );

    (sid, jid)
}

// ── Test 1: full happy-path flow ───────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn end_to_end_session_job_attach_status() {
    let (base, dir) = spawn_server().await;
    let client = reqwest::Client::new();
    let workspace = dir.path().to_str().expect("workspace path utf-8");

    // ── 1. session.open ────────────────────────────────────────────────────────
    let open = rpc(
        &client,
        &base,
        "session.open",
        serde_json::json!({
            "workspace": workspace,
            "sandbox_tier": "dev-none",
            "capabilities": ["proc.spawn"],
        }),
    )
    .await;
    assert!(
        open.get("error").is_none(),
        "session.open returned error: {open}"
    );
    let sid = open["result"]["session_id"]
        .as_str()
        .expect("session_id must be a string");
    assert!(
        sid.starts_with("s_"),
        "session_id must start with s_, got: {sid}"
    );
    // Confirm the other response fields are present (wire-shape check).
    assert!(
        open["result"]["granted_tier"].is_string(),
        "granted_tier must be present: {open}"
    );
    assert!(
        open["result"]["cap_token"].is_string(),
        "cap_token must be present: {open}"
    );

    // ── 2. job.start ───────────────────────────────────────────────────────────
    // `JobStartRequest` flattens `JobPayload` via `#[serde(flatten)]`, so the
    // wire params are: `session_id`, `kind`, and the payload fields at the top
    // level (e.g. `command` for the `Command` variant).
    let start = rpc(
        &client,
        &base,
        "job.start",
        serde_json::json!({
            "session_id": sid,
            "kind": "command",
            "command": "printf 'alpha\\nbeta\\ngamma\\n'",
        }),
    )
    .await;
    assert!(
        start.get("error").is_none(),
        "job.start returned error: {start}"
    );
    let jid = start["result"]["job_id"]
        .as_str()
        .expect("job_id must be a string");
    assert!(
        jid.starts_with("j_"),
        "job_id must start with j_, got: {jid}"
    );

    // ── 3. Poll job.status to terminal ─────────────────────────────────────────
    // `JobStatusResponse` has `{job_id, status: JobStatusProto, seq}`.
    // `JobStatusProto` is internally tagged: `{"status":"exited","code":0}`.
    // Terminal variants: see `TERMINAL_STATUSES`.
    let mut final_status_resp = serde_json::Value::Null;
    let mut became_terminal = false;
    for _ in 0..100 {
        let status_resp = rpc(
            &client,
            &base,
            "job.status",
            serde_json::json!({"session_id": sid, "job_id": jid}),
        )
        .await;
        assert!(
            status_resp.get("error").is_none(),
            "job.status returned error: {status_resp}"
        );
        let discriminant = status_resp["result"]["status"]["status"]
            .as_str()
            .unwrap_or("");
        if TERMINAL_STATUSES.contains(&discriminant) {
            became_terminal = true;
            final_status_resp = status_resp;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        became_terminal,
        "job {jid} did not reach a terminal state within the polling timeout"
    );
    // For a successful `printf` the exit code must be 0.
    let final_status = final_status_resp["result"]["status"]["status"]
        .as_str()
        .unwrap_or("");
    assert_eq!(
        final_status, "exited",
        "expected exited, got: {final_status_resp}"
    );
    let exit_code = final_status_resp["result"]["status"]["code"]
        .as_i64()
        .expect("code must be an integer");
    assert_eq!(exit_code, 0, "expected exit code 0, got: {exit_code}");

    // ── 4. GET /job/attach — finite SSE replay of a terminal job ───────────────
    // The job is already terminal so the SSE stream drains the log buffer and
    // closes.  `.text()` is therefore safe (will not block indefinitely).
    let attach_resp = client
        .get(format!("{base}/job/attach?session_id={sid}&job_id={jid}"))
        .send()
        .await
        .expect("GET /job/attach");
    assert_eq!(attach_resp.status(), 200, "attach must return 200");
    let ctype = attach_resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ctype.contains("text/event-stream"),
        "content-type must be text/event-stream, got: {ctype}"
    );
    let body = attach_resp.text().await.expect("attach body text");
    assert!(
        body.contains("id:"),
        "SSE body must contain id: field: {body}"
    );
    assert!(
        body.contains("data:"),
        "SSE body must contain data: field: {body}"
    );
    // Decode frames and assert the real log payload is present.
    let log_bytes = collected_log_bytes(&body);
    let log_text = String::from_utf8_lossy(&log_bytes);
    assert!(
        log_text.contains("alpha"),
        "decoded log must contain 'alpha', got: {log_text:?} (raw body: {body})"
    );
    assert!(
        log_text.contains("beta"),
        "decoded log must contain 'beta', got: {log_text:?}"
    );
    assert!(
        log_text.contains("gamma"),
        "decoded log must contain 'gamma', got: {log_text:?}"
    );

    // ── 5. job.cancel on an already-finished job ───────────────────────────────
    // A finished job cancel must return `result` (no `error`); `ok` will be
    // `false` because the job is already terminal — that is the documented
    // behavior of `JobCancelResponse`.
    let cancel = rpc(
        &client,
        &base,
        "job.cancel",
        serde_json::json!({"session_id": sid, "job_id": jid}),
    )
    .await;
    assert!(
        cancel.get("error").is_none(),
        "job.cancel on finished job must not return a JSON-RPC error: {cancel}"
    );
    // `result` must be present.
    assert!(
        cancel.get("result").is_some(),
        "job.cancel must return a result object: {cancel}"
    );
}

// ── Test 2: resume past end yields no frames ───────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn attach_resume_past_end_yields_no_frames() {
    let (base, dir) = spawn_server().await;
    let client = reqwest::Client::new();
    let workspace = dir.path().to_str().expect("workspace path utf-8");

    let (sid, jid) = started_finished_job(&client, &base, workspace).await;

    // 2a. from_offset way past the end via query param.
    let resp_qp = client
        .get(format!(
            "{base}/job/attach?session_id={sid}&job_id={jid}&from_offset=1000000"
        ))
        .send()
        .await
        .expect("GET /job/attach (from_offset)");
    assert_eq!(resp_qp.status(), 200);
    let body_qp = resp_qp.text().await.expect("body text (from_offset)");
    assert!(
        collected_log_bytes(&body_qp).is_empty(),
        "resume past last seq via from_offset must yield no frames, got: {body_qp}"
    );

    // 2b. Same offset via `Last-Event-ID` header (with from_offset=0 so the
    //     header override is the only thing suppressing replay).
    let resp_hdr = client
        .get(format!(
            "{base}/job/attach?session_id={sid}&job_id={jid}&from_offset=0"
        ))
        .header("last-event-id", "1000000")
        .send()
        .await
        .expect("GET /job/attach (Last-Event-ID)");
    assert_eq!(resp_hdr.status(), 200);
    let body_hdr = resp_hdr.text().await.expect("body text (Last-Event-ID)");
    assert!(
        collected_log_bytes(&body_hdr).is_empty(),
        "Last-Event-ID must override from_offset and suppress replay, got: {body_hdr}"
    );
}

// ── Test 3: unknown session → JSON-RPC NOT_FOUND (-32001) ─────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn unknown_session_index_returns_jsonrpc_not_found() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    // `axp.index` validates the session id first; an unknown id → NOT_FOUND.
    let resp = rpc(
        &client,
        &base,
        "axp.index",
        serde_json::json!({"session_id": "s_nope"}),
    )
    .await;

    // Must have an `error` field, not a `result`.
    assert!(
        resp.get("error").is_some(),
        "expected JSON-RPC error for unknown session, got: {resp}"
    );
    assert!(
        resp.get("result").is_none(),
        "must not have result when error is present: {resp}"
    );
    let code = resp["error"]["code"]
        .as_i64()
        .expect("error.code must be an integer");
    assert_eq!(
        code,
        axp_transport::NOT_FOUND,
        "expected NOT_FOUND (-32001), got code: {code} in response: {resp}"
    );
}
