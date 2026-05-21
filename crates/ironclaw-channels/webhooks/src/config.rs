//! Configuration for the `webhooks` channel.
//!
//! The webhooks channel is intentionally generic: a single configured
//! instance binds one HTTP listener that accepts POSTs from any third-party
//! service (`Stripe`, `Shopify`, monitoring alerts, `IoT` devices, custom
//! CI hooks, etc.). Each request is translated into a single
//! [`ironclaw_types::InboundEvent`] with `channel_type = "webhooks"`, and
//! the `platform_id` is derived from the request path segment after the
//! configured base path — that's how routing distinguishes inbound from
//! different sources.
//!
//! All fields are validated up-front at [`WebhooksConfig::from_value`] so
//! the factory can fail fast with a [`ChannelConfigError`].

use serde::Deserialize;
use thiserror::Error;

/// Default base path under which inbound webhooks are accepted.
pub const DEFAULT_PATH: &str = "/webhooks";

/// Default header that carries the HMAC-SHA256 signature when
/// [`WebhooksConfig::secret`] is set. The default mirrors the most common
/// pattern used by GitHub, Shopify, Stripe-style integrations.
pub const DEFAULT_SIGNATURE_HEADER: &str = "X-Webhook-Signature";

/// Default header prefix that must be stripped before hex-decoding. Empty
/// by default; set to e.g. `"sha256="` to match GitHub's format.
pub const DEFAULT_SIGNATURE_PREFIX: &str = "";

/// Errors that prevent a [`WebhooksConfig`] from being constructed.
#[derive(Debug, Error)]
pub enum ChannelConfigError {
    /// `host`/`port`/`path` had invalid shapes.
    #[error("invalid webhooks config: {0}")]
    Invalid(String),
    /// JSON did not deserialize.
    #[error("invalid webhooks JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Webhook ingress configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhooksConfig {
    /// Host/IP to bind the listener on.
    pub host: String,
    /// TCP port. `0` means "let the OS pick" and is honoured by the
    /// factory — useful for tests.
    pub port: u16,
    /// Base path. All POSTs under this prefix are accepted; the suffix
    /// becomes the `platform_id`. Defaults to [`DEFAULT_PATH`].
    pub path: String,
    /// Optional shared secret for HMAC-SHA256 signature verification. When
    /// set the request must carry [`signature_header`] containing the hex
    /// digest of `HMAC-SHA256(secret, raw_body)`; when None the listener
    /// accepts unsigned requests (useful for trusted internal networks).
    pub secret: Option<String>,
    /// Header to read the signature from. Ignored when `secret` is None.
    pub signature_header: String,
    /// Optional prefix that gets stripped before hex-decoding. e.g.
    /// `"sha256="` to match GitHub's `X-Hub-Signature-256` format.
    pub signature_prefix: String,
}

/// Internal serde shape — every field optional so callers can supply
/// only what they need; [`WebhooksConfig::from_value`] applies defaults.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Raw {
    host: Option<String>,
    port: Option<u16>,
    path: Option<String>,
    secret: Option<String>,
    signature_header: Option<String>,
    signature_prefix: Option<String>,
}

impl WebhooksConfig {
    /// Parse JSON into a config, applying defaults and validating shape.
    pub fn from_value(value: &serde_json::Value) -> Result<Self, ChannelConfigError> {
        let raw: Raw = serde_json::from_value(value.clone())?;

        let host = raw.host.unwrap_or_else(|| "127.0.0.1".to_string());
        if host.is_empty() {
            return Err(ChannelConfigError::Invalid("host is empty".into()));
        }

        let port = raw.port.unwrap_or(0);

        let path = raw.path.unwrap_or_else(|| DEFAULT_PATH.to_string());
        let path = canonical_path(&path)?;

        let signature_header = raw
            .signature_header
            .unwrap_or_else(|| DEFAULT_SIGNATURE_HEADER.to_string());
        if signature_header.trim().is_empty() {
            return Err(ChannelConfigError::Invalid(
                "signature_header may not be empty".into(),
            ));
        }
        let signature_prefix = raw
            .signature_prefix
            .unwrap_or_else(|| DEFAULT_SIGNATURE_PREFIX.to_string());

        let secret = raw.secret.filter(|s| !s.is_empty());

        Ok(Self {
            host,
            port,
            path,
            secret,
            signature_header,
            signature_prefix,
        })
    }
}

