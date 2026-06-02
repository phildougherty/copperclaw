//! Matrix channel configuration parsed from `ChannelSetup::config`.
//!
//! Required fields: `homeserver_url`, `access_token`, `user_id`.
//! Optional fields: `rooms` (list of room ids / aliases to limit `/sync` to),
//! `sync_timeout_ms` (default [`DEFAULT_SYNC_TIMEOUT_MS`]), and `txn_prefix`
//! (default [`DEFAULT_TXN_PREFIX`]) used as a per-process prefix on Matrix
//! transaction ids.

use copperclaw_channels_core::AdapterError;
use serde_json::Value;

/// Default `/sync` long-poll timeout in milliseconds (30 seconds).
pub const DEFAULT_SYNC_TIMEOUT_MS: u64 = 30_000;

/// Default transaction-id prefix used by [`MatrixApi`](crate::api::MatrixApi).
pub const DEFAULT_TXN_PREFIX: &str = "copperclaw";

/// Fully resolved Matrix channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixConfig {
    /// Base homeserver URL, e.g. `https://matrix.org`. Trailing slash stripped.
    pub homeserver_url: String,
    /// Bot user's access token (Bearer credential).
    pub access_token: String,
    /// Bot's MXID, e.g. `@bot:matrix.org`.
    pub user_id: String,
    /// Rooms the `/sync` filter limits to. Empty disables filtering.
    pub rooms: Vec<String>,
    /// `/sync` long-poll timeout in milliseconds.
    pub sync_timeout_ms: u64,
    /// Prefix attached to every Matrix `txnId`.
    pub txn_prefix: String,
}

impl MatrixConfig {
    /// Parse a JSON config value into a [`MatrixConfig`].
    ///
    /// The expected shape:
    ///
    /// ```json
    /// {
    ///   "homeserver_url": "https://matrix.org",
    ///   "access_token":   "...",
    ///   "user_id":        "@bot:matrix.org",
    ///   "rooms":          ["!abc:matrix.org", "#alias:matrix.org"],
    ///   "sync_timeout_ms": 30000,
    ///   "txn_prefix":     "copperclaw"
    /// }
    /// ```
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        let obj = value.as_object().ok_or_else(|| {
            AdapterError::BadRequest("matrix config must be a JSON object".into())
        })?;

        let homeserver_url = required_nonempty_string(obj, "homeserver_url")?
            .trim_end_matches('/')
            .to_owned();
        let access_token = required_nonempty_string(obj, "access_token")?;
        let user_id = required_nonempty_string(obj, "user_id")?;

        if !user_id.starts_with('@') {
            return Err(AdapterError::BadRequest(format!(
                "matrix user_id must start with `@`, got `{user_id}`"
            )));
        }

        let rooms = match obj.get("rooms") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(arr)) => {
                let mut out = Vec::with_capacity(arr.len());
                for v in arr {
                    let s = v.as_str().ok_or_else(|| {
                        AdapterError::BadRequest("matrix rooms must be strings".into())
                    })?;
                    if s.is_empty() {
                        return Err(AdapterError::BadRequest(
                            "matrix rooms entries must not be empty".into(),
                        ));
                    }
                    if !(s.starts_with('!') || s.starts_with('#')) {
                        return Err(AdapterError::BadRequest(format!(
                            "matrix room `{s}` must start with `!` (room id) or `#` (alias)"
                        )));
                    }
                    out.push(s.to_owned());
                }
                out
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "matrix rooms must be an array".into(),
                ));
            }
        };

        let sync_timeout_ms = match obj.get("sync_timeout_ms") {
            None | Some(Value::Null) => DEFAULT_SYNC_TIMEOUT_MS,
            Some(Value::Number(n)) => n.as_u64().ok_or_else(|| {
                AdapterError::BadRequest(
                    "matrix sync_timeout_ms must be a non-negative integer".into(),
                )
            })?,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "matrix sync_timeout_ms must be a number".into(),
                ));
            }
        };

        let txn_prefix = match obj.get("txn_prefix") {
            None | Some(Value::Null) => DEFAULT_TXN_PREFIX.to_owned(),
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "matrix txn_prefix must not be empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "matrix txn_prefix must be a string".into(),
                ));
            }
        };

        Ok(Self {
            homeserver_url,
            access_token,
            user_id,
            rooms,
            sync_timeout_ms,
            txn_prefix,
        })
    }
}

