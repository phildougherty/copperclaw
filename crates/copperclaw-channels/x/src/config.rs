//! Twitter / X channel configuration parsed from `ChannelSetup::config`.
//!
//! Required fields: `bearer_token`, `user_id`.
//! Optional fields:
//! - `api_base` (default [`DEFAULT_API_BASE`]) — v2 REST base URL.
//! - `media_base` (default [`DEFAULT_MEDIA_BASE`]) — the v1.1 media upload
//!   base URL. X has not migrated media upload to v2, so we still talk to
//!   `upload.twitter.com`.
//! - `since_id_filename` (default [`DEFAULT_SINCE_ID_FILENAME`]) — the file
//!   inside `data_dir` used to persist the last seen DM event id.
//! - `poll_interval_ms` (default [`DEFAULT_POLL_INTERVAL_MS`]) — interval
//!   between successive `dm_events` polls.

use copperclaw_channels_core::AdapterError;
use serde_json::Value;

/// Default v2 REST base URL.
pub const DEFAULT_API_BASE: &str = "https://api.twitter.com";

/// Default v1.1 media base URL.
pub const DEFAULT_MEDIA_BASE: &str = "https://upload.twitter.com";

/// Default filename used to persist the last seen DM event id.
pub const DEFAULT_SINCE_ID_FILENAME: &str = "x_dm_since_id.txt";

/// Default interval between `dm_events` polls (30 seconds).
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 30_000;

/// Fully-resolved Twitter / X channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XConfig {
    /// User-context `OAuth2` bearer token. Must carry `dm.read` / `dm.write`.
    pub bearer_token: String,
    /// Numeric string id of the bot user (e.g. `"783214"`).
    pub user_id: String,
    /// v2 REST base URL. Trailing slash trimmed.
    pub api_base: String,
    /// v1.1 media upload base URL. Trailing slash trimmed.
    pub media_base: String,
    /// Which media-upload protocol to use. `"v1"` (default) hits the
    /// legacy `upload.twitter.com/1.1/media/upload.json` endpoint;
    /// `"v2"` hits `api.twitter.com/2/media/upload` with multipart.
    pub media_api_version: MediaApiVersion,
    /// Filename for the persisted `since_id` token.
    pub since_id_filename: String,
    /// Interval between successive `dm_events` polls, in milliseconds.
    pub poll_interval_ms: u64,
}

/// Which Twitter / X media-upload protocol to use.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum MediaApiVersion {
    /// `POST upload.twitter.com/1.1/media/upload.json` (base64
    /// `media_data` form field).
    V1,
    /// `POST api.twitter.com/2/media/upload` (multipart with a
    /// `media` part).
    V2,
}

impl MediaApiVersion {
    /// Parse `"v1"` / `"v2"` (case-insensitive).
    pub fn parse(s: &str) -> Result<Self, AdapterError> {
        match s.to_ascii_lowercase().as_str() {
            "v1" | "1" | "1.1" => Ok(Self::V1),
            "v2" | "2" => Ok(Self::V2),
            _ => Err(AdapterError::BadRequest(format!(
                "x media_api_version must be `v1` or `v2`, got {s:?}"
            ))),
        }
    }
}

impl XConfig {
    /// Parse a JSON config value into an [`XConfig`].
    ///
    /// Expected shape:
    ///
    /// ```json
    /// {
    ///   "bearer_token":      "...",
    ///   "user_id":           "783214",
    ///   "api_base":          "https://api.twitter.com",
    ///   "media_base":        "https://upload.twitter.com",
    ///   "since_id_filename": "x_dm_since_id.txt",
    ///   "poll_interval_ms":  30000
    /// }
    /// ```
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        let obj = value
            .as_object()
            .ok_or_else(|| AdapterError::BadRequest("x config must be a JSON object".into()))?;

        let bearer_token = required_nonempty_string(obj, "bearer_token")?;
        let user_id = required_nonempty_string(obj, "user_id")?;

