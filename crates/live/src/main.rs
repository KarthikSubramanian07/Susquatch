//! `susq-live`: the only Rust process that talks to the exchange. See the library docs.

use clap::Parser;
use susq_live::{
    cli::{Cli, Command},
    commands, runtime,
};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    runtime::init_tracing(cli.json_logs);
    let result = match &cli.cmd {
        Command::Run => commands::run(&cli, false).await,
        Command::PaperRun => commands::run(&cli, true).await,
        Command::Smoke { exchange } => commands::smoke(&cli, exchange).await,
        Command::CancelAll { reason } => commands::cancel_all(&cli, reason).await,
        Command::Lease => commands::lease(&cli),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "susq-live failed");
            std::process::ExitCode::FAILURE
        }
    }
}
