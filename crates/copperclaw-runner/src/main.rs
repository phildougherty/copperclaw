//! `copperclaw-runner` binary entrypoint.
//!
//! The binary parses CLI args, reads the JSON config file, opens the
//! session's databases, wires up the provider + MCP tool context, and
//! delegates to [`copperclaw_runner::run_loop`].
//!
//! See `PLAN.md` § 6 (T5) for the broader contract.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use copperclaw_db::session::{SessionPaths, open_inbound_rw_no_mmap, open_outbound};
use copperclaw_providers::{AnthropicProvider, CodexProvider, OllamaProvider};
use copperclaw_runner::{
    RunnerConfig, RunnerDeps, RunnerToolCtx, SubagentRunnerDeps, compaction::CompactionCfg,
    resolve_max_task_tokens, resolve_max_tool_turns, resolve_provider_deadline,
    resolve_tool_deadline_secs, run_loop,
};
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "copperclaw-runner",
    version,
    about = "Copperclaw container agent runner"
)]
struct Cli {
    /// Path to the runner JSON config file. May also be supplied via
    /// `COPPERCLAW_RUNNER_CONFIG`.
    #[arg(long, env = "COPPERCLAW_RUNNER_CONFIG")]
    config: PathBuf,
}

#[tokio::main]
#[allow(clippy::too_many_lines)] // single linear startup; splitting hurts clarity.
async fn main() -> Result<()> {
    // Same reasoning as copperclaw-host: runner shares stdout with the
    // container poll loop's formatter output; tracing belongs on stderr.
    let use_ansi = std::io::IsTerminal::is_terminal(&std::io::stderr());
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_ansi(use_ansi)
        .init();

    let cli = Cli::parse();
    let env = copperclaw_runner::config::SystemEnv;
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

    let provider: Arc<dyn copperclaw_providers::AgentProvider> =
        build_provider(&cfg, &env).context("build provider")?;

    let compaction = CompactionCfg {
        model_input_window: cfg.model_input_window,
        safety_margin_tokens: cfg.safety_margin_tokens,
        // Reserve the per-turn output budget so the threshold check
        // never lets `input + max_tokens > window` slip through to the
        // provider (the exact failure mode that crashed Haiku-4.5 with
        // a long transcript).
        output_reserve_tokens: cfg.max_tokens as usize,
        // Soft target pulls compaction far below the hard window so a
        // long session stops replaying a large transcript every turn.
        // `0` (operator opt-out) falls back to hard-window-only.
        soft_target_tokens: cfg.soft_compaction_target_tokens,
        summary_model: cfg.model.clone(),
        summary_effort: copperclaw_types::Effort::Low,
        summary_max_tokens: 1024,
        archive_dir: paths.outbox.join("_compactions"),
    };
    // Per-turn transcript shrinker: stub stale, oversized tool-result
    // bodies so old file reads / command stdout / diffs aren't re-sent
    // verbatim every turn. Recent results (current turn included) stay
    // full. Sourced from the same config knobs.
    let elision = copperclaw_runner::ElisionCfg {
        recent_results_kept: cfg.recent_tool_results_kept,
        max_result_bytes: cfg.tool_result_elide_bytes,
    };

    // Build the in-process MCP tool inventory once and reuse it on
    // every turn: the `ToolDef` list goes to the provider so the
    // model can call tools, and the `tool_map` lets the runner
    // dispatch the calls back to their handlers against the same
    // `ToolContext` the model sees.
    let tool_set = copperclaw_mcp::build_tool_set();
    let tool_defs: Vec<copperclaw_providers::ToolDef> = tool_set
        .iter()
        .map(|e| copperclaw_providers::ToolDef {
            name: e.tool.name.to_string(),
            description: e.tool.description.as_deref().unwrap_or("").to_string(),
            input_schema: serde_json::Value::Object((*e.tool.input_schema).clone()),
        })
        .collect();
    let tool_map: std::sync::Arc<
        std::collections::HashMap<String, std::sync::Arc<copperclaw_mcp::ToolEntry>>,
    > = std::sync::Arc::new(
        tool_set
            .into_iter()
            .map(|e| (e.tool.name.to_string(), std::sync::Arc::new(e)))
            .collect(),
    );

    // Wire the subagent deps onto the ctx so the `explore` tool can
    // open a fresh bounded LLM loop with the same provider, model,
    // and tool inventory the parent runner uses.
    let provider_deadline = resolve_provider_deadline(&env);
    let subagent_deps = SubagentRunnerDeps {
        provider: provider.clone(),
        tool_map: tool_map.clone(),
        system: cfg.system.clone(),
        model: cfg.model.clone(),
        effort: cfg.effort,
        per_turn_max_tokens: cfg.max_tokens,
        temperature: cfg.temperature,
        assistant_name: cfg.assistant_name.clone(),
        provider_deadline,
    };
    let mut tool_ctx_inner = RunnerToolCtx::new(outbound.clone(), paths.outbox.clone())
        .with_subagent(subagent_deps)
        .with_breadcrumbs_from_env();
    if let Some(parent) = cfg.source_session_id {
        tool_ctx_inner = tool_ctx_inner.with_source_session_id(parent);
    }
    let tool_ctx: Arc<dyn copperclaw_mcp::ToolContext> = Arc::new(tool_ctx_inner);

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
        elision,
        max_turns: None,
        idle_sleep: std::time::Duration::from_millis(copperclaw_runner::POLL_INTERVAL_MS),
        heartbeat_path: Some(paths.heartbeat.clone()),
        session_id: cfg.session_id,
        agent_group_id: cfg.agent_group_id,
        turn_seq: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
        tool_map,
        max_tool_turns: resolve_max_tool_turns(&env),
        max_task_tokens: resolve_max_task_tokens(&env),
        provider_deadline,
        tool_deadline_secs: resolve_tool_deadline_secs(&env),
        // Keep the typing indicator alive across long LLM streams by
        // refreshing the heartbeat file on every chunk + every ~3s
        // tick. The host's typing-ticker gates on container=Running
        // (which gates on a fresh heartbeat), so a stale heartbeat
        // would otherwise let the bubble vanish mid-response.
        activity_pinger: std::sync::Arc::new(copperclaw_runner::HeartbeatPinger::new(Some(
            paths.heartbeat.clone(),
        ))),
        // Slice-3.5 opt-in: surface the model's reasoning blocks as
        // collapsed native UI primitives when the host has the
        // per-group `surface_thinking` flag on. Default-off keeps the
        // historical "drop on the floor" behaviour for existing groups.
        surface_thinking: cfg.surface_thinking,
    };

    tracing::info!(
        session_id = %cfg.session_id,
        agent_group_id = %cfg.agent_group_id,
        provider = %cfg.provider,
        model = %cfg.model,
        "copperclaw-runner starting"
    );
    run_loop(deps).await
}

