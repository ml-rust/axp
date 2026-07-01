//! Real-HTTP tests for `axp-client` against `axp_transport::serve`.

use std::time::Duration;

use axp_client::{AttachJobOptions, Client, Error};
use axp_proto::{
    Capability, DescribeRequest, EnforcementTier, IndexRequest, JobAttachRequest, JobId,
    JobPayload, JobStartRequest, JobStartResponse, JobStatusProto, JobStatusRequest,
    SessionAuditRequest, SessionCloseRequest, SessionId, SessionOpenRequest, SessionOpenResponse,
};

const MCP_PROVIDER: &str = "mcp_docs";
const MCP_BRIDGE: &str = "axp-mcp-bridge";
const MCP_TOOL: &str = "mcp_search";
const MCP_DESC: &str = "Search documentation with an external MCP bridge";

async fn spawn_server() -> String {
    spawn_server_with_state(axp_transport::AppState::new()).await
}

async fn spawn_server_with_state(state: axp_transport::AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axp_transport::serve(listener, state).await;
    });
    format!("http://{addr}")
}

fn mcp_search_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": {"type": "string"}
        },
        "required": ["query"],
        "additionalProperties": false
    })
}

fn mcp_search_state(schema: serde_json::Value) -> axp_transport::AppState {
    axp_transport::AppState::with_mcp_tools(
        MCP_PROVIDER.to_owned(),
        MCP_BRIDGE.to_owned(),
        vec!["call".to_owned()],
        vec![(MCP_TOOL.to_owned(), MCP_DESC.to_owned(), schema)],
    )
    .expect("MCP tool state")
}

fn is_terminal(status: &JobStatusProto) -> bool {
    matches!(
        status,
        JobStatusProto::Exited { .. } | JobStatusProto::Killed | JobStatusProto::Failed { .. }
    )
}

