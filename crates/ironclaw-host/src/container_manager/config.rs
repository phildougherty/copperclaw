//! Manager configuration, hot-swappable rotatable subset, and `.env` parsing.

use std::path::{Path, PathBuf};
use tracing::warn;

/// Env-var names treated as secrets that the SIGHUP handler re-reads
/// from the install's `.env`. Missing keys after rotation are dropped
/// from the forwarded set — we never fall back to a stale value.
pub const ROTATABLE_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "TAVILY_API_KEY",
    "EXA_API_KEY",
    "BRAVE_SEARCH_API_KEY",
    "SERPAPI_API_KEY",
    "OLLAMA_BASE_URL",
    "IRONCLAW_CODEX_BINARY",
    "IRONCLAW_CODEX_ARGS",
];

/// Subset of [`ManagerConfig`] that the SIGHUP handler can hot-swap.
/// Held behind an `Arc<RwLock<...>>` on [`super::ContainerManager`] so the
/// handler can update it without restarting the host. Reads during
/// `build_spec` take a short-lived read-lock; writes during
/// `reload_env` take a write-lock.
///
/// Note on already-running containers: Docker's env is immutable
/// post-creation. A rotated key only takes effect for containers
/// spawned **after** the reload. With the default
/// `idle_timeout_secs = 300`, an idle container respawns within
/// 5 minutes of the next inbound message and picks up the new key
/// at that point.
#[derive(Debug, Clone, Default)]
pub struct RotatableConfig {
    /// Current `ANTHROPIC_API_KEY`. `None` means the var is absent
    /// (or was removed during rotation).
    pub anthropic_api_key: Option<String>,
    /// Current `ANTHROPIC_BASE_URL` override.
    pub anthropic_base_url: Option<String>,
    /// Additional provider-key env-vars to forward into spawned
    /// containers. Keys here are only forwarded when their value is
    /// non-empty. Tracks the web-search provider keys today
    /// (`TAVILY_API_KEY`, `EXA_API_KEY`, `BRAVE_SEARCH_API_KEY`,
    /// `SERPAPI_API_KEY`).
    pub forward_env: Vec<(String, String)>,
}

impl RotatableConfig {
    /// Build from a flat env-var map (typically the process env
    /// snapshot at boot or a re-read of `.env` on SIGHUP). Empty
    /// strings are treated as absent.
    pub fn from_env_map(map: &std::collections::HashMap<String, String>) -> Self {
        let anthropic_api_key = map
            .get("ANTHROPIC_API_KEY")
            .filter(|v| !v.is_empty())
            .cloned();
        let anthropic_base_url = map
            .get("ANTHROPIC_BASE_URL")
            .filter(|v| !v.is_empty())
            .cloned();
        let forward_env = ROTATABLE_ENV_KEYS[2..]
            .iter()
            .filter_map(|k| {
                map.get(*k)
                    .filter(|v| !v.is_empty())
                    .map(|v| ((*k).to_string(), v.clone()))
            })
            .collect();
        Self {
            anthropic_api_key,
            anthropic_base_url,
            forward_env,
        }
    }
}

