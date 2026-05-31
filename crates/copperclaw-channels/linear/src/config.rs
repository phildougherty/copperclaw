//! Configuration loaded from the host-provided JSON blob.
//!
//! Shape (all top-level fields except `api_key` and `webhook_secret` are
//! optional):
//!
//! ```json
//! {
//!   "api_key": "lin_api_...",
//!   "webhook_secret": "...",
//!   "webhook": { "host": "127.0.0.1", "port": 8083, "path": "/linear/webhook" },
//!   "api_base": "https://api.linear.app/graphql",
//!   "bot_user_id": "abcd-...",
//!   "bot_username": "copperclaw-bot"
//! }
//! ```

use copperclaw_channels_core::AdapterError;
use serde_json::Value;

/// Default bind host for the webhook HTTP server.
pub const DEFAULT_HOST: &str = "127.0.0.1";
/// Default bind port for the webhook HTTP server.
pub const DEFAULT_PORT: u16 = 8083;
/// Default URL path the webhook is mounted at.
pub const DEFAULT_PATH: &str = "/linear/webhook";
/// Default Linear GraphQL API base URL.
pub const DEFAULT_API_BASE: &str = "https://api.linear.app/graphql";

/// Parsed Linear channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinearConfig {
    /// Linear API key (raw token; Linear historically uses a raw value in
    /// the `Authorization` header without a `Bearer` prefix). OAuth Bearer
    /// tokens are also accepted — the api client passes the value through
    /// verbatim.
    pub api_key: String,
    /// Webhook signing secret (used to validate `Linear-Signature`).
    pub webhook_secret: String,
    /// HTTP webhook bind settings.
    pub webhook: WebhookConfig,
    /// Linear GraphQL API base URL. Overridable for tests.
    pub api_base: String,
    /// Bot user id used to detect mentions in inbound comment bodies.
    /// Optional — when absent, mention detection from `@id` references is
    /// disabled.
    pub bot_user_id: Option<String>,
    /// Bot username used to detect mentions of the form `@bot_username`.
    /// Optional — when absent, the textual mention check is disabled.
    pub bot_username: Option<String>,
}

/// Bind settings for the webhook HTTP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookConfig {
    /// Host to bind to (default `127.0.0.1`).
    pub host: String,
    /// Port to bind to (default `8083`).
    pub port: u16,
    /// HTTP path the webhook is mounted at (default `/linear/webhook`).
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

impl LinearConfig {
    /// Parse from the raw `serde_json::Value` the host passes through
    /// [`copperclaw_channels_core::ChannelSetup::config`].
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        let obj = value.as_object().ok_or_else(|| {
            AdapterError::BadRequest("linear config must be a JSON object".into())
        })?;

        let api_key = required_string(obj, "api_key")?;
        let webhook_secret = required_string(obj, "webhook_secret")?;
        let webhook = WebhookConfig::from_value(obj.get("webhook"))?;
        let api_base = match obj.get("api_base") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::Null) | None => DEFAULT_API_BASE.to_owned(),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "linear config field `api_base` must be non-empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "linear config field `api_base` must be a string".into(),
                ));
            }
        };
        let bot_user_id = optional_string(obj, "bot_user_id")?;
        let bot_username = optional_string(obj, "bot_username")?;

        Ok(Self {
            api_key,
            webhook_secret,
            webhook,
            api_base,
            bot_user_id,
            bot_username,
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
            AdapterError::BadRequest("linear `webhook` must be a JSON object".into())
        })?;
        let host = match obj.get("host") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_HOST.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "linear `webhook.host` must be a string".into(),
                ));
            }
        };
        let port = match obj.get("port") {
            Some(Value::Number(n)) => n
                .as_u64()
                .and_then(|u| u16::try_from(u).ok())
                .ok_or_else(|| {
                    AdapterError::BadRequest(
                        "linear `webhook.port` must be a u16 in range".into(),
                    )
                })?,
            Some(Value::Null) | None => DEFAULT_PORT,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "linear `webhook.port` must be a number".into(),
                ));
            }
        };
        let path = match obj.get("path") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_PATH.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "linear `webhook.path` must be a string".into(),
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
            "linear config field `{key}` must be non-empty"
        ))),
        Some(_) => Err(AdapterError::BadRequest(format!(
            "linear config field `{key}` must be a string"
        ))),
        None => Err(AdapterError::BadRequest(format!(
            "linear config missing required field `{key}`"
        ))),
    }
}