async fn wait_for_terminal_job(
    client: &Client,
    session: &SessionOpenResponse,
    started: &JobStartResponse,
) {
    let mut terminal = None;
    for _ in 0..100 {
        let status = client
            .job_status(&JobStatusRequest {
                session_id: session.session_id.clone(),
                cap_token: session.cap_token.clone(),
                job_id: started.job_id.clone(),
            })
            .await
            .expect("job status");
        if is_terminal(&status.status) {
            terminal = Some(status);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(terminal.is_some(), "job did not reach a terminal state");
}

struct FinishedCommandJob {
    client: Client,
    session: SessionOpenResponse,
    started: JobStartResponse,
    _workspace: tempfile::TempDir,
}

async fn finished_command_job(command: &str) -> FinishedCommandJob {
    let base = spawn_server().await;
    let client = Client::new(&base).expect("client");
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path().to_string_lossy().into_owned();

    let session = client
        .open_session(&SessionOpenRequest {
            workspace,
            sandbox_tier: EnforcementTier::DevNone,
            capabilities: vec![Capability("proc.spawn".to_owned())],
        })
        .await
        .expect("open session");

    let started = client
        .start_job(&JobStartRequest {
            session_id: session.session_id.clone(),
            cap_token: session.cap_token.clone(),
            payload: JobPayload::Command {
                command: command.to_owned(),
            },
            cwd: None,
            capabilities: Vec::new(),
        })
        .await
        .expect("start job");

    wait_for_terminal_job(&client, &session, &started).await;
    FinishedCommandJob {
        client,
        session,
        started,
        _workspace: dir,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn client_drives_session_job_and_attach_over_real_http() {
    let base = spawn_server().await;
    let client = Client::new(&base).expect("client");
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path().to_string_lossy().into_owned();

    let session = client
        .open_session(&SessionOpenRequest {
            workspace,
            sandbox_tier: EnforcementTier::DevNone,
            capabilities: vec![Capability("proc.spawn".to_owned())],
        })
        .await
        .expect("open session");

    let index = client
        .index(&IndexRequest {
            session_id: session.session_id.clone(),
            cap_token: session.cap_token.clone(),
        })
        .await
        .expect("index");
    assert!(
        index.entries.iter().any(|entry| entry.name == "git_diff"),
        "expected git_diff in index"
    );

    let started = client
        .start_job(&JobStartRequest {
            session_id: session.session_id.clone(),
            cap_token: session.cap_token.clone(),
            payload: JobPayload::Command {
                command: "printf 'axp-client\\n'".to_owned(),
            },
            cwd: None,
            capabilities: Vec::new(),
        })
        .await
        .expect("start job");

    wait_for_terminal_job(&client, &session, &started).await;

    let frames = client
        .attach_job(&JobAttachRequest {
            session_id: session.session_id.clone(),
            cap_token: session.cap_token.clone(),
            job_id: started.job_id.clone(),
            from_offset: 0,
        })
        .await
        .expect("attach");
    let output: Vec<u8> = frames.into_iter().flat_map(|frame| frame.data).collect();
    assert_eq!(String::from_utf8_lossy(&output), "axp-client\n");

    let audit = client
        .session_audit(&SessionAuditRequest {
            session_id: session.session_id.clone(),
            cap_token: session.cap_token.clone(),
        })
        .await
        .expect("session audit");
    assert!(
        audit
            .events
            .iter()
            .any(|event| matches!(event.kind, axp_proto::SessionAuditEventKind::SessionOpened)),
        "expected session_opened audit event"
    );
    assert!(
        audit.events.iter().any(|event| matches!(
            &event.kind,
            axp_proto::SessionAuditEventKind::JobFinished { job_id, .. } if job_id == &started.job_id
        )),
        "expected job_finished audit event"
    );

    let closed = client
        .close_session(&SessionCloseRequest {
            session_id: session.session_id.clone(),
            cap_token: session.cap_token.clone(),
        })
        .await
        .expect("close session");
    assert!(closed.ok);

    let err = client
        .index(&IndexRequest {
            session_id: session.session_id,
            cap_token: session.cap_token,
        })
        .await
        .expect_err("closed session should not authorize");
    assert!(matches!(err, Error::Rpc { code: -32004, .. }));
}

#[tokio::test(flavor = "multi_thread")]
async fn client_discovers_and_describes_mcp_mounted_tool_over_real_http() {
    let schema = mcp_search_schema();
    let base = spawn_server_with_state(mcp_search_state(schema.clone())).await;
    let client = Client::new(&base).expect("client");
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path().to_string_lossy().into_owned();

    let session = client
        .open_session(&SessionOpenRequest {
            workspace,
            sandbox_tier: EnforcementTier::DevNone,
            capabilities: vec![Capability(format!("tool:{MCP_TOOL}"))],
        })
        .await
        .expect("open session");

    let index = client
        .index(&IndexRequest {
            session_id: session.session_id.clone(),
            cap_token: session.cap_token.clone(),
        })
        .await
        .expect("index");
    assert!(
        index
            .entries
            .iter()
            .any(|entry| entry.name == MCP_TOOL && entry.desc == MCP_DESC),
        "expected mounted MCP tool in index, got {:?}",
        index.entries
    );

    let detail = client
        .describe(&DescribeRequest {
            session_id: session.session_id.clone(),
            cap_token: session.cap_token.clone(),
            name: MCP_TOOL.to_owned(),
        })
        .await
        .expect("describe");
    assert_eq!(detail.signature, "mcp_search(input: object): string");
    assert_eq!(detail.schema, schema);

    let closed = client
        .close_session(&SessionCloseRequest {
            session_id: session.session_id,
            cap_token: session.cap_token,
        })
        .await
        .expect("close session");
    assert!(closed.ok);
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_job_preserves_finite_replay_from_start_cursor() {
    let fixture = finished_command_job("printf 'finite-replay\\n'").await;

    let frames = fixture
        .client
        .attach_job(&JobAttachRequest {
            session_id: fixture.session.session_id,
            cap_token: fixture.session.cap_token,
            job_id: fixture.started.job_id,
            from_offset: 0,
        })
        .await
        .expect("attach");

    let output: Vec<u8> = frames.into_iter().flat_map(|frame| frame.data).collect();
    assert_eq!(String::from_utf8_lossy(&output), "finite-replay\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_stream_uses_last_event_id_cursor() {
    let fixture = finished_command_job("printf 'cursor-replay\\n'").await;

    let mut stream = fixture
        .client
        .attach_job_stream_with_options(
            &JobAttachRequest {
                session_id: fixture.session.session_id,
                cap_token: fixture.session.cap_token,
                job_id: fixture.started.job_id,
                from_offset: 0,
            },
            AttachJobOptions::new().with_last_event_id(1_000_000),
        )
        .await
        .expect("attach stream");

    let frame = stream.next_frame().await.expect("next frame");
    assert!(
        frame.is_none(),
        "last event id should suppress replayed frames"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_job_decodes_rpc_error_response() {
    let fixture = finished_command_job("printf 'attach-error\\n'").await;

    let err = fixture
        .client
        .attach_job(&JobAttachRequest {
            session_id: fixture.session.session_id,
            cap_token: "ct_wrong".to_owned(),
            job_id: fixture.started.job_id,
            from_offset: 0,
        })
        .await
        .expect_err("structured attach should return an RPC error");

    let Error::Rpc { code, .. } = err else {
        panic!("structured attach error should decode to Error::Rpc, got {err:?}");
    };
    assert_eq!(code, -32004);
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_job_returns_http_status_for_non_rpc_not_found_response() {
    let base = spawn_server().await;
    let client = Client::new(format!("{base}/missing")).expect("client");

    let err = client
        .attach_job(&JobAttachRequest {
            session_id: SessionId("s_dummy".to_owned()),
            cap_token: "ct_dummy".to_owned(),
            job_id: JobId("j_dummy".to_owned()),
            from_offset: 0,
        })
        .await
        .expect_err("non-RPC 404 response");

    assert!(matches!(err, Error::HttpStatus(404)));
}

#[tokio::test(flavor = "multi_thread")]
async fn rpc_error_response_is_explicit() {
    let base = spawn_server().await;
    let client = Client::new(&base).expect("client");
    let err = client
        .index(&IndexRequest {
            session_id: axp_proto::SessionId("s_missing".to_owned()),
            cap_token: "ct_missing".to_owned(),
        })
        .await
        .expect_err("rpc error");
    assert!(matches!(err, Error::Rpc { code: -32004, .. }));
}