/// Normalize a configured path. We require a leading slash and forbid
/// trailing slashes (except for the bare `/`) so the prefix-matching in
/// the router doesn't have to think about either form.
fn canonical_path(input: &str) -> Result<String, ChannelConfigError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ChannelConfigError::Invalid("path may not be empty".into()));
    }
    if !trimmed.starts_with('/') {
        return Err(ChannelConfigError::Invalid(format!(
            "path must start with `/` (got {trimmed:?})"
        )));
    }
    if trimmed.contains("//") {
        return Err(ChannelConfigError::Invalid(format!(
            "path must not contain `//` (got {trimmed:?})"
        )));
    }
    let stripped = trimmed.trim_end_matches('/');
    if stripped.is_empty() {
        // The bare root: keep `/` so requests to exactly `/<suffix>` match.
        Ok("/".to_string())
    } else {
        Ok(stripped.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_when_empty_object() {
        let cfg = WebhooksConfig::from_value(&json!({})).unwrap();
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 0);
        assert_eq!(cfg.path, DEFAULT_PATH);
        assert!(cfg.secret.is_none());
        assert_eq!(cfg.signature_header, DEFAULT_SIGNATURE_HEADER);
        assert_eq!(cfg.signature_prefix, DEFAULT_SIGNATURE_PREFIX);
    }

    #[test]
    fn explicit_values_kept() {
        let cfg = WebhooksConfig::from_value(&json!({
            "host": "0.0.0.0",
            "port": 9999,
            "path": "/hooks",
            "secret": "topsecret",
            "signature_header": "X-Hub-Signature-256",
            "signature_prefix": "sha256="
        }))
        .unwrap();
        assert_eq!(cfg.host, "0.0.0.0");
        assert_eq!(cfg.port, 9999);
        assert_eq!(cfg.path, "/hooks");
        assert_eq!(cfg.secret.as_deref(), Some("topsecret"));
        assert_eq!(cfg.signature_header, "X-Hub-Signature-256");
        assert_eq!(cfg.signature_prefix, "sha256=");
    }

    #[test]
    fn empty_secret_treated_as_none() {
        let cfg = WebhooksConfig::from_value(&json!({"secret": ""})).unwrap();
        assert!(cfg.secret.is_none());
    }

    #[test]
    fn trailing_slash_stripped() {
        let cfg = WebhooksConfig::from_value(&json!({"path": "/h/"})).unwrap();
        assert_eq!(cfg.path, "/h");
    }

    #[test]
    fn bare_root_path_preserved() {
        let cfg = WebhooksConfig::from_value(&json!({"path": "/"})).unwrap();
        assert_eq!(cfg.path, "/");
    }

    #[test]
    fn empty_host_rejected() {
        let err = WebhooksConfig::from_value(&json!({"host": ""})).unwrap_err();
        assert!(matches!(err, ChannelConfigError::Invalid(_)));
    }

    #[test]
    fn path_without_leading_slash_rejected() {
        let err = WebhooksConfig::from_value(&json!({"path": "hooks"})).unwrap_err();
        assert!(matches!(err, ChannelConfigError::Invalid(_)));
    }

    #[test]
    fn path_with_double_slash_rejected() {
        let err = WebhooksConfig::from_value(&json!({"path": "/a//b"})).unwrap_err();
        assert!(matches!(err, ChannelConfigError::Invalid(_)));
    }

    #[test]
    fn empty_signature_header_rejected() {
        let err = WebhooksConfig::from_value(&json!({"signature_header": "   "})).unwrap_err();
        assert!(matches!(err, ChannelConfigError::Invalid(_)));
    }

    #[test]
    fn unknown_field_rejected() {
        let err = WebhooksConfig::from_value(&json!({"bogus": 1})).unwrap_err();
        assert!(matches!(err, ChannelConfigError::Json(_)));
    }
}
