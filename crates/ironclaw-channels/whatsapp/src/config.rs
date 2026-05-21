//! WhatsApp channel configuration parsed from `ChannelSetup::config`.
//!
//! Required fields: none. The crate ships a working default endpoint and
//! pulls keystore state from the per-channel data dir, so a channel can be
//! initialised with `{}`.
//!
//! Optional fields:
//!   - `endpoint` — WebSocket URL. Defaults to [`DEFAULT_ENDPOINT`].
//!   - `keystore_path` — absolute path to the keystore JSON. Defaults to
//!     `data_dir/whatsapp_keystore.json` (the factory fills this in when
//!     it constructs the adapter; the config carries the override).
//!   - `pairing_code` — optional phone-number pairing code (8 digits, no
//!     dashes). Reserved for future pairing flows; presently the adapter
//!     only logs it. Must match `^[0-9]{8}$` if present.
//!   - `connect_timeout_secs` — seconds to wait for a WebSocket connect.
//!     Defaults to [`DEFAULT_CONNECT_TIMEOUT_SECS`].
//!   - `request_timeout_secs` — seconds to wait for any single in-flight
//!     request before considering the gateway stalled. Defaults to
//!     [`DEFAULT_REQUEST_TIMEOUT_SECS`].
//!   - `heartbeat_interval_secs` — keepalive cadence. Defaults to
//!     [`DEFAULT_HEARTBEAT_INTERVAL_SECS`].

use ironclaw_channels_core::AdapterError;
use serde_json::Value;

/// Default WebSocket endpoint for WhatsApp Web. Public so tests and host
/// tooling can spot-check the wired-in default.
pub const DEFAULT_ENDPOINT: &str = "wss://web.whatsapp.com/ws/chat";

/// Default connect timeout, in seconds.
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 15;

/// Default per-request timeout, in seconds.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Default heartbeat interval, in seconds. WhatsApp's reverse-engineered
/// protocol uses a 25-30s keepalive; we pick 25 to leave headroom.
pub const DEFAULT_HEARTBEAT_INTERVAL_SECS: u64 = 25;

/// Maximum allowed value (inclusive) for any of the timeout fields, to
/// catch obvious config typos like `"timeout": 9999999`.
pub const MAX_TIMEOUT_SECS: u64 = 60 * 60;

/// Fully resolved WhatsApp channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhatsAppConfig {
    /// WebSocket endpoint URL.
    pub endpoint: String,
    /// Path to the keystore JSON file. Empty until the factory fills it in.
    pub keystore_path: String,
    /// Optional phone-number pairing code (8 digits, no dashes).
    pub pairing_code: Option<String>,
    /// Connect timeout in seconds.
    pub connect_timeout_secs: u64,
    /// Per-request timeout in seconds.
    pub request_timeout_secs: u64,
    /// Heartbeat interval in seconds.
    pub heartbeat_interval_secs: u64,
}

impl Default for WhatsAppConfig {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.to_owned(),
            keystore_path: String::new(),
            pairing_code: None,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
            heartbeat_interval_secs: DEFAULT_HEARTBEAT_INTERVAL_SECS,
        }
    }
}

impl WhatsAppConfig {
    /// Parse a JSON config value into a [`WhatsAppConfig`].
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        let obj = if value.is_null() {
            serde_json::Map::new()
        } else {
            value
                .as_object()
                .cloned()
                .ok_or_else(|| {
                    AdapterError::BadRequest("whatsapp config must be a JSON object".into())
                })?
        };

