//! Webex channel configuration parsed from the host JSON blob.
//!
//! Required:
//! - `bot_token` — the bot's `Bearer` token (`<token>` from the Webex
//!   developer portal).
//! - `webhook_secret` — secret used to verify `X-Spark-Signature` on
//!   inbound webhook requests.
//!
//! Optional:
//! - `api_base` — override of the Webex REST base URL (default
//!   `https://webexapis.com/v1`). Tests use this to point at a wiremock.
//! - `webhook` — bind settings for the inbound webhook HTTP server.
//!   Defaults to `{ host: "127.0.0.1", port: 8084, path: "/webex/webhook" }`.
//! - `bot_person_id` — the bot's Webex personId. Filled in best-effort at
//!   init by calling `GET /people/me`; an admin may also pin it via config.
//! - `webhook_algo` — `"sha1"` (default) or `"sha256"`. Selects the HMAC
//!   algorithm used to verify `X-Spark-Signature`.

use crate::signature::{SignatureAlgo, SignatureError};
use copperclaw_channels_core::AdapterError;
use serde_json::Value;

/// Default bind host for the webhook HTTP server.
pub const DEFAULT_HOST: &str = "127.0.0.1";
/// Default bind port for the webhook HTTP server.
pub const DEFAULT_PORT: u16 = 8084;
/// Default URL path for the webhook HTTP server.
pub const DEFAULT_PATH: &str = "/webex/webhook";
/// Default Webex REST API base URL.
pub const DEFAULT_API_BASE: &str = "https://webexapis.com/v1";
/// Default signature algorithm name.
pub const DEFAULT_ALGO: &str = "sha1";

/// Parsed Webex configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebexConfig {
    /// Bot bearer token.
    pub bot_token: String,
    /// Webhook HMAC secret.
    pub webhook_secret: String,
    /// REST API base URL.
    pub api_base: String,
    /// Webhook bind settings.
    pub webhook: WebhookConfig,
    /// Optional pinned bot personId.
    pub bot_person_id: Option<String>,
    /// HMAC algorithm used for `X-Spark-Signature`.
    pub webhook_algo: SignatureAlgo,
}

/// Bind settings for the inbound webhook HTTP server.
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

impl WebexConfig {
    /// Parse from the `serde_json::Value` the host hands to
    /// [`copperclaw_channels_core::ChannelSetup::config`].
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        let obj = value
            .as_object()
            .ok_or_else(|| AdapterError::BadRequest("webex config must be a JSON object".into()))?;

        let bot_token = required_string(obj, "bot_token")?;
        let webhook_secret = required_string(obj, "webhook_secret")?;
        let api_base = match obj.get("api_base") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::Null) | None => DEFAULT_API_BASE.to_owned(),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "webex config field `api_base` must be non-empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "webex config field `api_base` must be a string".into(),
                ));
            }
        };
        let webhook = WebhookConfig::from_value(obj.get("webhook"))?;
        let bot_person_id = match obj.get("bot_person_id") {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "webex config field `bot_person_id` must be non-empty".into(),
                ));
            }
            Some(Value::Null) | None => None,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "webex config field `bot_person_id` must be a string".into(),
                ));
            }
        };
        let webhook_algo = match obj.get("webhook_algo") {
            Some(Value::String(s)) => SignatureAlgo::parse(s).map_err(|e| map_algo_err(&e))?,
            Some(Value::Null) | None => SignatureAlgo::Sha1,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "webex config field `webhook_algo` must be a string".into(),
                ));
            }
        };

        Ok(Self {
            bot_token,
            webhook_secret,
            api_base,
            webhook,
            bot_person_id,
            webhook_algo,
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
            AdapterError::BadRequest("webex `webhook` must be a JSON object".into())
        })?;
        let host = match obj.get("host") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_HOST.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "webex `webhook.host` must be a string".into(),
                ));
            }
        };
        let port = match obj.get("port") {
            Some(Value::Number(n)) => {
                n.as_u64()
                    .and_then(|u| u16::try_from(u).ok())
                    .ok_or_else(|| {
                        AdapterError::BadRequest(
                            "webex `webhook.port` must be a u16 in range".into(),
                        )
                    })?
            }
            Some(Value::Null) | None => DEFAULT_PORT,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "webex `webhook.port` must be a number".into(),
                ));
            }
        };
        let path = match obj.get("path") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_PATH.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "webex `webhook.path` must be a string".into(),
                ));
            }
        };
        Ok(Self { host, port, path })
    }
}

fn map_algo_err(err: &SignatureError) -> AdapterError {
    AdapterError::BadRequest(format!("webex config field `webhook_algo`: {err}"))
}

