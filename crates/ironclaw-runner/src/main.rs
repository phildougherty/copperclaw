//! `ironclaw-runner` binary entrypoint.
//!
//! The binary parses CLI args, reads the JSON config file, opens the
//! session's databases, wires up the provider + MCP tool context, and
//! delegates to [`ironclaw_runner::run_loop`].
//!
//! See `PLAN.md` § 6 (T5) for the broader contract.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use ironclaw_db::session::{open_inbound_rw_no_mmap, open_outbound, SessionPaths};
use ironclaw_providers::AnthropicProvider;
use ironclaw_runner::{
    compaction::CompactionCfg, resolve_provider_deadline, run_loop, RunnerConfig, RunnerDeps,
    RunnerToolCtx,
};
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "ironclaw-runner", version, about = "Ironclaw container agent runner")]
struct Cli {
    /// Path to the runner JSON config file. May also be supplied via
    /// `IRONCLAW_RUNNER_CONFIG`.
    #[arg(long, env = "IRONCLAW_RUNNER_CONFIG")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Same reasoning as ironclaw-host: runner shares stdout with the
    // container poll loop's formatter output; tracing belongs on stderr.
    let use_ansi = std::io::IsTerminal::is_terminal(&std::io::stderr());
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_ansi(use_ansi)
        .init();

    let cli = Cli::parse();
    let env = ironclaw_runner::config::SystemEnv;
    let cfg = RunnerConfig::from_file(&cli.config, &env)
        .with_context(|| format!("load config from {}", cli.config.display()))?;

    let paths = SessionPaths {
        root: cfg.session_dir.clone(),
        inbound_db: cfg.session_dir.join("inbound.db"),
        outbound_db: cfg.session_dir.join("outbound.db"),
        heartbeat: cfg.session_dir.join(".heartbeat"),
        inbox: cfg.session_dir.join("inbox"),
        outbox: cfg.session_dir.join("outbox"),
    };
    paths.ensure_dirs().context("ensure session dir tree")?;

    let inbound = open_inbound_rw_no_mmap(&paths).context("open inbound.db (rw)")?;
    let outbound = open_outbound(&paths).context("open outbound.db (rw)")?;

    let inbound = Arc::new(Mutex::new(inbound));
    let outbound = Arc::new(Mutex::new(outbound));

    let api_key = cfg
        .api_key
        .clone()
        .context("provider api key not set; configure `api_key_env`")?;
    let provider = Arc::new(match cfg.api_base_url.as_deref() {
        Some(base) => AnthropicProvider::with_base_url(api_key, base),
        None => AnthropicProvider::new(api_key),
    });
    let tool_ctx: Arc<dyn ironclaw_mcp::ToolContext> = Arc::new(RunnerToolCtx::new(
        outbound.clone(),
        paths.outbox.clone(),
    ));

    let compaction = CompactionCfg {
        model_input_window: cfg.model_input_window,
        safety_margin_tokens: cfg.safety_margin_tokens,
        summary_model: cfg.model.clone(),
        summary_effort: ironclaw_types::Effort::Low,
        summary_max_tokens: 1024,
        archive_dir: paths.outbox.join("_compactions"),
    };

    // Build the in-process MCP tool inventory once and reuse it on
    // every turn: the `ToolDef` list goes to the provider so the
    // model can call tools, and the `tool_map` lets the runner
    // dispatch the calls back to their handlers against the same
    // `ToolContext` the model sees.
    let tool_set = ironclaw_mcp::build_tool_set();
    let tool_defs: Vec<ironclaw_providers::ToolDef> = tool_set
        .iter()
        .map(|e| ironclaw_providers::ToolDef {
            name: e.tool.name.to_string(),
            description: e
                .tool
                .description
                .as_deref()
                .unwrap_or("")
                .to_string(),
            input_schema: serde_json::Value::Object(
                (*e.tool.input_schema).clone(),
            ),
        })
        .collect();
    let tool_map: std::sync::Arc<
        std::collections::HashMap<String, std::sync::Arc<ironclaw_mcp::ToolEntry>>,
    > = std::sync::Arc::new(
        tool_set
            .into_iter()
            .map(|e| (e.tool.name.to_string(), std::sync::Arc::new(e)))
            .collect(),
    );

    let deps = RunnerDeps {
        provider,
        tool_ctx,
        inbound,
        outbound,
        tools: tool_defs,
        system: cfg.system.clone(),
        model: cfg.model.clone(),
        effort: cfg.effort,
        max_tokens: cfg.max_tokens,
        temperature: cfg.temperature,
        assistant_name: cfg.assistant_name.clone(),
        compaction,
        max_turns: None,
        idle_sleep: std::time::Duration::from_millis(
            ironclaw_runner::POLL_INTERVAL_MS,
        ),
        heartbeat_path: Some(paths.heartbeat.clone()),
        session_id: cfg.session_id,
        agent_group_id: cfg.agent_group_id,
        turn_seq: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
        tool_map,
        max_tool_turns: 20,
        provider_deadline: resolve_provider_deadline(&env),
    };

    tracing::info!(
        session_id = %cfg.session_id,
        agent_group_id = %cfg.agent_group_id,
        model = %cfg.model,
        "ironclaw-runner starting"
    );
    run_loop(deps).await
}
