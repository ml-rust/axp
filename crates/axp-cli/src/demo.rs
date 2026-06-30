//! Deterministic runtime demo commands.

use std::time::{Duration, Instant};

use serde::Serialize;

const DEFAULT_POLL_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_POLL_INTERVAL_MS: u64 = 50;
const RUNTIME_MARKER: &str = "axp-runtime-smoke";

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

    /// Print one JSON object instead of human-readable lines.
    #[arg(long)]
    pub json: bool,

    /// Maximum time to wait for the job to reach a terminal status.
    #[arg(long, default_value_t = DEFAULT_POLL_TIMEOUT_MS)]
    pub poll_timeout_ms: u64,

    /// Delay between job status polls.
    #[arg(long, default_value_t = DEFAULT_POLL_INTERVAL_MS)]
    pub poll_interval_ms: u64,
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

    let total_started_at = Instant::now();
    let (session, session_open_ms) = timed_async(async {
        client
            .open_session(&axp_proto::SessionOpenRequest {
                workspace: workspace_path,
                sandbox_tier: axp_proto::EnforcementTier::DevNone,
                capabilities: vec![axp_proto::Capability("proc.spawn".to_owned())],
            })
            .await
    })
    .await?;

    let result = async {
        let (index, index_ms) = timed_async(async {
            client
                .index(&axp_proto::IndexRequest {
                    session_id: session.session_id.clone(),
                    cap_token: session.cap_token.clone(),
                })
                .await
        })
        .await?;
        let described_name = index
            .entries
            .iter()
            .map(|entry| &entry.name)
            .min()
            .cloned()
            .ok_or("server returned an empty capability index")?;
        let (described, describe_ms) = timed_async(async {
            client
                .describe(&axp_proto::DescribeRequest {
                    session_id: session.session_id.clone(),
                    cap_token: session.cap_token.clone(),
                    name: described_name.clone(),
                })
                .await
        })
        .await?;

        let (started, job_start_ms) = timed_async(async {
            client
                .start_job(&axp_proto::JobStartRequest {
                    session_id: session.session_id.clone(),
                    cap_token: session.cap_token.clone(),
                    payload: axp_proto::JobPayload::Command {
                        command: format!("printf '{RUNTIME_MARKER}\\n'"),
                    },
                    cwd: None,
                    capabilities: Vec::new(),
                })
                .await
        })
        .await?;

        let poll_timeout = Duration::from_millis(args.poll_timeout_ms);
        let poll_interval = Duration::from_millis(args.poll_interval_ms);
        let ((status, status_poll_count), job_status_poll_ms) = match timed_async(wait_terminal(
            &client,
            &session,
            &started.job_id,
            poll_timeout,
            poll_interval,
        ))
        .await
        {
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
        let terminal_status = status_label(&status.status);
        if !is_success_status(&status.status) {
            return Err(
                format!("job reached non-success terminal status: {terminal_status}").into(),
            );
        }

        let (frames, job_attach_ms) = timed_async(async {
            client
                .attach_job(&axp_proto::JobAttachRequest {
                    session_id: session.session_id.clone(),
                    cap_token: session.cap_token.clone(),
                    job_id: started.job_id,
                    from_offset: 0,
                })
                .await
        })
        .await?;
        let log_bytes: Vec<u8> = frames.into_iter().flat_map(|frame| frame.data).collect();
        let log_text = String::from_utf8_lossy(&log_bytes).trim_end().to_owned();
        let log_replay_success = log_text.contains(RUNTIME_MARKER);
        if !log_replay_success {
            return Err("attached log replay did not contain runtime marker".into());
        }

        Ok::<_, Box<dyn std::error::Error>>(RuntimeMeasurement {
            total_ms: duration_ms(total_started_at.elapsed()),
            steps: RuntimeStepDurations {
                session_open_ms,
                index_ms,
                describe_ms,
                job_start_ms,
                job_status_poll_ms,
                job_attach_ms,
            },
            catalog_count: index.entries.len(),
            described_capability: described_name,
            described_signature: described.signature,
            terminal_job_status: terminal_status,
            status_poll_count,
            log_replay_success,
            attach_bytes: log_bytes.len(),
            log_marker: RUNTIME_MARKER.to_owned(),
            log_text,
        })
    }
    .await;

    match result {
        Ok(measurement) => {
            let print_result = print_measurement(&measurement, args.json);
            let close_result = close_session(&client, &session).await;
            if let Err(err) = print_result {
                let _ = close_result;
                return Err(err.into());
            }
            close_result?;
        }
        Err(err) => {
            let _ = close_session(&client, &session).await;
            return Err(err);
        }
    }

    Ok(())
}

