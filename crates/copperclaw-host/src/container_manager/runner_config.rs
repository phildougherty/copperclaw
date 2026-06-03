//! Host-side mirror of the runner's `runner.json` schema, plus the assembler
//! that builds one for a given session.

use super::ContainerManager;
use super::config::SkillsMode;
use super::prompt::{
    SKILLS_CATALOGUE_FILENAME, SkillCatalogueEntry, assemble_system_prompt_with_catalogue,
    db_selector_to_skills_selector, remove_stale_catalogue, select_callable_skills,
};
use super::spawn::{CODING_SKILL_NAMES, CONTAINER_SESSION_DIR};
use copperclaw_db::session::{SessionPaths, open_inbound};
use copperclaw_db::tables::{
    agent_group_members, container_configs, messages_in, user_roles, users,
};
use copperclaw_modules::permissions::Role;
use copperclaw_types::Session;
use std::path::Path;
use tracing::warn;

/// `RunnerConfigFile` lives in `copperclaw-runner`, but pulling the
/// runner crate into the host as a non-test dep would create a
/// circular trail (the runner crate already pulls in `copperclaw-mcp`
/// and `copperclaw-providers`, both of which the host doesn't otherwise
/// need at runtime). Mirror the on-disk schema here — there's
/// exactly one consumer and it's a JSON file, so the duplication is
/// cheap.
#[derive(Debug, serde::Serialize)]
pub(crate) struct RunnerConfigForFile {
    pub(crate) session_id: String,
    pub(crate) agent_group_id: String,
    pub(crate) session_dir: String,
    /// Provider kind, e.g. `"anthropic"`, `"ollama"`, `"ollama-shim"`,
    /// `"codex"`. When unset the runner falls back to `"anthropic"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) provider: Option<String>,
    pub(crate) model: String,
    pub(crate) system: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) api_base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) assistant_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_messages_per_prompt: Option<u32>,
    /// Absolute path to the Codex CLI binary inside the container.
    /// Only set when `provider == "codex"`; sourced from the
    /// `COPPERCLAW_CODEX_BINARY` rotatable env var (no per-group
    /// override column yet — that's intentional for this slice).
    /// Falls back to the runner's `/usr/local/bin/codex` default
    /// when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) codex_binary: Option<String>,
    /// Extra args appended to every Codex spawn. Only set when
    /// `provider == "codex"`; sourced from `COPPERCLAW_CODEX_ARGS`
    /// (comma-separated). Falls back to the runner's `["--json"]`
    /// default when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) codex_args: Option<Vec<String>>,
    /// Parent session id for child agents created via `create_agent`.
    /// Threads `sessions.source_session_id` (set by
    /// `CreateAgentHandler`) into the runner so its `send_message`
    /// default routes back to the parent's inbound rather than the
    /// inherited messaging-group channel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_session_id: Option<String>,
    /// Slice-3.5 opt-in: surface the model's reasoning blocks as
    /// collapsed native UI primitives. Plumbed in from
    /// `container_configs.surface_thinking`. Skipped when None /
    /// default so existing runner config files don't gain a new
    /// always-present field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) surface_thinking: Option<bool>,
    /// Per-group tool-authorization profile (`minimal` / `messaging` /
    /// `coding` / `full`). The FUEL for the runner's layered `ToolPolicy`:
    /// plumbed in from `container_configs.tool_profile`, parsed by the
    /// runner into the positive allow-list ceiling enforced at every tool
    /// dispatch (beneath the host-owned `DISALLOWED_TOOLS` floor). Skipped
    /// when None / unset so an unconfigured group's `runner.json` shape
    /// stays bit-identical to the pre-fuel shape — the runner then falls
    /// back to its permissive `full` default, preserving the historical
    /// full tool surface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_profile: Option<String>,
    /// Resolved RBAC role of the sender that triggered this session
    /// (`minimal` taxonomy: `admin` / `member` / `guest`). Plumbed in
    /// from the host's permissions resolution (see
    /// [`ContainerManager::resolve_sender_role`]). The runner applies the
    /// per-role floor on top of the group profile: a `guest` sender is
    /// held to a read-only floor (no shell / file-mutation / self-mod)
    /// regardless of profile. Skipped when None (no resolvable sender /
    /// unconfigured deployment) so the runner applies no role floor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sender_role: Option<String>,
    /// Reasoning-effort tier (`low`/`medium`/`high`). Mapped onto each
    /// provider's native knob: `OpenRouter`'s unified API takes it as
    /// `reasoning: { effort: ... }` (forwarded to any underlying
    /// reasoning-capable model — `DeepSeek` R1, `OpenAI` o-series, etc.).
    /// Anthropic's native API ignores it.
    ///
    /// Sourced from per-group `container_configs.effort` when set,
    /// then `ManagerConfig::default_effort` (set from the host's
    /// `COPPERCLAW_DEFAULT_EFFORT` env var at boot). Skipped when None
    /// so a "use the model's default" deployment doesn't gain a new
    /// always-present field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) effort: Option<copperclaw_types::Effort>,
    /// Sampling temperature passed through to the provider. Sourced from
    /// `COPPERCLAW_DEFAULT_TEMPERATURE` in `.env`; a low value (~0.3)
    /// steadies agentic tool-calling on local models. Skipped when None
    /// so the model/provider default is left untouched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) temperature: Option<f32>,
    /// Per-turn output-token cap passed to the provider. Sourced from
    /// `COPPERCLAW_DEFAULT_MAX_TOKENS` in `.env`; raise it for large
    /// edits and reasoning models that spend budget thinking. Skipped
    /// when None, leaving the runner's built-in default (4096).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_tokens: Option<u32>,
}

