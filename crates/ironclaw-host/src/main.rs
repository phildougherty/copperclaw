//! `ironclaw` binary — clap-driven dispatcher.
//!
//! Subcommands:
//! - `ironclaw run` — boot the orchestrator (see `boot::run_host`).
//! - `ironclaw migrate` — run central migrations only, exit.
//! - `ironclaw version` — print version, exit.
//!
//! See `PLAN.md` § 6 T3.

#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use ironclaw_host::{boot, config, run_host, HostConfig};
use std::process::ExitCode;
use tokio_util::sync::CancellationToken;
use tracing::error;

#[derive(Debug, Parser)]
#[command(name = "ironclaw", version, about = "ironclaw host orchestrator")]
struct Cli {
    /// Optional path to a `.env` file to load before parsing config.
    #[arg(long, global = true)]
    env_file: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Boot the orchestrator (default).
    Run,
    /// Run central DB migrations, then exit.
    Migrate,
    /// Print version and exit.
    Version,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    config::load_dotenv_optional(cli.env_file.as_deref());

    let cfg = match HostConfig::from_env() {
        Ok(c) => c,
        Err(err) => {
            eprintln!("config error: {err}");
            return ExitCode::from(2);
        }
    };

    // Write logs to stderr so the cli channel can own stdout for chat
    // I/O without log lines interleaving with agent replies. Strip ANSI
    // when stderr isn't a TTY (journald, log files, container capture).
    let use_ansi = std::io::IsTerminal::is_terminal(&std::io::stderr());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&cfg.log_filter))
        .with_writer(std::io::stderr)
        .with_ansi(use_ansi)
        .init();

    match cli.command.unwrap_or(Command::Run) {
        Command::Run => match run_host(cfg, None, CancellationToken::new()).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                error!(?err, "ironclaw exited with error");
                ExitCode::from(err.exit_code())
            }
        },
        Command::Migrate => match boot::run_migrations_only(&cfg) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                error!(?err, "migration failed");
                ExitCode::from(err.exit_code())
            }
        },
        Command::Version => {
            println!("ironclaw {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
    }
}
