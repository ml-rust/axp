use std::{net::TcpListener, time::Duration};

use axp_client::Client;
use axp_proto::{
    Capability, DescribeRequest, EnforcementTier, IndexRequest, SessionCloseRequest,
    SessionOpenRequest, SessionOpenResponse,
};
use tokio::{process::Child, time::timeout};

const MCP_DESC: &str = "Search documentation with an external MCP bridge";
const MCP_LOOKUP_DESC: &str = "Lookup documentation pages with an external MCP bridge";

struct ServerProcess {
    child: Child,
}

impl ServerProcess {
    async fn shutdown(mut self) {
        let _ = self.child.start_kill();
        let _ = timeout(Duration::from_secs(2), self.child.wait()).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_mcp_mount_is_visible_in_runtime_discovery() {
    let (mut server, base) = spawn_axp_serve_with_mcp_mount().await;
    let client = Client::new(&base).expect("client");
    let workspace = tempfile::tempdir().expect("tempdir");

    let session = open_session_when_ready(&client, &mut server.child, &workspace).await;

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
            .any(|entry| entry.name == "search" && entry.desc == MCP_DESC),
        "expected mounted MCP tool in index, got {:?}",
        index.entries
    );

    let detail = client
        .describe(&DescribeRequest {
            session_id: session.session_id.clone(),
            cap_token: session.cap_token.clone(),
            name: "search".to_owned(),
        })
        .await
        .expect("describe mounted MCP tool");
    assert_eq!(detail.signature, "search(input: object): string");
    assert_eq!(
        detail.schema,
        serde_json::json!({
            "type": "object",
            "additionalProperties": true
        })
    );

    let closed = client
        .close_session(&SessionCloseRequest {
            session_id: session.session_id,
            cap_token: session.cap_token,
        })
        .await
        .expect("close session");
    assert!(closed.ok);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_mcp_config_tools_are_visible_in_runtime_discovery() {
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_path = config_dir.path().join("mcp.json");
    std::fs::write(
        &config_path,
        format!(
            r#"{{
                "provider": "docs",
                "bridge": {{
                    "program": "axp-mcp-bridge",
                    "args": ["call"]
                }},
                "tools": [
                    {{
                        "name": "search",
                        "desc": "{MCP_DESC}"
                    }},
                    {{
                        "name": "lookup",
                        "desc": "{MCP_LOOKUP_DESC}",
                        "schema": {{
                            "type": "object",
                            "properties": {{
                                "id": {{ "type": "string" }}
                            }},
                            "required": ["id"]
                        }}
                    }}
                ]
            }}"#
        ),
    )
    .expect("write MCP config");

    let (mut server, base) = spawn_axp_serve_with_mcp_config(&config_path).await;
    let client = Client::new(&base).expect("client");
    let workspace = tempfile::tempdir().expect("tempdir");

    let session = open_session_when_ready(&client, &mut server.child, &workspace).await;

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
            .any(|entry| entry.name == "search" && entry.desc == MCP_DESC),
        "expected search MCP tool in index, got {:?}",
        index.entries
    );
    assert!(
        index
            .entries
            .iter()
            .any(|entry| entry.name == "lookup" && entry.desc == MCP_LOOKUP_DESC),
        "expected lookup MCP tool in index, got {:?}",
        index.entries
    );

    let search = client
        .describe(&DescribeRequest {
            session_id: session.session_id.clone(),
            cap_token: session.cap_token.clone(),
            name: "search".to_owned(),
        })
        .await
        .expect("describe search MCP tool");
    assert_eq!(search.signature, "search(input: object): string");
    assert_eq!(
        search.schema,
        serde_json::json!({
            "type": "object",
            "additionalProperties": true
        })
    );

    let lookup = client
        .describe(&DescribeRequest {
            session_id: session.session_id.clone(),
            cap_token: session.cap_token.clone(),
            name: "lookup".to_owned(),
        })
        .await
        .expect("describe lookup MCP tool");
    assert_eq!(lookup.signature, "lookup(input: object): string");
    assert_eq!(
        lookup.schema,
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" }
            },
            "required": ["id"]
        })
    );

    let closed = client
        .close_session(&SessionCloseRequest {
            session_id: session.session_id,
            cap_token: session.cap_token,
        })
        .await
        .expect("close session");
    assert!(closed.ok);

    server.shutdown().await;
}

async fn spawn_axp_serve_with_mcp_mount() -> (ServerProcess, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve port");
    let addr = listener.local_addr().expect("reserved addr");
    drop(listener);

    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_axp"))
        .arg("serve")
        .arg("--addr")
        .arg(addr.to_string())
        .arg("--mcp-provider")
        .arg("docs")
        .arg("--mcp-tool")
        .arg("search")
        .arg("--mcp-desc")
        .arg(MCP_DESC)
        .arg("--mcp-bridge")
        .arg("axp-mcp-bridge")
        .kill_on_drop(true)
        .spawn()
        .expect("spawn axp serve");

    (ServerProcess { child }, format!("http://{addr}"))
}

async fn spawn_axp_serve_with_mcp_config(config_path: &std::path::Path) -> (ServerProcess, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve port");
    let addr = listener.local_addr().expect("reserved addr");
    drop(listener);

    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_axp"))
        .arg("serve")
        .arg("--addr")
        .arg(addr.to_string())
        .arg("--mcp-config")
        .arg(config_path)
        .kill_on_drop(true)
        .spawn()
        .expect("spawn axp serve");

    (ServerProcess { child }, format!("http://{addr}"))
}

async fn open_session_when_ready(
    client: &Client,
    child: &mut Child,
    workspace: &tempfile::TempDir,
) -> SessionOpenResponse {
    let request = SessionOpenRequest {
        workspace: workspace.path().to_string_lossy().into_owned(),
        sandbox_tier: EnforcementTier::DevNone,
        capabilities: vec![Capability("proc.spawn".to_owned())],
    };

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut last_error = match client.open_session(&request).await {
        Ok(session) => return session,
        Err(err) => Some(err),
    };

    loop {
        if let Some(status) = child.try_wait().expect("poll server process") {
            panic!("axp serve exited before accepting sessions: {status}");
        }

        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for axp serve to accept sessions; last error: {:?}",
                last_error
            );
        }

        tokio::time::sleep(Duration::from_millis(25)).await;

        match client.open_session(&request).await {
            Ok(session) => return session,
            Err(err) => last_error = Some(err),
        }
    }
}
