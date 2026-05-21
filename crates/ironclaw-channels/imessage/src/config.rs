//! Parser for [`IMessageConfig`] — the JSON shape the host hands the
//! factory.
//!
//! Schema (all fields optional; defaults shown):
//!
//! ```json
//! {
//!   "osascript_bin":     "osascript",
//!   "sqlite3_bin":       "sqlite3",
//!   "chat_db_path":      "~/Library/Messages/chat.db",
//!   "service_name":      "iMessage",
//!   "poll_interval_ms":  2000,
//!   "since_rowid_file":  "imessage_since_rowid.txt",
//!   "enable_polling":    true
//! }
//! ```
//!
//! `chat_db_path` is expanded for a leading `~/` (relative to `$HOME`) at
//! parse time. Other path/string fields are passed through unchanged.

use ironclaw_channels_core::AdapterError;
use serde_json::Value;
use std::path::PathBuf;

/// Default `osascript` binary name. Resolved against `PATH` at spawn time.
pub const DEFAULT_OSASCRIPT_BIN: &str = "osascript";

/// Default `sqlite3` binary name.
pub const DEFAULT_SQLITE3_BIN: &str = "sqlite3";

/// Default path (relative to `$HOME`) of the Messages.app chat database.
pub const DEFAULT_CHAT_DB_PATH: &str = "~/Library/Messages/chat.db";

/// Default Messages.app service name used in the AppleScript template.
pub const DEFAULT_SERVICE_NAME: &str = "iMessage";

/// Default poll interval (milliseconds) between successive `sqlite3` reads
/// of `chat.db`.
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 2_000;

/// Default filename (under `data_dir`) where the last seen `ROWID` is
/// persisted.
pub const DEFAULT_SINCE_ROWID_FILE: &str = "imessage_since_rowid.txt";

/// Parsed iMessage channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IMessageConfig {
    /// `osascript` program path.
    pub osascript_bin: String,
    /// `sqlite3` program path.
    pub sqlite3_bin: String,
    /// Filesystem path to the Messages.app chat database. A leading `~/`
    /// is expanded against `$HOME` at parse time.
    pub chat_db_path: PathBuf,
    /// Messages.app service name (always `"iMessage"` in practice; exposed
    /// for testability + potential SMS bridging).
    pub service_name: String,
    /// Milliseconds between poll attempts.
    pub poll_interval_ms: u64,
    /// Filename (under `data_dir`) where the inbound poll persists its
    /// high-water `ROWID`.
    pub since_rowid_file: String,
    /// When `false`, the adapter does not start a poll task; only outbound
    /// delivery works. Useful in test scenarios and on hosts where chat.db
    /// is unreadable (the user can still send via the agent).
    pub enable_polling: bool,
}

impl Default for IMessageConfig {
    fn default() -> Self {
        Self {
            osascript_bin: DEFAULT_OSASCRIPT_BIN.to_owned(),
            sqlite3_bin: DEFAULT_SQLITE3_BIN.to_owned(),
            chat_db_path: expand_home(DEFAULT_CHAT_DB_PATH),
            service_name: DEFAULT_SERVICE_NAME.to_owned(),
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            since_rowid_file: DEFAULT_SINCE_ROWID_FILE.to_owned(),
            enable_polling: true,
        }
    }
}

impl IMessageConfig {
    /// Parse from the host-provided JSON blob.
    ///
    /// - `Value::Null` and an empty object both yield [`Self::default`].
    /// - Unknown fields are rejected with [`AdapterError::BadRequest`].
    /// - Type mismatches are rejected with [`AdapterError::BadRequest`].
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let obj = value.as_object().ok_or_else(|| {
            AdapterError::BadRequest("imessage config must be a JSON object".into())
        })?;
        let mut cfg = Self::default();
        for (key, val) in obj {
            match key.as_str() {
                "osascript_bin" => {
                    cfg.osascript_bin = take_string(key, val)?;
                }
                "sqlite3_bin" => {
                    cfg.sqlite3_bin = take_string(key, val)?;
                }
                "chat_db_path" => {
                    cfg.chat_db_path = expand_home(&take_string(key, val)?);
                }
                "service_name" => {
                    cfg.service_name = take_string(key, val)?;
                }
                "poll_interval_ms" => {
                    cfg.poll_interval_ms = take_u64(key, val)?;
                }
                "since_rowid_file" => {
                    cfg.since_rowid_file = take_string(key, val)?;
                }
                "enable_polling" => {
                    cfg.enable_polling = take_bool(key, val)?;
                }
                other => {
                    return Err(AdapterError::BadRequest(format!(
                        "imessage config: unknown field `{other}`"
                    )));
                }
            }
        }
        Ok(cfg)
    }
}

/// Expand a leading `~/` to `$HOME` (best effort). When `$HOME` is unset
/// the tilde is left as-is.
pub fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn take_string(key: &str, value: &Value) -> Result<String, AdapterError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        _ => Err(AdapterError::BadRequest(format!(
            "imessage config: field `{key}` must be a string"
        ))),
    }
}

fn take_u64(key: &str, value: &Value) -> Result<u64, AdapterError> {
    value.as_u64().ok_or_else(|| {
        AdapterError::BadRequest(format!(
            "imessage config: field `{key}` must be a non-negative integer"
        ))
    })
}

