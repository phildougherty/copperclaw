//! Configuration loaded from the host-provided JSON blob.
//!
//! Shape (required: `bot_token`, `client_token`):
//!
//! ```json
//! {
//!   "bot_token": "ya29.<oauth-token>",
//!   "client_token": "<long-random-shared-secret>",
//!   "webhook": { "host": "127.0.0.1", "port": 8086, "path": "/gchat/webhook" },
//!   "api_base": "https://chat.googleapis.com",
//!   "bot_user_id": "users/12345"
//! }
//! ```

use ironclaw_channels_core::AdapterError;
use serde_json::Value;

/// Default bind host for the webhook HTTP server.
pub const DEFAULT_HOST: &str = "127.0.0.1";
/// Default bind port for the webhook HTTP server.
pub const DEFAULT_PORT: u16 = 8086;
/// Default URL path for the webhook handler.
pub const DEFAULT_PATH: &str = "/gchat/webhook";
/// Default Google Chat REST API base URL.
pub const DEFAULT_API_BASE: &str = "https://chat.googleapis.com";

/// Parsed Google Chat channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GchatConfig {
    /// Service-account-derived `OAuth2` bearer token. Operator is responsible
    /// for rotating it; this adapter does not refresh.
    pub bot_token: String,
    /// Shared secret required as the `token` query parameter on the webhook
    /// URL. Substitutes for full JWT verification in v1.
    pub client_token: String,
    /// HTTP webhook bind settings.
    pub webhook: WebhookConfig,
    /// Google Chat REST API base URL. Overridable for tests.
    pub api_base: String,
    /// Google user resource id for the bot (e.g. `users/12345`). Used for
    /// inbound filtering / mention disambiguation when present.
    pub bot_user_id: Option<String>,
}

/// Bind settings for the HTTP webhook server.
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

