//! Telegram channel configuration parsed from `ChannelSetup::config`.
//!
//! Two ingress modes are supported:
//!
//! - [`IngressMode::LongPoll`] — the adapter polls `getUpdates` from a
//!   background task.
//! - [`IngressMode::Webhook`] — the adapter spawns an axum HTTP server that
//!   receives Telegram's POSTs.
//!
//! Exactly one of `long_poll` / `webhook` must be present in the JSON
//! config. `api_base` defaults to the public Telegram endpoint but tests
//! override it to point at wiremock.

use ironclaw_channels_core::AdapterError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default Telegram Bot API base URL.
pub const DEFAULT_API_BASE: &str = "https://api.telegram.org";

/// Default long-poll timeout in seconds.
pub const DEFAULT_LONG_POLL_TIMEOUT_SECS: u64 = 60;

/// Default long-poll batch size.
pub const DEFAULT_LONG_POLL_LIMIT: u32 = 100;

/// Default webhook host (all interfaces).
pub const DEFAULT_WEBHOOK_HOST: &str = "0.0.0.0";

/// Default webhook port.
pub const DEFAULT_WEBHOOK_PORT: u16 = 8081;

/// Default webhook path.
pub const DEFAULT_WEBHOOK_PATH: &str = "/telegram";

/// Telegram Bot API hard cap on inbound file downloads is 20 MB.
/// We use this as the default `max_attachment_bytes`; files larger than the
/// cap are surfaced as [`ironclaw_types::MessageKind::System`] with a
/// "file too large" note instead of being downloaded.
pub const DEFAULT_MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

/// Configuration for the long-poll ingress mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LongPollConfig {
    /// `getUpdates` long-poll timeout, in seconds. Defaults to
    /// [`DEFAULT_LONG_POLL_TIMEOUT_SECS`].
    #[serde(default = "default_long_poll_timeout_secs")]
    pub timeout_secs: u64,

    /// Maximum number of updates returned per call. Defaults to
    /// [`DEFAULT_LONG_POLL_LIMIT`].
    #[serde(default = "default_long_poll_limit")]
    pub limit: u32,

    /// Optional `allowed_updates` filter forwarded verbatim to Telegram.
    #[serde(default)]
    pub allowed_updates: Vec<String>,
}

impl Default for LongPollConfig {
    fn default() -> Self {
        Self {
            timeout_secs: DEFAULT_LONG_POLL_TIMEOUT_SECS,
            limit: DEFAULT_LONG_POLL_LIMIT,
            allowed_updates: Vec::new(),
        }
    }
}

fn default_long_poll_timeout_secs() -> u64 {
    DEFAULT_LONG_POLL_TIMEOUT_SECS
}

fn default_long_poll_limit() -> u32 {
    DEFAULT_LONG_POLL_LIMIT
}

/// Configuration for the webhook ingress mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookConfig {
    #[serde(default = "default_webhook_host")]
    pub host: String,
    #[serde(default = "default_webhook_port")]
    pub port: u16,
    #[serde(default = "default_webhook_path")]
    pub path: String,
    /// Optional shared secret. When present, the adapter validates the
    /// `X-Telegram-Bot-Api-Secret-Token` header on every incoming POST.
    #[serde(default)]
    pub secret_token: Option<String>,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_WEBHOOK_HOST.to_owned(),
            port: DEFAULT_WEBHOOK_PORT,
            path: DEFAULT_WEBHOOK_PATH.to_owned(),
            secret_token: None,
        }
    }
}

fn default_webhook_host() -> String {
    DEFAULT_WEBHOOK_HOST.to_owned()
}

fn default_webhook_port() -> u16 {
    DEFAULT_WEBHOOK_PORT
}

fn default_webhook_path() -> String {
    DEFAULT_WEBHOOK_PATH.to_owned()
}

/// Which ingress mode the adapter should run in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngressMode {
    LongPoll(LongPollConfig),
    Webhook(WebhookConfig),
}

