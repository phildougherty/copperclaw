//! Configuration for the `WeChat` Work channel.
//!
//! Shape of the JSON blob the host passes in
//! [`copperclaw_channels_core::ChannelSetup::config`]:
//!
//! ```json
//! {
//!   "corp_id": "wx...",
//!   "corp_secret": "...",
//!   "agent_id": 1000002,
//!   "token": "...",
//!   "encoding_aes_key": "<43-char base64>",
//!   "api_base": "https://qyapi.weixin.qq.com",
//!   "webhook": { "host": "127.0.0.1", "port": 8088, "path": "/wechat/webhook" }
//! }
//! ```
//!
//! Required fields: `corp_id`, `corp_secret`, `agent_id`, `token`,
//! `encoding_aes_key`.

use copperclaw_channels_core::AdapterError;
use serde_json::Value;

/// Default bind host for the webhook server.
pub const DEFAULT_HOST: &str = "127.0.0.1";
/// Default bind port for the webhook server.
pub const DEFAULT_PORT: u16 = 8088;
/// Default URL path for the webhook server.
pub const DEFAULT_PATH: &str = "/wechat/webhook";
/// Default Work Weixin API base URL.
pub const DEFAULT_API_BASE: &str = "https://qyapi.weixin.qq.com";

/// Parsed `WeChat` Work channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeChatConfig {
    /// Company tenant id (`CorpId`).
    pub corp_id: String,
    /// Per-app secret used to mint access tokens.
    pub corp_secret: String,
    /// Numeric agent id (the app id within the corp).
    pub agent_id: i64,
    /// Plain-text token configured on the callback admin page; used to
    /// build the `msg_signature` SHA1.
    pub token: String,
    /// The 43-character base64 `EncodingAESKey` configured on the
    /// callback admin page; decoded to a 32-byte AES key on load.
    pub encoding_aes_key: String,
    /// Work Weixin REST base. Overridable for tests.
    pub api_base: String,
    /// HTTP webhook bind settings.
    pub webhook: WebhookConfig,
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

impl WeChatConfig {
    /// Parse from the raw `serde_json::Value` the host passes through.
    ///
    /// Validates that `encoding_aes_key` decodes to a 32-byte key.
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        let obj = value.as_object().ok_or_else(|| {
            AdapterError::BadRequest("wechat config must be a JSON object".into())
        })?;

        let corp_id = required_string(obj, "corp_id")?;
        let corp_secret = required_string(obj, "corp_secret")?;
        let agent_id = required_i64(obj, "agent_id")?;
        let token = required_string(obj, "token")?;
        let encoding_aes_key = required_string(obj, "encoding_aes_key")?;
        validate_aes_key(&encoding_aes_key)?;
        let api_base = match obj.get("api_base") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::Null) | None => DEFAULT_API_BASE.to_owned(),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "wechat config field `api_base` must be non-empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "wechat config field `api_base` must be a string".into(),
                ));
            }
        };
        let webhook = WebhookConfig::from_value(obj.get("webhook"))?;

        Ok(Self {
            corp_id,
            corp_secret,
            agent_id,
            token,
            encoding_aes_key,
            api_base,
            webhook,
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
        let obj = value
            .as_object()
            .ok_or_else(|| AdapterError::BadRequest("wechat `webhook` must be a JSON object".into()))?;
        let host = match obj.get("host") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_HOST.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "wechat `webhook.host` must be a string".into(),
                ));
            }
        };
        let port = match obj.get("port") {
            Some(Value::Number(n)) => n
                .as_u64()
                .and_then(|u| u16::try_from(u).ok())
                .ok_or_else(|| {
                    AdapterError::BadRequest("wechat `webhook.port` must be a u16 in range".into())
                })?,
            Some(Value::Null) | None => DEFAULT_PORT,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "wechat `webhook.port` must be a number".into(),
                ));
            }
        };
        let path = match obj.get("path") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_PATH.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "wechat `webhook.path` must be a string".into(),
                ));
            }
        };
        Ok(Self { host, port, path })
    }
}

