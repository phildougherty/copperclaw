//! Container-side agent runner for ironclaw.
//!
//! See `PLAN.md` § 6 (T5) for the responsibilities of this crate. In short,
//! the runner is the binary process that lives inside an ironclaw session
//! container. It polls `inbound.db::messages_in`, formats new rows into a
//! provider turn, drives the configured [`AgentProvider`] tool-use loop,
//! and writes the resulting effects into `outbound.db`.
//!
//! The crate is structured as a library with a thin binary wrapper. All of
//! the loop logic lives in [`run_loop`] so it can be exercised against a
//! stubbed provider in tests.
//!
//! [`AgentProvider`]: ironclaw_providers::AgentProvider

pub mod compaction;
pub mod config;
pub mod destinations;
pub mod disallowed;
pub mod formatter;
pub mod run;
pub mod state;
pub mod subagent;
pub mod tools;

pub use compaction::{compact, estimate_tokens, CompactionCfg};
pub use config::{RunnerConfig, RunnerConfigFile};
pub use destinations::{resolve_recipient, ResolvedRoute};
pub use disallowed::{is_disallowed, DISALLOWED_TOOLS};
pub use formatter::{format_messages, FormattedTurn};
pub use run::{
    resolve_provider_deadline, run_loop, RunnerDeps, ACTIVE_POLL_INTERVAL_MS,
    DEFAULT_PROVIDER_DEADLINE_MS, MAX_PROVIDER_DEADLINE_MS, MIN_PROVIDER_DEADLINE_MS,
    POLL_INTERVAL_MS, PROVIDER_DEADLINE_ENV,
};
pub use state::{load_state, save_state, PersistedState};
pub use subagent::{
    build_subagent_system, run_inner_loop, SubagentDeps, SubagentInputs, SUBAGENT_PREAMBLE,
};
pub use tools::{RunnerToolCtx, SubagentRunnerDeps};
