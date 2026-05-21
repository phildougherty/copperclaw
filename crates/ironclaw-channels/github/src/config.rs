//! Configuration loaded from the host-provided JSON blob.
//!
//! Shape:
//!
//! ```json
//! {
//!   "token": "ghp_...",
//!   "webhook_secret": "...",
//!   "webhook": { "host": "0.0.0.0", "port": 8082, "path": "/github/webhook" },
//!   "api_base": "https://api.github.com",
//!   "bot_login": "ironclaw-bot"
//! }
//! ```

use ironclaw_channels_core::AdapterError;
use serde_json::Value;

/// Default bind host for the GitHub webhook server.
pub const DEFAULT_HOST: &str = "127.0.0.1";
/// Default bind port for the GitHub webhook server.
pub const DEFAULT_PORT: u16 = 8082;
/// Default URL path for the GitHub webhook endpoint.
pub const DEFAULT_PATH: &str = "/github/webhook";
/// Default GitHub REST API base URL.
pub const DEFAULT_API_BASE: &str = "https://api.github.com";

/// Parsed GitHub channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubConfig {
    /// GitHub access token (personal access token or installation token).
    pub token: String,
    /// Shared secret used to verify the `X-Hub-Signature-256` header.
    pub webhook_secret: String,
    /// HTTP webhook bind settings.
    pub webhook: WebhookConfig,
    /// GitHub REST API base URL. Overridable for tests.
    pub api_base: String,
    /// Optional bot login (used to detect `@<bot_login>` mentions in bodies).
    pub bot_login: Option<String>,
}

/// Bind settings for the GitHub webhook HTTP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookConfig {
    /// Bind host (e.g. `0.0.0.0`).
    pub host: String,
    /// Bind port.
    pub port: u16,
    /// URL path the webhook handler is mounted at.
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

impl GithubConfig {
    /// Parse from the raw `serde_json::Value` the host passes through
    /// [`ironclaw_channels_core::ChannelSetup::config`].
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        let obj = value.as_object().ok_or_else(|| {
            AdapterError::BadRequest("github config must be a JSON object".into())
        })?;