        let endpoint = match obj.get("endpoint") {
            None | Some(Value::Null) => DEFAULT_ENDPOINT.to_owned(),
            Some(Value::String(s)) if !s.is_empty() => {
                validate_endpoint(s)?;
                s.clone()
            }
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "whatsapp endpoint must not be empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "whatsapp endpoint must be a string".into(),
                ));
            }
        };

        let keystore_path = match obj.get("keystore_path") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "whatsapp keystore_path must be a string".into(),
                ));
            }
        };

        let pairing_code = match obj.get("pairing_code") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => {
                if !is_valid_pairing_code(s) {
                    return Err(AdapterError::BadRequest(
                        "whatsapp pairing_code must be 8 ASCII digits".into(),
                    ));
                }
                Some(s.clone())
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "whatsapp pairing_code must be a string".into(),
                ));
            }
        };

        let connect_timeout_secs = read_u64(
            &obj,
            "connect_timeout_secs",
            DEFAULT_CONNECT_TIMEOUT_SECS,
        )?;
        let request_timeout_secs = read_u64(
            &obj,
            "request_timeout_secs",
            DEFAULT_REQUEST_TIMEOUT_SECS,
        )?;
        let heartbeat_interval_secs = read_u64(
            &obj,
            "heartbeat_interval_secs",
            DEFAULT_HEARTBEAT_INTERVAL_SECS,
        )?;

        Ok(Self {
            endpoint,
            keystore_path,
            pairing_code,
            connect_timeout_secs,
            request_timeout_secs,
            heartbeat_interval_secs,
        })
    }
}

fn read_u64(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    default: u64,
) -> Result<u64, AdapterError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(n)) => {
            let v = n.as_u64().ok_or_else(|| {
                AdapterError::BadRequest(format!(
                    "whatsapp {key} must be a non-negative integer"
                ))
            })?;
            if v == 0 {
                return Err(AdapterError::BadRequest(format!(
                    "whatsapp {key} must be at least 1"
                )));
            }
            if v > MAX_TIMEOUT_SECS {
                return Err(AdapterError::BadRequest(format!(
                    "whatsapp {key} must not exceed {MAX_TIMEOUT_SECS} seconds"
                )));
            }
            Ok(v)
        }
        Some(_) => Err(AdapterError::BadRequest(format!(
            "whatsapp {key} must be an integer"
        ))),
    }
}

fn validate_endpoint(s: &str) -> Result<(), AdapterError> {
    // We only enforce the scheme — full URL parsing happens at connect time.
    if !(s.starts_with("ws://") || s.starts_with("wss://")) {
        return Err(AdapterError::BadRequest(format!(
            "whatsapp endpoint must start with ws:// or wss://, got `{s}`"
        )));
    }
    Ok(())
}

