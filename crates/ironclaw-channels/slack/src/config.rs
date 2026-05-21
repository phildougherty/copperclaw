//! Configuration loaded from the host-provided JSON blob.
//!
//! Shape (all top-level fields except `bot_token` and `signing_secret` are
//! optional):
//!
//! ```json
//! {
//!   "bot_token": "xoxb-...",
//!   "signing_secret": "...",
//!   "webhook": { "host": "0.0.0.0", "port": 8082, "path": "/slack/events" },
//!   "api_base": "https://slack.com/api"
//! }
//! ```

use ironclaw_channels_core::AdapterError;
use serde_json::Value;

/// Default bind host for the Events API webhook.
pub const DEFAULT_HOST: &str = "0.0.0.0";
/// Default bind port for the Events API webhook.
pub const DEFAULT_PORT: u16 = 8082;
/// Default URL path for the Events API webhook.
pub const DEFAULT_PATH: &str = "/slack/events";
/// Default Slack Web API base URL.
pub const DEFAULT_API_BASE: &str = "https://slack.com/api";

/// Parsed Slack channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackConfig {
    /// Slack bot token (`xoxb-...`).
    pub bot_token: String,
    /// Slack signing secret (used to validate `X-Slack-Signature`).
    pub signing_secret: String,
    /// HTTP webhook bind settings.
    pub webhook: WebhookConfig,
    /// Slack Web API base URL. Overridable for tests.
    pub api_base: String,
}

/// Bind settings for the Events API HTTP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookConfig {
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_owned(),
            port: DEFAULT_PORT,
            path: DEFAULT_PATH.to_owned(),
        }
    }
}

impl SlackConfig {
    /// Parse from the raw `serde_json::Value` the host passes through
    /// [`ironclaw_channels_core::ChannelSetup::config`].
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        let obj = value
            .as_object()
            .ok_or_else(|| AdapterError::BadRequest("slack config must be a JSON object".into()))?;

        let bot_token = required_string(obj, "bot_token")?;
        let signing_secret = required_string(obj, "signing_secret")?;
        let webhook = WebhookConfig::from_value(obj.get("webhook"))?;
        let api_base = match obj.get("api_base") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::Null) | None => DEFAULT_API_BASE.to_owned(),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "slack config field `api_base` must be non-empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "slack config field `api_base` must be a string".into(),
                ));
            }
        };

        Ok(Self {
            bot_token,
            signing_secret,
            webhook,
            api_base,
        })
    }
}

impl WebhookConfig {
    fn from_value(value: Option<&Value>) -> Result<Self, AdapterError> {
        let Some(value) = value else {
            return Ok(Self::default());
        };
        if value.is_null() {
            return Ok(Self::default());
        }
        let obj = value.as_object().ok_or_else(|| {
            AdapterError::BadRequest("slack `webhook` must be a JSON object".into())
        })?;
        let host = match obj.get("host") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_HOST.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "slack `webhook.host` must be a string".into(),
                ));
            }
        };
        let port = match obj.get("port") {
            Some(Value::Number(n)) => n
                .as_u64()
                .and_then(|u| u16::try_from(u).ok())
                .ok_or_else(|| {
                    AdapterError::BadRequest(
                        "slack `webhook.port` must be a u16 in range".into(),
                    )
                })?,
            Some(Value::Null) | None => DEFAULT_PORT,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "slack `webhook.port` must be a number".into(),
                ));
            }
        };
        let path = match obj.get("path") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_PATH.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "slack `webhook.path` must be a string".into(),
                ));
            }
        };
        Ok(Self { host, port, path })
    }
}

