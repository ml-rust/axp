//! `axp-cli` — command-line interface for the AXP runtime.
//!
//! All logic lives here; `main.rs` is a one-liner that delegates to [`run`].

mod demo;
mod mcp_config;

use clap::Parser;

/// AXP — Agent Execution Protocol runtime.
#[derive(Debug, Parser)]
#[command(name = "axp", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Top-level subcommands.
#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Run the AXP runtime as an HTTP (JSON-RPC + SSE) server.
    Serve(ServeArgs),
    /// Run demos against an existing AXP runtime.
    Demo(demo::DemoArgs),
}

/// Arguments for the `serve` subcommand.
#[derive(Debug, clap::Args)]
pub struct ServeArgs {
    /// Address to bind the server to.
    #[arg(long, default_value = "127.0.0.1:7300")]
    pub addr: std::net::SocketAddr,

    /// Static MCP provider id to expose through the served runtime.
    #[arg(long = "mcp-provider")]
    pub mcp_provider: Option<String>,

    /// Static MCP tool name to expose through the served runtime.
    #[arg(long = "mcp-tool")]
    pub mcp_tool: Option<String>,

    /// Static MCP tool description to expose through the served runtime.
    #[arg(long = "mcp-desc")]
    pub mcp_desc: Option<String>,

    /// Bridge program used to call the static MCP tool.
    #[arg(long = "mcp-bridge")]
    pub mcp_bridge: Option<String>,

    /// JSON static MCP config file to expose through the served runtime.
    #[arg(long = "mcp-config")]
    pub mcp_config: Option<std::path::PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
struct ServeMcpTool {
    provider_id: String,
    tool_name: String,
    desc: String,
    bridge_program: String,
}

#[derive(Debug, PartialEq)]
enum ServeMcpMount {
    Tool(ServeMcpTool),
    Config(mcp_config::McpConfigMount),
}

/// Entry point called from `main.rs`.
///
/// Stays synchronous and returns an [`ExitCode`]; the `serve` arm boots a
/// Tokio runtime internally via [`tokio::runtime::Runtime::block_on`].
pub fn run() -> std::process::ExitCode {
    let cli = Cli::parse();

    match cli.command {
        None => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            std::process::ExitCode::SUCCESS
        }
        Some(Command::Serve(args)) => {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("failed to start async runtime: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            match rt.block_on(serve(args)) {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("server error: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        Some(Command::Demo(args)) => {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("failed to start async runtime: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            match rt.block_on(demo::run(args)) {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("demo error: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
    }
}

/// Boot the AXP HTTP server on `args.addr` and serve until terminated.
///
/// Binds the TCP listener, then delegates to [`axp_transport::serve`] which
/// owns the `axum::serve` call so that `axum` is not a direct dep of this
/// crate.
async fn serve(args: ServeArgs) -> std::io::Result<()> {
    let addr = args.addr;
    let state = match mcp_mount(args)? {
        Some(ServeMcpMount::Tool(mount)) => axp_transport::AppState::with_mcp_tool(
            mount.provider_id,
            mount.tool_name,
            mount.desc,
            mount.bridge_program,
        )
        .map_err(std::io::Error::other)?,
        Some(ServeMcpMount::Config(mount)) => axp_transport::AppState::with_mcp_tools(
            mount.provider_id,
            mount.bridge_program,
            mount.bridge_args,
            mount.tools,
        )
        .map_err(std::io::Error::other)?,
        None => axp_transport::AppState::new(),
    };
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("AXP runtime listening on http://{addr}");
    axp_transport::serve(listener, state).await
}

fn mcp_mount(args: ServeArgs) -> std::io::Result<Option<ServeMcpMount>> {
    let has_static_flag = args.mcp_provider.is_some()
        || args.mcp_tool.is_some()
        || args.mcp_desc.is_some()
        || args.mcp_bridge.is_some();
    if args.mcp_config.is_some() && has_static_flag {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--mcp-config cannot be combined with --mcp-provider, --mcp-tool, --mcp-desc, or --mcp-bridge",
        ));
    }

    if let Some(path) = args.mcp_config {
        return mcp_config::load(path).map(|mount| Some(ServeMcpMount::Config(mount)));
    }

    match (
        args.mcp_provider,
        args.mcp_tool,
        args.mcp_desc,
        args.mcp_bridge,
    ) {
        (None, None, None, None) => Ok(None),
        (Some(provider_id), Some(tool_name), Some(desc), Some(bridge_program)) => {
            Ok(Some(ServeMcpMount::Tool(ServeMcpTool {
                provider_id,
                tool_name,
                desc,
                bridge_program,
            })))
        }
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--mcp-provider, --mcp-tool, --mcp-desc, and --mcp-bridge must be supplied together",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn serve_parses_addr() {
        let cli = Cli::try_parse_from(["axp", "serve", "--addr", "127.0.0.1:9000"]).expect("parse");
        match cli.command {
            Some(Command::Serve(args)) => {
                assert_eq!(args.addr.to_string(), "127.0.0.1:9000")
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn serve_addr_has_default() {
        let cli = Cli::try_parse_from(["axp", "serve"]).expect("parse");
        match cli.command {
            Some(Command::Serve(args)) => {
                assert_eq!(args.addr.to_string(), "127.0.0.1:7300")
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn serve_parses_mcp_tool_mount_flags() {
        let cli = Cli::try_parse_from([
            "axp",
            "serve",
            "--mcp-provider",
            "docs",
            "--mcp-tool",
            "search",
            "--mcp-desc",
            "Search documentation with an external MCP bridge",
            "--mcp-bridge",
            "axp-mcp-bridge",
        ])
        .expect("parse");
        match cli.command {
            Some(Command::Serve(args)) => {
                assert_eq!(args.mcp_provider.as_deref(), Some("docs"));
                assert_eq!(args.mcp_tool.as_deref(), Some("search"));
                assert_eq!(
                    args.mcp_desc.as_deref(),
                    Some("Search documentation with an external MCP bridge")
                );
                assert_eq!(args.mcp_bridge.as_deref(), Some("axp-mcp-bridge"));
                assert_eq!(args.mcp_config, None);
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn serve_parses_mcp_config() {
        let cli = Cli::try_parse_from(["axp", "serve", "--mcp-config", "mcp.json"]).expect("parse");
        match cli.command {
            Some(Command::Serve(args)) => {
                assert_eq!(
                    args.mcp_config.as_deref(),
                    Some(std::path::Path::new("mcp.json"))
                );
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn default_serve_has_no_mcp_tool_mount() {
        let cli = Cli::try_parse_from(["axp", "serve"]).expect("parse");
        match cli.command {
            Some(Command::Serve(args)) => {
                let mount = mcp_mount(args).expect("valid default");
                assert_eq!(mount, None);
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn partial_mcp_tool_mount_is_rejected() {
        let cli = Cli::try_parse_from(["axp", "serve", "--mcp-provider", "docs"]).expect("parse");
        match cli.command {
            Some(Command::Serve(args)) => {
                let err = mcp_mount(args).expect_err("partial MCP args must fail");
                assert!(err.to_string().contains("must be supplied together"));
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn mcp_config_rejects_static_mcp_flags() {
        let cli = Cli::try_parse_from([
            "axp",
            "serve",
            "--mcp-config",
            "mcp.json",
            "--mcp-provider",
            "docs",
        ])
        .expect("parse");
        match cli.command {
            Some(Command::Serve(args)) => {
                let err = mcp_mount(args).expect_err("mixed MCP config and static flags must fail");
                assert!(err.to_string().contains("cannot be combined"));
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }
}
