//! Configuration for the `WhatsApp` Cloud channel.
//!
//! Shape of the JSON blob the host passes in
//! [`ironclaw_channels_core::ChannelSetup::config`]:
//!
//! ```json
//! {
//!   "access_token": "EAAG...",
//!   "app_secret": "...",
//!   "verify_token": "...",
//!   "graph_base": "https://graph.facebook.com/v18.0",
//!   "default_phone_number_id": "1234567890",
//!   "bot_phone_number_id": "1234567890",
//!   "webhook": { "host": "127.0.0.1", "port": 8087, "path": "/whatsapp-cloud/webhook" }
//! }
//! ```
//!
//! Required fields: `access_token`, `app_secret`, `verify_token`.

use ironclaw_channels_core::AdapterError;
use serde_json::Value;

/// Default bind host for the webhook server.
pub const DEFAULT_HOST: &str = "127.0.0.1";
/// Default bind port for the webhook server.
pub const DEFAULT_PORT: u16 = 8087;
/// Default URL path for the webhook server.
pub const DEFAULT_PATH: &str = "/whatsapp-cloud/webhook";
/// Default Graph API base URL.
pub const DEFAULT_GRAPH_BASE: &str = "https://graph.facebook.com/v18.0";

/// Parsed `WhatsApp` Cloud channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhatsappCloudConfig {
    /// Bearer token used for Graph API calls. Persistent system-user token.
    pub access_token: String,
    /// App secret used to verify the `X-Hub-Signature-256` header.
    pub app_secret: String,
    /// Verify token that must match `hub.verify_token` on the GET handshake.
    pub verify_token: String,
    /// Graph API base URL. Overridable for tests.
    pub graph_base: String,
    /// HTTP webhook bind settings.
    pub webhook: WebhookConfig,
    /// Phone-number-id used when an outbound `platform_id` has no `<pnid>:`
    /// prefix. Optional.
    pub default_phone_number_id: Option<String>,
    /// Our own phone-number-id, surfaced for tracing / mention detection.
    /// Optional.
    pub bot_phone_number_id: Option<String>,
}

/// Bind settings for the webhook HTTP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookConfig {
    /// Host to bind to.
    pub host: String,
    /// Port to bind to.
    pub port: u16,
    /// URL path the webhook is served at.
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

impl WhatsappCloudConfig {
    /// Parse from the raw `serde_json::Value` the host passes through.
    ///
    /// Strict: unknown top-level types return `BadRequest`.
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        let obj = value.as_object().ok_or_else(|| {
            AdapterError::BadRequest("whatsapp-cloud config must be a JSON object".into())
        })?;