/// Public so the events router can re-validate at runtime if needed.
pub fn validate_aes_key(key: &str) -> Result<(), AdapterError> {
    use base64::Engine;
    // Work Weixin specifies a 43-character base64 key. Adding the standard
    // `=` pad yields 44 chars and decodes to exactly 32 bytes.
    if key.len() != 43 {
        return Err(AdapterError::BadRequest(format!(
            "wechat encoding_aes_key must be 43 characters (got {})",
            key.len()
        )));
    }
    let padded = format!("{key}=");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(padded.as_bytes())
        .map_err(|e| {
            AdapterError::BadRequest(format!("wechat encoding_aes_key is not valid base64: {e}"))
        })?;
    if decoded.len() != 32 {
        return Err(AdapterError::BadRequest(format!(
            "wechat encoding_aes_key must decode to 32 bytes (got {})",
            decoded.len()
        )));
    }
    Ok(())
}

fn required_string(
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String, AdapterError> {
    match obj.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err(AdapterError::BadRequest(format!(
            "wechat config field `{key}` must be non-empty"
        ))),
        Some(_) => Err(AdapterError::BadRequest(format!(
            "wechat config field `{key}` must be a string"
        ))),
        None => Err(AdapterError::BadRequest(format!(
            "wechat config missing required field `{key}`"
        ))),
    }
}