/// Host-side knobs that don't change per-session.
#[derive(Debug, Clone)]
pub struct ManagerConfig {
    /// Label propagated to spawned containers so orphan cleanup picks
    /// them up across restarts.
    pub install_slug: String,
    /// Absolute path to the host's data dir (parent of `sessions/`).
    pub data_dir: PathBuf,
    /// Default image tag used when a `container_config` row doesn't
    /// pin one. Computed at boot from the default spec.
    pub default_image_tag: String,
    /// Default provider, e.g. `"anthropic"`. Pulled from
    /// `IRONCLAW_DEFAULT_PROVIDER` or `"anthropic"` as a fallback.
    pub default_provider: String,
    /// Default model id.
    pub default_model: String,
    /// Default reasoning-effort tier (`low`/`medium`/`high`). Read from
    /// `IRONCLAW_DEFAULT_EFFORT` at boot. `None` (or `Medium`) means
    /// "use the model's default" and emits no `reasoning.effort`
    /// field on the wire. `OpenRouter` / `DeepSeek` R1 / `OpenAI` o-series
    /// honour `low`/`high` to budget more or less chain-of-thought.
    pub default_effort: Option<ironclaw_types::Effort>,
    /// `ANTHROPIC_API_KEY` value the runner inside the container will
    /// see. Read from the host's process env at boot.
    pub anthropic_api_key: Option<String>,
    /// Optional override base URL (e.g. `OpenRouter`'s
    /// `https://openrouter.ai/api/v1`).
    pub anthropic_base_url: Option<String>,
    /// Seconds without inbound activity before the manager stops the
    /// container and flips `container_status=idle`.
    pub idle_timeout_secs: u64,
    /// Seconds without heartbeat refresh before the manager
    /// considers the runner dead, stops the container (best effort),
    /// and resets `container_status=stopped` for respawn.
    pub heartbeat_stale_secs: u64,
    /// Grace period for `runtime.stop` calls — sent as SIGTERM
    /// timeout. The runtime sends SIGKILL after.
    pub stop_grace_secs: u64,
    /// Directory containing global `SKILL.md` bundles. When set, the
    /// manager loads each enabled skill's body into the runner's
    /// system prompt at spawn so the model knows what tools it has.
    /// `None` keeps the system prompt empty (legacy behaviour).
    pub skills_dir: Option<PathBuf>,
    /// Per-group override root. When set, `<groups_dir>/<ag_uuid>/skills/`
    /// is scanned alongside the global skills directory and skills
    /// with matching names shadow the global ones.
    pub groups_dir: Option<PathBuf>,
    /// How skill bodies are surfaced to the agent. See [`SkillsMode`].
    /// `Inline` (default) preserves today's behaviour; `Callable` shifts
    /// bodies behind a `load_skill` MCP tool to keep the system prompt
    /// small. Set via `IRONCLAW_SKILLS_MODE` at boot.
    pub skills_mode: SkillsMode,
    /// Expose host Nvidia GPUs to every spawned container (`docker run
    /// --gpus all` equivalent). Default off because the device request
    /// fails the spawn outright on hosts without `nvidia-container-
    /// toolkit`. Set via `IRONCLAW_CONTAINER_GPU=1` (or `all`, `true`).
    pub gpu_passthrough: bool,
    /// Extra environment variables to forward into every spawned
    /// session container. Used to plumb operator-supplied API keys
    /// (Tavily / Exa / Brave / `SerpAPI` / etc.) and arbitrary
    /// `IRONCLAW_*` settings through to the runner. Keys with empty
    /// values are skipped so an unset operator env doesn't write
    /// `FOO=` lines into the container env.
    pub forward_env: Vec<(String, String)>,
}

/// How skill bodies reach the agent. The default mirrors today's
/// behaviour (inline every selected skill body into the system prompt
/// at spawn) so flipping to callable is opt-in per operator. Callable
/// mode advertises a compact index of skill names + descriptions in the
/// prompt and exposes a `load_skill` MCP tool that returns a named
/// skill's body on demand. The trade-off: bodies move from the always-on
/// prompt window (cheap to use, expensive every turn) to a tool call
/// (more turns, but every other turn pays nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkillsMode {
    /// Inline every selected skill's body into the system prompt at
    /// spawn time. No `skills.json` is written.
    #[default]
    Inline,
    /// Emit a name+description index in the system prompt and write
    /// `skills.json` next to `runner.json` for the runner's
    /// `load_skill` MCP tool.
    Callable,
}

impl SkillsMode {
    /// Parse the operator-facing string form. Accepts `"inline"` and
    /// `"callable"`; unknown values fall back to [`SkillsMode::Inline`]
    /// with a `WARN` so a typo never silently mutes skills.
    pub fn parse_or_default(s: Option<&str>) -> Self {
        match s.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            None | Some("" | "inline") => Self::Inline,
            Some("callable") => Self::Callable,
            Some(other) => {
                warn!(value = other, "unknown IRONCLAW_SKILLS_MODE; falling back to inline");
                Self::Inline
            }
        }
    }
}