/// Fully resolved Telegram configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub api_base: String,
    pub mode: IngressMode,
    /// When `true` (the default), inbound `document` / `photo` / `audio` /
    /// `video` / `voice` / `video_note` / `sticker` attachments are
    /// downloaded via `getFile` and surfaced as
    /// [`ironclaw_types::MessageKind::Chat`] events with the file written
    /// under `data_dir/inbox/<msg_id>/<filename>` and metadata embedded in
    /// `content["attachment"]`.
    ///
    /// When `false`, the adapter falls back to the legacy behaviour: an
    /// attachment becomes a [`ironclaw_types::MessageKind::System`] event
    /// carrying only platform metadata (no bytes).
    pub attachment_download: bool,
    /// Refuse to download any attachment whose `file_size` exceeds this
    /// many bytes. Defaults to [`DEFAULT_MAX_ATTACHMENT_BYTES`] (20 MB),
    /// matching the Telegram Bot API's own hard cap. Oversized files are
    /// surfaced as `MessageKind::System` with a `reason: "too_large"` note.
    pub max_attachment_bytes: u64,
}

impl TelegramConfig {
    /// Parse a JSON config value into a [`TelegramConfig`].
    ///
    /// The expected shape:
    ///
    /// ```json
    /// {
    ///   "bot_token": "...",
    ///   "mode": "long_poll" | "webhook",
    ///   "long_poll": { "timeout_secs": 60, "limit": 100, "allowed_updates": [] },
    ///   "webhook":   { "host": "0.0.0.0", "port": 8081, "path": "/telegram",
    ///                  "secret_token": "..." },
    ///   "api_base":  "https://api.telegram.org"
    /// }
    /// ```
    ///
    /// Exactly one of `long_poll` / `webhook` may be populated; the `mode`
    /// field is optional when only one block is present.
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        let obj = value.as_object().ok_or_else(|| {
            AdapterError::BadRequest("telegram config must be a JSON object".into())
        })?;

        let bot_token = match obj.get("bot_token") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "telegram bot_token must not be empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "telegram bot_token must be a string".into(),
                ));
            }
            None => {
                return Err(AdapterError::BadRequest(
                    "telegram bot_token is required".into(),
                ));
            }
        };

        let api_base = match obj.get("api_base") {
            Some(Value::String(s)) if !s.is_empty() => s.trim_end_matches('/').to_owned(),
            Some(Value::Null) | None => DEFAULT_API_BASE.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "telegram api_base must be a string".into(),
                ));
            }
        };

        let long_poll_block = obj.get("long_poll");
        let webhook_block = obj.get("webhook");
        let mode_str = obj.get("mode").and_then(Value::as_str).map(str::to_owned);

        let mode = resolve_mode(mode_str.as_deref(), long_poll_block, webhook_block)?;

        let attachment_download = match obj.get("attachment_download") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "telegram attachment_download must be a boolean".into(),
                ));
            }
        };

        let max_attachment_bytes = match obj.get("max_attachment_bytes") {
            None | Some(Value::Null) => DEFAULT_MAX_ATTACHMENT_BYTES,
            Some(Value::Number(n)) => n.as_u64().ok_or_else(|| {
                AdapterError::BadRequest(
                    "telegram max_attachment_bytes must be a non-negative integer".into(),
                )
            })?,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "telegram max_attachment_bytes must be a number".into(),
                ));
            }
        };

        Ok(Self {
            bot_token,
            api_base,
            mode,
            attachment_download,
            max_attachment_bytes,
        })
    }
}

fn resolve_mode(
    mode_str: Option<&str>,
    long_poll_block: Option<&Value>,
    webhook_block: Option<&Value>,
) -> Result<IngressMode, AdapterError> {
    let lp_present = long_poll_block.is_some_and(|v| !v.is_null());
    let wh_present = webhook_block.is_some_and(|v| !v.is_null());

    if let Some(mode) = mode_str {
        return match mode {
            "long_poll" => Ok(IngressMode::LongPoll(parse_long_poll(long_poll_block)?)),
            "webhook" => Ok(IngressMode::Webhook(parse_webhook(webhook_block)?)),
            other => Err(AdapterError::BadRequest(format!(
                "unknown telegram mode `{other}`; expected `long_poll` or `webhook`"
            ))),
        };
    }

    match (lp_present, wh_present) {
        (true, false) => Ok(IngressMode::LongPoll(parse_long_poll(long_poll_block)?)),
        (false, true) => Ok(IngressMode::Webhook(parse_webhook(webhook_block)?)),
        (true, true) => Err(AdapterError::BadRequest(
            "telegram config must specify exactly one of `long_poll` or `webhook`".into(),
        )),
        (false, false) => Err(AdapterError::BadRequest(
            "telegram config must specify `mode` or a `long_poll` / `webhook` block".into(),
        )),
    }
}