/// Build the [`copperclaw_providers::AgentProvider`] dictated by
/// `cfg.provider`. Anthropic uses `cfg.api_key` + optional
/// `cfg.api_base_url`. Ollama (native) reads `OLLAMA_BASE_URL` from the
/// container env (defaults to `http://localhost:11434` — only useful if
/// the operator binds the host's Ollama into the container; the more
/// common deployment is to set `OLLAMA_BASE_URL` to a reachable URL).
/// `ollama-shim` keeps the historical Anthropic-shaped proxy flow.
/// `codex` spawns the configured Codex CLI binary as a one-turn-per-spawn
/// subprocess. The binary path and extra args are sourced (in order)
/// from `cfg.codex_binary` / `cfg.codex_args`, then the
/// `COPPERCLAW_CODEX_BINARY` / `COPPERCLAW_CODEX_ARGS` env vars, then the
/// `/usr/local/bin/codex` / `--json` defaults.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn build_provider(
    cfg: &RunnerConfig,
    env: &dyn copperclaw_runner::config::EnvLookup,
) -> Result<Arc<dyn copperclaw_providers::AgentProvider>> {
    match cfg.provider.as_str() {
        "ollama" => {
            let base_url = env
                .get("OLLAMA_BASE_URL")
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            let model = if cfg.model.is_empty() {
                None
            } else {
                Some(cfg.model.clone())
            };
            Ok(Arc::new(OllamaProvider::new(base_url, model)))
        }
        "ollama-shim" => {
            let base_url = cfg
                .api_base_url
                .clone()
                .or_else(|| env.get("OLLAMA_BASE_URL"))
                .context("ollama-shim requires api_base_url or OLLAMA_BASE_URL pointing at the Anthropic-shaped proxy")?;
            let model = if cfg.model.is_empty() {
                None
            } else {
                Some(cfg.model.clone())
            };
            Ok(Arc::new(OllamaProvider::shim(base_url, model)))
        }
        "codex" => {
            let binary = cfg
                .codex_binary
                .clone()
                .or_else(|| env.get("COPPERCLAW_CODEX_BINARY"))
                .unwrap_or_else(|| "/usr/local/bin/codex".to_string());
            let args =
                cfg.codex_args
                    .clone()
                    .unwrap_or_else(|| match env.get("COPPERCLAW_CODEX_ARGS") {
                        Some(raw) => raw
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                            .collect::<Vec<_>>(),
                        None => vec!["--json".to_string()],
                    });
            Ok(Arc::new(CodexProvider::new(
                std::path::PathBuf::from(binary),
                args,
            )))
        }
        _ => {
            let api_key = cfg
                .api_key
                .clone()
                .context("provider api key not set; configure `api_key_env`")?;
            Ok(Arc::new(match cfg.api_base_url.as_deref() {
                Some(base) => AnthropicProvider::with_base_url(api_key, base),
                None => AnthropicProvider::new(api_key),
            }))
        }
    }
}

