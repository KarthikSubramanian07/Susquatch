//! Command line.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// susq-live: the only Rust process that talks to the exchange.
#[derive(Debug, Parser)]
#[command(name = "susq-live", version, about)]
pub struct Cli {
    /// Repository checkout on the server (`$SUSQ_HOME`); relative config paths resolve against it.
    #[arg(long, env = "SUSQ_HOME", default_value = ".")]
    pub home: PathBuf,
    /// Config file (default: `<home>/config/susq.toml`).
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Emit JSON logs.
    #[arg(long, env = "SUSQ_LOG_JSON")]
    pub json_logs: bool,
    #[command(subcommand)]
    pub cmd: Command,
}

/// What to do.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Trade: paper unless SUSQ_MODE=live AND ops/GO_LIVE contains "go live".
    Run(RunArgs),
    /// Trade in paper mode whatever the environment says (fills simulated against live books).
    PaperRun(RunArgs),
    /// Place one contract (non-marketable, at the bid) and cancel it, through every gate.
    Smoke {
        /// Exchange to use (pick a liquid one).
        #[arg(long)]
        exchange: String,
    },
    /// Cancel every open order in the tournament (needs the lease).
    CancelAll {
        #[arg(long, default_value = "manual cancel-all")]
        reason: String,
    },
    /// Print the trader lease on disk.
    Lease {
        /// Print the paper-mode lease (`<lease.path>` with a `.paper` suffix) instead.
        #[arg(long)]
        paper: bool,
    },
}

/// Options of the trading loop.
#[derive(Debug, Clone, Default, Args)]
pub struct RunArgs {
    /// Election night: also poll the results sites (`susq-enr`) and publish every
    /// `CountyResultsBatch` on NATS `enr.county_results` for the needle.
    #[arg(long)]
    pub election: bool,
    /// Results-source config for `--election` (default: `<home>/config/enr_sources.toml`).
    #[arg(long)]
    pub enr_config: Option<PathBuf>,
}

impl Cli {
    /// Resolved config path.
    pub fn config_path(&self) -> PathBuf {
        self.config
            .clone()
            .unwrap_or_else(|| self.home.join("config/susq.toml"))
    }
}
