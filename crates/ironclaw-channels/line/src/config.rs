//! Configuration for the LINE channel.
//!
//! The LINE Messaging API ships two separate credentials per bot:
//!
//! - `channel_secret` — used to verify the HMAC-SHA256 signature LINE
//!   stamps onto every inbound webhook delivery
//!   (`X-Line-Signature`). The signature is base64-encoded, unlike the
//!   GitHub-style hex digests other channels use.
//! - `channel_access_token` — Bearer token for the egress REST API
//!   (`/v2/bot/message/reply` and `/v2/bot/message/push`).
//!
//! Both are required. The webhook listener and the egress client live
//! in the same crate so we can constrain access to these secrets to a
//! single module boundary.

use serde::Deserialize;
use thiserror::Error;

/// Default base path the LINE webhook listener binds.
pub const DEFAULT_WEBHOOK_PATH: &str = "/line/webhook";
/// Default bind host. Safer than `0.0.0.0`; users wanting public
/// exposure typically reverse-proxy.
pub const DEFAULT_BIND_HOST: &str = "127.0.0.1";
/// LINE production API base URL.
pub const DEFAULT_API_BASE: &str = "https://api.line.me";

/// Errors raised at config parse time.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// A field had an invalid shape (empty when required, malformed
    /// URL, …).
    #[error("invalid line config: {0}")]
    Invalid(String),
    /// JSON deserialization failed (e.g. unknown field).
    #[error("invalid line JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Bind / path settings for the webhook listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookBind {
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// Fully validated LINE channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineConfig {
    /// LINE channel secret. Used to verify inbound webhook signatures.
    pub channel_secret: String,
    /// LINE channel access token (Bearer auth for egress).
    pub channel_access_token: String,
    /// API base URL — set to a wiremock URL in tests or to a regional
    /// LINE endpoint if needed. No trailing slash (stripped in
    /// `from_value`).
    pub api_base: String,
    /// Bind config for the webhook listener.
    pub webhook: WebhookBind,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Raw {
    channel_secret: Option<String>,
    channel_access_token: Option<String>,
    api_base: Option<String>,
    webhook: Option<RawWebhook>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawWebhook {
    host: Option<String>,
    port: Option<u16>,
    path: Option<String>,
}

impl LineConfig {
    /// Parse and validate a JSON config blob.
    pub fn from_value(value: &serde_json::Value) -> Result<Self, ConfigError> {
        let raw: Raw = serde_json::from_value(value.clone())?;

        let channel_secret = raw
            .channel_secret
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ConfigError::Invalid("channel_secret is required".into()))?;
        let channel_access_token = raw
            .channel_access_token
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ConfigError::Invalid("channel_access_token is required".into()))?;

        let api_base = raw
            .api_base
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string());
        let api_base = canonical_url(&api_base)?;

        let webhook_raw = raw.webhook.unwrap_or_default();
        let host = webhook_raw
            .host
            .unwrap_or_else(|| DEFAULT_BIND_HOST.to_string());
        if host.is_empty() {
            return Err(ConfigError::Invalid("webhook.host is empty".into()));
        }
        let port = webhook_raw.port.unwrap_or(0);
        let path = canonical_path(
            &webhook_raw
                .path
                .unwrap_or_else(|| DEFAULT_WEBHOOK_PATH.to_string()),
        )?;

        Ok(Self {
            channel_secret,
            channel_access_token,
            api_base,
            webhook: WebhookBind { host, port, path },
        })
    }
}

fn canonical_url(input: &str) -> Result<String, ConfigError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Invalid("api_base is empty".into()));
    }
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(ConfigError::Invalid(format!(
            "api_base must start with http:// or https:// (got {trimmed:?})"
        )));
    }
    Ok(trimmed.trim_end_matches('/').to_string())
}

fn canonical_path(input: &str) -> Result<String, ConfigError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Invalid("webhook.path is empty".into()));
    }
    if !trimmed.starts_with('/') {
        return Err(ConfigError::Invalid(format!(
            "webhook.path must start with `/` (got {trimmed:?})"
        )));
    }
    if trimmed.contains("//") {
        return Err(ConfigError::Invalid(format!(
            "webhook.path must not contain `//` (got {trimmed:?})"
        )));
    }
    let stripped = trimmed.trim_end_matches('/');
    Ok(if stripped.is_empty() {
        "/".into()
    } else {
        stripped.into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn good() -> serde_json::Value {
        json!({
            "channel_secret": "secret",
            "channel_access_token": "tok",
        })
    }

    #[test]
    fn minimum_required_fields() {
        let cfg = LineConfig::from_value(&good()).unwrap();
        assert_eq!(cfg.channel_secret, "secret");
        assert_eq!(cfg.channel_access_token, "tok");
        assert_eq!(cfg.api_base, DEFAULT_API_BASE);
        assert_eq!(cfg.webhook.host, DEFAULT_BIND_HOST);
        assert_eq!(cfg.webhook.port, 0);
        assert_eq!(cfg.webhook.path, DEFAULT_WEBHOOK_PATH);
    }

    #[test]
    fn missing_channel_secret_rejected() {
        let v = json!({"channel_access_token": "t"});
        assert!(matches!(
            LineConfig::from_value(&v).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn missing_token_rejected() {
        let v = json!({"channel_secret": "s"});
        assert!(matches!(
            LineConfig::from_value(&v).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn empty_secret_treated_as_missing() {
        let v = json!({"channel_secret": "", "channel_access_token": "t"});
        assert!(matches!(
            LineConfig::from_value(&v).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn api_base_overridable() {
        let mut v = good();
        v["api_base"] = json!("https://api-test.line.me/");
        let cfg = LineConfig::from_value(&v).unwrap();
        assert_eq!(cfg.api_base, "https://api-test.line.me");
    }

    #[test]
    fn api_base_without_scheme_rejected() {
        let mut v = good();
        v["api_base"] = json!("api.line.me");
        assert!(matches!(
            LineConfig::from_value(&v).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn webhook_path_normalized() {
        let mut v = good();
        v["webhook"] = json!({"path": "/cb/"});
        let cfg = LineConfig::from_value(&v).unwrap();
        assert_eq!(cfg.webhook.path, "/cb");
    }

    #[test]
    fn webhook_path_without_leading_slash_rejected() {
        let mut v = good();
        v["webhook"] = json!({"path": "cb"});
        assert!(matches!(
            LineConfig::from_value(&v).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn empty_webhook_host_rejected() {
        let mut v = good();
        v["webhook"] = json!({"host": ""});
        assert!(matches!(
            LineConfig::from_value(&v).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn unknown_field_rejected() {
        let mut v = good();
        v["bogus"] = json!(1);
        assert!(matches!(
            LineConfig::from_value(&v).unwrap_err(),
            ConfigError::Json(_)
        ));
    }
}
