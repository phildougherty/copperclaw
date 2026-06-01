//! Configuration for the Mattermost channel.
//!
//! The Mattermost channel splits cleanly along the API boundary:
//!
//! - **Inbound:** Mattermost's "outgoing webhook" feature posts JSON to
//!   a URL we own. Each webhook carries a `token` field that we compare
//!   against the configured [`Config::webhook_token`] (constant-time
//!   compare). Outgoing webhooks fire on a configurable trigger word or
//!   on every message in a channel.
//! - **Egress:** standard Mattermost REST API (`POST
//!   /api/v4/posts`) authenticated with a bot account's Personal Access
//!   Token. No native client protocol — HTTP/JSON only.
//!
//! [`Config::from_value`] parses the channel-config blob the host hands
//! the factory at boot, applies defaults, and validates the result.

use serde::Deserialize;
use thiserror::Error;

/// Default base path the outgoing-webhook server listens at.
pub const DEFAULT_WEBHOOK_PATH: &str = "/mattermost/webhook";

/// Default bind host. `127.0.0.1` is a safer default than `0.0.0.0` —
/// operators wanting public exposure typically front the host with a
/// reverse proxy.
pub const DEFAULT_BIND_HOST: &str = "127.0.0.1";

/// Errors raised at config-parse time.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Field had an invalid shape (empty when required, malformed URL, …).
    #[error("invalid mattermost config: {0}")]
    Invalid(String),
    /// JSON deserialization failed (e.g. unknown field).
    #[error("invalid mattermost JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Bind / path settings for the outgoing-webhook listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookBind {
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// Fully validated Mattermost channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MattermostConfig {
    /// Base URL of the Mattermost server, e.g. `https://chat.example.com`.
    /// No trailing slash — `from_value` strips one if supplied.
    pub server_url: String,
    /// Personal Access Token for the bot account used to send replies.
    pub access_token: String,
    /// Shared secret the Mattermost outgoing webhook sends as `token` in
    /// each request body. The channel rejects requests whose token
    /// doesn't match.
    pub webhook_token: String,
    /// Listener bind + path for outgoing-webhook ingress.
    pub webhook: WebhookBind,
    /// Optional Mattermost bot user id. When set, inbound messages from
    /// this user id are dropped so the bot doesn't talk to itself.
    pub bot_user_id: Option<String>,
}

/// Internal serde shape — every field optional so callers can supply
/// only what they need; [`MattermostConfig::from_value`] applies
/// defaults and validates.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Raw {
    server_url: Option<String>,
    access_token: Option<String>,
    webhook_token: Option<String>,
    webhook: Option<RawWebhook>,
    bot_user_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawWebhook {
    host: Option<String>,
    port: Option<u16>,
    path: Option<String>,
}

impl MattermostConfig {
    /// Parse and validate a JSON config blob.
    pub fn from_value(value: &serde_json::Value) -> Result<Self, ConfigError> {
        let raw: Raw = serde_json::from_value(value.clone())?;

        let server_url = raw
            .server_url
            .ok_or_else(|| ConfigError::Invalid("server_url is required".into()))?;
        let server_url = canonical_server_url(&server_url)?;

        let access_token = raw
            .access_token
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ConfigError::Invalid("access_token is required".into()))?;
        let webhook_token = raw
            .webhook_token
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ConfigError::Invalid("webhook_token is required".into()))?;

        let webhook_raw = raw.webhook.unwrap_or_default();
        let host = webhook_raw
            .host
            .unwrap_or_else(|| DEFAULT_BIND_HOST.to_string());
        if host.is_empty() {
            return Err(ConfigError::Invalid("webhook.host is empty".into()));
        }
        let port = webhook_raw.port.unwrap_or(0);
        let path = webhook_raw
            .path
            .unwrap_or_else(|| DEFAULT_WEBHOOK_PATH.to_string());
        let path = canonical_path(&path)?;

        Ok(Self {
            server_url,
            access_token,
            webhook_token,
            webhook: WebhookBind { host, port, path },
            bot_user_id: raw.bot_user_id.filter(|s| !s.is_empty()),
        })
    }
}

