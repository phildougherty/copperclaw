//! Host-side mirror of the runner's `runner.json` schema, plus the assembler
//! that builds one for a given session.

use super::config::SkillsMode;
use super::prompt::{
    assemble_system_prompt_with_catalogue, db_selector_to_skills_selector, remove_stale_catalogue,
    select_callable_skills, SkillCatalogueEntry, SKILLS_CATALOGUE_FILENAME,
};
use super::spawn::{CODING_SKILL_NAMES, CONTAINER_SESSION_DIR};
use super::ContainerManager;
use ironclaw_db::tables::container_configs;
use ironclaw_types::Session;
use std::path::Path;
use tracing::warn;

/// `RunnerConfigFile` lives in `ironclaw-runner`, but pulling the
/// runner crate into the host as a non-test dep would create a
/// circular trail (the runner crate already pulls in `ironclaw-mcp`
/// and `ironclaw-providers`, both of which the host doesn't otherwise
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
    /// `IRONCLAW_CODEX_BINARY` rotatable env var (no per-group
    /// override column yet — that's intentional for this slice).
    /// Falls back to the runner's `/usr/local/bin/codex` default
    /// when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) codex_binary: Option<String>,
    /// Extra args appended to every Codex spawn. Only set when
    /// `provider == "codex"`; sourced from `IRONCLAW_CODEX_ARGS`
    /// (comma-separated). Falls back to the runner's `["--json"]`
    /// default when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) codex_args: Option<Vec<String>>,
}

impl ContainerManager {
    #[allow(clippy::too_many_lines)] // single linear assembly; provider/skills branches read more clearly inline
    pub(super) fn runner_config_for(
        &self,
        session: &Session,
        cc: Option<&container_configs::ContainerConfig>,
        session_root: Option<&Path>,
    ) -> RunnerConfigForFile {
        let provider_raw = session
            .agent_provider
            .clone()
            .or_else(|| cc.and_then(|c| c.provider.clone()))
            .unwrap_or_else(|| self.cfg.default_provider.clone());
        // Normalize aliases for the runner; unknown values pass through
        // (the runner logs + falls back to anthropic). Empty string is
        // treated as "use the default" so an empty default_provider doesn't
        // leak into the JSON file.
        let provider = match provider_raw.as_str() {
            "" => None,
            "claude" => Some("anthropic".to_string()),
            other => Some(other.to_string()),
        };

        let model = cc
            .and_then(|c| c.model.clone())
            .unwrap_or_else(|| self.cfg.default_model.clone());
        let assistant_name = cc.and_then(|c| c.assistant_name.clone());
        let max_messages = cc.and_then(|c| c.max_messages_per_prompt);
        let selector = cc.map_or(ironclaw_skills::SkillsSelector::All, |c| {
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
            !coding_enabled && matches!(selector, ironclaw_skills::SkillsSelector::All);
        let exclude_names: &[&str] = if exclude_coding { CODING_SKILL_NAMES } else { &[] };
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
            if let Err(err) = std::fs::write(&host_path_marker, root.to_string_lossy().as_bytes())
            {
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
        // binary outside Ironclaw), so it also doesn't need
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
        let api_base_url = match provider.as_deref() {
            Some("ollama" | "ollama-shim" | "codex") => None,
            _ => self
                .rotatable
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .anthropic_base_url
                .clone(),
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
                .find(|(k, _)| k == "IRONCLAW_CODEX_BINARY")
                .map(|(_, v)| v.clone());
            let codex_args = rotatable
                .forward_env
                .iter()
                .find(|(k, _)| k == "IRONCLAW_CODEX_ARGS")
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
        }
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
    use ironclaw_db::central::CentralDb;
    use ironclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};
    use ironclaw_db::tables::sessions::{create as create_session, CreateSession};
    use ironclaw_types::AgentGroupId;
    use std::path::PathBuf;

    fn manager_cfg(data_dir: PathBuf) -> ManagerConfig {
        ManagerConfig {
            install_slug: "test".into(),
            data_dir,
            default_image_tag: "ironclaw/session:test".into(),
            default_provider: "anthropic".into(),
            default_model: "claude-sonnet-4-6".into(),
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
            },
        )
        .unwrap()
    }

    fn write_skill_md(parent: &std::path::Path, name: &str, body: &str) {
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!(
            "---\nname: {name}\ndescription: desc-of-{name}\n---\n\n{body}"
        );
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
    /// `IRONCLAW_CODEX_BINARY` / `IRONCLAW_CODEX_ARGS` overrides reach
    /// the file when set on the rotatable env.
    #[test]
    fn runner_config_propagates_codex_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut mc = manager_cfg(tmp.path().to_path_buf());
        mc.forward_env.push((
            "IRONCLAW_CODEX_BINARY".into(),
            "/opt/codex/bin/codex".into(),
        ));
        mc.forward_env.push((
            "IRONCLAW_CODEX_ARGS".into(),
            "--json,--quiet".into(),
        ));
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

    /// When `IRONCLAW_CODEX_BINARY` / `IRONCLAW_CODEX_ARGS` are unset,
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
            updated_at: chrono::Utc::now(),
        };
        let cfg = mgr.runner_config_for(&session, Some(&cc), None);
        assert_eq!(cfg.provider.as_deref(), Some("anthropic"));
        assert_eq!(cfg.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert!(cfg.api_base_url.is_some());
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
        assert!(rc.system.contains("You are an Ironclaw agent"));
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

    /// Finding 1: an operator who flips `IRONCLAW_SKILLS_MODE` from
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
        // Inline shape: the body is inlined, the `load_skill` callable
        // index instructions are NOT mentioned.
        assert!(
            rc.system.contains("alpha body marker"),
            "fallback prompt must inline skill bodies"
        );
        assert!(
            !rc.system.contains("`load_skill`"),
            "fallback prompt must not advertise load_skill when no catalogue was written"
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
}
