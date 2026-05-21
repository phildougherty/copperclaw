//! Configuration loaded from the host-provided JSON blob.
//!
//! Required fields:
//! - `bot_token` — Microsoft Graph application access token used as the
//!   `Authorization: Bearer ...` header on every outbound call. The channel
//!   does not perform the OAuth dance itself; callers supply an already-issued
//!   token.
//! - `client_state_secret` — opaque string the subscription creator sets as
//!   the `clientState` field of every change-notification. The webhook
//!   handler rejects any notification whose `clientState` does not match.
//!
//! Optional fields:
//! - `graph_base` — Microsoft Graph base URL. Defaults to
//!   `https://graph.microsoft.com/v1.0`. Overridable for tests.
//! - `webhook` — bind settings for the inbound change-notification webhook.
//!   Defaults to `127.0.0.1:8085` with path `/teams/webhook`.
//! - `bot_user_id` — Microsoft Graph user id for the bot itself. Used to
//!   suppress self-notifications. Optional because app-only tokens cannot
//!   discover it via `GET /me`; admins set it explicitly.

use ironclaw_channels_core::AdapterError;
use serde_json::Value;

/// Default bind host for the change-notification webhook.
pub const DEFAULT_HOST: &str = "127.0.0.1";
/// Default bind port for the change-notification webhook.
pub const DEFAULT_PORT: u16 = 8085;
/// Default URL path for the change-notification webhook.
pub const DEFAULT_PATH: &str = "/teams/webhook";
/// Default Microsoft Graph base URL.
pub const DEFAULT_GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

/// Parsed Teams channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamsConfig {
    /// Microsoft Graph application access token (Bearer).
    pub bot_token: String,
    /// Shared secret echoed back as `clientState` on every change-notification.
    pub client_state_secret: String,
    /// Microsoft Graph base URL.
    pub graph_base: String,
    /// HTTP webhook bind settings.
    pub webhook: WebhookConfig,
    /// Microsoft Graph user id of the bot (used to filter self-notifications).
    pub bot_user_id: Option<String>,
}

/// Bind settings for the change-notification HTTP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookConfig {
    /// Bind host (IP address).
    pub host: String,
    /// Bind port.
    pub port: u16,
    /// URL path the webhook is mounted at.
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

impl TeamsConfig {
    /// Parse the raw `serde_json::Value` the host passes through
    /// [`ironclaw_channels_core::ChannelSetup::config`].
    ///
    /// Returns [`AdapterError::BadRequest`] on missing required fields,
    /// wrong types, or out-of-range numeric values.
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        let obj = value
            .as_object()
            .ok_or_else(|| AdapterError::BadRequest("teams config must be a JSON object".into()))?;

