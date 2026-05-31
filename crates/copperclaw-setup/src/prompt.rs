//! Prompt abstraction.
//!
//! Steps interact with the user exclusively through the [`Prompt`] trait so
//! the same step code drives all three modes:
//!
//! - [`Interactive`]: thin wrapper around `dialoguer` for terminal use.
//! - [`EnvBacked`]: reads answers from `COPPERCLAW_SETUP_*` env vars.
//! - [`Scripted`]: feeds canned answers in test order. Used in unit tests.

use std::collections::HashMap;
use std::sync::Mutex;

/// Errors a [`Prompt`] can return.
#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    /// Headless mode is missing a required env var.
    #[error("missing environment variable: {0}")]
    Missing(String),
    /// Scripted prompt ran out of answers.
    #[error("scripted prompt exhausted (key: {key})")]
    ScriptExhausted {
        /// Prompt key that was requested.
        key: String,
    },
    /// Underlying I/O failure (only the interactive impl produces this).
    #[error("prompt I/O: {0}")]
    Io(#[from] std::io::Error),
    /// Parse failure converting a string answer to the expected type.
    #[error("parse error for key `{key}`: {message}")]
    Parse {
        /// Prompt key.
        key: String,
        /// Detail.
        message: String,
    },
}

/// Prompt interface.
///
/// Every method takes a stable string `key` so the headless impl can map it
/// to `COPPERCLAW_SETUP_<KEY>`. Keys must be `SCREAMING_SNAKE_CASE`.
pub trait Prompt {
    /// Free-form string question with an optional default.
    fn input(&self, key: &str, message: &str, default: Option<&str>) -> Result<String, PromptError>;

    /// Yes/no question with a default. Returns the boolean answer.
    fn confirm(&self, key: &str, message: &str, default: bool) -> Result<bool, PromptError>;

    /// Hidden input (password / token).
    fn secret(&self, key: &str, message: &str) -> Result<String, PromptError>;
}

/// `dialoguer`-backed prompt for an attached TTY.
#[derive(Debug, Default)]
pub struct Interactive;

impl Interactive {
    /// Construct a fresh interactive prompt.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Prompt for Interactive {
    fn input(&self, _key: &str, message: &str, default: Option<&str>) -> Result<String, PromptError> {
        let mut input: dialoguer::Input<String> = dialoguer::Input::new();
        input = input.with_prompt(message);
        if let Some(default) = default {
            input = input.default(default.to_string());
        }
        input
            .interact_text()
            .map_err(|e| PromptError::Io(std::io::Error::other(e.to_string())))
    }

    fn confirm(&self, _key: &str, message: &str, default: bool) -> Result<bool, PromptError> {
        dialoguer::Confirm::new()
            .with_prompt(message)
            .default(default)
            .interact()
            .map_err(|e| PromptError::Io(std::io::Error::other(e.to_string())))
    }

    fn secret(&self, _key: &str, message: &str) -> Result<String, PromptError> {
        dialoguer::Password::new()
            .with_prompt(message)
            .allow_empty_password(true)
            .interact()
            .map_err(|e| PromptError::Io(std::io::Error::other(e.to_string())))
    }
}

/// Env-var-backed prompt. Used in `--headless` mode.
///
/// Reads from a snapshot of the process environment supplied at construction
/// time so tests can drive the impl without mutating the global environment.
#[derive(Debug, Default, Clone)]
pub struct EnvBacked {
    /// Mapping of full env-var name → value.
    pub env: HashMap<String, String>,
}

impl EnvBacked {
    /// Snapshot the current process environment.
    #[must_use]
    pub fn from_process_env() -> Self {
        Self {
            env: std::env::vars().collect(),
        }
    }

    /// Construct from an explicit map (testing).
    #[must_use]
    pub fn with_env(env: HashMap<String, String>) -> Self {
        Self { env }
    }