        let token = required_string(obj, "token")?;
        let webhook_secret = required_string(obj, "webhook_secret")?;
        let webhook = WebhookConfig::from_value(obj.get("webhook"))?;
        let api_base = match obj.get("api_base") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::Null) | None => DEFAULT_API_BASE.to_owned(),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "github config field `api_base` must be non-empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "github config field `api_base` must be a string".into(),
                ));
            }
        };
        let bot_login = match obj.get("bot_login") {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "github config field `bot_login` must be non-empty".into(),
                ));
            }
            Some(Value::Null) | None => None,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "github config field `bot_login` must be a string".into(),
                ));
            }
        };

        Ok(Self {
            token,
            webhook_secret,
            webhook,
            api_base,
            bot_login,
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
            AdapterError::BadRequest("github `webhook` must be a JSON object".into())
        })?;
        let host = match obj.get("host") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_HOST.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "github `webhook.host` must be a string".into(),
                ));
            }
        };
        let port = match obj.get("port") {
            Some(Value::Number(n)) => n
                .as_u64()
                .and_then(|u| u16::try_from(u).ok())
                .ok_or_else(|| {
                    AdapterError::BadRequest(
                        "github `webhook.port` must be a u16 in range".into(),
                    )
                })?,
            Some(Value::Null) | None => DEFAULT_PORT,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "github `webhook.port` must be a number".into(),
                ));
            }
        };
        let path = match obj.get("path") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_PATH.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "github `webhook.path` must be a string".into(),
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
            "github config field `{key}` must be non-empty"
        ))),
        Some(_) => Err(AdapterError::BadRequest(format!(
            "github config field `{key}` must be a string"
        ))),
        None => Err(AdapterError::BadRequest(format!(
            "github config missing required field `{key}`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full_value() -> Value {
        json!({
            "token": "ghp_abc",
            "webhook_secret": "sec",
            "webhook": {"host": "127.0.0.1", "port": 9090, "path": "/gh/x"},
            "api_base": "https://example.test/api",
            "bot_login": "bot"
        })
    }

    #[test]
    fn parses_full_config() {
        let c = GithubConfig::from_value(&full_value()).unwrap();
        assert_eq!(c.token, "ghp_abc");
        assert_eq!(c.webhook_secret, "sec");
        assert_eq!(c.webhook.host, "127.0.0.1");
        assert_eq!(c.webhook.port, 9090);
        assert_eq!(c.webhook.path, "/gh/x");
        assert_eq!(c.api_base, "https://example.test/api");
        assert_eq!(c.bot_login.as_deref(), Some("bot"));
    }

    #[test]
    fn defaults_when_optional_fields_omitted() {
        let c = GithubConfig::from_value(&json!({
            "token": "ghp",
            "webhook_secret": "s"
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
        assert_eq!(c.api_base, DEFAULT_API_BASE);
        assert_eq!(c.webhook.host, DEFAULT_HOST);
        assert_eq!(c.webhook.port, DEFAULT_PORT);
        assert_eq!(c.webhook.path, DEFAULT_PATH);
        assert!(c.bot_login.is_none());
    }

    #[test]
    fn rejects_non_object_root() {
        let err = GithubConfig::from_value(&json!("x")).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_object_array() {
        let err = GithubConfig::from_value(&json!([1, 2])).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_missing_token() {
        let err = GithubConfig::from_value(&json!({"webhook_secret":"s"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("token")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_webhook_secret() {
        let err = GithubConfig::from_value(&json!({"token":"x"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("webhook_secret")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_token() {
        let err =
            GithubConfig::from_value(&json!({"token":"", "webhook_secret":"s"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("non-empty")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_webhook_secret() {
        let err =
            GithubConfig::from_value(&json!({"token":"x", "webhook_secret":""})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("non-empty")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_string_token() {
        let err =
            GithubConfig::from_value(&json!({"token":42, "webhook_secret":"s"})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_webhook_secret() {
        let err =
            GithubConfig::from_value(&json!({"token":"x", "webhook_secret": 1})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_object_webhook() {
        let err = GithubConfig::from_value(&json!({
            "token":"x","webhook_secret":"s","webhook":7
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_null_is_default() {
        let c = GithubConfig::from_value(&json!({
            "token":"x","webhook_secret":"s","webhook": null
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
    }

    #[test]
    fn webhook_partial_defaults_remaining() {
        let c = GithubConfig::from_value(&json!({
            "token":"x","webhook_secret":"s","webhook":{"port": 1234}
        }))
        .unwrap();
        assert_eq!(c.webhook.host, DEFAULT_HOST);
        assert_eq!(c.webhook.port, 1234);
        assert_eq!(c.webhook.path, DEFAULT_PATH);
    }

    #[test]
    fn webhook_rejects_bad_host_type() {
        let err = GithubConfig::from_value(&json!({
            "token":"x","webhook_secret":"s","webhook":{"host": 1}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_bad_port_type() {
        let err = GithubConfig::from_value(&json!({
            "token":"x","webhook_secret":"s","webhook":{"port": "bad"}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_port_out_of_range() {
        let err = GithubConfig::from_value(&json!({
            "token":"x","webhook_secret":"s","webhook":{"port": 100_000}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_bad_path_type() {
        let err = GithubConfig::from_value(&json!({
            "token":"x","webhook_secret":"s","webhook":{"path": 9}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_null_fields_default() {
        let c = GithubConfig::from_value(&json!({
            "token":"x","webhook_secret":"s",
            "webhook":{"host": null, "port": null, "path": null}
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
    }

    #[test]
    fn rejects_empty_api_base() {
        let err = GithubConfig::from_value(&json!({
            "token":"x","webhook_secret":"s","api_base":""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_api_base() {
        let err = GithubConfig::from_value(&json!({
            "token":"x","webhook_secret":"s","api_base":1
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn api_base_null_defaults() {
        let c = GithubConfig::from_value(&json!({
            "token":"x","webhook_secret":"s","api_base": null
        }))
        .unwrap();
        assert_eq!(c.api_base, DEFAULT_API_BASE);
    }

    #[test]
    fn api_base_override_applied() {
        let c = GithubConfig::from_value(&json!({
            "token":"x","webhook_secret":"s","api_base": "https://gh.test"
        }))
        .unwrap();
        assert_eq!(c.api_base, "https://gh.test");
    }

    #[test]
    fn rejects_empty_bot_login() {
        let err = GithubConfig::from_value(&json!({
            "token":"x","webhook_secret":"s","bot_login":""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_bot_login() {
        let err = GithubConfig::from_value(&json!({
            "token":"x","webhook_secret":"s","bot_login": 7
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn bot_login_null_is_none() {
        let c = GithubConfig::from_value(&json!({
            "token":"x","webhook_secret":"s","bot_login": null
        }))
        .unwrap();
        assert!(c.bot_login.is_none());
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
        let c = GithubConfig::from_value(&full_value()).unwrap();
        assert!(format!("{c:?}").contains("ghp_abc"));
        assert!(format!("{:?}", c.webhook).contains("127.0.0.1"));
    }

    #[test]
    fn clone_eq_works() {
        let c = GithubConfig::from_value(&full_value()).unwrap();
        let c2 = c.clone();
        assert_eq!(c, c2);
    }
}