impl ContainerManager {
    #[allow(clippy::too_many_lines)] // single linear assembly; provider/skills branches read more clearly inline
    pub(super) fn runner_config_for(
        &self,
        session: &Session,
        cc: Option<&container_configs::ContainerConfig>,
        session_root: Option<&Path>,
    ) -> RunnerConfigForFile {
        // Resolve the provider (precedence + alias normalisation) via the
        // shared helper so the runner config and the egress auto-inject can
        // never disagree about which endpoint the group talks to. Unknown
        // values pass through (the runner logs + falls back to anthropic);
        // an empty value is treated as "use the default" so an empty
        // default_provider doesn't leak into the JSON file.
        let provider = self.resolved_provider(session, cc);

        let model = cc
            .and_then(|c| c.model.clone())
            .unwrap_or_else(|| self.cfg.default_model.clone());
        let assistant_name = cc.and_then(|c| c.assistant_name.clone());
        let max_messages = cc.and_then(|c| c.max_messages_per_prompt);
        let selector = cc.map_or(copperclaw_skills::SkillsSelector::All, |c| {
            db_selector_to_skills_selector(&c.skills)
        });
        // The coding-skills cap. When the per-group `coding_enabled`
        // flag is false (the default) AND the selector is the catch-all
        // `All`, the four coding bundle skills are filtered from the
        // resolved set. Explicit selector lists are honoured as-is —
        // an operator who listed a coding skill by name picked it
        // deliberately. New groups (`cc == None`) get the same default
        // "off" behaviour.
        let coding_enabled = cc.is_some_and(|c| c.coding_enabled);
        let exclude_coding =
            !coding_enabled && matches!(selector, copperclaw_skills::SkillsSelector::All);
        let exclude_names: &[&str] = if exclude_coding {
            CODING_SKILL_NAMES
        } else {
            &[]
        };
        let now = chrono::Utc::now();

        // Build the Callable-mode catalogue first (if applicable) so we
        // can (a) reuse it to render the prompt index — making the
        // on-disk `skills.json` and the in-prompt catalogue the same
        // data — and (b) fall back to Inline-mode prompt assembly if
        // the catalogue write fails, so the agent isn't left holding a
        // Callable prompt that points at a missing file.
        let mut effective_mode = self.cfg.skills_mode;
        let mut catalogue_for_prompt: Option<Vec<SkillCatalogueEntry>> = None;
        if let (SkillsMode::Callable, Some(root)) = (self.cfg.skills_mode, session_root) {
            let entries = select_callable_skills(
                self.cfg.skills_dir.as_deref(),
                self.cfg.groups_dir.as_deref(),
                session.agent_group_id,
                &selector,
                exclude_names,
            );
            let path = root.join(SKILLS_CATALOGUE_FILENAME);
            if entries.is_empty() {
                // Nothing to advertise; behave like Inline for this spawn
                // so `load_skill` is not in the prompt at all.
                effective_mode = SkillsMode::Inline;
                remove_stale_catalogue(&path);
            } else {
                match serde_json::to_vec_pretty(&entries) {
                    Ok(json) => match std::fs::write(&path, json) {
                        Ok(()) => catalogue_for_prompt = Some(entries),
                        Err(err) => {
                            warn!(
                                ?err,
                                path = %path.display(),
                                "could not write skills catalogue; falling back to inline-skills prompt"
                            );
                            effective_mode = SkillsMode::Inline;
                            remove_stale_catalogue(&path);
                        }
                    },
                    Err(err) => {
                        warn!(
                            ?err,
                            "could not serialise skills catalogue; falling back to inline-skills prompt"
                        );
                        effective_mode = SkillsMode::Inline;
                        remove_stale_catalogue(&path);
                    }
                }
            }
        } else if let Some(root) = session_root {
            // Inline mode (either configured or fallen-back-to): drop
            // any catalogue from a previous spawn so `load_skill` never
            // reads stale bodies that don't match the inlined prompt.
            remove_stale_catalogue(&root.join(SKILLS_CATALOGUE_FILENAME));
        }

        // Snapshot the central DB's tasks for this session into
        // `<session_root>/tasks.json`. The runner can't reach the central
        // DB from inside its container, so this snapshot is the host-side
        // half of the `list_tasks` MCP tool. Refreshed on every spawn;
        // for live updates within a long-running session see the sweep
        // loop hook.
        if let Some(root) = session_root {
            super::tasks_snapshot::write_tasks_snapshot(&self.central, session.id, root);
            // Drop a `.host_path` file at the session root containing the
            // absolute host path of the bind-mount source. The
            // `artifact_path` MCP tool reads it so the agent can tell the
            // operator where to find files it wrote under /data. Without
            // this, every "I built X" turn dead-ends because the user
            // can't actually reach the artifacts.
            let host_path_marker = root.join(".host_path");
            if let Err(err) = std::fs::write(&host_path_marker, root.to_string_lossy().as_bytes()) {
                warn!(
                    ?err,
                    path = %host_path_marker.display(),
                    "could not write .host_path discovery file; artifact_path tool will error"
                );
            }
        }

        let system = assemble_system_prompt_with_catalogue(
            self.cfg.skills_dir.as_deref(),
            self.cfg.groups_dir.as_deref(),
            session.agent_group_id,
            &selector,
            session_root,
            session.id,
            now,
            assistant_name.as_deref(),
            effective_mode,
            catalogue_for_prompt.as_deref(),
            exclude_names,
        );

        // Pick the api_key_env that matches the wire format. Ollama
        // native doesn't authenticate (or uses its own bearer in front
        // of a proxy); the runner accepts a missing api_key when
        // provider=ollama. Codex talks to a local CLI subprocess that
        // brokers its own auth (the operator pre-authenticates the
        // binary outside Copperclaw), so it also doesn't need
        // ANTHROPIC_API_KEY. Ollama-shim talks the Anthropic envelope
        // at a proxy that may or may not require a key — keep
        // ANTHROPIC_API_KEY so the operator can set one if they need it.
        let api_key_env: Option<String> = match provider.as_deref() {
            Some("ollama" | "codex") => None,
            _ => Some("ANTHROPIC_API_KEY".to_string()),
        };

        // For ollama-shim we route api_base_url to OLLAMA_BASE_URL (or
        // leave it None and let the runner read OLLAMA_BASE_URL from the
        // container env). For native ollama, api_base_url is irrelevant
        // — the runner reads OLLAMA_BASE_URL directly. Codex spawns a
        // subprocess and has no HTTP base URL at all.
        //
        // For the Anthropic-envelope providers (anthropic / claude /
        // ollama-shim-via-ANTHROPIC) the endpoint is the single source of
        // truth shared with `build_spec`: when the credential broker is
        // enabled for this session, the runner MUST hit the host-side broker
        // loopback (not the operator's real `ANTHROPIC_BASE_URL`). The runner
        // prefers `runner.json`'s `api_base_url` over the `ANTHROPIC_BASE_URL`
        // env var, so writing the operator URL here would silently bypass the
        // broker (the runner would talk straight to the real endpoint with the
        // capability token, which the real endpoint rejects). The real
        // upstream URL lives in the broker config and is what the broker
        // forwards to host-side with the real key. When the broker is disabled
        // (the default) we keep the historical behaviour: the rotatable real
        // base URL.
        let api_base_url = match provider.as_deref() {
            Some("ollama" | "ollama-shim" | "codex") => None,
            // Mirror `build_spec`'s broker gate exactly: the broker is "enabled"
            // only when both the broker state AND its loopback base URL are
            // wired in (they are set together by `with_broker`). Gating on both
            // means this path and the env-var path in `build_spec` can never
            // disagree about which endpoint the runner hits.
            _ => self
                .broker
                .as_ref()
                .and(self.broker_base_url.clone())
                .or_else(|| {
                    self.rotatable
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .anthropic_base_url
                        .clone()
                }),
        };

        // Codex binary + args. Sourced from the rotatable forward_env
        // map so an operator can edit `.env` + SIGHUP to swap binaries
        // without restarting the host. Per-group `container_config`
        // columns are a future enhancement; the env-var override is
        // intentionally global for this slice. When neither field is
        // set the runner falls back to its `/usr/local/bin/codex` /
        // `["--json"]` defaults.
        let (codex_binary, codex_args) = if provider.as_deref() == Some("codex") {
            let rotatable = self
                .rotatable
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let codex_binary = rotatable
                .forward_env
                .iter()
                .find(|(k, _)| k == "COPPERCLAW_CODEX_BINARY")
                .map(|(_, v)| v.clone());
            let codex_args = rotatable
                .forward_env
                .iter()
                .find(|(k, _)| k == "COPPERCLAW_CODEX_ARGS")
                .map(|(_, v)| {
                    v.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                });
            (codex_binary, codex_args)
        } else {
            (None, None)
        };

        // Plumb the per-group slice-3.5 `surface_thinking` flag into
        // the runner's JSON config file. Skip emitting the field when
        // off (the default) so the runner_config.json shape for groups
        // that never opt in stays bit-identical to the pre-3.5 shape.
        let surface_thinking = cc
            .map(|c| c.surface_thinking)
            .filter(|on| *on)
            .map(|_| true);

        // Plumb the per-group tool-profile (the FUEL for the runner's
        // layered ToolPolicy) into `runner.json`. Only emit a recognised
        // profile name; an unknown / malformed value is dropped so the
        // runner falls back to its permissive `full` default rather than
        // mis-parsing. Skip emitting when unset for the same reason
        // `surface_thinking` is skipped: keep the unconfigured-group
        // `runner.json` shape stable.
        let tool_profile = cc.and_then(|c| c.tool_profile.as_deref()).and_then(|p| {
            let parsed = copperclaw_modules::permissions::ToolProfile::parse(p);
            if parsed.is_none() {
                warn!(
                    agent_group = %session.agent_group_id.as_uuid(),
                    tool_profile = p,
                    "unknown tool_profile in container config; runner will fall back to `full`"
                );
            }
            parsed.map(|profile| profile.as_str().to_string())
        });

        // Resolve the triggering sender's RBAC role and plumb it into
        // `runner.json` so the runner applies the per-role floor (a guest
        // is held read-only regardless of profile). Only attempted on the
        // real spawn path (`session_root.is_some()`); test callers that
        // pass `None` skip it (→ no role floor). Best-effort: an
        // unresolvable sender (unknown channel / no `users` row) yields
        // `None` so unconfigured deployments keep their historical
        // behaviour.
        let sender_role = session_root
            .is_some()
            .then(|| self.resolve_sender_role(session))
            .flatten();

        // Effort precedence: per-group `container_configs.effort` (when
        // present) wins over the host-wide `default_effort` (which the
        // operator sets via `COPPERCLAW_DEFAULT_EFFORT` in `.env`).
        // `Medium` is treated as "no override" so the wire-level
        // `reasoning.effort` field is only emitted when the operator
        // deliberately picked low or high. (Most providers default to
        // their own "medium" already, so emitting it is just noise.)
        let effort = cc
            .and_then(|c| c.effort)
            .or(self.cfg.default_effort)
            .filter(|e| !matches!(e, copperclaw_types::Effort::Medium));

        // Host-wide sampling temperature from `COPPERCLAW_DEFAULT_TEMPERATURE`
        // (`.env`). Low (~0.3) makes tool-calling steadier on local
        // models; unset leaves the provider/model default in place.
        let temperature = std::env::var("COPPERCLAW_DEFAULT_TEMPERATURE")
            .ok()
            .and_then(|s| s.trim().parse::<f32>().ok());

        // Host-wide per-turn output-token cap from
        // `COPPERCLAW_DEFAULT_MAX_TOKENS` (`.env`). Unset → the runner's
        // 4096 default; raise it for large edits / reasoning models.
        let max_tokens = std::env::var("COPPERCLAW_DEFAULT_MAX_TOKENS")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());