fn canonical_server_url(input: &str) -> Result<String, ConfigError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Invalid("server_url is empty".into()));
    }
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(ConfigError::Invalid(format!(
            "server_url must start with http:// or https:// (got {trimmed:?})"
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
            "server_url": "https://chat.example.com",
            "access_token": "tok-abc",
            "webhook_token": "wh-secret",
        })
    }

    #[test]
    fn minimum_required_fields() {
        let cfg = MattermostConfig::from_value(&good()).unwrap();
        assert_eq!(cfg.server_url, "https://chat.example.com");
        assert_eq!(cfg.access_token, "tok-abc");
        assert_eq!(cfg.webhook_token, "wh-secret");
        assert_eq!(cfg.webhook.host, DEFAULT_BIND_HOST);
        assert_eq!(cfg.webhook.port, 0);
        assert_eq!(cfg.webhook.path, DEFAULT_WEBHOOK_PATH);
        assert!(cfg.bot_user_id.is_none());
    }

    #[test]
    fn trailing_slash_stripped_from_server_url() {
        let mut v = good();
        v["server_url"] = json!("https://chat.example.com/");
        let cfg = MattermostConfig::from_value(&v).unwrap();
        assert_eq!(cfg.server_url, "https://chat.example.com");
    }

    #[test]
    fn explicit_webhook_bind_kept() {
        let mut v = good();
        v["webhook"] = json!({"host": "0.0.0.0", "port": 9123, "path": "/mm"});
        let cfg = MattermostConfig::from_value(&v).unwrap();
        assert_eq!(cfg.webhook.host, "0.0.0.0");
        assert_eq!(cfg.webhook.port, 9123);
        assert_eq!(cfg.webhook.path, "/mm");
    }

    #[test]
    fn bot_user_id_passes_through() {
        let mut v = good();
        v["bot_user_id"] = json!("bot-9");
        assert_eq!(
            MattermostConfig::from_value(&v).unwrap().bot_user_id,
            Some("bot-9".into())
        );
    }

    #[test]
    fn empty_bot_user_id_is_none() {
        let mut v = good();
        v["bot_user_id"] = json!("");
        assert!(
            MattermostConfig::from_value(&v)
                .unwrap()
                .bot_user_id
                .is_none()
        );
    }

    #[test]
    fn missing_server_url_rejected() {
        let v = json!({"access_token": "t", "webhook_token": "w"});
        assert!(matches!(
            MattermostConfig::from_value(&v).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn missing_access_token_rejected() {
        let v = json!({"server_url": "https://x.example", "webhook_token": "w"});
        assert!(matches!(
            MattermostConfig::from_value(&v).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn missing_webhook_token_rejected() {
        let v = json!({"server_url": "https://x.example", "access_token": "t"});
        assert!(matches!(
            MattermostConfig::from_value(&v).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn server_url_without_scheme_rejected() {
        let mut v = good();
        v["server_url"] = json!("chat.example.com");
        assert!(matches!(
            MattermostConfig::from_value(&v).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn empty_server_url_rejected() {
        let mut v = good();
        v["server_url"] = json!("  ");
        assert!(matches!(
            MattermostConfig::from_value(&v).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn empty_token_treated_as_missing() {
        let mut v = good();
        v["access_token"] = json!("");
        assert!(matches!(
            MattermostConfig::from_value(&v).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn webhook_path_without_leading_slash_rejected() {
        let mut v = good();
        v["webhook"] = json!({"path": "mm"});
        assert!(matches!(
            MattermostConfig::from_value(&v).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn webhook_path_with_double_slash_rejected() {
        let mut v = good();
        v["webhook"] = json!({"path": "/a//b"});
        assert!(matches!(
            MattermostConfig::from_value(&v).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn empty_webhook_host_rejected() {
        let mut v = good();
        v["webhook"] = json!({"host": ""});
        assert!(matches!(
            MattermostConfig::from_value(&v).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn unknown_field_rejected() {
        let mut v = good();
        v["bogus"] = json!(1);
        assert!(matches!(
            MattermostConfig::from_value(&v).unwrap_err(),
            ConfigError::Json(_)
        ));
    }
}