fn required_string(
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String, AdapterError> {
    match obj.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err(AdapterError::BadRequest(format!(
            "webex config field `{key}` must be non-empty"
        ))),
        Some(_) => Err(AdapterError::BadRequest(format!(
            "webex config field `{key}` must be a string"
        ))),
        None => Err(AdapterError::BadRequest(format!(
            "webex config missing required field `{key}`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full_value() -> Value {
        json!({
            "bot_token": "tok-abc",
            "webhook_secret": "sec",
            "api_base": "https://example.test/api",
            "webhook": {"host": "127.0.0.1", "port": 9090, "path": "/x"},
            "bot_person_id": "PERSON123",
            "webhook_algo": "sha256"
        })
    }

    #[test]
    fn parses_full_config() {
        let c = WebexConfig::from_value(&full_value()).unwrap();
        assert_eq!(c.bot_token, "tok-abc");
        assert_eq!(c.webhook_secret, "sec");
        assert_eq!(c.api_base, "https://example.test/api");
        assert_eq!(c.webhook.host, "127.0.0.1");
        assert_eq!(c.webhook.port, 9090);
        assert_eq!(c.webhook.path, "/x");
        assert_eq!(c.bot_person_id.as_deref(), Some("PERSON123"));
        assert_eq!(c.webhook_algo, SignatureAlgo::Sha256);
    }

    #[test]
    fn defaults_when_optional_fields_omitted() {
        let c = WebexConfig::from_value(&json!({
            "bot_token": "tok",
            "webhook_secret": "s"
        }))
        .unwrap();
        assert_eq!(c.api_base, DEFAULT_API_BASE);
        assert_eq!(c.webhook, WebhookConfig::default());
        assert!(c.bot_person_id.is_none());
        assert_eq!(c.webhook_algo, SignatureAlgo::Sha1);
    }

    #[test]
    fn rejects_non_object_root() {
        let err = WebexConfig::from_value(&json!(7)).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_missing_bot_token() {
        let err = WebexConfig::from_value(&json!({"webhook_secret": "s"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("bot_token")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_webhook_secret() {
        let err = WebexConfig::from_value(&json!({"bot_token": "t"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("webhook_secret")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_bot_token() {
        let err = WebexConfig::from_value(&json!({
            "bot_token": "", "webhook_secret":"s"
        }))
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("non-empty")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_string_bot_token() {
        let err = WebexConfig::from_value(&json!({
            "bot_token": 9, "webhook_secret":"s"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_empty_webhook_secret() {
        let err = WebexConfig::from_value(&json!({
            "bot_token":"t", "webhook_secret":""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_empty_api_base() {
        let err = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","api_base":""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_api_base() {
        let err = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","api_base":1
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn api_base_null_defaults() {
        let c = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","api_base": null
        }))
        .unwrap();
        assert_eq!(c.api_base, DEFAULT_API_BASE);
    }

    #[test]
    fn rejects_non_object_webhook() {
        let err = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","webhook":7
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_null_is_default() {
        let c = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","webhook": null
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
    }

    #[test]
    fn webhook_partial_defaults_remaining() {
        let c = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","webhook":{"port": 12345}
        }))
        .unwrap();
        assert_eq!(c.webhook.host, DEFAULT_HOST);
        assert_eq!(c.webhook.port, 12345);
        assert_eq!(c.webhook.path, DEFAULT_PATH);
    }

    #[test]
    fn webhook_rejects_bad_host_type() {
        let err = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","webhook":{"host": 1}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_bad_port_type() {
        let err = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","webhook":{"port":"x"}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_port_out_of_range() {
        let err = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","webhook":{"port": 100_000}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_bad_path_type() {
        let err = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","webhook":{"path": 9}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_null_fields_default() {
        let c = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s",
            "webhook":{"host":null,"port":null,"path":null}
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
    }

    #[test]
    fn rejects_non_string_bot_person_id() {
        let err = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","bot_person_id": 5
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_empty_bot_person_id() {
        let err = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","bot_person_id": ""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn bot_person_id_null_is_none() {
        let c = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","bot_person_id": null
        }))
        .unwrap();
        assert!(c.bot_person_id.is_none());
    }

    #[test]
    fn webhook_algo_defaults_to_sha1() {
        let c = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s"
        }))
        .unwrap();
        assert_eq!(c.webhook_algo, SignatureAlgo::Sha1);
    }

    #[test]
    fn webhook_algo_accepts_explicit_sha1() {
        let c = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","webhook_algo":"sha1"
        }))
        .unwrap();
        assert_eq!(c.webhook_algo, SignatureAlgo::Sha1);
    }

    #[test]
    fn webhook_algo_accepts_sha256() {
        let c = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","webhook_algo":"sha256"
        }))
        .unwrap();
        assert_eq!(c.webhook_algo, SignatureAlgo::Sha256);
    }

    #[test]
    fn webhook_algo_null_is_default() {
        let c = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","webhook_algo": null
        }))
        .unwrap();
        assert_eq!(c.webhook_algo, SignatureAlgo::Sha1);
    }

    #[test]
    fn webhook_algo_rejects_unknown_string() {
        let err = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","webhook_algo":"md5"
        }))
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("md5")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn webhook_algo_rejects_non_string() {
        let err = WebexConfig::from_value(&json!({
            "bot_token":"t","webhook_secret":"s","webhook_algo": 1
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_default_constants_match() {
        let d = WebhookConfig::default();
        assert_eq!(d.host, DEFAULT_HOST);
        assert_eq!(d.port, DEFAULT_PORT);
        assert_eq!(d.path, DEFAULT_PATH);
    }

    #[test]
    fn debug_format_present() {
        let c = WebexConfig::from_value(&full_value()).unwrap();
        let s = format!("{c:?}");
        assert!(s.contains("tok-abc"));
        assert!(s.contains("Sha256"));
    }

    #[test]
    fn clone_eq_works() {
        let c = WebexConfig::from_value(&full_value()).unwrap();
        let c2 = c.clone();
        assert_eq!(c, c2);
    }

    #[test]
    fn default_algo_string_is_sha1() {
        // Sanity check that the constant matches the parsed default.
        let parsed = SignatureAlgo::parse(DEFAULT_ALGO).unwrap();
        assert_eq!(parsed, SignatureAlgo::Sha1);
    }
}