/// Read the `.env` file at `explicit_path` (or return an empty map
/// when `None`). We **do not** call `dotenvy` here because dotenvy
/// mutates the process env, which would race with other handlers and
/// leak rotated-away values to anything that's already read the env.
/// Instead we parse a minimal subset by hand.
pub(crate) fn read_env_file(
    explicit_path: Option<&Path>,
) -> std::collections::HashMap<String, String> {
    let Some(path) = explicit_path else {
        return std::collections::HashMap::new();
    };
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) => {
            warn!(path = %path.display(), ?err, "SIGHUP: could not read env file");
            return std::collections::HashMap::new();
        }
    };
    parse_dotenv_content(&content)
}

/// Parse a `.env`-style document. Handles comments (`#`), blank
/// lines, optional `export` prefixes, and single-/double-quoted
/// values. The parser is deliberately small: it does not expand
/// `${VAR}` references or honour escape sequences inside quotes.
pub(crate) fn parse_dotenv_content(content: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        if key.is_empty() {
            continue;
        }
        let value = strip_quotes(v.trim()).to_string();
        out.insert(key.to_string(), value);
    }
    out
}

/// Strip a single layer of matching single or double quotes.
fn strip_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skills_mode_parse_or_default_handles_known_and_unknown() {
        assert_eq!(SkillsMode::parse_or_default(None), SkillsMode::Inline);
        assert_eq!(SkillsMode::parse_or_default(Some("")), SkillsMode::Inline);
        assert_eq!(
            SkillsMode::parse_or_default(Some("inline")),
            SkillsMode::Inline
        );
        assert_eq!(
            SkillsMode::parse_or_default(Some("INLINE")),
            SkillsMode::Inline
        );
        assert_eq!(
            SkillsMode::parse_or_default(Some("callable")),
            SkillsMode::Callable
        );
        assert_eq!(
            SkillsMode::parse_or_default(Some("Callable")),
            SkillsMode::Callable
        );
        // Unknown falls back without panicking.
        assert_eq!(
            SkillsMode::parse_or_default(Some("on")),
            SkillsMode::Inline
        );
    }

    #[test]
    fn parse_dotenv_content_handles_quotes_export_and_comments() {
        let raw = "
# leading comment
ANTHROPIC_API_KEY=sk-plain
export TAVILY_API_KEY=\"tav-quoted\"
BRAVE_SEARCH_API_KEY='br-single'

# trailing comment
SERPAPI_API_KEY=
NOT_A_PAIR_LINE
";
        let map = parse_dotenv_content(raw);
        assert_eq!(map.get("ANTHROPIC_API_KEY"), Some(&"sk-plain".to_string()));
        assert_eq!(map.get("TAVILY_API_KEY"), Some(&"tav-quoted".to_string()));
        assert_eq!(map.get("BRAVE_SEARCH_API_KEY"), Some(&"br-single".to_string()));
        assert_eq!(map.get("SERPAPI_API_KEY"), Some(&String::new()));
        assert!(!map.contains_key("NOT_A_PAIR_LINE"));
    }

    #[test]
    fn rotatable_config_drops_empty_values() {
        let mut m = std::collections::HashMap::new();
        m.insert("ANTHROPIC_API_KEY".into(), "sk-1".into());
        m.insert("ANTHROPIC_BASE_URL".into(), String::new());
        m.insert("TAVILY_API_KEY".into(), "tav-1".into());
        let cfg = RotatableConfig::from_env_map(&m);
        assert_eq!(cfg.anthropic_api_key.as_deref(), Some("sk-1"));
        assert!(cfg.anthropic_base_url.is_none(), "empty value must be dropped");
        assert_eq!(cfg.forward_env.len(), 1);
        assert_eq!(cfg.forward_env[0], ("TAVILY_API_KEY".into(), "tav-1".into()));
    }
}
