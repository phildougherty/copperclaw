//! `copperclaw` binary — clap-driven dispatcher.
//!
//! Subcommands:
//! - `copperclaw run` — boot the orchestrator in the foreground (default
//!   if no subcommand is given).
//! - `copperclaw start` — daemonize and run in the background; returns
//!   once the admin socket is ready.
//! - `copperclaw stop` — signal the running host to shut down (SIGTERM
//!   then SIGKILL after a grace period).
//! - `copperclaw status` — print PID, uptime, paths and active session
//!   count; `--json` for machine-readable output.
//! - `copperclaw logs [-f]` — print the last 50 lines of the host log
//!   (or follow the tail).
//! - `copperclaw migrate` — run central migrations only, exit.
//! - `copperclaw version` — print version, exit.
//!
//! ## Tracing / log configuration
//!
//! By default all tracing output goes to **stderr** so the cli channel's
//! stdout stays clean for chat I/O.
//!
//! When `COPPERCLAW_LOG_DIR=<path>` is set the host additionally writes
//! daily-rotating log files to `<path>/host.log.<YYYY-MM-DD>` using
//! `tracing-appender::rolling::daily`.  Stderr output is kept in place so
//! interactive runs continue to show logs on screen.  The log-dir is
//! created automatically if it doesn't exist; creation failures are written
//! to stderr but do not abort the host.
//!
//! See `PLAN.md` § 6 T3 for the foreground boot sequence. The daemon
//! lifecycle wrapping for `start` / `stop` / `status` / `logs` lives in
//! [`copperclaw_host::daemon`].

#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use copperclaw_host::{boot, config, daemon, run_host, HostConfig};
use std::process::ExitCode;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing_subscriber::prelude::*;

#[derive(Debug, Parser)]
#[command(name = "copperclaw", version, about = "copperclaw host orchestrator")]
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

    // Optional file-based rolling appender. Enabled when COPPERCLAW_LOG_DIR
    // is set in the environment (or the loaded .env file). Off by default
    // so existing installs see no change in behaviour.
    //
    // We need to keep the `_guard` alive for the duration of `main` — when
    // it drops, the background writer thread is joined and the file is
    // flushed. Binding it to a `let` in `main` achieves that.
    let log_dir = std::env::var("COPPERCLAW_LOG_DIR").ok();
    let _appender_guard = if let Some(ref dir) = log_dir {
        // Create the directory if it doesn't exist.  A failure here is
        // non-fatal — we log the error to stderr and continue without
        // file logging.
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("warning: could not create COPPERCLAW_LOG_DIR {dir:?}: {e}; file logging disabled");
            setup_stderr_only(&cfg, use_ansi);
            None
        } else {
            let file_appender = tracing_appender::rolling::daily(dir, "host.log");
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

            // Fan-out: stderr + rolling file.
            let stderr_layer = tracing_subscriber::fmt::layer()
                .with_ansi(use_ansi)
                .with_writer(std::io::stderr);
            let file_layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(non_blocking);

            tracing_subscriber::registry()
                .with(tracing_subscriber::EnvFilter::new(&cfg.log_filter))
                .with(stderr_layer)
                .with(file_layer)
                .init();

            Some(guard)
        }
    } else {
        setup_stderr_only(&cfg, use_ansi);
        None
    };

    match cli.command.unwrap_or(Command::Run) {
        Command::Run => match run_host(cfg, None, CancellationToken::new(), cli.env_file.clone()).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                error!(?err, "copperclaw exited with error");
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
            println!("copperclaw {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
    }
}

/// Install the stderr-only tracing subscriber (the default path when
/// `COPPERCLAW_LOG_DIR` is not set).
fn setup_stderr_only(cfg: &HostConfig, use_ansi: bool) {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&cfg.log_filter))
        .with_writer(std::io::stderr)
        .with_ansi(use_ansi)
        .init();
}

fn run_start(cfg: &HostConfig) -> ExitCode {
    match daemon::cmd_start(cfg, &[]) {
        Ok(out) => {
            println!(
                "copperclaw started (pid {}, socket {}, log {})",
                out.pid,
                out.socket.display(),
                out.log.display(),
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("copperclaw start: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}

fn run_stop(cfg: &HostConfig, strict: bool) -> ExitCode {
    match daemon::cmd_stop(cfg, strict) {
        Ok(daemon::StopOutcome::NotRunning) => {
            println!("copperclaw: not running");
            ExitCode::SUCCESS
        }
        Ok(daemon::StopOutcome::StalePidCleared(pid)) => {
            println!("copperclaw: stale pid {pid} cleared");
            ExitCode::SUCCESS
        }
        Ok(daemon::StopOutcome::Graceful(pid)) => {
            println!("copperclaw: stopped pid {pid} (SIGTERM)");
            ExitCode::SUCCESS
        }
        Ok(daemon::StopOutcome::Killed(pid)) => {
            println!("copperclaw: killed pid {pid} (SIGKILL after grace)");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("copperclaw stop: {err}");
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
            eprintln!("copperclaw status: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}

fn run_logs(cfg: &HostConfig, lines: usize, follow: bool) -> ExitCode {
    match daemon::cmd_logs(cfg, lines, follow) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("copperclaw logs: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}