fn is_valid_pairing_code(s: &str) -> bool {
    s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_empty_object_with_defaults() {
        let cfg = WhatsAppConfig::from_value(&json!({})).unwrap();
        assert_eq!(cfg.endpoint, DEFAULT_ENDPOINT);
        assert_eq!(cfg.keystore_path, "");
        assert_eq!(cfg.pairing_code, None);
        assert_eq!(cfg.connect_timeout_secs, DEFAULT_CONNECT_TIMEOUT_SECS);
        assert_eq!(cfg.request_timeout_secs, DEFAULT_REQUEST_TIMEOUT_SECS);
        assert_eq!(cfg.heartbeat_interval_secs, DEFAULT_HEARTBEAT_INTERVAL_SECS);
    }

    #[test]
    fn parses_null_as_empty_object() {
        let cfg = WhatsAppConfig::from_value(&Value::Null).unwrap();
        assert_eq!(cfg.endpoint, DEFAULT_ENDPOINT);
    }

    #[test]
    fn parses_full_config() {
        let cfg = WhatsAppConfig::from_value(&json!({
            "endpoint": "wss://example.test/ws",
            "keystore_path": "/var/lib/wa/store.json",
            "pairing_code": "12345678",
            "connect_timeout_secs": 5,
            "request_timeout_secs": 10,
            "heartbeat_interval_secs": 20
        }))
        .unwrap();
        assert_eq!(cfg.endpoint, "wss://example.test/ws");
        assert_eq!(cfg.keystore_path, "/var/lib/wa/store.json");
        assert_eq!(cfg.pairing_code.as_deref(), Some("12345678"));
        assert_eq!(cfg.connect_timeout_secs, 5);
        assert_eq!(cfg.request_timeout_secs, 10);
        assert_eq!(cfg.heartbeat_interval_secs, 20);
    }

    #[test]
    fn rejects_non_object() {
        let err = WhatsAppConfig::from_value(&json!("nope")).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_empty_endpoint() {
        let err = WhatsAppConfig::from_value(&json!({"endpoint": ""})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_endpoint() {
        let err = WhatsAppConfig::from_value(&json!({"endpoint": 7})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_endpoint_without_ws_scheme() {
        let err = WhatsAppConfig::from_value(&json!({"endpoint": "https://x"})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn accepts_plain_ws_endpoint() {
        let cfg =
            WhatsAppConfig::from_value(&json!({"endpoint": "ws://127.0.0.1:9999/ws"})).unwrap();
        assert_eq!(cfg.endpoint, "ws://127.0.0.1:9999/ws");
    }

    #[test]
    fn keystore_path_null_defaults_to_empty() {
        let cfg = WhatsAppConfig::from_value(&json!({"keystore_path": null})).unwrap();
        assert_eq!(cfg.keystore_path, "");
    }

    #[test]
    fn keystore_path_wrong_type_errors() {
        let err = WhatsAppConfig::from_value(&json!({"keystore_path": 1})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn pairing_code_must_be_eight_digits() {
        let cases = [
            "1234567",   // too short
            "123456789", // too long
            "1234567a",  // non-digit
            "abcdefgh",  // letters
            "",          // empty
        ];
        for c in cases {
            let err =
                WhatsAppConfig::from_value(&json!({"pairing_code": c})).unwrap_err();
            assert!(
                matches!(err, AdapterError::BadRequest(_)),
                "case `{c}` should error"
            );
        }
    }

    #[test]
    fn pairing_code_null_is_none() {
        let cfg = WhatsAppConfig::from_value(&json!({"pairing_code": null})).unwrap();
        assert_eq!(cfg.pairing_code, None);
    }

    #[test]
    fn pairing_code_wrong_type_errors() {
        let err = WhatsAppConfig::from_value(&json!({"pairing_code": 12_345_678})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn timeout_zero_is_rejected() {
        let err =
            WhatsAppConfig::from_value(&json!({"connect_timeout_secs": 0})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn timeout_above_max_is_rejected() {
        let err = WhatsAppConfig::from_value(&json!({
            "request_timeout_secs": MAX_TIMEOUT_SECS + 1
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn timeout_negative_is_rejected() {
        let err =
            WhatsAppConfig::from_value(&json!({"heartbeat_interval_secs": -1})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn timeout_wrong_type_is_rejected() {
        let err = WhatsAppConfig::from_value(&json!({"connect_timeout_secs": "fast"}))
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn timeout_null_falls_back_to_default() {
        let cfg = WhatsAppConfig::from_value(&json!({
            "connect_timeout_secs": null,
            "request_timeout_secs": null,
            "heartbeat_interval_secs": null
        }))
        .unwrap();
        assert_eq!(cfg.connect_timeout_secs, DEFAULT_CONNECT_TIMEOUT_SECS);
        assert_eq!(cfg.request_timeout_secs, DEFAULT_REQUEST_TIMEOUT_SECS);
        assert_eq!(cfg.heartbeat_interval_secs, DEFAULT_HEARTBEAT_INTERVAL_SECS);
    }

    #[test]
    fn clone_and_eq_roundtrip() {
        let cfg = WhatsAppConfig::from_value(&json!({})).unwrap();
        let copy = cfg.clone();
        assert_eq!(cfg, copy);
    }

    #[test]
    fn debug_format_renders() {
        let cfg = WhatsAppConfig::default();
        assert!(format!("{cfg:?}").contains("WhatsAppConfig"));
    }

    #[test]
    fn defaults_constants_are_sensible() {
        assert_eq!(DEFAULT_CONNECT_TIMEOUT_SECS, 15);
        assert_eq!(DEFAULT_REQUEST_TIMEOUT_SECS, 30);
        assert_eq!(DEFAULT_HEARTBEAT_INTERVAL_SECS, 25);
        assert_eq!(MAX_TIMEOUT_SECS, 3600);
        assert_eq!(DEFAULT_ENDPOINT, "wss://web.whatsapp.com/ws/chat");
    }

    #[test]
    fn is_valid_pairing_code_helper() {
        assert!(is_valid_pairing_code("00000000"));
        assert!(is_valid_pairing_code("12345678"));
        assert!(!is_valid_pairing_code("1234567"));
        assert!(!is_valid_pairing_code("123456789"));
        assert!(!is_valid_pairing_code("1234567a"));
    }
}
