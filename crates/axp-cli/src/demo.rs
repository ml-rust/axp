//! Deterministic runtime demo commands.

use std::time::Duration;

/// Arguments for the `demo` subcommand group.
#[derive(Debug, clap::Args)]
pub struct DemoArgs {
    #[command(subcommand)]
    pub command: DemoCommand,
}

/// Demo commands.
#[derive(Debug, clap::Subcommand)]
pub enum DemoCommand {
    /// Exercise JSON-RPC and SSE against an existing server.
    Runtime(RuntimeArgs),
}

/// Arguments for `demo runtime`.
#[derive(Debug, clap::Args)]
pub struct RuntimeArgs {
    /// AXP server base URL.
    #[arg(long, default_value = "http://127.0.0.1:7300")]
    pub addr: String,
}

/// Run the requested demo command.
pub async fn run(args: DemoArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        DemoCommand::Runtime(args) => runtime(args).await,
    }
}

async fn runtime(args: RuntimeArgs) -> Result<(), Box<dyn std::error::Error>> {
    let client = axp_client::Client::new(&args.addr)?;
    let workspace = tempfile::tempdir()?;
    let workspace_path = workspace.path().to_string_lossy().into_owned();

    let session = client
        .open_session(&axp_proto::SessionOpenRequest {
            workspace: workspace_path,
            sandbox_tier: axp_proto::EnforcementTier::DevNone,
            capabilities: vec![axp_proto::Capability("proc.spawn".to_owned())],
        })
        .await?;

    let index = client
        .index(&axp_proto::IndexRequest {
            session_id: session.session_id.clone(),
            cap_token: session.cap_token.clone(),
        })
        .await?;
    let mut names: Vec<String> = index
        .entries
        .iter()
        .map(|entry| entry.name.clone())
        .collect();
    names.sort();
    let described_name = names
        .first()
        .cloned()
        .ok_or("server returned an empty capability index")?;
    let described = client
        .describe(&axp_proto::DescribeRequest {
            session_id: session.session_id.clone(),
            cap_token: session.cap_token.clone(),
            name: described_name.clone(),
        })
        .await?;

    let started = client
        .start_job(&axp_proto::JobStartRequest {
            session_id: session.session_id.clone(),
            cap_token: session.cap_token.clone(),
            payload: axp_proto::JobPayload::Command {
                command: "printf 'axp-runtime\\n'".to_owned(),
            },
            cwd: None,
            capabilities: Vec::new(),
        })
        .await?;

    let status = match wait_terminal(&client, &session, &started.job_id).await {
        Ok(status) => status,
        Err(e) => {
            let _ = client
                .cancel_job(&axp_proto::JobCancelRequest {
                    session_id: session.session_id.clone(),
                    cap_token: session.cap_token.clone(),
                    job_id: started.job_id.clone(),
                })
                .await;
            return Err(e);
        }
    };
    let frames = client
        .attach_job(&axp_proto::JobAttachRequest {
            session_id: session.session_id,
            cap_token: session.cap_token,
            job_id: started.job_id,
            from_offset: 0,
        })
        .await?;
    let log_bytes: Vec<u8> = frames
        .iter()
        .flat_map(|frame| frame.data.iter().copied())
        .collect();
    let log_text = String::from_utf8_lossy(&log_bytes).trim_end().to_owned();

    println!("runtime ok");
    println!("catalog_count={}", names.len());
    println!("described={described_name}");
    println!("signature={}", described.signature);
    println!("job_status={}", status_label(&status.status));
    println!("log={log_text}");

    Ok(())
}

async fn wait_terminal(
    client: &axp_client::Client,
    session: &axp_proto::SessionOpenResponse,
    job_id: &axp_proto::JobId,
) -> Result<axp_proto::JobStatusResponse, Box<dyn std::error::Error>> {
    for _ in 0..100 {
        let status = client
            .job_status(&axp_proto::JobStatusRequest {
                session_id: session.session_id.clone(),
                cap_token: session.cap_token.clone(),
                job_id: job_id.clone(),
            })
            .await?;
        if is_terminal(&status.status) {
            return Ok(status);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err("job did not reach a terminal state within timeout".into())
}

fn is_terminal(status: &axp_proto::JobStatusProto) -> bool {
    matches!(
        status,
        axp_proto::JobStatusProto::Exited { .. }
            | axp_proto::JobStatusProto::Killed
            | axp_proto::JobStatusProto::Failed { .. }
    )
}

fn status_label(status: &axp_proto::JobStatusProto) -> String {
    match status {
        axp_proto::JobStatusProto::Pending => "pending".to_owned(),
        axp_proto::JobStatusProto::Running => "running".to_owned(),
        axp_proto::JobStatusProto::Exited { code } => format!("exited:{code}"),
        axp_proto::JobStatusProto::Killed => "killed".to_owned(),
        axp_proto::JobStatusProto::Failed { reason } => format!("failed:{reason}"),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::{Cli, Command};

    use super::{DemoArgs, DemoCommand};

    #[test]
    fn runtime_addr_has_default() {
        let cli = Cli::try_parse_from(["axp", "demo", "runtime"]).expect("parse");
        match cli.command {
            Some(Command::Demo(DemoArgs {
                command: DemoCommand::Runtime(args),
            })) => {
                assert_eq!(args.addr, "http://127.0.0.1:7300")
            }
            other => panic!("expected Demo Runtime, got {other:?}"),
        }
    }
}