fn parse_long_poll(value: Option<&Value>) -> Result<LongPollConfig, AdapterError> {
    match value {
        Some(Value::Null) | None => Ok(LongPollConfig::default()),
        Some(v) => serde_json::from_value::<LongPollConfig>(v.clone()).map_err(|e| {
            AdapterError::BadRequest(format!("telegram long_poll config invalid: {e}"))
        }),
    }
}

fn parse_webhook(value: Option<&Value>) -> Result<WebhookConfig, AdapterError> {
    match value {
        Some(Value::Null) | None => Ok(WebhookConfig::default()),
        Some(v) => serde_json::from_value::<WebhookConfig>(v.clone())
            .map_err(|e| AdapterError::BadRequest(format!("telegram webhook config invalid: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_long_poll_mode_explicit() {
        let cfg = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "mode": "long_poll",
            "long_poll": { "timeout_secs": 30, "limit": 10, "allowed_updates": ["message"] }
        }))
        .unwrap();
        assert_eq!(cfg.bot_token, "t");
        assert_eq!(cfg.api_base, DEFAULT_API_BASE);
        match cfg.mode {
            IngressMode::LongPoll(lp) => {
                assert_eq!(lp.timeout_secs, 30);
                assert_eq!(lp.limit, 10);
                assert_eq!(lp.allowed_updates, vec!["message".to_string()]);
            }
            IngressMode::Webhook(_) => panic!("expected long_poll"),
        }
    }

    #[test]
    fn parses_long_poll_mode_implicit() {
        let cfg = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "long_poll": {}
        }))
        .unwrap();
        match cfg.mode {
            IngressMode::LongPoll(lp) => {
                assert_eq!(lp.timeout_secs, DEFAULT_LONG_POLL_TIMEOUT_SECS);
                assert_eq!(lp.limit, DEFAULT_LONG_POLL_LIMIT);
                assert!(lp.allowed_updates.is_empty());
            }
            IngressMode::Webhook(_) => panic!("expected long_poll"),
        }
    }

    #[test]
    fn parses_webhook_mode_explicit() {
        let cfg = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "mode": "webhook",
            "webhook": {
                "host": "127.0.0.1",
                "port": 9000,
                "path": "/hook",
                "secret_token": "shh"
            }
        }))
        .unwrap();
        match cfg.mode {
            IngressMode::Webhook(wh) => {
                assert_eq!(wh.host, "127.0.0.1");
                assert_eq!(wh.port, 9000);
                assert_eq!(wh.path, "/hook");
                assert_eq!(wh.secret_token.as_deref(), Some("shh"));
            }
            IngressMode::LongPoll(_) => panic!("expected webhook"),
        }
    }

    #[test]
    fn parses_webhook_mode_implicit() {
        let cfg = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "webhook": {}
        }))
        .unwrap();
        match cfg.mode {
            IngressMode::Webhook(wh) => {
                assert_eq!(wh.host, DEFAULT_WEBHOOK_HOST);
                assert_eq!(wh.port, DEFAULT_WEBHOOK_PORT);
                assert_eq!(wh.path, DEFAULT_WEBHOOK_PATH);
                assert!(wh.secret_token.is_none());
            }
            IngressMode::LongPoll(_) => panic!("expected webhook"),
        }
    }

    #[test]
    fn explicit_mode_overrides_when_both_blocks_present_in_serde() {
        // When mode is `long_poll`, webhook block is ignored.
        let cfg = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "mode": "long_poll",
            "long_poll": {},
            "webhook": { "port": 1111 }
        }))
        .unwrap();
        assert!(matches!(cfg.mode, IngressMode::LongPoll(_)));
    }

    #[test]
    fn api_base_override_trims_trailing_slash() {
        let cfg = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "long_poll": {},
            "api_base": "http://localhost:9999/"
        }))
        .unwrap();
        assert_eq!(cfg.api_base, "http://localhost:9999");
    }

    #[test]
    fn missing_token_errors() {
        let err = TelegramConfig::from_value(&json!({"long_poll": {}})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn empty_token_errors() {
        let err = TelegramConfig::from_value(&json!({"bot_token": "", "long_poll": {}}))
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn non_string_token_errors() {
        let err = TelegramConfig::from_value(&json!({"bot_token": 42, "long_poll": {}}))
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn non_string_api_base_errors() {
        let err = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "long_poll": {},
            "api_base": 7
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn non_object_config_errors() {
        let err = TelegramConfig::from_value(&json!("nope")).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn both_blocks_no_explicit_mode_errors() {
        let err = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "long_poll": {},
            "webhook": {}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn neither_block_nor_mode_errors() {
        let err = TelegramConfig::from_value(&json!({"bot_token": "t"})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn unknown_mode_string_errors() {
        let err = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "mode": "psychic"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn malformed_long_poll_block_errors() {
        let err = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "long_poll": { "timeout_secs": "not a number" }
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn malformed_webhook_block_errors() {
        let err = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "webhook": { "port": "not a number" }
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn long_poll_config_default_matches_constants() {
        let lp = LongPollConfig::default();
        assert_eq!(lp.timeout_secs, DEFAULT_LONG_POLL_TIMEOUT_SECS);
        assert_eq!(lp.limit, DEFAULT_LONG_POLL_LIMIT);
        assert!(lp.allowed_updates.is_empty());
    }

    #[test]
    fn webhook_config_default_matches_constants() {
        let wh = WebhookConfig::default();
        assert_eq!(wh.host, DEFAULT_WEBHOOK_HOST);
        assert_eq!(wh.port, DEFAULT_WEBHOOK_PORT);
        assert_eq!(wh.path, DEFAULT_WEBHOOK_PATH);
        assert!(wh.secret_token.is_none());
    }

    #[test]
    fn api_base_null_falls_back_to_default() {
        let cfg = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "long_poll": {},
            "api_base": null,
        }))
        .unwrap();
        assert_eq!(cfg.api_base, DEFAULT_API_BASE);
    }

    #[test]
    fn long_poll_block_null_uses_defaults() {
        let cfg = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "mode": "long_poll",
            "long_poll": null,
        }))
        .unwrap();
        assert!(matches!(cfg.mode, IngressMode::LongPoll(_)));
    }

    #[test]
    fn webhook_block_null_uses_defaults() {
        let cfg = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "mode": "webhook",
            "webhook": null,
        }))
        .unwrap();
        assert!(matches!(cfg.mode, IngressMode::Webhook(_)));
    }

    #[test]
    fn clone_and_equality() {
        let cfg = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "long_poll": {}
        }))
        .unwrap();
        let copy = cfg.clone();
        assert_eq!(cfg, copy);
    }

    #[test]
    fn debug_format_renders() {
        let cfg = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "long_poll": {}
        }))
        .unwrap();
        let s = format!("{cfg:?}");
        assert!(s.contains("TelegramConfig"));
    }

    #[test]
    fn attachment_download_defaults_to_true_and_can_be_overridden() {
        let cfg = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "long_poll": {}
        }))
        .unwrap();
        assert!(cfg.attachment_download);
        let off = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "long_poll": {},
            "attachment_download": false
        }))
        .unwrap();
        assert!(!off.attachment_download);
    }

    #[test]
    fn attachment_download_null_uses_default() {
        let cfg = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "long_poll": {},
            "attachment_download": null
        }))
        .unwrap();
        assert!(cfg.attachment_download);
    }

    #[test]
    fn attachment_download_non_bool_errors() {
        let err = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "long_poll": {},
            "attachment_download": "yes"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("attachment_download")));
    }

    #[test]
    fn max_attachment_bytes_defaults_to_20mb() {
        let cfg = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "long_poll": {}
        }))
        .unwrap();
        assert_eq!(cfg.max_attachment_bytes, DEFAULT_MAX_ATTACHMENT_BYTES);
    }

    #[test]
    fn max_attachment_bytes_can_be_overridden() {
        let cfg = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "long_poll": {},
            "max_attachment_bytes": 1024
        }))
        .unwrap();
        assert_eq!(cfg.max_attachment_bytes, 1024);
    }

    #[test]
    fn max_attachment_bytes_negative_errors() {
        let err = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "long_poll": {},
            "max_attachment_bytes": -1
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("max_attachment_bytes")));
    }

    #[test]
    fn max_attachment_bytes_wrong_type_errors() {
        let err = TelegramConfig::from_value(&json!({
            "bot_token": "t",
            "long_poll": {},
            "max_attachment_bytes": "lots"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("max_attachment_bytes")));
    }

    #[test]
    fn default_max_attachment_bytes_matches_telegram_cap() {
        assert_eq!(DEFAULT_MAX_ATTACHMENT_BYTES, 20 * 1024 * 1024);
    }
}