fn required_i64(obj: &serde_json::Map<String, Value>, key: &str) -> Result<i64, AdapterError> {
    match obj.get(key) {
        Some(Value::Number(n)) => n.as_i64().ok_or_else(|| {
            AdapterError::BadRequest(format!("wechat config field `{key}` must be an integer"))
        }),
        Some(_) => Err(AdapterError::BadRequest(format!(
            "wechat config field `{key}` must be a number"
        ))),
        None => Err(AdapterError::BadRequest(format!(
            "wechat config missing required field `{key}`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // A real-looking 43-character base64 key (43 chars + `=` padded yields
    // 32 raw bytes). Constructed at runtime to avoid magic literals in the
    // source.
    fn good_aes_key() -> String {
        use base64::Engine;
        let raw = [7u8; 32];
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        encoded.trim_end_matches('=').to_owned()
    }

    fn full_value() -> Value {
        json!({
            "corp_id": "wx-corp",
            "corp_secret": "secret",
            "agent_id": 1_000_002,
            "token": "tok",
            "encoding_aes_key": good_aes_key(),
            "api_base": "https://example.test",
            "webhook": {"host": "127.0.0.1", "port": 9100, "path": "/wc"}
        })
    }

    #[test]
    fn parses_full_config() {
        let c = WeChatConfig::from_value(&full_value()).unwrap();
        assert_eq!(c.corp_id, "wx-corp");
        assert_eq!(c.corp_secret, "secret");
        assert_eq!(c.agent_id, 1_000_002);
        assert_eq!(c.token, "tok");
        assert_eq!(c.api_base, "https://example.test");
        assert_eq!(c.webhook.host, "127.0.0.1");
        assert_eq!(c.webhook.port, 9100);
        assert_eq!(c.webhook.path, "/wc");
    }

    #[test]
    fn defaults_when_optional_fields_omitted() {
        let c = WeChatConfig::from_value(&json!({
            "corp_id":"c", "corp_secret":"s", "agent_id": 7,
            "token":"t", "encoding_aes_key": good_aes_key()
        }))
        .unwrap();
        assert_eq!(c.api_base, DEFAULT_API_BASE);
        assert_eq!(c.webhook, WebhookConfig::default());
    }

    #[test]
    fn rejects_non_object_root() {
        let err = WeChatConfig::from_value(&json!("string")).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_missing_corp_id() {
        let err = WeChatConfig::from_value(&json!({
            "corp_secret":"s","agent_id":1,"token":"t","encoding_aes_key": good_aes_key()
        }))
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("corp_id")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_corp_secret() {
        let err = WeChatConfig::from_value(&json!({
            "corp_id":"c","agent_id":1,"token":"t","encoding_aes_key": good_aes_key()
        }))
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("corp_secret")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_agent_id() {
        let err = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","token":"t","encoding_aes_key": good_aes_key()
        }))
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("agent_id")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_token() {
        let err = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","agent_id":1,
            "encoding_aes_key": good_aes_key()
        }))
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("token")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_aes_key() {
        let err = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","agent_id":1,"token":"t"
        }))
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("encoding_aes_key")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_short_aes_key() {
        let err = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","agent_id":1,"token":"t","encoding_aes_key": "short"
        }))
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("43 characters")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_base64_aes_key() {
        let mut k = String::from("!");
        k.push_str(&"a".repeat(42));
        let err = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","agent_id":1,"token":"t","encoding_aes_key": k
        }))
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("base64")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_corp_id() {
        let err = WeChatConfig::from_value(&json!({
            "corp_id":"","corp_secret":"s","agent_id":1,"token":"t","encoding_aes_key": good_aes_key()
        }))
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("non-empty")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_string_corp_id() {
        let err = WeChatConfig::from_value(&json!({
            "corp_id": 5,"corp_secret":"s","agent_id":1,"token":"t","encoding_aes_key": good_aes_key()
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_numeric_agent_id() {
        let err = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","agent_id":"nope","token":"t",
            "encoding_aes_key": good_aes_key()
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_integer_agent_id() {
        let err = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","agent_id": 9_999_999_999_999_999_999u64,
            "token":"t","encoding_aes_key": good_aes_key()
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_object_webhook() {
        let err = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","agent_id":1,"token":"t",
            "encoding_aes_key": good_aes_key(),"webhook": 5
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_null_is_default() {
        let c = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","agent_id":1,"token":"t",
            "encoding_aes_key": good_aes_key(),"webhook": null
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
    }

    #[test]
    fn webhook_partial_defaults_remaining() {
        let c = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","agent_id":1,"token":"t",
            "encoding_aes_key": good_aes_key(),
            "webhook": {"port": 1234}
        }))
        .unwrap();
        assert_eq!(c.webhook.host, DEFAULT_HOST);
        assert_eq!(c.webhook.port, 1234);
        assert_eq!(c.webhook.path, DEFAULT_PATH);
    }

    #[test]
    fn webhook_rejects_bad_host_type() {
        let err = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","agent_id":1,"token":"t",
            "encoding_aes_key": good_aes_key(),
            "webhook": {"host": 1}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_bad_port_type() {
        let err = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","agent_id":1,"token":"t",
            "encoding_aes_key": good_aes_key(),
            "webhook": {"port":"x"}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_port_out_of_range() {
        let err = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","agent_id":1,"token":"t",
            "encoding_aes_key": good_aes_key(),
            "webhook": {"port": 999_999}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_rejects_bad_path_type() {
        let err = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","agent_id":1,"token":"t",
            "encoding_aes_key": good_aes_key(),
            "webhook": {"path": 9}
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn webhook_null_fields_default() {
        let c = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","agent_id":1,"token":"t",
            "encoding_aes_key": good_aes_key(),
            "webhook": {"host": null, "port": null, "path": null}
        }))
        .unwrap();
        assert_eq!(c.webhook, WebhookConfig::default());
    }

    #[test]
    fn rejects_empty_api_base() {
        let err = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","agent_id":1,"token":"t",
            "encoding_aes_key": good_aes_key(),"api_base": ""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_api_base() {
        let err = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","agent_id":1,"token":"t",
            "encoding_aes_key": good_aes_key(),"api_base": 3
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn api_base_null_defaults() {
        let c = WeChatConfig::from_value(&json!({
            "corp_id":"c","corp_secret":"s","agent_id":1,"token":"t",
            "encoding_aes_key": good_aes_key(),"api_base": null
        }))
        .unwrap();
        assert_eq!(c.api_base, DEFAULT_API_BASE);
    }

    #[test]
    fn validate_aes_key_rejects_decoded_short() {
        // A 43-char base64 string that decodes to fewer than 32 bytes is
        // impossible with standard alphabet, but the path is covered by
        // length check up front.
        let err = validate_aes_key("short").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn validate_aes_key_accepts_proper_key() {
        validate_aes_key(&good_aes_key()).unwrap();
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
        let c = WeChatConfig::from_value(&full_value()).unwrap();
        assert!(format!("{c:?}").contains("wx-corp"));
        assert!(format!("{:?}", c.webhook).contains("127.0.0.1"));
    }

    #[test]
    fn clone_eq_works() {
        let c = WeChatConfig::from_value(&full_value()).unwrap();
        let c2 = c.clone();
        assert_eq!(c, c2);
    }
}