    /// Full name of the env var for a given key.
    #[must_use]
    pub fn var_name(key: &str) -> String {
        format!("COPPERCLAW_SETUP_{key}")
    }

    fn get(&self, key: &str) -> Option<&String> {
        self.env.get(&Self::var_name(key))
    }
}

impl Prompt for EnvBacked {
    fn input(&self, key: &str, _message: &str, default: Option<&str>) -> Result<String, PromptError> {
        if let Some(value) = self.get(key) {
            return Ok(value.clone());
        }
        if let Some(default) = default {
            return Ok(default.to_string());
        }
        Err(PromptError::Missing(Self::var_name(key)))
    }

    fn confirm(&self, key: &str, _message: &str, default: bool) -> Result<bool, PromptError> {
        let Some(value) = self.get(key) else {
            return Ok(default);
        };
        match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "y" | "on" => Ok(true),
            "0" | "false" | "no" | "n" | "off" => Ok(false),
            other => Err(PromptError::Parse {
                key: Self::var_name(key),
                message: format!("expected boolean, got `{other}`"),
            }),
        }
    }

    fn secret(&self, key: &str, _message: &str) -> Result<String, PromptError> {
        self.get(key)
            .cloned()
            .ok_or_else(|| PromptError::Missing(Self::var_name(key)))
    }
}

/// Scripted prompt used in tests. Returns canned answers in FIFO order per
/// key.
#[derive(Debug, Default)]
pub struct Scripted {
    answers: Mutex<HashMap<String, Vec<String>>>,
}

impl Scripted {
    /// Fresh empty script.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a canned answer for `key` onto the queue.
    pub fn push(&self, key: &str, value: impl Into<String>) {
        self.answers
            .lock()
            .expect("scripted prompt poisoned")
            .entry(key.to_string())
            .or_default()
            .push(value.into());
    }

    /// Convenience builder: push and return `self`.
    #[must_use]
    pub fn with(self, key: &str, value: impl Into<String>) -> Self {
        self.push(key, value);
        self
    }

    fn pop(&self, key: &str) -> Result<String, PromptError> {
        let mut map = self
            .answers
            .lock()
            .expect("scripted prompt poisoned");
        let queue = map.get_mut(key).ok_or_else(|| PromptError::ScriptExhausted {
            key: key.to_string(),
        })?;
        if queue.is_empty() {
            return Err(PromptError::ScriptExhausted {
                key: key.to_string(),
            });
        }
        Ok(queue.remove(0))
    }
}

impl Prompt for Scripted {
    fn input(&self, key: &str, _message: &str, default: Option<&str>) -> Result<String, PromptError> {
        match self.pop(key) {
            Ok(v) => Ok(v),
            Err(PromptError::ScriptExhausted { .. }) if default.is_some() => {
                Ok(default.unwrap().to_string())
            }
            Err(e) => Err(e),
        }
    }

