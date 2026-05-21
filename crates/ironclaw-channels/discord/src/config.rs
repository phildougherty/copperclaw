//! Configuration parsing for the Discord channel adapter.
//!
//! `DiscordConfig` is built from the host's `channels.config_json` blob.
//! Only `bot_token` is required; everything else has a sensible default.
//!
//! Example JSON:
//! ```json
//! {
//!   "bot_token": "...",
//!   "intents": 33281,
//!   "api_base": "https://discord.com/api/v10",
//!   "gateway_url": "wss://gateway.discord.gg/?v=10&encoding=json"
//! }
//! ```

use ironclaw_channels_core::AdapterError;
use serde_json::Value;

/// Default Discord REST API base URL.
pub const DEFAULT_API_BASE: &str = "https://discord.com/api/v10";

/// Default Discord gateway URL.
pub const DEFAULT_GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

/// `GUILDS | GUILD_MESSAGES | GUILD_MESSAGE_REACTIONS | DIRECT_MESSAGES | MESSAGE_CONTENT`.
///
/// Equivalent to `(1<<0) | (1<<9) | (1<<10) | (1<<12) | (1<<15) = 38_401`.
/// (The PLAN text quoted `33_281`; the actual sum of those five bits is
/// `38_401`. We use the literal bit-OR so the constant stays correct.)
pub const DEFAULT_INTENTS: u64 = (1 << 0) | (1 << 9) | (1 << 10) | (1 << 12) | (1 << 15);

/// Parsed Discord channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordConfig {
    pub bot_token: String,
    pub intents: u64,
    pub api_base: String,
    pub gateway_url: String,
}

impl DiscordConfig {
    /// Parse a `DiscordConfig` from the host-provided JSON blob.
    ///
    /// Errors with `AdapterError::BadRequest` if `bot_token` is missing or
    /// fields are the wrong shape.
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        let obj = value.as_object().ok_or_else(|| {
            AdapterError::BadRequest("discord config must be a JSON object".into())
        })?;

        let bot_token = match obj.get("bot_token") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "discord config field `bot_token` must be a non-empty string".into(),
                ));
            }
            None => {
                return Err(AdapterError::BadRequest(
                    "discord config field `bot_token` is required".into(),
                ));
            }
        };

        let intents = match obj.get("intents") {
            Some(Value::Number(n)) => n.as_u64().ok_or_else(|| {
                AdapterError::BadRequest(
                    "discord config field `intents` must be a non-negative integer".into(),
                )
            })?,
            Some(Value::Null) | None => DEFAULT_INTENTS,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "discord config field `intents` must be an integer".into(),
                ));
            }
        };

        let api_base = match obj.get("api_base") {
            Some(Value::String(s)) => s.trim_end_matches('/').to_owned(),
            Some(Value::Null) | None => DEFAULT_API_BASE.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "discord config field `api_base` must be a string".into(),
                ));
            }
        };

        let gateway_url = match obj.get("gateway_url") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_GATEWAY_URL.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "discord config field `gateway_url` must be a string".into(),
                ));
            }
        };

        Ok(Self {
            bot_token,
            intents,
            api_base,
            gateway_url,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_intents_value_is_38401() {
        // 1 + 512 + 1024 + 4096 + 32768 = 38_401.
        assert_eq!(DEFAULT_INTENTS, 38_401);
        // Sanity-check each contributing bit.
        assert_eq!(DEFAULT_INTENTS & (1 << 0), 1);
        assert_eq!(DEFAULT_INTENTS & (1 << 9), 512);
        assert_eq!(DEFAULT_INTENTS & (1 << 10), 1024);
        assert_eq!(DEFAULT_INTENTS & (1 << 12), 4096);
        assert_eq!(DEFAULT_INTENTS & (1 << 15), 32_768);
    }

    #[test]
    fn parses_minimal_config() {
        let cfg = DiscordConfig::from_value(&json!({ "bot_token": "abc" })).unwrap();
        assert_eq!(cfg.bot_token, "abc");
        assert_eq!(cfg.intents, DEFAULT_INTENTS);
        assert_eq!(cfg.api_base, DEFAULT_API_BASE);
        assert_eq!(cfg.gateway_url, DEFAULT_GATEWAY_URL);
    }

    #[test]
    fn accepts_overrides() {
        let cfg = DiscordConfig::from_value(&json!({
            "bot_token": "tok",
            "intents": 1,
            "api_base": "http://localhost:8000/api",
            "gateway_url": "ws://localhost:9000/",
        }))
        .unwrap();
        assert_eq!(cfg.bot_token, "tok");
        assert_eq!(cfg.intents, 1);
        assert_eq!(cfg.api_base, "http://localhost:8000/api");
        assert_eq!(cfg.gateway_url, "ws://localhost:9000/");
    }

    #[test]
    fn trailing_slash_in_api_base_is_stripped() {
        let cfg = DiscordConfig::from_value(&json!({
            "bot_token": "tok",
            "api_base": "http://x/api/v10/",
        }))
        .unwrap();
        assert_eq!(cfg.api_base, "http://x/api/v10");
    }

    #[test]
    fn null_optional_fields_use_defaults() {
        let cfg = DiscordConfig::from_value(&json!({
            "bot_token": "tok",
            "intents": null,
            "api_base": null,
            "gateway_url": null,
        }))
        .unwrap();
        assert_eq!(cfg.intents, DEFAULT_INTENTS);
        assert_eq!(cfg.api_base, DEFAULT_API_BASE);
        assert_eq!(cfg.gateway_url, DEFAULT_GATEWAY_URL);
    }

    #[test]
    fn rejects_non_object_config() {
        let err = DiscordConfig::from_value(&json!([])).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_missing_bot_token() {
        let err = DiscordConfig::from_value(&json!({})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_empty_bot_token() {
        let err = DiscordConfig::from_value(&json!({"bot_token": ""})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_bot_token() {
        let err = DiscordConfig::from_value(&json!({"bot_token": 42})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_integer_intents() {
        let err = DiscordConfig::from_value(&json!({
            "bot_token": "t",
            "intents": "lots",
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_negative_intents() {
        let err = DiscordConfig::from_value(&json!({
            "bot_token": "t",
            "intents": -1,
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_api_base() {
        let err = DiscordConfig::from_value(&json!({
            "bot_token": "t",
            "api_base": 7,
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_gateway_url() {
        let err = DiscordConfig::from_value(&json!({
            "bot_token": "t",
            "gateway_url": false,
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn debug_clone_eq() {
        let a = DiscordConfig::from_value(&json!({"bot_token": "x"})).unwrap();
        let b = a.clone();
        assert_eq!(a, b);
        let _ = format!("{a:?}");
    }
}