impl GchatConfig {
    /// Parse from the raw `serde_json::Value` the host passes through
    /// [`ironclaw_channels_core::ChannelSetup::config`].
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        let obj = value
            .as_object()
            .ok_or_else(|| AdapterError::BadRequest("gchat config must be a JSON object".into()))?;
        let bot_token = required_string(obj, "bot_token")?;
        let client_token = required_string(obj, "client_token")?;
        let webhook = WebhookConfig::from_value(obj.get("webhook"))?;
        let api_base = match obj.get("api_base") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::Null) | None => DEFAULT_API_BASE.to_owned(),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "gchat config field `api_base` must be non-empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "gchat config field `api_base` must be a string".into(),
                ));
            }
        };
        let bot_user_id = match obj.get("bot_user_id") {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(Value::Null) | None => None,
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "gchat config field `bot_user_id` must be non-empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "gchat config field `bot_user_id` must be a string".into(),
                ));
            }
        };
        Ok(Self {
            bot_token,
            client_token,
            webhook,
            api_base,
            bot_user_id,
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
            AdapterError::BadRequest("gchat `webhook` must be a JSON object".into())
        })?;
        let host = match obj.get("host") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_HOST.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "gchat `webhook.host` must be a string".into(),
                ));
            }
        };
        let port = match obj.get("port") {
            Some(Value::Number(n)) => n
                .as_u64()
                .and_then(|u| u16::try_from(u).ok())
                .ok_or_else(|| {
                    AdapterError::BadRequest("gchat `webhook.port` must be a u16 in range".into())
                })?,
            Some(Value::Null) | None => DEFAULT_PORT,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "gchat `webhook.port` must be a number".into(),
                ));
            }
        };
        let path = match obj.get("path") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_PATH.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "gchat `webhook.path` must be a string".into(),
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
            "gchat config field `{key}` must be non-empty"
        ))),
        Some(_) => Err(AdapterError::BadRequest(format!(
            "gchat config field `{key}` must be a string"
        ))),
        None => Err(AdapterError::BadRequest(format!(
            "gchat config missing required field `{key}`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full_value() -> Value {
        json!({
            "bot_token": "ya29.token",
            "client_token": "secret-xyz",
            "webhook": {"host": "127.0.0.1", "port": 9091, "path": "/x"},
            "api_base": "https://example.test",
            "bot_user_id": "users/9000"
        })
    }

    #[test]
    fn parses_full_config() {
        let c = GchatConfig::from_value(&full_value()).unwrap();
        assert_eq!(c.bot_token, "ya29.token");
        assert_eq!(c.client_token, "secret-xyz");
        assert_eq!(c.webhook.host, "127.0.0.1");
        assert_eq!(c.webhook.port, 9091);
        assert_eq!(c.webhook.path, "/x");
        assert_eq!(c.api_base, "https://example.test");
        assert_eq!(c.bot_user_id.as_deref(), Some("users/9000"));
    }

    #[test]
    fn defaults_when_optional_fields_omitted() {
        let c = GchatConfig::from_value(&json!({
            "bot_token": "x",
            "client_token": "y"
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
        assert_eq!(c.api_base, DEFAULT_API_BASE);
        assert!(c.bot_user_id.is_none());
        assert_eq!(c.webhook.host, DEFAULT_HOST);
        assert_eq!(c.webhook.port, DEFAULT_PORT);
        assert_eq!(c.webhook.path, DEFAULT_PATH);
    }

    #[test]
    fn rejects_non_object_root() {
        let err = GchatConfig::from_value(&json!("foo")).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_missing_bot_token() {
        let err = GchatConfig::from_value(&json!({"client_token": "y"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("bot_token")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_client_token() {
        let err = GchatConfig::from_value(&json!({"bot_token": "x"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("client_token")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_bot_token() {
        let err = GchatConfig::from_value(&json!({"bot_token": "", "client_token": "y"}))
            .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("non-empty")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_string_bot_token() {
        let err = GchatConfig::from_value(&json!({"bot_token": 12, "client_token": "y"}))
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_client_token() {
        let err = GchatConfig::from_value(&json!({"bot_token": "x", "client_token": 12}))
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_object_webhook() {
        let err = GchatConfig::from_value(&json!({
            "bot_token": "x", "client_token": "y", "webhook": 7
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_null_is_default() {
        let c = GchatConfig::from_value(&json!({
            "bot_token": "x", "client_token": "y", "webhook": null
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
    }

    #[test]
    fn webhook_partial_defaults_remaining() {
        let c = GchatConfig::from_value(&json!({
            "bot_token": "x", "client_token": "y", "webhook": {"port": 1234}
        }))
        .unwrap();
        assert_eq!(c.webhook.host, DEFAULT_HOST);
        assert_eq!(c.webhook.port, 1234);
        assert_eq!(c.webhook.path, DEFAULT_PATH);
    }

    #[test]
    fn webhook_rejects_bad_host_type() {
        let err = GchatConfig::from_value(&json!({
            "bot_token": "x", "client_token": "y", "webhook": {"host": 1}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_bad_port_type() {
        let err = GchatConfig::from_value(&json!({
            "bot_token": "x", "client_token": "y", "webhook": {"port": "bad"}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_port_out_of_range() {
        let err = GchatConfig::from_value(&json!({
            "bot_token": "x", "client_token": "y", "webhook": {"port": 100_000}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_bad_path_type() {
        let err = GchatConfig::from_value(&json!({
            "bot_token": "x", "client_token": "y", "webhook": {"path": 9}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_null_fields_default() {
        let c = GchatConfig::from_value(&json!({
            "bot_token": "x", "client_token": "y",
            "webhook": {"host": null, "port": null, "path": null}
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
    }

    #[test]
    fn rejects_empty_api_base() {
        let err = GchatConfig::from_value(&json!({
            "bot_token": "x", "client_token": "y", "api_base": ""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_api_base() {
        let err = GchatConfig::from_value(&json!({
            "bot_token": "x", "client_token": "y", "api_base": 1
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn api_base_null_defaults() {
        let c = GchatConfig::from_value(&json!({
            "bot_token": "x", "client_token": "y", "api_base": null
        }))
        .unwrap();
        assert_eq!(c.api_base, DEFAULT_API_BASE);
    }

    #[test]
    fn rejects_empty_bot_user_id() {
        let err = GchatConfig::from_value(&json!({
            "bot_token": "x", "client_token": "y", "bot_user_id": ""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_bot_user_id() {
        let err = GchatConfig::from_value(&json!({
            "bot_token": "x", "client_token": "y", "bot_user_id": 1
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn bot_user_id_null_is_none() {
        let c = GchatConfig::from_value(&json!({
            "bot_token": "x", "client_token": "y", "bot_user_id": null
        }))
        .unwrap();
        assert!(c.bot_user_id.is_none());
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
        let c = GchatConfig::from_value(&full_value()).unwrap();
        assert!(format!("{c:?}").contains("ya29.token"));
        assert!(format!("{:?}", c.webhook).contains("127.0.0.1"));
    }

    #[test]
    fn clone_eq_works() {
        let c = GchatConfig::from_value(&full_value()).unwrap();
        let c2 = c.clone();
        assert_eq!(c, c2);
    }
}
