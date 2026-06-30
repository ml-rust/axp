//! Real-HTTP tests for `axp-client` against `axp_transport::serve`.

use std::time::Duration;

use axp_client::{AttachJobOptions, Client, Error};
use axp_proto::{
    Capability, EnforcementTier, IndexRequest, JobAttachRequest, JobPayload, JobStartRequest,
    JobStartResponse, JobStatusProto, JobStatusRequest, SessionAuditRequest, SessionCloseRequest,
    SessionOpenRequest, SessionOpenResponse,
};

async fn spawn_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axp_transport::serve(listener, axp_transport::AppState::new()).await;
    });
    format!("http://{addr}")
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