fn required_nonempty_string(
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String, AdapterError> {
    match obj.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err(AdapterError::BadRequest(format!(
            "matrix {key} must not be empty"
        ))),
        Some(_) => Err(AdapterError::BadRequest(format!(
            "matrix {key} must be a string"
        ))),
        None => Err(AdapterError::BadRequest(format!(
            "matrix {key} is required"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_minimal_config() {
        let cfg = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://matrix.org",
            "access_token": "tok",
            "user_id": "@bot:matrix.org"
        }))
        .unwrap();
        assert_eq!(cfg.homeserver_url, "https://matrix.org");
        assert_eq!(cfg.access_token, "tok");
        assert_eq!(cfg.user_id, "@bot:matrix.org");
        assert!(cfg.rooms.is_empty());
        assert_eq!(cfg.sync_timeout_ms, DEFAULT_SYNC_TIMEOUT_MS);
        assert_eq!(cfg.txn_prefix, DEFAULT_TXN_PREFIX);
    }

    #[test]
    fn trims_trailing_slash_on_homeserver_url() {
        let cfg = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://matrix.org/",
            "access_token": "tok",
            "user_id": "@b:m.org"
        }))
        .unwrap();
        assert_eq!(cfg.homeserver_url, "https://matrix.org");
    }

    #[test]
    fn parses_full_config() {
        let cfg = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok",
            "user_id": "@b:m.org",
            "rooms": ["!a:m.org", "#alias:m.org"],
            "sync_timeout_ms": 5000,
            "txn_prefix": "p"
        }))
        .unwrap();
        assert_eq!(cfg.rooms, vec!["!a:m.org", "#alias:m.org"]);
        assert_eq!(cfg.sync_timeout_ms, 5000);
        assert_eq!(cfg.txn_prefix, "p");
    }

    #[test]
    fn missing_homeserver_url_errors() {
        let err = MatrixConfig::from_value(&json!({
            "access_token": "tok",
            "user_id": "@b:m.org"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("homeserver_url")));
    }

    #[test]
    fn missing_access_token_errors() {
        let err = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "user_id": "@b:m.org"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("access_token")));
    }

    #[test]
    fn missing_user_id_errors() {
        let err = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("user_id")));
    }

    #[test]
    fn empty_homeserver_url_errors() {
        let err = MatrixConfig::from_value(&json!({
            "homeserver_url": "",
            "access_token": "tok",
            "user_id": "@b:m.org"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("homeserver_url")));
    }

    #[test]
    fn empty_token_errors() {
        let err = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "",
            "user_id": "@b:m.org"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("access_token")));
    }

    #[test]
    fn non_string_homeserver_url_errors() {
        let err = MatrixConfig::from_value(&json!({
            "homeserver_url": 7,
            "access_token": "tok",
            "user_id": "@b:m.org"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn user_id_without_at_prefix_errors() {
        let err = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok",
            "user_id": "bot:m.org"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("user_id")));
    }

    #[test]
    fn rooms_must_be_array() {
        let err = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok",
            "user_id": "@b:m.org",
            "rooms": "!a:m.org"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rooms_entries_must_be_strings() {
        let err = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok",
            "user_id": "@b:m.org",
            "rooms": [7]
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rooms_entries_must_start_with_correct_sigil() {
        let err = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok",
            "user_id": "@b:m.org",
            "rooms": ["badroom"]
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("must start")));
    }

    #[test]
    fn rooms_alias_entry_is_accepted() {
        let cfg = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok",
            "user_id": "@b:m.org",
            "rooms": ["#alias:m.org"]
        }))
        .unwrap();
        assert_eq!(cfg.rooms, vec!["#alias:m.org"]);
    }

    #[test]
    fn rooms_empty_string_rejected() {
        let err = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok",
            "user_id": "@b:m.org",
            "rooms": [""]
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rooms_null_defaults_to_empty() {
        let cfg = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok",
            "user_id": "@b:m.org",
            "rooms": null
        }))
        .unwrap();
        assert!(cfg.rooms.is_empty());
    }

    #[test]
    fn sync_timeout_ms_accepts_number() {
        let cfg = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok",
            "user_id": "@b:m.org",
            "sync_timeout_ms": 1234
        }))
        .unwrap();
        assert_eq!(cfg.sync_timeout_ms, 1234);
    }

    #[test]
    fn sync_timeout_ms_rejects_negative() {
        let err = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok",
            "user_id": "@b:m.org",
            "sync_timeout_ms": -1
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn sync_timeout_ms_non_number_errors() {
        let err = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok",
            "user_id": "@b:m.org",
            "sync_timeout_ms": "fast"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn sync_timeout_ms_null_defaults() {
        let cfg = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok",
            "user_id": "@b:m.org",
            "sync_timeout_ms": null
        }))
        .unwrap();
        assert_eq!(cfg.sync_timeout_ms, DEFAULT_SYNC_TIMEOUT_MS);
    }

    #[test]
    fn txn_prefix_overridable() {
        let cfg = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok",
            "user_id": "@b:m.org",
            "txn_prefix": "myapp"
        }))
        .unwrap();
        assert_eq!(cfg.txn_prefix, "myapp");
    }

    #[test]
    fn txn_prefix_empty_string_errors() {
        let err = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok",
            "user_id": "@b:m.org",
            "txn_prefix": ""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn txn_prefix_wrong_type_errors() {
        let err = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok",
            "user_id": "@b:m.org",
            "txn_prefix": 7
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn non_object_config_errors() {
        let err = MatrixConfig::from_value(&json!("nope")).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn clone_and_eq_roundtrip() {
        let cfg = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok",
            "user_id": "@b:m.org"
        }))
        .unwrap();
        let copy = cfg.clone();
        assert_eq!(cfg, copy);
    }

    #[test]
    fn debug_format_renders() {
        let cfg = MatrixConfig::from_value(&json!({
            "homeserver_url": "https://m.org",
            "access_token": "tok",
            "user_id": "@b:m.org"
        }))
        .unwrap();
        let s = format!("{cfg:?}");
        assert!(s.contains("MatrixConfig"));
    }

    #[test]
    fn defaults_constants() {
        assert_eq!(DEFAULT_SYNC_TIMEOUT_MS, 30_000);
        assert_eq!(DEFAULT_TXN_PREFIX, "copperclaw");
    }
}