async fn wait_terminal(
    client: &axp_client::Client,
    session: &axp_proto::SessionOpenResponse,
    job_id: &axp_proto::JobId,
    timeout: Duration,
    interval: Duration,
) -> Result<(axp_proto::JobStatusResponse, u64), Box<dyn std::error::Error>> {
    let started_at = Instant::now();
    let mut poll_count = 0;

    loop {
        let status = client
            .job_status(&axp_proto::JobStatusRequest {
                session_id: session.session_id.clone(),
                cap_token: session.cap_token.clone(),
                job_id: job_id.clone(),
            })
            .await?;
        poll_count += 1;
        if is_terminal(&status.status) {
            return Ok((status, poll_count));
        }

        let elapsed = started_at.elapsed();
        let remaining = match timeout.checked_sub(elapsed) {
            Some(remaining) => remaining,
            None => break,
        };
        if remaining.is_zero() {
            break;
        }
        let sleep_for = interval.min(remaining);
        if !sleep_for.is_zero() {
            tokio::time::sleep(sleep_for).await;
        }
    }
    Err("job did not reach a terminal state within timeout".into())
}

async fn timed_async<T, E>(
    future: impl std::future::Future<Output = Result<T, E>>,
) -> Result<(T, u64), E> {
    let started_at = Instant::now();
    let value = future.await?;
    Ok((value, duration_ms(started_at.elapsed())))
}

