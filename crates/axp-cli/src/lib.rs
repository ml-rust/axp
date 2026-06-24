//! `axp-cli` — command-line interface for the AXP runtime.
//!
//! All logic lives here; `main.rs` is a one-liner that delegates to [`run`].

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
    // Subcommands land in later units.
}

/// Entry point called from `main.rs`.
pub fn run() -> std::process::ExitCode {
    let cli = Cli::parse();

    match cli.command {
        None => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            std::process::ExitCode::SUCCESS
        }
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
}
