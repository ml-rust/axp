//! `axp-cli` — command-line interface for the AXP runtime.
//!
//! All logic lives here; `main.rs` is a one-liner that delegates to [`run`].

mod demo;

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
    let state = axp_transport::AppState::new();
    let listener = tokio::net::TcpListener::bind(args.addr).await?;
    println!("AXP runtime listening on http://{}", args.addr);
    axp_transport::serve(listener, state).await
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
}
