//! Runner configuration.
//!
//! The runner is configured by a JSON file + environment variables. The
//! JSON file lives at the path passed via `--config` (or `IRONCLAW_RUNNER_CONFIG`).
//! Environment variables override individual fields when set.
//!
//! All fields with security implications (API keys) are resolved by
//! reading the named environment variable at startup rather than baking
//! the key into the JSON file. That way the file can be checked into
//! version control or shipped over the bind-mount without leaking secrets.

use std::path::{Path, PathBuf};

use ironclaw_types::{AgentGroupId, Effort, SessionId};
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
    /// Provider-native model id.
    pub model: Option<String>,
    /// Tier-of-effort hint.
    pub effort: Option<Effort>,
    /// System prompt to prepend to every turn.
    pub system: Option<String>,
    /// Name of the environment variable to read the API key from.
    pub api_key_env: Option<String>,
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
    /// Provider-native model id.
    pub model: String,
    /// Tier-of-effort hint.
    pub effort: Effort,
    /// System prompt.
    pub system: String,
    /// API key value, resolved at startup from `api_key_env`.
    pub api_key: Option<String>,
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
        Ok(Self {
            session_id,
            agent_group_id,
            session_dir,
            model,
            effort: file.effort.unwrap_or(Effort::Medium),
            system,
            api_key,
            model_input_window: file
                .model_input_window
                .unwrap_or(crate::compaction::DEFAULT_INPUT_WINDOW),
            safety_margin_tokens: file
                .safety_margin_tokens
                .unwrap_or(crate::compaction::DEFAULT_SAFETY_MARGIN),
            max_tokens: file.max_tokens.unwrap_or(4096),
            assistant_name: file.assistant_name,
            temperature: file.temperature,
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
        Self(iter.into_iter().map(|(k, v)| (k.into(), v.into())).collect())
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
            session_dir: Some("/tmp/ironclaw/session".into()),
            model: Some("claude-sonnet-4-5".into()),
            effort: Some(Effort::High),
            system: Some("you are an agent".into()),
            api_key_env: Some("ANTHROPIC_API_KEY".into()),
            model_input_window: Some(200_000),
            safety_margin_tokens: Some(8_000),
            max_tokens: Some(4096),
            assistant_name: Some("Claude".into()),
            temperature: Some(0.7),
        }
    }

    #[test]
    fn from_file_struct_happy_path() {
        let env = MapEnv::from_pairs([("ANTHROPIC_API_KEY", "key-xyz")]);
        let cfg = RunnerConfig::from_file_struct(good_file(), &env).unwrap();
        assert_eq!(cfg.model, "claude-sonnet-4-5");
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
    fn defaults_when_fields_omitted() {
        let mut file = good_file();
        file.effort = None;
        file.model_input_window = None;
        file.safety_margin_tokens = None;
        file.max_tokens = None;
        let env = MapEnv::default();
        let cfg = RunnerConfig::from_file_struct(file, &env).unwrap();
        assert_eq!(cfg.effort, Effort::Medium);
        assert_eq!(cfg.model_input_window, crate::compaction::DEFAULT_INPUT_WINDOW);
        assert_eq!(cfg.safety_margin_tokens, crate::compaction::DEFAULT_SAFETY_MARGIN);
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
        assert!(matches!(err, ConfigError::InvalidValue { field: "session_id", .. }));
    }

    #[test]
    fn invalid_agent_group_id_is_error() {
        let mut file = good_file();
        file.agent_group_id = Some("nope".into());
        let env = MapEnv::default();
        let err = RunnerConfig::from_file_struct(file, &env).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidValue { field: "agent_group_id", .. }));
    }

    #[test]
    fn from_file_reads_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cfg.json");
        std::fs::write(&path, serde_json::to_vec(&good_file()).unwrap()).unwrap();
        let env = MapEnv::default();
        let cfg = RunnerConfig::from_file(&path, &env).unwrap();
        assert_eq!(cfg.model, "claude-sonnet-4-5");
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
