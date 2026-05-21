//! `iclaw` — ironclaw admin CLI.
//!
//! Parses argv via [`ironclaw_iclaw::Cli`], dials the host Unix socket, and
//! prints the result. See `PLAN.md` § 6 (T9) and § A2.

use std::process::ExitCode;

use clap::Parser as _;
use ironclaw_iclaw::{Cli, IclawClient, SocketTransport, run_cli};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // Resolve the socket path up-front so the transport is ready before
    // we delegate to `run_cli`. If the argv fails to parse we fall back
    // to the default path; `run_cli` will re-parse and emit a helpful
    // diagnostic.
    let socket_path = Cli::try_parse().map_or_else(
        |_| std::path::PathBuf::from("data/iclaw.sock"),
        |cli| cli.socket,
    );

    let client = IclawClient::connect(socket_path);
    let transport = SocketTransport(client);
    let out = run_cli(std::env::args_os(), &transport).await;
    if !out.stdout.is_empty() {
        print!("{}", out.stdout);
    }
    if !out.stderr.is_empty() {
        eprint!("{}", out.stderr);
    }
    out.code
}