fn optional_string(
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, AdapterError> {
    match obj.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
        Some(Value::String(_)) => Err(AdapterError::BadRequest(format!(
            "linear config field `{key}` must be non-empty"
        ))),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(AdapterError::BadRequest(format!(
            "linear config field `{key}` must be a string"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full_value() -> Value {
        json!({
            "api_key": "lin_api_abc",
            "webhook_secret": "ws",
            "webhook": {"host": "127.0.0.1", "port": 9091, "path": "/lx"},
            "api_base": "https://example.test/gql",
            "bot_user_id": "u-bot",
            "bot_username": "copperclaw"
        })
    }

    #[test]
    fn parses_full_config() {
        let c = LinearConfig::from_value(&full_value()).unwrap();
        assert_eq!(c.api_key, "lin_api_abc");
        assert_eq!(c.webhook_secret, "ws");
        assert_eq!(c.webhook.host, "127.0.0.1");
        assert_eq!(c.webhook.port, 9091);
        assert_eq!(c.webhook.path, "/lx");
        assert_eq!(c.api_base, "https://example.test/gql");
        assert_eq!(c.bot_user_id.as_deref(), Some("u-bot"));
        assert_eq!(c.bot_username.as_deref(), Some("copperclaw"));
    }

    #[test]
    fn defaults_when_optional_fields_omitted() {
        let c = LinearConfig::from_value(&json!({
            "api_key": "k", "webhook_secret": "s"
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
        assert_eq!(c.api_base, DEFAULT_API_BASE);
        assert_eq!(c.webhook.host, DEFAULT_HOST);
        assert_eq!(c.webhook.port, DEFAULT_PORT);
        assert_eq!(c.webhook.path, DEFAULT_PATH);
        assert!(c.bot_user_id.is_none());
        assert!(c.bot_username.is_none());
    }

    #[test]
    fn rejects_non_object_root() {
        let err = LinearConfig::from_value(&json!("string")).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_missing_api_key() {
        let err = LinearConfig::from_value(&json!({"webhook_secret":"s"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("api_key")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_webhook_secret() {
        let err = LinearConfig::from_value(&json!({"api_key":"k"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("webhook_secret")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_api_key() {
        let err =
            LinearConfig::from_value(&json!({"api_key":"", "webhook_secret":"s"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("non-empty")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_string_api_key() {
        let err = LinearConfig::from_value(&json!({"api_key": 42, "webhook_secret":"s"}))
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_object_webhook() {
        let err = LinearConfig::from_value(&json!({
            "api_key":"k","webhook_secret":"s","webhook":7
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_null_is_default() {
        let c = LinearConfig::from_value(&json!({
            "api_key":"k","webhook_secret":"s","webhook": null
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
    }

    #[test]
    fn webhook_partial_defaults_remaining() {
        let c = LinearConfig::from_value(&json!({
            "api_key":"k","webhook_secret":"s","webhook":{"port": 1234}
        }))
        .unwrap();
        assert_eq!(c.webhook.host, DEFAULT_HOST);
        assert_eq!(c.webhook.port, 1234);
        assert_eq!(c.webhook.path, DEFAULT_PATH);
    }

    #[test]
    fn webhook_rejects_bad_host_type() {
        let err = LinearConfig::from_value(&json!({
            "api_key":"k","webhook_secret":"s","webhook":{"host": 1}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_bad_port_type() {
        let err = LinearConfig::from_value(&json!({
            "api_key":"k","webhook_secret":"s","webhook":{"port": "bad"}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_port_out_of_range() {
        let err = LinearConfig::from_value(&json!({
            "api_key":"k","webhook_secret":"s","webhook":{"port": 100_000}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_bad_path_type() {
        let err = LinearConfig::from_value(&json!({
            "api_key":"k","webhook_secret":"s","webhook":{"path": 9}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_null_fields_default() {
        let c = LinearConfig::from_value(&json!({
            "api_key":"k","webhook_secret":"s",
            "webhook":{"host": null, "port": null, "path": null}
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
    }

    #[test]
    fn rejects_empty_api_base() {
        let err = LinearConfig::from_value(&json!({
            "api_key":"k","webhook_secret":"s","api_base":""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_api_base() {
        let err = LinearConfig::from_value(&json!({
            "api_key":"k","webhook_secret":"s","api_base":1
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn api_base_null_defaults() {
        let c = LinearConfig::from_value(&json!({
            "api_key":"k","webhook_secret":"s","api_base": null
        }))
        .unwrap();
        assert_eq!(c.api_base, DEFAULT_API_BASE);
    }

    #[test]
    fn bot_user_id_null_is_none() {
        let c = LinearConfig::from_value(&json!({
            "api_key":"k","webhook_secret":"s","bot_user_id": null
        }))
        .unwrap();
        assert!(c.bot_user_id.is_none());
    }

    #[test]
    fn bot_username_empty_rejected() {
        let err = LinearConfig::from_value(&json!({
            "api_key":"k","webhook_secret":"s","bot_username":""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn bot_username_non_string_rejected() {
        let err = LinearConfig::from_value(&json!({
            "api_key":"k","webhook_secret":"s","bot_username": 9
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn bot_user_id_non_string_rejected() {
        let err = LinearConfig::from_value(&json!({
            "api_key":"k","webhook_secret":"s","bot_user_id": 9
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_default_has_expected_constants() {
        let d = WebhookConfig::default();
        assert_eq!(d.host, DEFAULT_HOST);
        assert_eq!(d.port, DEFAULT_PORT);
        assert_eq!(d.path, DEFAULT_PATH);
    }

    #[test]
    fn debug_and_clone_eq_works() {
        let c = LinearConfig::from_value(&full_value()).unwrap();
        let c2 = c.clone();
        assert_eq!(c, c2);
        assert!(format!("{c:?}").contains("lin_api_abc"));
        assert!(format!("{:?}", c.webhook).contains("127.0.0.1"));
    }
}
