//! `ironclaw-setup` binary entry point.
//!
//! Thin wrapper around [`ironclaw_setup::cli::run_from_args`]; tests live
//! in the library crate.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let use_ansi = std::io::IsTerminal::is_terminal(&std::io::stdout());
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(use_ansi)
        .init();
    let args = std::env::args_os();
    let code = ironclaw_setup::cli::run_from_args(args)
        .map_err(|e| anyhow::anyhow!("setup failed: {e}"))?;
    std::process::exit(code);
}