        RunnerConfigForFile {
            session_id: session.id.as_uuid().to_string(),
            agent_group_id: session.agent_group_id.as_uuid().to_string(),
            // The container always sees its session dir at `/data` —
            // that's where the bind mount lands and where the runner
            // looks for `inbound.db`/`outbound.db`.
            session_dir: CONTAINER_SESSION_DIR.to_string(),
            provider,
            model,
            system,
            api_key_env,
            api_base_url,
            assistant_name,
            max_messages_per_prompt: max_messages,
            codex_binary,
            codex_args,
            source_session_id: session.source_session_id.map(|s| s.as_uuid().to_string()),
            surface_thinking,
            tool_profile,
            sender_role,
            effort,
            temperature,
            max_tokens,
        }
    }

    /// Resolve the RBAC role of the sender that triggered this session,
    /// for the runner's tool-policy role floor.
    ///
    /// Reads the session's most-recent pending inbound to find the
    /// triggering sender's `(channel_type, platform_id)`, derives the
    /// canonical `UserId`, then maps the host's RBAC tables onto the
    /// runner-side role taxonomy:
    ///
    /// - an Owner/Admin grant in `user_roles` (global or scoped to this
    ///   group) → [`Role::Admin`]
    /// - else a member of this agent group (`agent_group_members`) →
    ///   [`Role::Member`]
    /// - else a sender that maps to a known `users` row → [`Role::Guest`]
    /// - else (no triggering sender resolvable / no `users` row) → `None`
    ///
    /// Returning `None` means "no role floor": the group profile + the
    /// host-owned tool floor still apply. This keeps an unconfigured
    /// deployment (no roles, no members) running exactly as before —
    /// the floor only narrows once an operator actually grants roles or
    /// adds members.
    ///
    /// Best-effort: any DB error along the way logs a WARN and resolves
    /// to `None` rather than blocking the spawn.
    fn resolve_sender_role(&self, session: &Session) -> Option<String> {
        let paths = SessionPaths::new(&self.cfg.data_dir, session.agent_group_id, session.id);
        let conn = match open_inbound(&paths) {
            Ok(c) => c,
            Err(err) => {
                warn!(session = %session.id.as_uuid(), ?err, "sender-role: open inbound failed");
                return None;
            }
        };
        // `first_poll = true` so a wake-only inbound is considered; the
        // newest pending row (seq DESC) is the sender that triggered this
        // spawn. Agent-to-agent rows carry no channel sender, so they
        // resolve to `None` (no role floor) — correct for inter-agent
        // traffic, which the parent's own profile already bounds.
        let pending = match messages_in::get_pending(&conn, true, 1) {
            Ok(rows) => rows,
            Err(err) => {
                warn!(session = %session.id.as_uuid(), ?err, "sender-role: read pending failed");
                return None;
            }
        };
        let row = pending.first()?;
        let channel_type = row.channel_type.as_ref()?;
        let platform_id = row.platform_id.as_deref()?;
        let user = match users::get_by_identity(&self.central, channel_type.as_str(), platform_id) {
            Ok(Some(u)) => u,
            Ok(None) => return None,
            Err(err) => {
                warn!(session = %session.id.as_uuid(), ?err, "sender-role: user lookup failed");
                return None;
            }
        };
        let role = self.role_for_user(user.id, session.agent_group_id);
        Some(role.as_str().to_string())
    }

    /// Map the host RBAC tables onto the runner-side [`Role`] taxonomy for
    /// a resolved user. See [`Self::resolve_sender_role`] for the ladder.
    fn role_for_user(
        &self,
        user_id: copperclaw_types::UserId,
        agent_group_id: copperclaw_types::AgentGroupId,
    ) -> Role {
        // Owner/Admin grant (any scope) maps to module Admin.
        if let Ok(grants) = user_roles::list_for_user(&self.central, user_id) {
            let is_admin = grants.iter().any(|g| {
                matches!(g.role, user_roles::Role::Owner | user_roles::Role::Admin)
                    && (g.agent_group_id.is_none() || g.agent_group_id == Some(agent_group_id))
            });
            if is_admin {
                return Role::Admin;
            }
        }
        // Member of this agent group → Member.
        if let Ok(members) = agent_group_members::list(&self.central, agent_group_id) {
            if members.iter().any(|m| m.user_id == user_id) {
                return Role::Member;
            }
        }
        // Known sender with no elevated grant / membership → Guest.
        Role::Guest
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::{ManagerConfig, SkillsMode};
    use super::super::spawn::{
        CONTAINER_SESSION_DIR, DEFAULT_HEARTBEAT_STALE_SECS, DEFAULT_IDLE_TIMEOUT_SECS,
        DEFAULT_STOP_GRACE_SECS,
    };
    use super::*;
    use copperclaw_db::central::CentralDb;
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::sessions::{CreateSession, create as create_session};
    use copperclaw_types::AgentGroupId;
    use std::path::PathBuf;

    fn manager_cfg(data_dir: PathBuf) -> ManagerConfig {
        ManagerConfig {
            install_slug: "test".into(),
            data_dir,
            default_image_tag: "copperclaw/session:test".into(),
            default_provider: "anthropic".into(),
            default_model: "claude-sonnet-4-6".into(),
            default_effort: None,
            anthropic_api_key: Some("sk-test".into()),
            anthropic_base_url: Some("https://openrouter.ai/api/v1".into()),
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            heartbeat_stale_secs: DEFAULT_HEARTBEAT_STALE_SECS,
            stop_grace_secs: DEFAULT_STOP_GRACE_SECS,
            skills_dir: None,
            groups_dir: None,
            skills_mode: SkillsMode::Inline,
            gpu_passthrough: false,
            forward_env: Vec::new(),
            egress_mode: copperclaw_container_rt::EgressMode::AllowAll,
        }
    }

    fn fixture_session(db: &CentralDb) -> Session {
        let ag = create_ag(
            db,
            CreateAgentGroup {
                name: "demo".into(),
                folder: "demo".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        create_session(
            db,
            CreateSession {
                agent_group_id: ag.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap()
    }

    fn write_skill_md(parent: &std::path::Path, name: &str, body: &str) {
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!("---\nname: {name}\ndescription: desc-of-{name}\n---\n\n{body}");
        std::fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn runner_config_uses_session_then_container_config_then_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let cfg = mgr.runner_config_for(&session, None, None);
        assert_eq!(cfg.model, "claude-sonnet-4-6");
        assert_eq!(cfg.session_dir, CONTAINER_SESSION_DIR);
        assert_eq!(cfg.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(
            cfg.api_base_url.as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
    }

    /// Per-group `container_config.provider = "ollama"` must reach the
    /// runner-config file so the runner builds an Ollama provider rather
    /// than the default Anthropic one. Caught regression: the old
    /// `let _ = provider;` line silently swallowed the field and the
    /// runner ignored every per-group choice.
    #[test]
    fn runner_config_propagates_ollama_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let cc = container_configs::ContainerConfig {
            agent_group_id: session.agent_group_id,
            provider: Some("ollama".into()),
            model: Some("llama3.1:8b".into()),
            effort: None,
            image_tag: None,
            assistant_name: None,
            max_messages_per_prompt: None,
            skills: container_configs::SkillsSelector::All,
            mcp_servers: serde_json::json!({}),
            packages_apt: vec![],
            packages_npm: vec![],
            additional_mounts: serde_json::json!([]),
            cli_scope: container_configs::CliScope::Group,
            config_fingerprint: None,
            egress_allow: vec![],
            resource_limits: serde_json::json!({}),
            coding_enabled: false,
            surface_thinking: false,
            tool_profile: None,
            updated_at: chrono::Utc::now(),
        };
        let cfg = mgr.runner_config_for(&session, Some(&cc), None);
        assert_eq!(cfg.provider.as_deref(), Some("ollama"));
        assert_eq!(cfg.model, "llama3.1:8b");
        // Ollama native doesn't authenticate via ANTHROPIC_API_KEY,
        // so the runner must not be told to pull one.
        assert!(cfg.api_key_env.is_none());
        // And the per-rotatable Anthropic base URL is irrelevant —
        // we must not leak it into an ollama config.
        assert!(cfg.api_base_url.is_none());
    }

    /// Per-group `container_config.provider = "codex"` must reach the
    /// runner-config file so the runner spawns the Codex CLI rather
    /// than the default Anthropic provider. Asserts the no-HTTP shape:
    /// no `api_key_env`, no `api_base_url`, and that the runner's
    /// `COPPERCLAW_CODEX_BINARY` / `COPPERCLAW_CODEX_ARGS` overrides reach
    /// the file when set on the rotatable env.
    #[test]
    fn runner_config_propagates_codex_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut mc = manager_cfg(tmp.path().to_path_buf());
        mc.forward_env.push((
            "COPPERCLAW_CODEX_BINARY".into(),
            "/opt/codex/bin/codex".into(),
        ));
        mc.forward_env
            .push(("COPPERCLAW_CODEX_ARGS".into(), "--json,--quiet".into()));
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            mc,
        );
        let session = fixture_session(&db);
        let cc = container_configs::ContainerConfig {
            agent_group_id: session.agent_group_id,
            provider: Some("codex".into()),
            model: Some("codex-default".into()),
            effort: None,
            image_tag: None,
            assistant_name: None,
            max_messages_per_prompt: None,
            skills: container_configs::SkillsSelector::All,
            mcp_servers: serde_json::json!({}),
            packages_apt: vec![],
            packages_npm: vec![],
            additional_mounts: serde_json::json!([]),
            cli_scope: container_configs::CliScope::Group,
            config_fingerprint: None,
            egress_allow: vec![],
            resource_limits: serde_json::json!({}),
            coding_enabled: false,
            surface_thinking: false,
            tool_profile: None,
            updated_at: chrono::Utc::now(),
        };
        let cfg = mgr.runner_config_for(&session, Some(&cc), None);
        assert_eq!(cfg.provider.as_deref(), Some("codex"));
        // Subprocess provider does not use ANTHROPIC_API_KEY.
        assert!(cfg.api_key_env.is_none());
        // No HTTP base URL — Codex is a local subprocess.
        assert!(cfg.api_base_url.is_none());
        // Env-supplied codex overrides land in the JSON file.
        assert_eq!(cfg.codex_binary.as_deref(), Some("/opt/codex/bin/codex"));
        assert_eq!(
            cfg.codex_args.as_deref(),
            Some(&["--json".to_string(), "--quiet".to_string()][..])
        );
    }

    /// When `COPPERCLAW_CODEX_BINARY` / `COPPERCLAW_CODEX_ARGS` are unset,
    /// the host writes `None` for both fields and the runner picks up
    /// its hard-coded defaults (`/usr/local/bin/codex`, `["--json"]`).
    #[test]
    fn runner_config_codex_omits_overrides_when_env_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let cc = container_configs::ContainerConfig {
            agent_group_id: session.agent_group_id,
            provider: Some("codex".into()),
            model: Some("codex-default".into()),
            effort: None,
            image_tag: None,
            assistant_name: None,
            max_messages_per_prompt: None,
            skills: container_configs::SkillsSelector::All,
            mcp_servers: serde_json::json!({}),
            packages_apt: vec![],
            packages_npm: vec![],
            additional_mounts: serde_json::json!([]),
            cli_scope: container_configs::CliScope::Group,
            config_fingerprint: None,
            egress_allow: vec![],
            resource_limits: serde_json::json!({}),
            coding_enabled: false,
            surface_thinking: false,
            tool_profile: None,
            updated_at: chrono::Utc::now(),
        };
        let cfg = mgr.runner_config_for(&session, Some(&cc), None);
        assert_eq!(cfg.provider.as_deref(), Some("codex"));
        assert!(cfg.api_key_env.is_none());
        assert!(cfg.api_base_url.is_none());
        assert!(cfg.codex_binary.is_none());
        assert!(cfg.codex_args.is_none());
    }

    /// `claude` is an alias for `anthropic` — both must still resolve
    /// to `api_key_env=ANTHROPIC_API_KEY` and the rotatable base URL.
    #[test]
    fn runner_config_claude_alias_resolves_to_anthropic_envelope() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let cc = container_configs::ContainerConfig {
            agent_group_id: session.agent_group_id,
            provider: Some("claude".into()),
            model: None,
            effort: None,
            image_tag: None,
            assistant_name: None,
            max_messages_per_prompt: None,
            skills: container_configs::SkillsSelector::All,
            mcp_servers: serde_json::json!({}),
            packages_apt: vec![],
            packages_npm: vec![],
            additional_mounts: serde_json::json!([]),
            cli_scope: container_configs::CliScope::Group,
            config_fingerprint: None,
            egress_allow: vec![],
            resource_limits: serde_json::json!({}),
            coding_enabled: false,
            surface_thinking: false,
            tool_profile: None,
            updated_at: chrono::Utc::now(),
        };
        let cfg = mgr.runner_config_for(&session, Some(&cc), None);
        assert_eq!(cfg.provider.as_deref(), Some("anthropic"));
        assert_eq!(cfg.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert!(cfg.api_base_url.is_some());
    }

    // ----- Credential broker: runner.json endpoint redirection ----------------

    /// HIGH-bug regression: when the credential broker is ENABLED and the
    /// operator has set `ANTHROPIC_BASE_URL` (the common `OpenRouter`
    /// deployment — `manager_cfg` sets it to `https://openrouter.ai/api/v1`),
    /// `runner.json`'s `api_base_url` MUST point at the broker loopback, not
    /// the operator's real URL. The runner prefers `runner.json`'s
    /// `api_base_url` over the `ANTHROPIC_BASE_URL` env var, so writing the
    /// operator URL here silently bypassed the broker: the runner talked
    /// straight to the real endpoint with the capability token (which the real
    /// endpoint rejects). This pins the redirection that closes the bypass.
    #[test]
    fn runner_config_with_broker_enabled_points_api_base_url_at_broker_loopback() {
        use super::super::broker::{BrokerConfig, BrokerState};

        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        // Real upstream key + a non-default upstream base — exactly the
        // deployment the bug fired on. The broker holds these host-side.
        let broker_cfg = BrokerConfig::resolve(
            true,
            Some("sk-test"),
            Some("https://openrouter.ai/api/v1"),
            None,
        )
        .unwrap();
        let broker = std::sync::Arc::new(BrokerState::new(broker_cfg));
        let loopback = "http://127.0.0.1:48080";
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        )
        .with_broker(std::sync::Arc::clone(&broker), loopback.into());

        let session = fixture_session(&db);
        let cfg = mgr.runner_config_for(&session, None, None);
        // The endpoint the runner hits is the broker loopback, NOT the
        // operator's configured `ANTHROPIC_BASE_URL`.
        assert_eq!(
            cfg.api_base_url.as_deref(),
            Some(loopback),
            "broker enabled: runner.json api_base_url must be the broker loopback"
        );
        assert_ne!(
            cfg.api_base_url.as_deref(),
            Some("https://openrouter.ai/api/v1"),
            "broker enabled: runner.json api_base_url must NOT be the operator's real URL"
        );
        // The runner still reads the ANTHROPIC_API_KEY slot — which build_spec
        // fills with the capability token, NOT the real key.
        assert_eq!(cfg.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
    }

    /// Default path (broker DISABLED): behaviour is unchanged — the runner
    /// hits the operator's real `ANTHROPIC_BASE_URL` with the real key as
    /// before. Regression guard so the fix can't break the common no-broker
    /// install.
    #[test]
    fn runner_config_with_broker_disabled_uses_real_base_url() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let cfg = mgr.runner_config_for(&session, None, None);
        assert_eq!(
            cfg.api_base_url.as_deref(),
            Some("https://openrouter.ai/api/v1"),
            "broker disabled: runner.json api_base_url must be the operator's real URL"
        );
        assert_eq!(cfg.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
    }

    /// The broker redirect only applies to the Anthropic-envelope providers.
    /// A native-ollama group must still get `api_base_url: None` even with the
    /// broker enabled (its model traffic goes via `OLLAMA_BASE_URL`, not the
    /// Anthropic broker), so the broker never accidentally captures it.
    #[test]
    fn runner_config_with_broker_enabled_leaves_ollama_base_url_none() {
        use super::super::broker::{BrokerConfig, BrokerState};

        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let broker_cfg = BrokerConfig::resolve(true, Some("sk-test"), None, None).unwrap();
        let broker = std::sync::Arc::new(BrokerState::new(broker_cfg));
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        )
        .with_broker(
            std::sync::Arc::clone(&broker),
            "http://127.0.0.1:48080".into(),
        );

        let session = fixture_session(&db);
        let cc = container_configs::ContainerConfig {
            agent_group_id: session.agent_group_id,
            provider: Some("ollama".into()),
            model: Some("llama3.1:8b".into()),
            effort: None,
            image_tag: None,
            assistant_name: None,
            max_messages_per_prompt: None,
            skills: container_configs::SkillsSelector::All,
            mcp_servers: serde_json::json!({}),
            packages_apt: vec![],
            packages_npm: vec![],
            additional_mounts: serde_json::json!([]),
            cli_scope: container_configs::CliScope::Group,
            config_fingerprint: None,
            egress_allow: vec![],
            resource_limits: serde_json::json!({}),
            coding_enabled: false,
            surface_thinking: false,
            tool_profile: None,
            updated_at: chrono::Utc::now(),
        };
        let cfg = mgr.runner_config_for(&session, Some(&cc), None);
        assert!(
            cfg.api_base_url.is_none(),
            "ollama provider must not be redirected at the Anthropic broker"
        );
    }

    /// Helper: build a `ContainerConfig` populated with the four coding
    /// skill names available, the `coding_enabled` flag set as requested,
    /// and an otherwise-default shape. Used by the three filter tests below.
    fn cc_with_coding_flag(
        agent_group_id: AgentGroupId,
        coding_enabled: bool,
    ) -> container_configs::ContainerConfig {
        container_configs::ContainerConfig {
            agent_group_id,
            provider: None,
            model: None,
            effort: None,
            image_tag: None,
            assistant_name: None,
            max_messages_per_prompt: None,
            skills: container_configs::SkillsSelector::All,
            mcp_servers: serde_json::json!({}),
            packages_apt: vec![],
            packages_npm: vec![],
            additional_mounts: serde_json::json!([]),
            cli_scope: container_configs::CliScope::Group,
            config_fingerprint: None,
            egress_allow: vec![],
            resource_limits: serde_json::json!({}),
            coding_enabled,
            surface_thinking: false,
            tool_profile: None,
            updated_at: chrono::Utc::now(),
        }
    }

    /// Writes the four coding-bundle skill stubs plus one non-coding
    /// `alpha` skill into `<root>` so the registry sees them.
    fn write_coding_bundle_plus_alpha(skills_root: &std::path::Path) {
        std::fs::create_dir_all(skills_root).unwrap();
        for name in CODING_SKILL_NAMES {
            write_skill_md(skills_root, name, &format!("body-of-{name}\n"));
        }
        write_skill_md(skills_root, "alpha", "alpha body\n");
    }

    #[test]
    fn runner_config_filters_coding_skills_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_root = tmp.path().join("skills");
        write_coding_bundle_plus_alpha(&skills_root);

        let db = CentralDb::open_in_memory().unwrap();
        let mut mgr_cfg = manager_cfg(tmp.path().to_path_buf());
        mgr_cfg.skills_dir = Some(skills_root);
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            mgr_cfg,
        );
        let session = fixture_session(&db);
        let cc = cc_with_coding_flag(session.agent_group_id, false);
        let cfg = mgr.runner_config_for(&session, Some(&cc), None);
        for name in CODING_SKILL_NAMES {
            assert!(
                !cfg.system.contains(&format!("name=\"{name}\"")),
                "coding skill `{name}` must not appear when coding_enabled=false; system was:\n{}",
                cfg.system,
            );
        }
        // The non-coding skill still appears.
        assert!(
            cfg.system.contains("name=\"alpha\""),
            "non-coding skill `alpha` must still appear; system was:\n{}",
            cfg.system,
        );
    }

    #[test]
    fn runner_config_includes_coding_skills_when_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_root = tmp.path().join("skills");
        write_coding_bundle_plus_alpha(&skills_root);

        let db = CentralDb::open_in_memory().unwrap();
        let mut mgr_cfg = manager_cfg(tmp.path().to_path_buf());
        mgr_cfg.skills_dir = Some(skills_root);
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            mgr_cfg,
        );
        let session = fixture_session(&db);
        let cc = cc_with_coding_flag(session.agent_group_id, true);
        let cfg = mgr.runner_config_for(&session, Some(&cc), None);
        for name in CODING_SKILL_NAMES {
            assert!(
                cfg.system.contains(&format!("name=\"{name}\"")),
                "coding skill `{name}` must appear when coding_enabled=true; system was:\n{}",
                cfg.system,
            );
        }
    }

    /// An explicit selector that lists a coding skill must keep that
    /// skill even when `coding_enabled=false`. The flag is a cap on the
    /// `All` default, not an override of operator-picked allowlists.
    #[test]
    fn runner_config_respects_explicit_selector_even_when_coding_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_root = tmp.path().join("skills");
        write_coding_bundle_plus_alpha(&skills_root);

        let db = CentralDb::open_in_memory().unwrap();
        let mut mgr_cfg = manager_cfg(tmp.path().to_path_buf());
        mgr_cfg.skills_dir = Some(skills_root);
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            mgr_cfg,
        );
        let session = fixture_session(&db);
        let mut cc = cc_with_coding_flag(session.agent_group_id, false);
        cc.skills = container_configs::SkillsSelector::Explicit(vec!["coding-task".into()]);
        let cfg = mgr.runner_config_for(&session, Some(&cc), None);
        assert!(
            cfg.system.contains("name=\"coding-task\""),
            "explicit selector must honour `coding-task` even when coding_enabled=false; system was:\n{}",
            cfg.system,
        );
    }

    #[test]
    fn runner_config_uses_skill_dir_when_configured() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "alpha body\n");
        let mut cfg = manager_cfg(td.path().to_path_buf());
        cfg.skills_dir = Some(skills);
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let rc = mgr.runner_config_for(&session, None, None);
        assert!(rc.system.contains("alpha body"));
        assert!(rc.system.contains("<skill name=\"alpha\""));
        // The new top-level assembler always prepends the universal
        // preamble + environment block, even when the session_root is
        // None (the briefing is the only optional piece). That gives us
        // a couple of cheap structural sanity checks here.
        assert!(rc.system.contains("You are a Copperclaw agent"));
        assert!(rc.system.contains("# Environment"));
    }

    #[test]
    fn runner_config_callable_mode_emits_index_and_writes_catalogue() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "alpha body\n");
        write_skill_md(&skills, "beta", "beta body\n");
        let session_root = td.path().join("session");
        std::fs::create_dir_all(&session_root).unwrap();
        let mut cfg = manager_cfg(td.path().to_path_buf());
        cfg.skills_dir = Some(skills);
        cfg.skills_mode = SkillsMode::Callable;
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let rc = mgr.runner_config_for(&session, None, Some(&session_root));
        // System prompt: index only, no inlined bodies.
        assert!(rc.system.contains("name=\"alpha\""));
        assert!(rc.system.contains("name=\"beta\""));
        assert!(!rc.system.contains("alpha body"));
        assert!(!rc.system.contains("beta body"));
        assert!(rc.system.contains("`load_skill`"));
        // Catalogue file written next to the session dir.
        let catalogue_path = session_root.join(SKILLS_CATALOGUE_FILENAME);
        assert!(catalogue_path.is_file(), "expected skills.json on disk");
        let bytes = std::fs::read(&catalogue_path).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        let names: Vec<&str> = parsed
            .iter()
            .filter_map(|e| e.get("name").and_then(|v| v.as_str()))
            .collect();
        assert!(names.contains(&"alpha") && names.contains(&"beta"));
        let bodies: String = parsed
            .iter()
            .filter_map(|e| e.get("body").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("|");
        assert!(bodies.contains("alpha body"));
        assert!(bodies.contains("beta body"));
    }

    #[test]
    fn runner_config_inline_mode_does_not_write_catalogue() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "alpha body\n");
        let session_root = td.path().join("session");
        std::fs::create_dir_all(&session_root).unwrap();
        let mut cfg = manager_cfg(td.path().to_path_buf());
        cfg.skills_dir = Some(skills);
        // SkillsMode::Inline is the default — explicit here for clarity.
        cfg.skills_mode = SkillsMode::Inline;
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let _rc = mgr.runner_config_for(&session, None, Some(&session_root));
        assert!(
            !session_root.join(SKILLS_CATALOGUE_FILENAME).exists(),
            "inline mode must not write skills.json"
        );
    }

    #[test]
    fn runner_config_callable_mode_removes_stale_catalogue_when_no_skills_selected() {
        let td = tempfile::tempdir().unwrap();
        let session_root = td.path().join("session");
        std::fs::create_dir_all(&session_root).unwrap();
        // Pre-existing catalogue from a prior spawn that selected skills.
        std::fs::write(
            session_root.join(SKILLS_CATALOGUE_FILENAME),
            r#"[{"name":"old","description":"","body":"old body"}]"#,
        )
        .unwrap();
        // No skills_dir configured → catalogue should be removed so the
        // agent doesn't see stale entries.
        let mut cfg = manager_cfg(td.path().to_path_buf());
        cfg.skills_mode = SkillsMode::Callable;
        cfg.skills_dir = None;
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let _rc = mgr.runner_config_for(&session, None, Some(&session_root));
        assert!(
            !session_root.join(SKILLS_CATALOGUE_FILENAME).exists(),
            "stale catalogue must be removed when no skills selected"
        );
    }

    /// Finding 1: an operator who flips `COPPERCLAW_SKILLS_MODE` from
    /// `callable` to `inline` between spawns must not leave a prior
    /// `skills.json` on disk — `load_skill` would otherwise hand the
    /// agent stale bodies that don't match the inlined prompt.
    #[test]
    fn runner_config_inline_mode_removes_stale_catalogue_from_prior_spawn() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "alpha body v2\n");
        let session_root = td.path().join("session");
        std::fs::create_dir_all(&session_root).unwrap();
        // Pre-existing catalogue from a previous Callable-mode spawn.
        std::fs::write(
            session_root.join(SKILLS_CATALOGUE_FILENAME),
            r#"[{"name":"alpha","description":"","body":"alpha body v1"}]"#,
        )
        .unwrap();

        let mut cfg = manager_cfg(td.path().to_path_buf());
        cfg.skills_dir = Some(skills);
        cfg.skills_mode = SkillsMode::Inline;
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let _ = mgr.runner_config_for(&session, None, Some(&session_root));
        assert!(
            !session_root.join(SKILLS_CATALOGUE_FILENAME).exists(),
            "inline-mode spawn must scrub a catalogue left by a prior callable spawn"
        );
    }

    /// Finding 2: when the `skills.json` write fails in Callable mode,
    /// the assembled prompt falls back to Inline-mode shape (skill
    /// bodies present, no `load_skill` advert) so the agent isn't left
    /// pointing at a missing catalogue.
    #[test]
    fn runner_config_callable_falls_back_to_inline_when_catalogue_write_fails() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "alpha body marker\n");
        let session_root = td.path().join("session");
        std::fs::create_dir_all(&session_root).unwrap();
        // Sabotage the write: pre-create `skills.json` as a directory so
        // `fs::write` to that path errors. Mirrors a real-world failure
        // mode (the path exists but isn't a regular file).
        std::fs::create_dir_all(session_root.join(SKILLS_CATALOGUE_FILENAME)).unwrap();

        let mut cfg = manager_cfg(td.path().to_path_buf());
        cfg.skills_dir = Some(skills);
        cfg.skills_mode = SkillsMode::Callable;
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let rc = mgr.runner_config_for(&session, None, Some(&session_root));
        // Inline shape: bodies are inlined and the callable-mode catalogue
        // header (which instructs the agent to call load_skill to fetch
        // bodies) is absent. The base preamble is allowed to mention
        // load_skill in passing; what must not appear is the catalogue
        // index instruction sentence.
        assert!(
            rc.system.contains("alpha body marker"),
            "fallback prompt must inline skill bodies"
        );
        assert!(
            !rc.system.contains("catalogue of skills available to you"),
            "fallback prompt must not include the callable-mode skill catalogue header"
        );
    }

    /// Finding 3: the Callable-mode prompt index and the on-disk
    /// catalogue must agree about which skills exist. The fix makes
    /// `select_callable_skills` the single source of truth used by both
    /// outputs; this test pins the consistency.
    #[test]
    fn runner_config_callable_prompt_index_and_catalogue_agree() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "alpha body\n");
        write_skill_md(&skills, "beta", "beta body\n");
        let session_root = td.path().join("session");
        std::fs::create_dir_all(&session_root).unwrap();
        let mut cfg = manager_cfg(td.path().to_path_buf());
        cfg.skills_dir = Some(skills);
        cfg.skills_mode = SkillsMode::Callable;
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let rc = mgr.runner_config_for(&session, None, Some(&session_root));
        let bytes = std::fs::read(session_root.join(SKILLS_CATALOGUE_FILENAME)).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        let catalogue_names: std::collections::BTreeSet<String> = parsed
            .iter()
            .filter_map(|e| e.get("name").and_then(|v| v.as_str()).map(String::from))
            .collect();
        // Every name in the catalogue must also appear in the prompt
        // index, and the prompt must not mention any name absent from
        // the catalogue.
        for name in &catalogue_names {
            assert!(
                rc.system.contains(&format!("name=\"{name}\"")),
                "catalogue entry {name} missing from prompt index"
            );
        }
        for candidate in ["alpha", "beta"] {
            let in_cat = catalogue_names.contains(candidate);
            let in_prompt = rc.system.contains(&format!("name=\"{candidate}\""));
            assert_eq!(
                in_cat, in_prompt,
                "{candidate}: catalogue/prompt disagreement (cat={in_cat}, prompt={in_prompt})"
            );
        }
    }

    // ----- Input 1: group tool-profile FUEL ---------------------------------

    /// Build a `ContainerConfig` for a session's group with the given
    /// `tool_profile` (all other fields default). Helper for the
    /// tool-profile plumbing tests.
    fn cc_with_tool_profile(
        agent_group_id: AgentGroupId,
        tool_profile: Option<&str>,
    ) -> container_configs::ContainerConfig {
        container_configs::ContainerConfig {
            agent_group_id,
            provider: None,
            model: None,
            effort: None,
            image_tag: None,
            assistant_name: None,
            max_messages_per_prompt: None,
            skills: container_configs::SkillsSelector::All,
            mcp_servers: serde_json::json!({}),
            packages_apt: vec![],
            packages_npm: vec![],
            additional_mounts: serde_json::json!([]),
            cli_scope: container_configs::CliScope::Group,
            config_fingerprint: None,
            egress_allow: vec![],
            resource_limits: serde_json::json!({}),
            coding_enabled: false,
            surface_thinking: false,
            tool_profile: tool_profile.map(str::to_string),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn runner_config_plumbs_messaging_tool_profile() {
        // End-to-end input #1: a group on the `messaging` profile must
        // emit `tool_profile: "messaging"` into runner.json. The runner's
        // policy tests prove that value blocks `shell` at dispatch.
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let cc = cc_with_tool_profile(session.agent_group_id, Some("messaging"));
        let rc = mgr.runner_config_for(&session, Some(&cc), None);
        assert_eq!(rc.tool_profile.as_deref(), Some("messaging"));
        // And it survives JSON serialization the runner will parse.
        let json = serde_json::to_value(&rc).unwrap();
        assert_eq!(json["tool_profile"], "messaging");
    }

    #[test]
    fn runner_config_omits_tool_profile_when_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let cc = cc_with_tool_profile(session.agent_group_id, None);
        let rc = mgr.runner_config_for(&session, Some(&cc), None);
        assert!(rc.tool_profile.is_none());
        // Skipped from the serialized shape (skip_serializing_if).
        let json = serde_json::to_value(&rc).unwrap();
        assert!(json.get("tool_profile").is_none());
    }

    #[test]
    fn runner_config_drops_unknown_tool_profile() {
        // A malformed profile name must not reach the runner (it would
        // silently fall back to `full` there); the host drops it so the
        // field is simply absent.
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let cc = cc_with_tool_profile(session.agent_group_id, Some("bogus"));
        let rc = mgr.runner_config_for(&session, Some(&cc), None);
        assert!(rc.tool_profile.is_none());
    }

    // ----- Input 2: resolved sender-role FUEL -------------------------------

    /// Write one pending inbound row carrying the given channel sender into
    /// the session's real inbound DB (under `data_dir`), so
    /// `resolve_sender_role` can find the triggering sender.
    fn seed_triggering_inbound(
        data_dir: &std::path::Path,
        session: &Session,
        channel: &str,
        platform_id: &str,
    ) {
        use copperclaw_db::tables::messages_in::{WriteInbound, insert};
        use copperclaw_types::{ChannelType, MessageId, MessageKind};
        let paths = SessionPaths::new(data_dir, session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let conn = open_inbound(&paths).unwrap();
        let row = WriteInbound {
            id: MessageId::new(),
            kind: MessageKind::Chat,
            timestamp: chrono::Utc::now(),
            content: serde_json::json!({"text": "hi"}),
            trigger: true,
            on_wake: false,
            process_after: None,
            recurrence: None,
            series_id: None,
            platform_id: Some(platform_id.to_string()),
            channel_type: Some(ChannelType::new(channel)),
            thread_id: None,
            source_session_id: None,
            reply_to: None,
            is_group: None,
        };
        insert(&conn, &row).unwrap();
    }

    #[test]
    fn runner_config_resolves_guest_sender_role() {
        // End-to-end input #2: a known sender who holds no elevated grant
        // and isn't a member resolves to `guest` and that role lands in
        // runner.json. The runner's policy tests prove a guest blocks
        // `shell`.
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        // The sender is a known user (has a `users` row) but no role / not
        // a member → guest.
        users::upsert(
            &db,
            users::UpsertUser {
                kind: "telegram".into(),
                identity: "user-42".into(),
                display_name: Some("Guest User".into()),
            },
        )
        .unwrap();
        seed_triggering_inbound(tmp.path(), &session, "telegram", "user-42");
        // session_root must be Some for the real-spawn resolution path.
        let session_root = SessionPaths::new(tmp.path(), session.agent_group_id, session.id).root;
        let rc = mgr.runner_config_for(&session, None, Some(&session_root));
        assert_eq!(rc.sender_role.as_deref(), Some("guest"));
    }

    #[test]
    fn runner_config_resolves_admin_sender_role() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let user = users::upsert(
            &db,
            users::UpsertUser {
                kind: "telegram".into(),
                identity: "boss".into(),
                display_name: None,
            },
        )
        .unwrap();
        // A global Admin grant maps to module Admin.
        user_roles::grant(&db, user.id, user_roles::Role::Admin, None, None).unwrap();
        seed_triggering_inbound(tmp.path(), &session, "telegram", "boss");
        let session_root = SessionPaths::new(tmp.path(), session.agent_group_id, session.id).root;
        let rc = mgr.runner_config_for(&session, None, Some(&session_root));
        assert_eq!(rc.sender_role.as_deref(), Some("admin"));
    }

    #[test]
    fn runner_config_resolves_member_sender_role() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let user = users::upsert(
            &db,
            users::UpsertUser {
                kind: "telegram".into(),
                identity: "teammate".into(),
                display_name: None,
            },
        )
        .unwrap();
        agent_group_members::add(&db, user.id, session.agent_group_id, None).unwrap();
        seed_triggering_inbound(tmp.path(), &session, "telegram", "teammate");
        let session_root = SessionPaths::new(tmp.path(), session.agent_group_id, session.id).root;
        let rc = mgr.runner_config_for(&session, None, Some(&session_root));
        assert_eq!(rc.sender_role.as_deref(), Some("member"));
    }

    #[test]
    fn runner_config_omits_sender_role_for_unknown_sender() {
        // No `users` row for the triggering sender → no role floor, so the
        // field is absent and an unconfigured deployment behaves as before.
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        seed_triggering_inbound(tmp.path(), &session, "telegram", "never-seen");
        let session_root = SessionPaths::new(tmp.path(), session.agent_group_id, session.id).root;
        let rc = mgr.runner_config_for(&session, None, Some(&session_root));
        assert!(rc.sender_role.is_none());
        let json = serde_json::to_value(&rc).unwrap();
        assert!(json.get("sender_role").is_none());
    }

    #[test]
    fn runner_config_skips_sender_role_without_session_root() {
        // Test callers that pass `session_root = None` must not trigger
        // role resolution (and must not create an inbound DB).
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let rc = mgr.runner_config_for(&session, None, None);
        assert!(rc.sender_role.is_none());
    }

    // ----- Input 3: active-skill allowed-tools catalogue --------------------

    fn write_skill_md_with_allowed(
        parent: &std::path::Path,
        name: &str,
        allowed: &str,
        body: &str,
    ) {
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!(
            "---\nname: {name}\ndescription: desc-of-{name}\nallowed-tools: {allowed}\n---\n\n{body}"
        );
        std::fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn callable_catalogue_carries_normalized_allowed_tools() {
        // End-to-end input #3: a skill declaring `allowed-tools: [Read]`
        // must land in skills.json with the normalized MCP name
        // (`read_file`), which the runner's load_skill feeds into the
        // active-skill policy layer to block shell.
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md_with_allowed(&skills, "reader", "[Read]", "read-only skill body\n");
        let session_root = td.path().join("session");
        std::fs::create_dir_all(&session_root).unwrap();
        let mut cfg = manager_cfg(td.path().to_path_buf());
        cfg.skills_dir = Some(skills);
        cfg.skills_mode = SkillsMode::Callable;
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let _rc = mgr.runner_config_for(&session, None, Some(&session_root));
        let bytes = std::fs::read(session_root.join(SKILLS_CATALOGUE_FILENAME)).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        let reader = parsed
            .iter()
            .find(|e| e.get("name").and_then(|v| v.as_str()) == Some("reader"))
            .expect("reader skill in catalogue");
        let allowed: Vec<String> = reader
            .get("allowed_tools")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .expect("allowed_tools present");
        assert_eq!(allowed, vec!["read_file".to_string()]);
        assert!(
            !allowed.contains(&"shell".to_string()),
            "read-only skill must not authorize shell"
        );
    }

    #[test]
    fn callable_catalogue_omits_allowed_tools_when_skill_declares_none() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "open", "no scope body\n");
        let session_root = td.path().join("session");
        std::fs::create_dir_all(&session_root).unwrap();
        let mut cfg = manager_cfg(td.path().to_path_buf());
        cfg.skills_dir = Some(skills);
        cfg.skills_mode = SkillsMode::Callable;
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let _rc = mgr.runner_config_for(&session, None, Some(&session_root));
        let bytes = std::fs::read(session_root.join(SKILLS_CATALOGUE_FILENAME)).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        let open = parsed
            .iter()
            .find(|e| e.get("name").and_then(|v| v.as_str()) == Some("open"))
            .expect("open skill in catalogue");
        assert!(
            open.get("allowed_tools").is_none(),
            "a skill with no allowed-tools must omit the field (skip_serializing_if)"
        );
    }
}
