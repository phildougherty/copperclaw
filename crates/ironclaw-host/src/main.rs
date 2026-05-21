//! `ironclaw` binary — clap-driven dispatcher.
//!
//! Subcommands:
//! - `ironclaw run` — boot the orchestrator in the foreground (default
//!   if no subcommand is given).
//! - `ironclaw start` — daemonize and run in the background; returns
//!   once the admin socket is ready.
//! - `ironclaw stop` — signal the running host to shut down (SIGTERM
//!   then SIGKILL after a grace period).
//! - `ironclaw status` — print PID, uptime, paths and active session
//!   count; `--json` for machine-readable output.
//! - `ironclaw logs [-f]` — print the last 50 lines of the host log
//!   (or follow the tail).
//! - `ironclaw migrate` — run central migrations only, exit.
//! - `ironclaw version` — print version, exit.
//!
//! See `PLAN.md` § 6 T3 for the foreground boot sequence. The daemon
//! lifecycle wrapping lives in [`ironclaw_host::daemon`].

#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use ironclaw_host::{boot, config, daemon, run_host, HostConfig};
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
    /// Boot the orchestrator in the foreground (default).
    Run,
    /// Daemonize: spawn the host in the background, write a PID file,
    /// and return once the admin socket is listening.
    Start,
    /// Signal the running host to shut down (SIGTERM, then SIGKILL).
    Stop {
        /// Exit non-zero if the host wasn't running.
        #[arg(long)]
        strict: bool,
    },
    /// Print PID, uptime, paths and active session count.
    Status {
        /// Emit JSON instead of a human-readable summary.
        #[arg(long)]
        json: bool,
    },
    /// Print the last `n` lines of the host log; `-f` to follow.
    Logs {
        /// Number of lines from the end of the log to print.
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        /// Stream additional log lines as they're written.
        #[arg(short = 'f', long)]
        follow: bool,
    },
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
        Command::Start => run_start(&cfg),
        Command::Stop { strict } => run_stop(&cfg, strict),
        Command::Status { json } => run_status(&cfg, json),
        Command::Logs { lines, follow } => run_logs(&cfg, lines, follow),
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

fn run_start(cfg: &HostConfig) -> ExitCode {
    match daemon::cmd_start(cfg, &[]) {
        Ok(out) => {
            println!(
                "ironclaw started (pid {}, socket {}, log {})",
                out.pid,
                out.socket.display(),
                out.log.display(),
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("ironclaw start: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}

fn run_stop(cfg: &HostConfig, strict: bool) -> ExitCode {
    match daemon::cmd_stop(cfg, strict) {
        Ok(daemon::StopOutcome::NotRunning) => {
            println!("ironclaw: not running");
            ExitCode::SUCCESS
        }
        Ok(daemon::StopOutcome::StalePidCleared(pid)) => {
            println!("ironclaw: stale pid {pid} cleared");
            ExitCode::SUCCESS
        }
        Ok(daemon::StopOutcome::Graceful(pid)) => {
            println!("ironclaw: stopped pid {pid} (SIGTERM)");
            ExitCode::SUCCESS
        }
        Ok(daemon::StopOutcome::Killed(pid)) => {
            println!("ironclaw: killed pid {pid} (SIGKILL after grace)");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("ironclaw stop: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}

fn run_status(cfg: &HostConfig, json: bool) -> ExitCode {
    match daemon::cmd_status(cfg) {
        Ok(snap) => {
            if json {
                let v = snap.render_json();
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            } else {
                print!("{}", snap.render_text());
            }
            if snap.running {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(3)
            }
        }
        Err(err) => {
            eprintln!("ironclaw status: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}

fn run_logs(cfg: &HostConfig, lines: usize, follow: bool) -> ExitCode {
    match daemon::cmd_logs(cfg, lines, follow) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ironclaw logs: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}