#[cfg(test)]
mod build_provider_tests {
    use super::*;
    use copperclaw_runner::config::MapEnv;
    use copperclaw_types::{AgentGroupId, Effort, SessionId};

    fn base_cfg(provider: &str) -> RunnerConfig {
        RunnerConfig {
            session_id: SessionId(uuid::Uuid::nil()),
            agent_group_id: AgentGroupId::new(),
            session_dir: std::path::PathBuf::from("/tmp/copperclaw/test"),
            provider: provider.to_string(),
            model: "test-model".to_string(),
            effort: Effort::Medium,
            system: String::new(),
            api_key: None,
            api_base_url: None,
            model_input_window: 200_000,
            safety_margin_tokens: 8_000,
            soft_compaction_target_tokens: copperclaw_runner::compaction::DEFAULT_SOFT_TARGET,
            recent_tool_results_kept: copperclaw_runner::formatter::DEFAULT_RECENT_TOOL_RESULTS,
            tool_result_elide_bytes: copperclaw_runner::formatter::DEFAULT_TOOL_RESULT_ELIDE_BYTES,
            max_tokens: 4096,
            assistant_name: None,
            temperature: None,
            codex_binary: None,
            codex_args: None,
            source_session_id: None,
            surface_thinking: false,
        }
    }

    #[test]
    fn codex_uses_explicit_config_fields() {
        let mut cfg = base_cfg("codex");
        cfg.codex_binary = Some("/explicit/codex".into());
        cfg.codex_args = Some(vec!["--json".into(), "--quiet".into()]);
        let env = MapEnv::default();
        let p = build_provider(&cfg, &env).unwrap();
        assert_eq!(p.name(), "codex");
    }

    #[test]
    fn codex_falls_back_to_env_vars() {
        let cfg = base_cfg("codex");
        let env = MapEnv::from_pairs([
            ("COPPERCLAW_CODEX_BINARY", "/env/codex"),
            ("COPPERCLAW_CODEX_ARGS", "--json,--no-color"),
        ]);
        let p = build_provider(&cfg, &env).unwrap();
        assert_eq!(p.name(), "codex");
    }

    #[test]
    fn codex_falls_back_to_defaults() {
        let cfg = base_cfg("codex");
        let env = MapEnv::default();
        let p = build_provider(&cfg, &env).unwrap();
        // No binary configured anywhere => provider still constructed
        // with the hard-coded default path. Name confirms the arm took.
        assert_eq!(p.name(), "codex");
    }

    #[test]
    fn anthropic_requires_api_key() {
        let cfg = base_cfg("anthropic");
        let env = MapEnv::default();
        // `Arc<dyn AgentProvider>` doesn't implement Debug, so we can't
        // use `.unwrap_err()`; collapse to a Debug-able String first.
        let result = build_provider(&cfg, &env)
            .map(|_| "ok")
            .map_err(|e| e.to_string());
        let err = result.unwrap_err();
        assert!(err.contains("api key"));
    }
}
