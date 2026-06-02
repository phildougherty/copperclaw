//! Configuration loaded from the host-provided JSON blob.
//!
//! Shape (only `api_key` and `from` are required):
//!
//! ```json
//! {
//!   "api_key": "re_...",
//!   "from": "agent@example.test",
//!   "api_base": "https://api.resend.com",
//!   "default_subject": "(no subject)"
//! }
//! ```

use copperclaw_channels_core::AdapterError;
use serde_json::Value;

/// Default Resend API base URL.
pub const DEFAULT_API_BASE: &str = "https://api.resend.com";
/// Default subject applied when an outbound message carries none.
pub const DEFAULT_SUBJECT: &str = "(no subject)";

/// Parsed Resend channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResendConfig {
    /// Resend API key (`re_...`).
    pub api_key: String,
    /// `From:` address used for every outbound message.
    pub from: String,
    /// Resend API base URL. Overridable for tests.
    pub api_base: String,
    /// Subject applied when the outbound payload omits one.
    pub default_subject: String,
}

impl ResendConfig {
    /// Parse from the raw `serde_json::Value` the host passes through
    /// [`copperclaw_channels_core::ChannelSetup::config`].
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        let obj = value.as_object().ok_or_else(|| {
            AdapterError::BadRequest("resend config must be a JSON object".into())
        })?;

        let api_key = required_string(obj, "api_key")?;
        let from = required_string(obj, "from")?;
        let api_base = optional_nonempty_string(obj, "api_base")?
            .unwrap_or_else(|| DEFAULT_API_BASE.to_owned());
        let default_subject = optional_nonempty_string(obj, "default_subject")?
            .unwrap_or_else(|| DEFAULT_SUBJECT.to_owned());

        Ok(Self {
            api_key,
            from,
            api_base,
            default_subject,
        })
    }
}

fn required_string(
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String, AdapterError> {
    match obj.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err(AdapterError::BadRequest(format!(
            "resend config field `{key}` must be non-empty"
        ))),
        Some(_) => Err(AdapterError::BadRequest(format!(
            "resend config field `{key}` must be a string"
        ))),
        None => Err(AdapterError::BadRequest(format!(
            "resend config missing required field `{key}`"
        ))),
    }
}

fn optional_nonempty_string(
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, AdapterError> {
    match obj.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
        Some(Value::String(_)) => Err(AdapterError::BadRequest(format!(
            "resend config field `{key}` must be non-empty"
        ))),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(AdapterError::BadRequest(format!(
            "resend config field `{key}` must be a string"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full_value() -> Value {
        json!({
            "api_key": "re_test",
            "from": "agent@example.test",
            "api_base": "https://example.test/r",
            "default_subject": "Hi from agent"
        })
    }

    #[test]
    fn parses_full_config() {
        let c = ResendConfig::from_value(&full_value()).unwrap();
        assert_eq!(c.api_key, "re_test");
        assert_eq!(c.from, "agent@example.test");
        assert_eq!(c.api_base, "https://example.test/r");
        assert_eq!(c.default_subject, "Hi from agent");
    }

    #[test]
    fn defaults_when_optional_fields_omitted() {
        let c = ResendConfig::from_value(&json!({
            "api_key": "re_abc",
            "from": "a@b.test"
        }))
        .unwrap();
        assert_eq!(c.api_base, DEFAULT_API_BASE);
        assert_eq!(c.default_subject, DEFAULT_SUBJECT);
    }

    #[test]
    fn rejects_non_object_root() {
        let err = ResendConfig::from_value(&json!("string")).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_array_root() {
        let err = ResendConfig::from_value(&json!([1, 2])).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_null_root() {
        let err = ResendConfig::from_value(&Value::Null).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_missing_api_key() {
        let err = ResendConfig::from_value(&json!({"from": "a@b.test"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("api_key")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_from() {
        let err = ResendConfig::from_value(&json!({"api_key": "re_x"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("from")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_api_key() {
        let err =
            ResendConfig::from_value(&json!({"api_key": "", "from": "a@b.test"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("non-empty")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_from() {
        let err = ResendConfig::from_value(&json!({"api_key": "re_x", "from": ""})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("non-empty")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_string_api_key() {
        let err = ResendConfig::from_value(&json!({"api_key": 9, "from": "a@b.test"})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("api_key")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_string_from() {
        let err = ResendConfig::from_value(&json!({"api_key": "re_x", "from": 42})).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("from")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_api_base() {
        let err = ResendConfig::from_value(&json!({
            "api_key": "re_x", "from": "a@b.test", "api_base": ""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_api_base() {
        let err = ResendConfig::from_value(&json!({
            "api_key": "re_x", "from": "a@b.test", "api_base": 42
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn api_base_null_defaults() {
        let c = ResendConfig::from_value(&json!({
            "api_key": "re_x", "from": "a@b.test", "api_base": null
        }))
        .unwrap();
        assert_eq!(c.api_base, DEFAULT_API_BASE);
    }

    #[test]
    fn rejects_empty_default_subject() {
        let err = ResendConfig::from_value(&json!({
            "api_key": "re_x", "from": "a@b.test", "default_subject": ""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_default_subject() {
        let err = ResendConfig::from_value(&json!({
            "api_key": "re_x", "from": "a@b.test", "default_subject": false
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn default_subject_null_defaults() {
        let c = ResendConfig::from_value(&json!({
            "api_key": "re_x", "from": "a@b.test", "default_subject": null
        }))
        .unwrap();
        assert_eq!(c.default_subject, DEFAULT_SUBJECT);
    }

    #[test]
    fn constants_have_expected_values() {
        assert_eq!(DEFAULT_API_BASE, "https://api.resend.com");
        assert_eq!(DEFAULT_SUBJECT, "(no subject)");
    }

    #[test]
    fn debug_format_present() {
        let c = ResendConfig::from_value(&full_value()).unwrap();
        let s = format!("{c:?}");
        assert!(s.contains("re_test"));
        assert!(s.contains("agent@example.test"));
    }

    #[test]
    fn clone_eq_works() {
        let c = ResendConfig::from_value(&full_value()).unwrap();
        let c2 = c.clone();
        assert_eq!(c, c2);
    }
}