        let api_base = match obj.get("api_base") {
            None | Some(Value::Null) => DEFAULT_API_BASE.to_owned(),
            Some(Value::String(s)) if !s.is_empty() => s.trim_end_matches('/').to_owned(),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "x api_base must not be empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "x api_base must be a string".into(),
                ));
            }
        };

        let media_base = match obj.get("media_base") {
            None | Some(Value::Null) => DEFAULT_MEDIA_BASE.to_owned(),
            Some(Value::String(s)) if !s.is_empty() => s.trim_end_matches('/').to_owned(),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "x media_base must not be empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "x media_base must be a string".into(),
                ));
            }
        };

        let since_id_filename = match obj.get("since_id_filename") {
            None | Some(Value::Null) => DEFAULT_SINCE_ID_FILENAME.to_owned(),
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "x since_id_filename must not be empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "x since_id_filename must be a string".into(),
                ));
            }
        };

        let poll_interval_ms = match obj.get("poll_interval_ms") {
            None | Some(Value::Null) => DEFAULT_POLL_INTERVAL_MS,
            Some(Value::Number(n)) => n.as_u64().ok_or_else(|| {
                AdapterError::BadRequest("x poll_interval_ms must be a non-negative integer".into())
            })?,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "x poll_interval_ms must be a number".into(),
                ));
            }
        };

        let media_api_version = match obj.get("media_api_version") {
            None | Some(Value::Null) => MediaApiVersion::V1,
            Some(Value::String(s)) => MediaApiVersion::parse(s)?,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "x media_api_version must be a string".into(),
                ));
            }
        };

        Ok(Self {
            bearer_token,
            user_id,
            api_base,
            media_base,
            media_api_version,
            since_id_filename,
            poll_interval_ms,
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
            "x {key} must not be empty"
        ))),
        Some(_) => Err(AdapterError::BadRequest(format!(
            "x {key} must be a string"
        ))),
        None => Err(AdapterError::BadRequest(format!("x {key} is required"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_minimal_config() {
        let cfg = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "783214"
        }))
        .unwrap();
        assert_eq!(cfg.bearer_token, "tok");
        assert_eq!(cfg.user_id, "783214");
        assert_eq!(cfg.api_base, DEFAULT_API_BASE);
        assert_eq!(cfg.media_base, DEFAULT_MEDIA_BASE);
        assert_eq!(cfg.since_id_filename, DEFAULT_SINCE_ID_FILENAME);
        assert_eq!(cfg.poll_interval_ms, DEFAULT_POLL_INTERVAL_MS);
    }

    #[test]
    fn parses_full_config() {
        let cfg = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "1",
            "api_base": "https://x.test",
            "media_base": "https://up.test",
            "since_id_filename": "sid.txt",
            "poll_interval_ms": 1234
        }))
        .unwrap();
        assert_eq!(cfg.api_base, "https://x.test");
        assert_eq!(cfg.media_base, "https://up.test");
        assert_eq!(cfg.since_id_filename, "sid.txt");
        assert_eq!(cfg.poll_interval_ms, 1234);
        // Default media-upload protocol is v1.1 (legacy).
        assert_eq!(cfg.media_api_version, MediaApiVersion::V1);
    }

    #[test]
    fn media_api_version_v2_opt_in() {
        let cfg = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "1",
            "media_api_version": "v2"
        }))
        .unwrap();
        assert_eq!(cfg.media_api_version, MediaApiVersion::V2);
    }

    #[test]
    fn media_api_version_case_and_alias_accepted() {
        for s in ["V2", "2"] {
            let cfg = XConfig::from_value(&json!({
                "bearer_token": "tok",
                "user_id": "1",
                "media_api_version": s,
            }))
            .unwrap();
            assert_eq!(cfg.media_api_version, MediaApiVersion::V2);
        }
        for s in ["V1", "1", "1.1"] {
            let cfg = XConfig::from_value(&json!({
                "bearer_token": "tok",
                "user_id": "1",
                "media_api_version": s,
            }))
            .unwrap();
            assert_eq!(cfg.media_api_version, MediaApiVersion::V1);
        }
    }

    #[test]
    fn media_api_version_unknown_errors() {
        let err = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "1",
            "media_api_version": "v3",
        }))
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => {
                assert!(m.contains("media_api_version"));
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn media_api_version_wrong_type_errors() {
        let err = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "1",
            "media_api_version": 2,
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn trims_trailing_slash_on_api_base() {
        let cfg = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "1",
            "api_base": "https://x.test/"
        }))
        .unwrap();
        assert_eq!(cfg.api_base, "https://x.test");
    }

    #[test]
    fn trims_trailing_slash_on_media_base() {
        let cfg = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "1",
            "media_base": "https://up.test/"
        }))
        .unwrap();
        assert_eq!(cfg.media_base, "https://up.test");
    }

    #[test]
    fn missing_bearer_token_errors() {
        let err = XConfig::from_value(&json!({ "user_id": "1" })).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("bearer_token")));
    }

    #[test]
    fn missing_user_id_errors() {
        let err = XConfig::from_value(&json!({ "bearer_token": "tok" })).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("user_id")));
    }

    #[test]
    fn empty_bearer_token_errors() {
        let err = XConfig::from_value(&json!({
            "bearer_token": "",
            "user_id": "1"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("bearer_token")));
    }

    #[test]
    fn empty_user_id_errors() {
        let err = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": ""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("user_id")));
    }

    #[test]
    fn non_string_bearer_token_errors() {
        let err = XConfig::from_value(&json!({
            "bearer_token": 7,
            "user_id": "1"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn non_string_user_id_errors() {
        let err = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": 7
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn api_base_wrong_type_errors() {
        let err = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "1",
            "api_base": 7
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn api_base_empty_string_errors() {
        let err = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "1",
            "api_base": ""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn media_base_wrong_type_errors() {
        let err = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "1",
            "media_base": 7
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn media_base_empty_string_errors() {
        let err = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "1",
            "media_base": ""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn since_id_filename_wrong_type_errors() {
        let err = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "1",
            "since_id_filename": 7
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn since_id_filename_empty_string_errors() {
        let err = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "1",
            "since_id_filename": ""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn poll_interval_wrong_type_errors() {
        let err = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "1",
            "poll_interval_ms": "fast"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn poll_interval_negative_errors() {
        let err = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "1",
            "poll_interval_ms": -1
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn null_optional_fields_use_defaults() {
        let cfg = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "1",
            "api_base": null,
            "media_base": null,
            "since_id_filename": null,
            "poll_interval_ms": null
        }))
        .unwrap();
        assert_eq!(cfg.api_base, DEFAULT_API_BASE);
        assert_eq!(cfg.media_base, DEFAULT_MEDIA_BASE);
        assert_eq!(cfg.since_id_filename, DEFAULT_SINCE_ID_FILENAME);
        assert_eq!(cfg.poll_interval_ms, DEFAULT_POLL_INTERVAL_MS);
    }

    #[test]
    fn non_object_config_errors() {
        let err = XConfig::from_value(&json!("nope")).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn clone_and_eq_roundtrip() {
        let cfg = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "1"
        }))
        .unwrap();
        let copy = cfg.clone();
        assert_eq!(cfg, copy);
    }

    #[test]
    fn debug_format_renders() {
        let cfg = XConfig::from_value(&json!({
            "bearer_token": "tok",
            "user_id": "1"
        }))
        .unwrap();
        let s = format!("{cfg:?}");
        assert!(s.contains("XConfig"));
    }

    #[test]
    fn defaults_constants() {
        assert_eq!(DEFAULT_API_BASE, "https://api.twitter.com");
        assert_eq!(DEFAULT_MEDIA_BASE, "https://upload.twitter.com");
        assert_eq!(DEFAULT_SINCE_ID_FILENAME, "x_dm_since_id.txt");
        assert_eq!(DEFAULT_POLL_INTERVAL_MS, 30_000);
    }
}