async fn close_session(
    client: &axp_client::Client,
    session: &axp_proto::SessionOpenResponse,
) -> Result<(), Box<dyn std::error::Error>> {
    client
        .close_session(&axp_proto::SessionCloseRequest {
            session_id: session.session_id.clone(),
            cap_token: session.cap_token.clone(),
        })
        .await?;
    Ok(())
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn is_terminal(status: &axp_proto::JobStatusProto) -> bool {
    matches!(
        status,
        axp_proto::JobStatusProto::Exited { .. }
            | axp_proto::JobStatusProto::Killed
            | axp_proto::JobStatusProto::Failed { .. }
    )
}

fn is_success_status(status: &axp_proto::JobStatusProto) -> bool {
    matches!(status, axp_proto::JobStatusProto::Exited { code: 0 })
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

fn print_measurement(
    measurement: &RuntimeMeasurement,
    json: bool,
) -> Result<(), serde_json::Error> {
    if json {
        println!("{}", serde_json::to_string(measurement)?);
    } else {
        print!("{}", format_human_measurement(measurement));
    }

    Ok(())
}

fn format_human_measurement(measurement: &RuntimeMeasurement) -> String {
    format!(
        concat!(
            "runtime ok\n",
            "total_ms={}\n",
            "session_open_ms={}\n",
            "index_ms={}\n",
            "describe_ms={}\n",
            "job_start_ms={}\n",
            "job_status_poll_ms={}\n",
            "job_attach_ms={}\n",
            "catalog_count={}\n",
            "described={}\n",
            "signature={}\n",
            "job_status={}\n",
            "status_poll_count={}\n",
            "log_replay_success={}\n",
            "attach_bytes={}\n",
            "log_marker={}\n",
            "log={}\n"
        ),
        measurement.total_ms,
        measurement.steps.session_open_ms,
        measurement.steps.index_ms,
        measurement.steps.describe_ms,
        measurement.steps.job_start_ms,
        measurement.steps.job_status_poll_ms,
        measurement.steps.job_attach_ms,
        measurement.catalog_count,
        measurement.described_capability,
        measurement.described_signature,
        measurement.terminal_job_status,
        measurement.status_poll_count,
        measurement.log_replay_success,
        measurement.attach_bytes,
        measurement.log_marker,
        measurement.log_text
    )
}

#[derive(Debug, Serialize)]
struct RuntimeMeasurement {
    total_ms: u64,
    steps: RuntimeStepDurations,
    catalog_count: usize,
    described_capability: String,
    described_signature: String,
    terminal_job_status: String,
    status_poll_count: u64,
    log_replay_success: bool,
    attach_bytes: usize,
    log_marker: String,
    log_text: String,
}

#[derive(Debug, Serialize)]
struct RuntimeStepDurations {
    session_open_ms: u64,
    index_ms: u64,
    describe_ms: u64,
    job_start_ms: u64,
    job_status_poll_ms: u64,
    job_attach_ms: u64,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::{Cli, Command};

    use super::{
        DEFAULT_POLL_INTERVAL_MS, DEFAULT_POLL_TIMEOUT_MS, DemoArgs, DemoCommand,
        RuntimeMeasurement, RuntimeStepDurations, format_human_measurement, is_success_status,
        status_label,
    };

    #[test]
    fn runtime_args_have_defaults() {
        let cli = Cli::try_parse_from(["axp", "demo", "runtime"]).expect("parse");
        match cli.command {
            Some(Command::Demo(DemoArgs {
                command: DemoCommand::Runtime(args),
            })) => {
                assert_eq!(args.addr, "http://127.0.0.1:7300");
                assert!(!args.json);
                assert_eq!(args.poll_timeout_ms, DEFAULT_POLL_TIMEOUT_MS);
                assert_eq!(args.poll_interval_ms, DEFAULT_POLL_INTERVAL_MS);
            }
            other => panic!("expected Demo Runtime, got {other:?}"),
        }
    }

    #[test]
    fn runtime_args_accept_measurement_options() {
        let cli = Cli::try_parse_from([
            "axp",
            "demo",
            "runtime",
            "--addr",
            "http://127.0.0.1:9999",
            "--json",
            "--poll-timeout-ms",
            "250",
            "--poll-interval-ms",
            "10",
        ])
        .expect("parse");
        match cli.command {
            Some(Command::Demo(DemoArgs {
                command: DemoCommand::Runtime(args),
            })) => {
                assert_eq!(args.addr, "http://127.0.0.1:9999");
                assert!(args.json);
                assert_eq!(args.poll_timeout_ms, 250);
                assert_eq!(args.poll_interval_ms, 10);
            }
            other => panic!("expected Demo Runtime, got {other:?}"),
        }
    }

    #[test]
    fn status_labels_include_terminal_details() {
        assert_eq!(
            status_label(&axp_proto::JobStatusProto::Exited { code: 0 }),
            "exited:0"
        );
        assert_eq!(
            status_label(&axp_proto::JobStatusProto::Failed {
                reason: "nope".to_owned()
            }),
            "failed:nope"
        );
    }

    #[test]
    fn success_status_requires_zero_exit() {
        assert!(is_success_status(&axp_proto::JobStatusProto::Exited {
            code: 0
        }));
        assert!(!is_success_status(&axp_proto::JobStatusProto::Exited {
            code: 2
        }));
        assert!(!is_success_status(&axp_proto::JobStatusProto::Killed));
    }

    #[test]
    fn measurement_renders_as_single_json_object() {
        let measurement = runtime_measurement();

        assert_eq!(
            serde_json::to_string(&measurement).expect("json"),
            concat!(
                "{\"total_ms\":12,",
                "\"steps\":{\"session_open_ms\":1,\"index_ms\":2,\"describe_ms\":3,",
                "\"job_start_ms\":4,\"job_status_poll_ms\":5,\"job_attach_ms\":6},",
                "\"catalog_count\":7,",
                "\"described_capability\":\"proc.spawn\",",
                "\"described_signature\":\"sig\",",
                "\"terminal_job_status\":\"exited:0\",",
                "\"status_poll_count\":2,",
                "\"log_replay_success\":true,",
                "\"attach_bytes\":18,",
                "\"log_marker\":\"axp-runtime-smoke\",",
                "\"log_text\":\"axp-runtime-smoke\"}"
            )
        );
    }

    #[test]
    fn measurement_renders_as_human_readable_lines() {
        let measurement = runtime_measurement();

        assert_eq!(
            format_human_measurement(&measurement),
            concat!(
                "runtime ok\n",
                "total_ms=12\n",
                "session_open_ms=1\n",
                "index_ms=2\n",
                "describe_ms=3\n",
                "job_start_ms=4\n",
                "job_status_poll_ms=5\n",
                "job_attach_ms=6\n",
                "catalog_count=7\n",
                "described=proc.spawn\n",
                "signature=sig\n",
                "job_status=exited:0\n",
                "status_poll_count=2\n",
                "log_replay_success=true\n",
                "attach_bytes=18\n",
                "log_marker=axp-runtime-smoke\n",
                "log=axp-runtime-smoke\n"
            )
        );
    }

    fn runtime_measurement() -> RuntimeMeasurement {
        RuntimeMeasurement {
            total_ms: 12,
            steps: RuntimeStepDurations {
                session_open_ms: 1,
                index_ms: 2,
                describe_ms: 3,
                job_start_ms: 4,
                job_status_poll_ms: 5,
                job_attach_ms: 6,
            },
            catalog_count: 7,
            described_capability: "proc.spawn".to_owned(),
            described_signature: "sig".to_owned(),
            terminal_job_status: "exited:0".to_owned(),
            status_poll_count: 2,
            log_replay_success: true,
            attach_bytes: 18,
            log_marker: "axp-runtime-smoke".to_owned(),
            log_text: "axp-runtime-smoke".to_owned(),
        }
    }
}
