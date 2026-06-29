//! Real-HTTP tests for `axp-client` against `axp_transport::serve`.

use std::time::Duration;

use axp_client::{Client, Error};
use axp_proto::{
    Capability, EnforcementTier, IndexRequest, JobAttachRequest, JobPayload, JobStartRequest,
    JobStatusProto, JobStatusRequest, SessionOpenRequest,
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

    let frames = client
        .attach_job(&JobAttachRequest {
            session_id: session.session_id,
            cap_token: session.cap_token,
            job_id: started.job_id,
            from_offset: 0,
        })
        .await
        .expect("attach");
    let output: Vec<u8> = frames.into_iter().flat_map(|frame| frame.data).collect();
    assert_eq!(String::from_utf8_lossy(&output), "axp-client\n");
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