fn take_bool(key: &str, value: &Value) -> Result<bool, AdapterError> {
    value.as_bool().ok_or_else(|| {
        AdapterError::BadRequest(format!(
            "imessage config: field `{key}` must be a boolean"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_when_null() {
        let c = IMessageConfig::from_value(&Value::Null).unwrap();
        assert_eq!(c, IMessageConfig::default());
    }

    #[test]
    fn defaults_when_empty_object() {
        let c = IMessageConfig::from_value(&json!({})).unwrap();
        assert_eq!(c, IMessageConfig::default());
    }

    #[test]
    fn full_config_parses() {
        let c = IMessageConfig::from_value(&json!({
            "osascript_bin": "/usr/bin/osascript",
            "sqlite3_bin": "/usr/bin/sqlite3",
            "chat_db_path": "/tmp/chat.db",
            "service_name": "iMessage",
            "poll_interval_ms": 500,
            "since_rowid_file": "rowid.txt",
            "enable_polling": false
        }))
        .unwrap();
        assert_eq!(c.osascript_bin, "/usr/bin/osascript");
        assert_eq!(c.sqlite3_bin, "/usr/bin/sqlite3");
        assert_eq!(c.chat_db_path, PathBuf::from("/tmp/chat.db"));
        assert_eq!(c.service_name, "iMessage");
        assert_eq!(c.poll_interval_ms, 500);
        assert_eq!(c.since_rowid_file, "rowid.txt");
        assert!(!c.enable_polling);
    }

    #[test]
    fn rejects_non_object_top_level() {
        let err = IMessageConfig::from_value(&json!("nope")).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_unknown_field() {
        let err = IMessageConfig::from_value(&json!({ "frob": 1 })).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("frob")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_string_osascript_bin() {
        let err =
            IMessageConfig::from_value(&json!({ "osascript_bin": 5 })).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_sqlite3_bin() {
        let err =
            IMessageConfig::from_value(&json!({ "sqlite3_bin": 5 })).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_chat_db_path() {
        let err =
            IMessageConfig::from_value(&json!({ "chat_db_path": 5 })).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_service_name() {
        let err =
            IMessageConfig::from_value(&json!({ "service_name": 5 })).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_since_rowid_file() {
        let err =
            IMessageConfig::from_value(&json!({ "since_rowid_file": 5 }))
                .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_negative_poll_interval() {
        let err =
            IMessageConfig::from_value(&json!({ "poll_interval_ms": -1 }))
                .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_string_poll_interval() {
        let err =
            IMessageConfig::from_value(&json!({ "poll_interval_ms": "fast" }))
                .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn poll_interval_zero_is_accepted() {
        let c =
            IMessageConfig::from_value(&json!({ "poll_interval_ms": 0 })).unwrap();
        assert_eq!(c.poll_interval_ms, 0);
    }

    #[test]
    fn rejects_non_bool_enable_polling() {
        let err =
            IMessageConfig::from_value(&json!({ "enable_polling": "yes" }))
                .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn expand_home_with_tilde_uses_home_or_falls_back() {
        // We can't safely mutate $HOME in this process (the workspace
        // lints forbid unsafe blocks, and `set_var` is unsafe under
        // edition 2024). Instead, we exercise the branch using whatever
        // $HOME the test runner exposes — every CI environment we ship
        // to sets it. If HOME is genuinely missing, the function returns
        // the input unchanged, which we also accept.
        let p = expand_home("~/Library/Messages/chat.db");
        if let Some(home) = std::env::var_os("HOME") {
            let expected = PathBuf::from(home).join("Library/Messages/chat.db");
            assert_eq!(p, expected);
        } else {
            assert_eq!(p, PathBuf::from("~/Library/Messages/chat.db"));
        }
    }

    #[test]
    fn expand_home_without_tilde_is_passthrough() {
        let p = expand_home("/abs/path");
        assert_eq!(p, PathBuf::from("/abs/path"));
    }

    #[test]
    fn expand_home_just_tilde_is_passthrough() {
        // A bare `~` (not followed by `/`) is not expanded.
        let p = expand_home("~");
        assert_eq!(p, PathBuf::from("~"));
    }

    #[test]
    fn default_constants_match_struct() {
        let d = IMessageConfig::default();
        assert_eq!(d.osascript_bin, DEFAULT_OSASCRIPT_BIN);
        assert_eq!(d.sqlite3_bin, DEFAULT_SQLITE3_BIN);
        assert_eq!(d.service_name, DEFAULT_SERVICE_NAME);
        assert_eq!(d.poll_interval_ms, DEFAULT_POLL_INTERVAL_MS);
        assert_eq!(d.since_rowid_file, DEFAULT_SINCE_ROWID_FILE);
        assert!(d.enable_polling);
    }

    #[test]
    fn default_chat_db_path_is_expanded() {
        // Whatever HOME contains, the default should not start with `~/`.
        let d = IMessageConfig::default();
        let s = d.chat_db_path.to_string_lossy();
        assert!(
            !s.starts_with("~/"),
            "default chat_db_path was not expanded: {s}"
        );
    }

    #[test]
    fn config_clone_eq_debug() {
        let a = IMessageConfig::default();
        let b = a.clone();
        assert_eq!(a, b);
        let s = format!("{a:?}");
        assert!(s.contains("IMessageConfig"));
    }
}
