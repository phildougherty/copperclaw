//! Runner configuration.
//!
//! The runner is configured by a JSON file + environment variables. The
//! JSON file lives at the path passed via `--config` (or `COPPERCLAW_RUNNER_CONFIG`).
//! Environment variables override individual fields when set.
//!
//! All fields with security implications (API keys) are resolved by
//! reading the named environment variable at startup rather than baking
//! the key into the JSON file. That way the file can be checked into
//! version control or shipped over the bind-mount without leaking secrets.

use std::path::{Path, PathBuf};

use copperclaw_types::{AgentGroupId, Effort, SessionId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors returned by config parsing.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// I/O failure reading the config file.
    #[error("io error reading config: {0}")]
    Io(#[from] std::io::Error),
    /// JSON parse failure.
    #[error("json parse error: {0}")]
    Json(#[from] serde_json::Error),
    /// A required field was missing.
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    /// A field carried a value that could not be parsed (e.g. bad UUID).
    #[error("invalid value for {field}: {message}")]
    InvalidValue {
        /// Field name.
        field: &'static str,
        /// Description of the failure.
        message: String,
    },
}

/// On-disk JSON schema for the runner config. Field types are deliberately
/// permissive (`Option<String>`) so the file can be partially-populated and
/// any missing pieces filled in from environment.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RunnerConfigFile {
    /// `SessionId` as a UUID string.
    pub session_id: Option<String>,
    /// `AgentGroupId` as a UUID string.
    pub agent_group_id: Option<String>,
    /// Absolute path to this session's data directory.
    pub session_dir: Option<String>,
    /// Provider kind to talk to. Recognized values: `"anthropic"`
    /// (default), `"ollama"`, `"ollama-shim"`, `"codex"`. Unknown
    /// values fall back to `"anthropic"` and a WARN is logged.
    pub provider: Option<String>,
    /// Provider-native model id.
    pub model: Option<String>,
    /// Tier-of-effort hint.
    pub effort: Option<Effort>,
    /// System prompt to prepend to every turn.
    pub system: Option<String>,
    /// Name of the environment variable to read the API key from.
    pub api_key_env: Option<String>,
    /// Override the default Anthropic base URL. Set to e.g.
    /// `https://openrouter.ai/api/v1` to route requests through an
    /// Anthropic-API-compatible gateway (`OpenRouter`, an internal proxy,
    /// etc.). Falls back to the provider's hard-coded default when None.
    pub api_base_url: Option<String>,
    /// Override token window (defaults to `compaction::DEFAULT_INPUT_WINDOW`).
    pub model_input_window: Option<usize>,
    /// Override compaction safety margin in tokens.
    pub safety_margin_tokens: Option<usize>,
    /// Max output tokens per turn.
    pub max_tokens: Option<u32>,
    /// Display name of the assistant.
    pub assistant_name: Option<String>,
    /// Sampling temperature.
    pub temperature: Option<f32>,
    /// Absolute path to the Codex binary inside the container. Only
    /// consulted when `provider == "codex"`. Falls back to the
    /// `COPPERCLAW_CODEX_BINARY` env var, then `/usr/local/bin/codex`.
    pub codex_binary: Option<String>,
    /// Extra args appended to every Codex spawn (after the binary
    /// path). Only consulted when `provider == "codex"`. Falls back
    /// to `COPPERCLAW_CODEX_ARGS` (comma-separated), then `["--json"]`.
    pub codex_args: Option<Vec<String>>,
    /// Session id of the parent agent that spawned this one, written
    /// by the host's `CreateAgentHandler`. When set, the runner's
    /// `send_message` defaults `to: None` calls to "report up to the
    /// parent" (emit a `MessageKind::Agent` row whose body carries
    /// the parent session id), rather than dumping into the user
    /// channel inherited from the parent's MG.
    pub source_session_id: Option<String>,
    /// Slice-3.5 opt-in flag for surfacing the model's `thinking` /
    /// `redacted_thinking` blocks to the user as collapsed native UI
    /// primitives. Plumbed in from
    /// `container_configs.surface_thinking` by the host's container
    /// manager. Defaults to `false` — surfacing model
    /// chain-of-thought has privacy implications.
    #[serde(default)]
    pub surface_thinking: Option<bool>,
}

/// Fully-resolved runner config.
#[derive(Debug, Clone, PartialEq)]
pub struct RunnerConfig {
    /// Session id.
    pub session_id: SessionId,
    /// Agent group id.
    pub agent_group_id: AgentGroupId,
    /// Absolute path to the session directory.
    pub session_dir: PathBuf,
    /// Resolved provider kind. One of `"anthropic"`, `"ollama"`,
    /// `"ollama-shim"`, `"codex"`. Defaults to `"anthropic"` when
    /// unset / unknown.
    pub provider: String,
    /// Provider-native model id.
    pub model: String,
    /// Tier-of-effort hint.
    pub effort: Effort,
    /// System prompt.
    pub system: String,
    /// API key value, resolved at startup from `api_key_env`.
    pub api_key: Option<String>,
    /// Override the Anthropic base URL. Passed straight through to
    /// `AnthropicProvider::with_base_url` when present.
    pub api_base_url: Option<String>,
    /// Token window.
    pub model_input_window: usize,
    /// Safety margin.
    pub safety_margin_tokens: usize,
    /// Max output tokens per turn.
    pub max_tokens: u32,
    /// Display name of the assistant.
    pub assistant_name: Option<String>,
    /// Sampling temperature.
    pub temperature: Option<f32>,
    /// Absolute path to the Codex binary inside the container. Only
    /// meaningful when `provider == "codex"`; ignored otherwise.
    /// Sourced from `codex_binary` in the JSON file, falling back to
    /// `COPPERCLAW_CODEX_BINARY` and finally `/usr/local/bin/codex`.
    pub codex_binary: Option<String>,
    /// Extra args passed on every Codex spawn (after the binary path).
    /// Only meaningful when `provider == "codex"`. Sourced from
    /// `codex_args` in the JSON file, falling back to a
    /// comma-separated `COPPERCLAW_CODEX_ARGS`, then `["--json"]`.
    pub codex_args: Option<Vec<String>>,
    /// Parent session that spawned this one. `Some(_)` for child
    /// sessions created via `create_agent`; `None` for sessions
    /// kicked off by a real user channel.
    pub source_session_id: Option<SessionId>,
    /// Per-group slice-3.5 opt-in: surface the model's reasoning
    /// blocks as collapsed native UI primitives. Default `false` —
    /// privacy-default tenet. Plumbed in from
    /// `container_configs.surface_thinking`.
    pub surface_thinking: bool,
}

impl RunnerConfig {
    /// Resolve a [`RunnerConfigFile`] into a [`RunnerConfig`], pulling the
    /// API key from the named environment variable (if set).
    ///
    /// Use [`Self::from_file`] for the convenience wrapper that also reads
    /// the JSON file from disk.
    pub fn from_file_struct(
        file: RunnerConfigFile,
        env: &dyn EnvLookup,
    ) -> Result<Self, ConfigError> {
        let session_id = parse_uuid::<SessionId>(file.session_id.as_deref(), "session_id")?;
        let agent_group_id =
            parse_uuid::<AgentGroupId>(file.agent_group_id.as_deref(), "agent_group_id")?;
        let session_dir = file
            .session_dir
            .map(PathBuf::from)
            .ok_or(ConfigError::MissingField("session_dir"))?;
        let model = file.model.ok_or(ConfigError::MissingField("model"))?;
        let system = file.system.unwrap_or_default();
        let api_key = file.api_key_env.as_deref().and_then(|name| env.get(name));
        // Prefer the explicit file field; otherwise pick up
        // `ANTHROPIC_BASE_URL` from the environment so a single env var
        // configures every session (matches how api_key is sourced).
        let api_base_url = file.api_base_url.or_else(|| env.get("ANTHROPIC_BASE_URL"));
        let provider = match file.provider.as_deref() {
            None | Some("" | "anthropic" | "claude") => "anthropic".to_string(),
            Some("ollama") => "ollama".to_string(),
            Some("ollama-shim") => "ollama-shim".to_string(),
            Some("codex") => "codex".to_string(),
            Some(other) => {
                tracing::warn!(
                    provider = other,
                    "unknown provider kind in runner config; falling back to anthropic"
                );
                "anthropic".to_string()
            }
        };
        let source_session_id = file
            .source_session_id
            .as_deref()
            .map(|s| parse_uuid::<SessionId>(Some(s), "source_session_id"))
            .transpose()?;
        Ok(Self {
            session_id,
            agent_group_id,
            session_dir,
            provider,
            model,
            effort: file.effort.unwrap_or(Effort::Medium),
            system,
            api_key,
            api_base_url,
            model_input_window: file
                .model_input_window
                .unwrap_or(crate::compaction::DEFAULT_INPUT_WINDOW),
            safety_margin_tokens: file
                .safety_margin_tokens
                .unwrap_or(crate::compaction::DEFAULT_SAFETY_MARGIN),
            max_tokens: file.max_tokens.unwrap_or(4096),
            assistant_name: file.assistant_name,
            temperature: file.temperature,
            codex_binary: file.codex_binary,
            codex_args: file.codex_args,
            source_session_id,
            surface_thinking: file.surface_thinking.unwrap_or(false),
        })
    }

    /// Read the JSON config file from `path` and resolve it.
    pub fn from_file(path: &Path, env: &dyn EnvLookup) -> Result<Self, ConfigError> {
        let bytes = std::fs::read(path)?;
        let file: RunnerConfigFile = serde_json::from_slice(&bytes)?;
        Self::from_file_struct(file, env)
    }
}

/// Trait implemented by anything that can resolve an environment variable.
///
/// Injected for testability — production callers pass [`SystemEnv`]; tests
/// pass [`MapEnv`].
pub trait EnvLookup {
    /// Return the value of the named variable, or `None` if unset.
    fn get(&self, name: &str) -> Option<String>;
}

/// `EnvLookup` that calls into `std::env::var` at runtime.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemEnv;

impl EnvLookup for SystemEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// `EnvLookup` backed by an in-memory map. Useful for tests.
#[derive(Debug, Default, Clone)]
pub struct MapEnv(pub std::collections::HashMap<String, String>);

impl MapEnv {
    /// Build a [`MapEnv`] from `(key, value)` pairs.
    pub fn from_pairs<I, K, V>(iter: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self(
            iter.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }
}

impl EnvLookup for MapEnv {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

fn parse_uuid<T>(input: Option<&str>, field: &'static str) -> Result<T, ConfigError>
where
    T: From<uuid::Uuid>,
{
    let s = input.ok_or(ConfigError::MissingField(field))?;
    let id = uuid::Uuid::parse_str(s).map_err(|e| ConfigError::InvalidValue {
        field,
        message: e.to_string(),
    })?;
    Ok(T::from(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_file() -> RunnerConfigFile {
        RunnerConfigFile {
            session_id: Some(uuid::Uuid::nil().to_string()),
            agent_group_id: Some(uuid::Uuid::nil().to_string()),
            session_dir: Some("/tmp/copperclaw/session".into()),
            provider: None,
            model: Some("claude-sonnet-4-6".into()),
            effort: Some(Effort::High),
            system: Some("you are an agent".into()),
            api_key_env: Some("ANTHROPIC_API_KEY".into()),
            api_base_url: None,
            model_input_window: Some(200_000),
            safety_margin_tokens: Some(8_000),
            max_tokens: Some(4096),
            assistant_name: Some("Claude".into()),
            temperature: Some(0.7),
            codex_binary: None,
            codex_args: None,
            source_session_id: None,
            surface_thinking: None,
        }
    }

    #[test]
    fn from_file_struct_happy_path() {
        let env = MapEnv::from_pairs([("ANTHROPIC_API_KEY", "key-xyz")]);
        let cfg = RunnerConfig::from_file_struct(good_file(), &env).unwrap();
        assert_eq!(cfg.model, "claude-sonnet-4-6");
        assert_eq!(cfg.effort, Effort::High);
        assert_eq!(cfg.api_key.as_deref(), Some("key-xyz"));
        assert_eq!(cfg.system, "you are an agent");
        assert_eq!(cfg.assistant_name.as_deref(), Some("Claude"));
        assert!((cfg.temperature.unwrap() - 0.7).abs() < 1e-6);
    }

    #[test]
    fn api_key_missing_env_resolves_to_none() {
        let env = MapEnv::default();
        let cfg = RunnerConfig::from_file_struct(good_file(), &env).unwrap();
        assert!(cfg.api_key.is_none());
    }

    #[test]
    fn api_base_url_picked_up_from_file_field() {
        let mut file = good_file();
        file.api_base_url = Some("https://openrouter.ai/api/v1".into());
        let env = MapEnv::from_pairs([("ANTHROPIC_API_KEY", "k")]);
        let cfg = RunnerConfig::from_file_struct(file, &env).unwrap();
        assert_eq!(
            cfg.api_base_url.as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
    }

    #[test]
    fn api_base_url_picked_up_from_env_when_file_silent() {
        let env = MapEnv::from_pairs([
            ("ANTHROPIC_API_KEY", "k"),
            ("ANTHROPIC_BASE_URL", "https://proxy.example/v1"),
        ]);
        let cfg = RunnerConfig::from_file_struct(good_file(), &env).unwrap();
        assert_eq!(
            cfg.api_base_url.as_deref(),
            Some("https://proxy.example/v1")
        );
    }

    #[test]
    fn api_base_url_file_overrides_env() {
        let mut file = good_file();
        file.api_base_url = Some("https://file.example/v1".into());
        let env = MapEnv::from_pairs([
            ("ANTHROPIC_API_KEY", "k"),
            ("ANTHROPIC_BASE_URL", "https://env.example/v1"),
        ]);
        let cfg = RunnerConfig::from_file_struct(file, &env).unwrap();
        assert_eq!(cfg.api_base_url.as_deref(), Some("https://file.example/v1"));
    }

    #[test]
    fn provider_default_is_anthropic() {
        let env = MapEnv::default();
        let cfg = RunnerConfig::from_file_struct(good_file(), &env).unwrap();
        assert_eq!(cfg.provider, "anthropic");
    }

    #[test]
    fn provider_ollama_passes_through() {
        let mut file = good_file();
        file.provider = Some("ollama".into());
        let env = MapEnv::default();
        let cfg = RunnerConfig::from_file_struct(file, &env).unwrap();
        assert_eq!(cfg.provider, "ollama");
    }

    #[test]
    fn provider_ollama_shim_passes_through() {
        let mut file = good_file();
        file.provider = Some("ollama-shim".into());
        let env = MapEnv::default();
        let cfg = RunnerConfig::from_file_struct(file, &env).unwrap();
        assert_eq!(cfg.provider, "ollama-shim");
    }

    #[test]
    fn provider_codex_passes_through() {
        let mut file = good_file();
        file.provider = Some("codex".into());
        let env = MapEnv::default();
        let cfg = RunnerConfig::from_file_struct(file, &env).unwrap();
        assert_eq!(cfg.provider, "codex");
    }

    #[test]
    fn codex_binary_and_args_default_to_none() {
        let env = MapEnv::default();
        let cfg = RunnerConfig::from_file_struct(good_file(), &env).unwrap();
        assert!(cfg.codex_binary.is_none());
        assert!(cfg.codex_args.is_none());
    }

    #[test]
    fn codex_binary_and_args_pass_through_from_file() {
        let mut file = good_file();
        file.provider = Some("codex".into());
        file.codex_binary = Some("/opt/codex/bin/codex".into());
        file.codex_args = Some(vec!["--json".into(), "--no-color".into()]);
        let env = MapEnv::default();
        let cfg = RunnerConfig::from_file_struct(file, &env).unwrap();
        assert_eq!(cfg.provider, "codex");
        assert_eq!(cfg.codex_binary.as_deref(), Some("/opt/codex/bin/codex"));
        assert_eq!(
            cfg.codex_args.as_deref(),
            Some(&["--json".to_string(), "--no-color".to_string()][..])
        );
    }

    #[test]
    fn codex_empty_args_round_trip() {
        let mut file = good_file();
        file.provider = Some("codex".into());
        file.codex_args = Some(Vec::new());
        let env = MapEnv::default();
        let cfg = RunnerConfig::from_file_struct(file, &env).unwrap();
        assert_eq!(cfg.codex_args.as_deref(), Some(&[][..]));
    }

    #[test]
    fn provider_claude_alias_maps_to_anthropic() {
        let mut file = good_file();
        file.provider = Some("claude".into());
        let env = MapEnv::default();
        let cfg = RunnerConfig::from_file_struct(file, &env).unwrap();
        assert_eq!(cfg.provider, "anthropic");
    }

    #[test]
    fn provider_unknown_falls_back_to_anthropic() {
        let mut file = good_file();
        file.provider = Some("bogus".into());
        let env = MapEnv::default();
        let cfg = RunnerConfig::from_file_struct(file, &env).unwrap();
        assert_eq!(cfg.provider, "anthropic");
    }

    #[test]
    fn defaults_when_fields_omitted() {
        let mut file = good_file();
        file.effort = None;
        file.model_input_window = None;
        file.safety_margin_tokens = None;
        file.max_tokens = None;
        let env = MapEnv::default();
        let cfg = RunnerConfig::from_file_struct(file, &env).unwrap();
        assert_eq!(cfg.effort, Effort::Medium);
        assert_eq!(
            cfg.model_input_window,
            crate::compaction::DEFAULT_INPUT_WINDOW
        );
        assert_eq!(
            cfg.safety_margin_tokens,
            crate::compaction::DEFAULT_SAFETY_MARGIN
        );
        assert_eq!(cfg.max_tokens, 4096);
    }

    #[test]
    fn missing_session_id_is_error() {
        let mut file = good_file();
        file.session_id = None;
        let env = MapEnv::default();
        let err = RunnerConfig::from_file_struct(file, &env).unwrap_err();
        assert!(matches!(err, ConfigError::MissingField("session_id")));
    }

    #[test]
    fn missing_agent_group_id_is_error() {
        let mut file = good_file();
        file.agent_group_id = None;
        let env = MapEnv::default();
        let err = RunnerConfig::from_file_struct(file, &env).unwrap_err();
        assert!(matches!(err, ConfigError::MissingField("agent_group_id")));
    }

    #[test]
    fn missing_session_dir_is_error() {
        let mut file = good_file();
        file.session_dir = None;
        let env = MapEnv::default();
        let err = RunnerConfig::from_file_struct(file, &env).unwrap_err();
        assert!(matches!(err, ConfigError::MissingField("session_dir")));
    }

    #[test]
    fn missing_model_is_error() {
        let mut file = good_file();
        file.model = None;
        let env = MapEnv::default();
        let err = RunnerConfig::from_file_struct(file, &env).unwrap_err();
        assert!(matches!(err, ConfigError::MissingField("model")));
    }

    #[test]
    fn invalid_session_id_is_error() {
        let mut file = good_file();
        file.session_id = Some("not-a-uuid".into());
        let env = MapEnv::default();
        let err = RunnerConfig::from_file_struct(file, &env).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::InvalidValue {
                field: "session_id",
                ..
            }
        ));
    }

    #[test]
    fn invalid_agent_group_id_is_error() {
        let mut file = good_file();
        file.agent_group_id = Some("nope".into());
        let env = MapEnv::default();
        let err = RunnerConfig::from_file_struct(file, &env).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::InvalidValue {
                field: "agent_group_id",
                ..
            }
        ));
    }

    #[test]
    fn from_file_reads_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cfg.json");
        std::fs::write(&path, serde_json::to_vec(&good_file()).unwrap()).unwrap();
        let env = MapEnv::default();
        let cfg = RunnerConfig::from_file(&path, &env).unwrap();
        assert_eq!(cfg.model, "claude-sonnet-4-6");
    }

    #[test]
    fn from_file_propagates_io_error() {
        let tmp = tempfile::tempdir().unwrap();
        let env = MapEnv::default();
        let err = RunnerConfig::from_file(&tmp.path().join("missing.json"), &env).unwrap_err();
        assert!(matches!(err, ConfigError::Io(_)));
    }

    #[test]
    fn from_file_propagates_json_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.json");
        std::fs::write(&path, "not json").unwrap();
        let env = MapEnv::default();
        let err = RunnerConfig::from_file(&path, &env).unwrap_err();
        assert!(matches!(err, ConfigError::Json(_)));
    }

    #[test]
    fn system_env_is_idempotent() {
        let s = SystemEnv;
        // No assumption about what's set; just make sure the method runs.
        let _ = s.get("PATH");
        let _ = s.get("DEFINITELY_NOT_SET_XYZZY");
    }

    #[test]
    fn map_env_from_pairs() {
        let env = MapEnv::from_pairs([("k", "v"), ("x", "y")]);
        assert_eq!(env.get("k").as_deref(), Some("v"));
        assert_eq!(env.get("x").as_deref(), Some("y"));
        assert!(env.get("missing").is_none());
    }

    #[test]
    fn config_error_display_covers_all() {
        let e = ConfigError::MissingField("model");
        assert!(e.to_string().contains("model"));
        let e = ConfigError::InvalidValue {
            field: "session_id",
            message: "bad".into(),
        };
        assert!(e.to_string().contains("session_id"));
        assert!(e.to_string().contains("bad"));
    }

    #[test]
    fn config_file_serde_roundtrip() {
        let f = good_file();
        let s = serde_json::to_string(&f).unwrap();
        let back: RunnerConfigFile = serde_json::from_str(&s).unwrap();
        assert_eq!(f, back);
    }
}