    fn confirm(&self, key: &str, _message: &str, default: bool) -> Result<bool, PromptError> {
        match self.pop(key) {
            Ok(v) => match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "y" => Ok(true),
                "0" | "false" | "no" | "n" => Ok(false),
                other => Err(PromptError::Parse {
                    key: key.to_string(),
                    message: format!("expected boolean, got `{other}`"),
                }),
            },
            Err(PromptError::ScriptExhausted { .. }) => Ok(default),
            Err(e) => Err(e),
        }
    }

    fn secret(&self, key: &str, _message: &str) -> Result<String, PromptError> {
        self.pop(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- EnvBacked ----

    #[test]
    fn env_var_name_is_prefixed() {
        assert_eq!(EnvBacked::var_name("DATA_DIR"), "COPPERCLAW_SETUP_DATA_DIR");
    }

    #[test]
    fn env_input_uses_env_when_present() {
        let mut env = HashMap::new();
        env.insert("COPPERCLAW_SETUP_DATA_DIR".into(), "/srv/x".into());
        let p = EnvBacked::with_env(env);
        assert_eq!(p.input("DATA_DIR", "where?", None).unwrap(), "/srv/x");
    }

    #[test]
    fn env_input_uses_default_when_missing() {
        let p = EnvBacked::default();
        assert_eq!(
            p.input("MISSING", "?", Some("def")).unwrap(),
            "def".to_string()
        );
    }

    #[test]
    fn env_input_errors_without_default() {
        let p = EnvBacked::default();
        let err = p.input("MISSING", "?", None).unwrap_err();
        match err {
            PromptError::Missing(name) => assert_eq!(name, "COPPERCLAW_SETUP_MISSING"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn env_confirm_returns_default_when_missing() {
        let p = EnvBacked::default();
        assert!(p.confirm("F", "?", true).unwrap());
        assert!(!p.confirm("F", "?", false).unwrap());
    }

    #[test]
    fn env_confirm_parses_true_variants() {
        for v in ["1", "true", "TRUE", "yes", "Y", "on"] {
            let mut env = HashMap::new();
            env.insert("COPPERCLAW_SETUP_X".into(), v.into());
            let p = EnvBacked::with_env(env);
            assert!(p.confirm("X", "?", false).unwrap(), "value={v}");
        }
    }

    #[test]
    fn env_confirm_parses_false_variants() {
        for v in ["0", "false", "no", "N", "off"] {
            let mut env = HashMap::new();
            env.insert("COPPERCLAW_SETUP_X".into(), v.into());
            let p = EnvBacked::with_env(env);
            assert!(!p.confirm("X", "?", true).unwrap(), "value={v}");
        }
    }

    #[test]
    fn env_confirm_garbage_is_parse_error() {
        let mut env = HashMap::new();
        env.insert("COPPERCLAW_SETUP_X".into(), "maybe".into());
        let p = EnvBacked::with_env(env);
        let err = p.confirm("X", "?", false).unwrap_err();
        assert!(matches!(err, PromptError::Parse { .. }));
    }

    #[test]
    fn env_secret_requires_present() {
        let p = EnvBacked::default();
        let err = p.secret("TOKEN", "tok?").unwrap_err();
        assert!(matches!(err, PromptError::Missing(_)));
    }

    #[test]
    fn env_secret_reads_value() {
        let mut env = HashMap::new();
        env.insert("COPPERCLAW_SETUP_TOKEN".into(), "sk-123".into());
        let p = EnvBacked::with_env(env);
        assert_eq!(p.secret("TOKEN", "?").unwrap(), "sk-123");
    }

    #[test]
    fn from_process_env_does_not_panic() {
        let _p = EnvBacked::from_process_env();
    }

    // ---- Scripted ----

    #[test]
    fn scripted_input_returns_queued_then_exhausts() {
        let p = Scripted::new();
        p.push("X", "first");
        p.push("X", "second");
        assert_eq!(p.input("X", "?", None).unwrap(), "first");
        assert_eq!(p.input("X", "?", None).unwrap(), "second");
        let err = p.input("X", "?", None).unwrap_err();
        assert!(matches!(err, PromptError::ScriptExhausted { .. }));
    }

    #[test]
    fn scripted_input_falls_back_to_default_when_exhausted() {
        let p = Scripted::new();
        let got = p.input("X", "?", Some("def")).unwrap();
        assert_eq!(got, "def");
    }

    #[test]
    fn scripted_confirm_returns_queued() {
        let p = Scripted::new().with("Y", "yes").with("N", "no");
        assert!(p.confirm("Y", "?", false).unwrap());
        assert!(!p.confirm("N", "?", true).unwrap());
    }

    #[test]
    fn scripted_confirm_returns_default_when_exhausted() {
        let p = Scripted::new();
        assert!(p.confirm("Z", "?", true).unwrap());
        assert!(!p.confirm("Z", "?", false).unwrap());
    }

    #[test]
    fn scripted_confirm_garbage_is_parse_error() {
        let p = Scripted::new().with("Z", "maybe");
        let err = p.confirm("Z", "?", false).unwrap_err();
        assert!(matches!(err, PromptError::Parse { .. }));
    }

    #[test]
    fn scripted_secret_returns_queued() {
        let p = Scripted::new().with("TOK", "sk-1");
        assert_eq!(p.secret("TOK", "?").unwrap(), "sk-1");
    }

    #[test]
    fn scripted_secret_exhausts() {
        let p = Scripted::new();
        let err = p.secret("TOK", "?").unwrap_err();
        assert!(matches!(err, PromptError::ScriptExhausted { .. }));
    }

    #[test]
    fn scripted_with_builder_chains() {
        let p = Scripted::new().with("A", "1").with("A", "2");
        assert_eq!(p.input("A", "?", None).unwrap(), "1");
        assert_eq!(p.input("A", "?", None).unwrap(), "2");
    }

    #[test]
    fn interactive_constructor_smoke() {
        let _p = Interactive::new();
    }

    #[test]
    fn prompt_error_display_missing() {
        let e = PromptError::Missing("COPPERCLAW_SETUP_X".into());
        assert!(e.to_string().contains("COPPERCLAW_SETUP_X"));
    }

    #[test]
    fn prompt_error_display_exhausted() {
        let e = PromptError::ScriptExhausted { key: "X".into() };
        assert!(e.to_string().contains('X'));
    }

    #[test]
    fn prompt_error_display_parse() {
        let e = PromptError::Parse {
            key: "X".into(),
            message: "bad".into(),
        };
        let s = e.to_string();
        assert!(s.contains('X'));
        assert!(s.contains("bad"));
    }

    #[test]
    fn env_backed_drives_full_step_set() {
        // End-to-end check: feed the canonical env-var names into EnvBacked
        // and confirm each step that uses them gets the expected answer.
        let mut env = HashMap::new();
        env.insert("COPPERCLAW_SETUP_DATA_DIR".into(), "/srv/x".into());
        env.insert("COPPERCLAW_SETUP_BUILD_IMAGE".into(), "no".into());
        env.insert("COPPERCLAW_SETUP_USE_ONECLI".into(), "no".into());
        env.insert("COPPERCLAW_SETUP_ANTHROPIC_API_KEY".into(), "sk-test".into());
        env.insert("COPPERCLAW_SETUP_MOUNTS".into(), String::new());
        env.insert("COPPERCLAW_SETUP_WRITE_SERVICE_UNIT".into(), "no".into());
        env.insert("COPPERCLAW_SETUP_TIMEZONE".into(), "Etc/UTC".into());
        env.insert("COPPERCLAW_SETUP_FIRST_CHANNEL".into(), "cli".into());
        let p = EnvBacked::with_env(env);
        assert_eq!(p.input("DATA_DIR", "?", None).unwrap(), "/srv/x");
        assert!(!p.confirm("BUILD_IMAGE", "?", true).unwrap());
        assert!(!p.confirm("USE_ONECLI", "?", true).unwrap());
        assert_eq!(p.secret("ANTHROPIC_API_KEY", "?").unwrap(), "sk-test");
        assert_eq!(p.input("MOUNTS", "?", Some("")).unwrap(), "");
        assert!(!p.confirm("WRITE_SERVICE_UNIT", "?", true).unwrap());
        assert_eq!(p.input("TIMEZONE", "?", None).unwrap(), "Etc/UTC");
        assert_eq!(p.input("FIRST_CHANNEL", "?", None).unwrap(), "cli");
    }

    #[test]
    fn prompt_error_io_from_std() {
        let inner = std::io::Error::new(std::io::ErrorKind::Other, "boom");
        let err: PromptError = inner.into();
        assert!(matches!(err, PromptError::Io(_)));
    }
}