fn required_string(
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String, AdapterError> {
    match obj.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err(AdapterError::BadRequest(format!(
            "slack config field `{key}` must be non-empty"
        ))),
        Some(_) => Err(AdapterError::BadRequest(format!(
            "slack config field `{key}` must be a string"
        ))),
        None => Err(AdapterError::BadRequest(format!(
            "slack config missing required field `{key}`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full_value() -> Value {
        json!({
            "bot_token": "xoxb-abc",
            "signing_secret": "sec",
            "webhook": {"host": "127.0.0.1", "port": 9090, "path": "/x"},
            "api_base": "https://example.test/api"
        })
    }

    #[test]
    fn parses_full_config() {
        let c = SlackConfig::from_value(&full_value()).unwrap();
        assert_eq!(c.bot_token, "xoxb-abc");
        assert_eq!(c.signing_secret, "sec");
        assert_eq!(c.webhook.host, "127.0.0.1");
        assert_eq!(c.webhook.port, 9090);
        assert_eq!(c.webhook.path, "/x");
        assert_eq!(c.api_base, "https://example.test/api");
    }

    #[test]
    fn defaults_when_optional_fields_omitted() {
        let c = SlackConfig::from_value(&json!({
            "bot_token": "xoxb",
            "signing_secret": "s"
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
        assert_eq!(c.api_base, DEFAULT_API_BASE);
        assert_eq!(c.webhook.host, DEFAULT_HOST);
        assert_eq!(c.webhook.port, DEFAULT_PORT);
        assert_eq!(c.webhook.path, DEFAULT_PATH);
    }

    #[test]
    fn rejects_non_object_root() {
        let err = SlackConfig::from_value(&json!("string")).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_missing_bot_token() {
        let err = SlackConfig::from_value(&json!({"signing_secret":"s"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("bot_token")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_signing_secret() {
        let err = SlackConfig::from_value(&json!({"bot_token":"x"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("signing_secret")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_bot_token() {
        let err =
            SlackConfig::from_value(&json!({"bot_token":"", "signing_secret":"s"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("non-empty")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_string_bot_token() {
        let err = SlackConfig::from_value(&json!({"bot_token":42, "signing_secret":"s"}))
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_object_webhook() {
        let err = SlackConfig::from_value(&json!({
            "bot_token":"x","signing_secret":"s","webhook":7
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_null_is_default() {
        let c = SlackConfig::from_value(&json!({
            "bot_token":"x","signing_secret":"s","webhook": null
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
    }

    #[test]
    fn webhook_partial_defaults_remaining() {
        let c = SlackConfig::from_value(&json!({
            "bot_token":"x","signing_secret":"s","webhook":{"port": 1234}
        }))
        .unwrap();
        assert_eq!(c.webhook.host, DEFAULT_HOST);
        assert_eq!(c.webhook.port, 1234);
        assert_eq!(c.webhook.path, DEFAULT_PATH);
    }

    #[test]
    fn webhook_rejects_bad_host_type() {
        let err = SlackConfig::from_value(&json!({
            "bot_token":"x","signing_secret":"s","webhook":{"host": 1}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_bad_port_type() {
        let err = SlackConfig::from_value(&json!({
            "bot_token":"x","signing_secret":"s","webhook":{"port": "bad"}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_port_out_of_range() {
        let err = SlackConfig::from_value(&json!({
            "bot_token":"x","signing_secret":"s","webhook":{"port": 100_000}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_bad_path_type() {
        let err = SlackConfig::from_value(&json!({
            "bot_token":"x","signing_secret":"s","webhook":{"path": 9}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_null_fields_default() {
        let c = SlackConfig::from_value(&json!({
            "bot_token":"x","signing_secret":"s",
            "webhook":{"host": null, "port": null, "path": null}
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
    }

    #[test]
    fn rejects_empty_api_base() {
        let err = SlackConfig::from_value(&json!({
            "bot_token":"x","signing_secret":"s","api_base":""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_api_base() {
        let err = SlackConfig::from_value(&json!({
            "bot_token":"x","signing_secret":"s","api_base":1
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn api_base_null_defaults() {
        let c = SlackConfig::from_value(&json!({
            "bot_token":"x","signing_secret":"s","api_base": null
        }))
        .unwrap();
        assert_eq!(c.api_base, DEFAULT_API_BASE);
    }

    #[test]
    fn webhook_default_has_expected_constants() {
        let d = WebhookConfig::default();
        assert_eq!(d.host, DEFAULT_HOST);
        assert_eq!(d.port, DEFAULT_PORT);
        assert_eq!(d.path, DEFAULT_PATH);
    }

    #[test]
    fn debug_format_present() {
        let c = SlackConfig::from_value(&full_value()).unwrap();
        assert!(format!("{c:?}").contains("xoxb-abc"));
        assert!(format!("{:?}", c.webhook).contains("127.0.0.1"));
    }

    #[test]
    fn clone_eq_works() {
        let c = SlackConfig::from_value(&full_value()).unwrap();
        let c2 = c.clone();
        assert_eq!(c, c2);
    }
}
