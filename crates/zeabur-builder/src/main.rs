//! `zeabur-builder`: the controller binary. M0 ships only the install-time subcommands.

mod output;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Zeabur Builder: turns Git commits into container images with `BuildKit`.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the custom resource definitions as a Kubernetes List, for `kubectl apply -f -`.
    Crds,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Crds => zeabur_builder_api::definitions_json().map_err(|e| e.to_string()),
    };
    match result {
        Ok(text) => output::stdout_line(&text),
        Err(message) => {
            output::stderr_line(&format!("error: {message}"));
            ExitCode::FAILURE
        }
    }
}
