//! Container-side agent runner for copperclaw.
//!
//! See `PLAN.md` § 6 (T5) for the responsibilities of this crate. In short,
//! the runner is the binary process that lives inside a copperclaw session
//! container. It polls `inbound.db::messages_in`, formats new rows into a
//! provider turn, drives the configured [`AgentProvider`] tool-use loop,
//! and writes the resulting effects into `outbound.db`.
//!
//! The crate is structured as a library with a thin binary wrapper. All of
//! the loop logic lives in [`run_loop`] so it can be exercised against a
//! stubbed provider in tests.
//!
//! [`AgentProvider`]: copperclaw_providers::AgentProvider

pub mod compaction;
pub mod config;
pub mod destinations;
pub mod disallowed;
pub mod formatter;
pub mod run;
pub mod state;
pub mod subagent;
pub mod tools;

pub use compaction::{CompactionCfg, compact, estimate_tokens};
pub use config::{RunnerConfig, RunnerConfigFile};
pub use destinations::{ResolvedRoute, resolve_recipient};
pub use disallowed::{DISALLOWED_TOOLS, is_disallowed};
pub use formatter::{ElisionCfg, FormattedTurn, elide_stale_tool_results, format_messages};
pub use run::{
    ACTIVE_POLL_INTERVAL_MS, DEFAULT_MAX_TOOL_TURNS, DEFAULT_PROVIDER_DEADLINE_MS,
    DEFAULT_TOOL_DEADLINE_SECS, MAX_MAX_TOOL_TURNS, MAX_PROVIDER_DEADLINE_MS,
    MAX_TOOL_DEADLINE_SECS, MAX_TOOL_TURNS_ENV, MIN_MAX_TOOL_TURNS, MIN_PROVIDER_DEADLINE_MS,
    MIN_TOOL_DEADLINE_SECS, POLL_INTERVAL_MS, PROVIDER_DEADLINE_ENV, RunnerDeps, TOOL_DEADLINE_ENV,
    resolve_max_tool_turns, resolve_provider_deadline, resolve_tool_deadline_secs, run_loop,
};
// Production wiring for the typing-indicator-keepalive path: the
// runner binary constructs a HeartbeatPinger so each LLM stream
// refreshes the heartbeat file (and thus the host's typing-ticker
// stays willing to fire) across long provider calls.
pub use run::provider_call::{HeartbeatPinger, NoopPinger, ProviderActivityPinger};
pub use state::{PersistedState, load_state, save_state};
pub use subagent::{
    SUBAGENT_PREAMBLE, SubagentDeps, SubagentInputs, build_subagent_system, run_inner_loop,
};
pub use tools::{RunnerToolCtx, SubagentRunnerDeps};
