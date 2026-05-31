//! `cclaw` — copperclaw admin CLI.
//!
//! Parses argv via [`copperclaw_cclaw::Cli`], dials the host Unix socket, and
//! prints the result. See `PLAN.md` § 6 (T9) and § A2.

use std::process::ExitCode;

use clap::Parser as _;
use copperclaw_cclaw::{Cli, CclawClient, SocketTransport, run_cli};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // Resolve the socket path up-front so the transport is ready before
    // we delegate to `run_cli`. If the argv fails to parse we use the
    // platform's default install location (or the legacy relative path
    // when HOME is unset); `run_cli` will re-parse and emit clap's
    // diagnostic.
    let socket_path = Cli::try_parse().map_or_else(
        |_| {
            copperclaw_cclaw::default_user_socket()
                .unwrap_or_else(|| std::path::PathBuf::from("data/cclaw.sock"))
        },
        |cli| cli.resolve_socket(),
    );

    let client = CclawClient::connect(socket_path);
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