        let bot_token = required_string(obj, "bot_token")?;
        let client_state_secret = required_string(obj, "client_state_secret")?;
        let webhook = WebhookConfig::from_value(obj.get("webhook"))?;
        let graph_base = match obj.get("graph_base") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::Null) | None => DEFAULT_GRAPH_BASE.to_owned(),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "teams config field `graph_base` must be non-empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "teams config field `graph_base` must be a string".into(),
                ));
            }
        };
        let bot_user_id = match obj.get("bot_user_id") {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "teams config field `bot_user_id` must be non-empty".into(),
                ));
            }
            Some(Value::Null) | None => None,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "teams config field `bot_user_id` must be a string".into(),
                ));
            }
        };

        Ok(Self {
            bot_token,
            client_state_secret,
            graph_base,
            webhook,
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
            AdapterError::BadRequest("teams `webhook` must be a JSON object".into())
        })?;
        let host = match obj.get("host") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_HOST.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "teams `webhook.host` must be a string".into(),
                ));
            }
        };
        let port = match obj.get("port") {
            Some(Value::Number(n)) => n
                .as_u64()
                .and_then(|u| u16::try_from(u).ok())
                .ok_or_else(|| {
                    AdapterError::BadRequest(
                        "teams `webhook.port` must be a u16 in range".into(),
                    )
                })?,
            Some(Value::Null) | None => DEFAULT_PORT,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "teams `webhook.port` must be a number".into(),
                ));
            }
        };
        let path = match obj.get("path") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_PATH.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "teams `webhook.path` must be a string".into(),
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
            "teams config field `{key}` must be non-empty"
        ))),
        Some(_) => Err(AdapterError::BadRequest(format!(
            "teams config field `{key}` must be a string"
        ))),
        None => Err(AdapterError::BadRequest(format!(
            "teams config missing required field `{key}`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full_value() -> Value {
        json!({
            "bot_token": "Bearer-abc",
            "client_state_secret": "shh",
            "graph_base": "https://example.test/v1.0",
            "webhook": {"host": "127.0.0.1", "port": 9090, "path": "/teams/cb"},
            "bot_user_id": "BOT-USER-1"
        })
    }

    #[test]
    fn parses_full_config() {
        let c = TeamsConfig::from_value(&full_value()).unwrap();
        assert_eq!(c.bot_token, "Bearer-abc");
        assert_eq!(c.client_state_secret, "shh");
        assert_eq!(c.graph_base, "https://example.test/v1.0");
        assert_eq!(c.webhook.host, "127.0.0.1");
        assert_eq!(c.webhook.port, 9090);
        assert_eq!(c.webhook.path, "/teams/cb");
        assert_eq!(c.bot_user_id.as_deref(), Some("BOT-USER-1"));
    }

    #[test]
    fn defaults_when_optional_fields_omitted() {
        let c = TeamsConfig::from_value(&json!({
            "bot_token": "tok",
            "client_state_secret": "s"
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
        assert_eq!(c.graph_base, DEFAULT_GRAPH_BASE);
        assert!(c.bot_user_id.is_none());
        assert_eq!(c.webhook.host, DEFAULT_HOST);
        assert_eq!(c.webhook.port, DEFAULT_PORT);
        assert_eq!(c.webhook.path, DEFAULT_PATH);
    }

    #[test]
    fn rejects_non_object_root() {
        let err = TeamsConfig::from_value(&json!("not an object")).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_missing_bot_token() {
        let err = TeamsConfig::from_value(&json!({"client_state_secret": "s"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("bot_token")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_client_state_secret() {
        let err = TeamsConfig::from_value(&json!({"bot_token": "tok"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("client_state_secret")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_bot_token() {
        let err = TeamsConfig::from_value(
            &json!({"bot_token": "", "client_state_secret": "s"}),
        )
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("non-empty")),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_string_bot_token() {
        let err = TeamsConfig::from_value(
            &json!({"bot_token": 1, "client_state_secret": "s"}),
        )
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_empty_client_state_secret() {
        let err = TeamsConfig::from_value(
            &json!({"bot_token": "t", "client_state_secret": ""}),
        )
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_client_state_secret() {
        let err = TeamsConfig::from_value(
            &json!({"bot_token": "t", "client_state_secret": 9}),
        )
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_object_webhook() {
        let err = TeamsConfig::from_value(&json!({
            "bot_token": "t", "client_state_secret": "s", "webhook": 7
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_null_is_default() {
        let c = TeamsConfig::from_value(&json!({
            "bot_token": "t", "client_state_secret": "s", "webhook": null
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
    }

    #[test]
    fn webhook_partial_defaults_remaining() {
        let c = TeamsConfig::from_value(&json!({
            "bot_token": "t", "client_state_secret": "s", "webhook": {"port": 1234}
        }))
        .unwrap();
        assert_eq!(c.webhook.host, DEFAULT_HOST);
        assert_eq!(c.webhook.port, 1234);
        assert_eq!(c.webhook.path, DEFAULT_PATH);
    }

    #[test]
    fn webhook_rejects_bad_host_type() {
        let err = TeamsConfig::from_value(&json!({
            "bot_token": "t", "client_state_secret": "s", "webhook": {"host": 1}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_bad_port_type() {
        let err = TeamsConfig::from_value(&json!({
            "bot_token": "t", "client_state_secret": "s", "webhook": {"port": "bad"}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_port_out_of_range() {
        let err = TeamsConfig::from_value(&json!({
            "bot_token": "t", "client_state_secret": "s", "webhook": {"port": 100_000}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_bad_path_type() {
        let err = TeamsConfig::from_value(&json!({
            "bot_token": "t", "client_state_secret": "s", "webhook": {"path": 9}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_null_fields_default() {
        let c = TeamsConfig::from_value(&json!({
            "bot_token": "t", "client_state_secret": "s",
            "webhook": {"host": null, "port": null, "path": null}
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
    }

    #[test]
    fn rejects_empty_graph_base() {
        let err = TeamsConfig::from_value(&json!({
            "bot_token": "t", "client_state_secret": "s", "graph_base": ""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_graph_base() {
        let err = TeamsConfig::from_value(&json!({
            "bot_token": "t", "client_state_secret": "s", "graph_base": 5
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn graph_base_null_defaults() {
        let c = TeamsConfig::from_value(&json!({
            "bot_token": "t", "client_state_secret": "s", "graph_base": null
        }))
        .unwrap();
        assert_eq!(c.graph_base, DEFAULT_GRAPH_BASE);
    }

    #[test]
    fn rejects_empty_bot_user_id() {
        let err = TeamsConfig::from_value(&json!({
            "bot_token": "t", "client_state_secret": "s", "bot_user_id": ""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_bot_user_id() {
        let err = TeamsConfig::from_value(&json!({
            "bot_token": "t", "client_state_secret": "s", "bot_user_id": 5
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn bot_user_id_null_defaults_to_none() {
        let c = TeamsConfig::from_value(&json!({
            "bot_token": "t", "client_state_secret": "s", "bot_user_id": null
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
        let c = TeamsConfig::from_value(&full_value()).unwrap();
        let s = format!("{c:?}");
        assert!(s.contains("Bearer-abc"));
    }

    #[test]
    fn clone_eq_works() {
        let c = TeamsConfig::from_value(&full_value()).unwrap();
        let c2 = c.clone();
        assert_eq!(c, c2);
    }
}