        let access_token = required_string(obj, "access_token")?;
        let app_secret = required_string(obj, "app_secret")?;
        let verify_token = required_string(obj, "verify_token")?;
        let webhook = WebhookConfig::from_value(obj.get("webhook"))?;
        let graph_base = match obj.get("graph_base") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::Null) | None => DEFAULT_GRAPH_BASE.to_owned(),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "whatsapp-cloud config field `graph_base` must be non-empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "whatsapp-cloud config field `graph_base` must be a string".into(),
                ));
            }
        };
        let default_phone_number_id = optional_string(obj, "default_phone_number_id")?;
        let bot_phone_number_id = optional_string(obj, "bot_phone_number_id")?;

        Ok(Self {
            access_token,
            app_secret,
            verify_token,
            graph_base,
            webhook,
            default_phone_number_id,
            bot_phone_number_id,
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
            AdapterError::BadRequest("whatsapp-cloud `webhook` must be a JSON object".into())
        })?;
        let host = match obj.get("host") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_HOST.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "whatsapp-cloud `webhook.host` must be a string".into(),
                ));
            }
        };
        let port = match obj.get("port") {
            Some(Value::Number(n)) => n
                .as_u64()
                .and_then(|u| u16::try_from(u).ok())
                .ok_or_else(|| {
                    AdapterError::BadRequest(
                        "whatsapp-cloud `webhook.port` must be a u16 in range".into(),
                    )
                })?,
            Some(Value::Null) | None => DEFAULT_PORT,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "whatsapp-cloud `webhook.port` must be a number".into(),
                ));
            }
        };
        let path = match obj.get("path") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_PATH.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "whatsapp-cloud `webhook.path` must be a string".into(),
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
            "whatsapp-cloud config field `{key}` must be non-empty"
        ))),
        Some(_) => Err(AdapterError::BadRequest(format!(
            "whatsapp-cloud config field `{key}` must be a string"
        ))),
        None => Err(AdapterError::BadRequest(format!(
            "whatsapp-cloud config missing required field `{key}`"
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
            "whatsapp-cloud config field `{key}` must be non-empty when present"
        ))),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(AdapterError::BadRequest(format!(
            "whatsapp-cloud config field `{key}` must be a string"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full_value() -> Value {
        json!({
            "access_token": "EAAG-token",
            "app_secret": "a-secret",
            "verify_token": "v-token",
            "graph_base": "https://example.test/v18.0",
            "default_phone_number_id": "111",
            "bot_phone_number_id": "222",
            "webhook": {"host": "127.0.0.1", "port": 9999, "path": "/wa"}
        })
    }

    #[test]
    fn parses_full_config() {
        let c = WhatsappCloudConfig::from_value(&full_value()).unwrap();
        assert_eq!(c.access_token, "EAAG-token");
        assert_eq!(c.app_secret, "a-secret");
        assert_eq!(c.verify_token, "v-token");
        assert_eq!(c.graph_base, "https://example.test/v18.0");
        assert_eq!(c.default_phone_number_id.as_deref(), Some("111"));
        assert_eq!(c.bot_phone_number_id.as_deref(), Some("222"));
        assert_eq!(c.webhook.host, "127.0.0.1");
        assert_eq!(c.webhook.port, 9999);
        assert_eq!(c.webhook.path, "/wa");
    }

    #[test]
    fn defaults_when_optional_fields_omitted() {
        let c = WhatsappCloudConfig::from_value(&json!({
            "access_token": "t",
            "app_secret": "s",
            "verify_token": "v"
        }))
        .unwrap();
        assert_eq!(c.graph_base, DEFAULT_GRAPH_BASE);
        assert_eq!(c.webhook, WebhookConfig::default());
        assert!(c.default_phone_number_id.is_none());
        assert!(c.bot_phone_number_id.is_none());
    }

    #[test]
    fn rejects_non_object_root() {
        let err = WhatsappCloudConfig::from_value(&json!("string")).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_missing_access_token() {
        let err = WhatsappCloudConfig::from_value(
            &json!({"app_secret": "s", "verify_token": "v"}),
        )
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("access_token")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_app_secret() {
        let err = WhatsappCloudConfig::from_value(
            &json!({"access_token": "t", "verify_token": "v"}),
        )
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("app_secret")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_verify_token() {
        let err = WhatsappCloudConfig::from_value(
            &json!({"access_token": "t", "app_secret": "s"}),
        )
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("verify_token")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_access_token() {
        let err = WhatsappCloudConfig::from_value(&json!({
            "access_token": "", "app_secret":"s", "verify_token":"v"
        }))
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("non-empty")),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_string_access_token() {
        let err = WhatsappCloudConfig::from_value(&json!({
            "access_token": 1, "app_secret":"s", "verify_token":"v"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_object_webhook() {
        let err = WhatsappCloudConfig::from_value(&json!({
            "access_token":"t","app_secret":"s","verify_token":"v","webhook": 5
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_null_is_default() {
        let c = WhatsappCloudConfig::from_value(&json!({
            "access_token":"t","app_secret":"s","verify_token":"v","webhook": null
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
    }

    #[test]
    fn webhook_partial_defaults_remaining() {
        let c = WhatsappCloudConfig::from_value(&json!({
            "access_token":"t","app_secret":"s","verify_token":"v",
            "webhook":{"port": 1234}
        }))
        .unwrap();
        assert_eq!(c.webhook.host, DEFAULT_HOST);
        assert_eq!(c.webhook.port, 1234);
        assert_eq!(c.webhook.path, DEFAULT_PATH);
    }

    #[test]
    fn webhook_rejects_bad_host_type() {
        let err = WhatsappCloudConfig::from_value(&json!({
            "access_token":"t","app_secret":"s","verify_token":"v",
            "webhook":{"host": 1}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_bad_port_type() {
        let err = WhatsappCloudConfig::from_value(&json!({
            "access_token":"t","app_secret":"s","verify_token":"v",
            "webhook":{"port": "no"}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_port_out_of_range() {
        let err = WhatsappCloudConfig::from_value(&json!({
            "access_token":"t","app_secret":"s","verify_token":"v",
            "webhook":{"port": 99999}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_bad_path_type() {
        let err = WhatsappCloudConfig::from_value(&json!({
            "access_token":"t","app_secret":"s","verify_token":"v",
            "webhook":{"path": 9}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_null_fields_default() {
        let c = WhatsappCloudConfig::from_value(&json!({
            "access_token":"t","app_secret":"s","verify_token":"v",
            "webhook":{"host": null, "port": null, "path": null}
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
    }

    #[test]
    fn rejects_empty_graph_base() {
        let err = WhatsappCloudConfig::from_value(&json!({
            "access_token":"t","app_secret":"s","verify_token":"v","graph_base":""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_graph_base() {
        let err = WhatsappCloudConfig::from_value(&json!({
            "access_token":"t","app_secret":"s","verify_token":"v","graph_base":3
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn graph_base_null_defaults() {
        let c = WhatsappCloudConfig::from_value(&json!({
            "access_token":"t","app_secret":"s","verify_token":"v","graph_base": null
        }))
        .unwrap();
        assert_eq!(c.graph_base, DEFAULT_GRAPH_BASE);
    }

    #[test]
    fn optional_phone_number_id_empty_string_rejected() {
        let err = WhatsappCloudConfig::from_value(&json!({
            "access_token":"t","app_secret":"s","verify_token":"v",
            "default_phone_number_id": ""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn optional_phone_number_id_non_string_rejected() {
        let err = WhatsappCloudConfig::from_value(&json!({
            "access_token":"t","app_secret":"s","verify_token":"v",
            "bot_phone_number_id": 5
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn optional_phone_number_id_null_is_none() {
        let c = WhatsappCloudConfig::from_value(&json!({
            "access_token":"t","app_secret":"s","verify_token":"v",
            "default_phone_number_id": null,
            "bot_phone_number_id": null
        }))
        .unwrap();
        assert!(c.default_phone_number_id.is_none());
        assert!(c.bot_phone_number_id.is_none());
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
        let c = WhatsappCloudConfig::from_value(&full_value()).unwrap();
        assert!(format!("{c:?}").contains("EAAG-token"));
        assert!(format!("{:?}", c.webhook).contains("127.0.0.1"));
    }

    #[test]
    fn clone_eq_works() {
        let c = WhatsappCloudConfig::from_value(&full_value()).unwrap();
        let c2 = c.clone();
        assert_eq!(c, c2);
    }
}
