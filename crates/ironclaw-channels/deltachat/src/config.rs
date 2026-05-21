//! Delta Chat channel configuration parsed from `ChannelSetup::config`.
//!
//! Required: `account_id` (the deltachat-rpc-server account this adapter
//! binds to).
//! Optional: `rpc_server_bin` (default [`DEFAULT_RPC_SERVER_BIN`]),
//! `extra_args` (default empty), `event_poll_ms` (default
//! [`DEFAULT_EVENT_POLL_MS`]).

use ironclaw_channels_core::AdapterError;
use serde_json::Value;

/// Default binary name for `deltachat-rpc-server`. Looked up via `PATH`.
pub const DEFAULT_RPC_SERVER_BIN: &str = "deltachat-rpc-server";

/// Default delay between empty `get_next_event` results before reissuing.
///
/// The deltachat RPC server blocks until an event is available, so this
/// value only affects the spacing between reconnect attempts when the
/// transport returns an error.
pub const DEFAULT_EVENT_POLL_MS: u64 = 200;

/// Default cap on inbound attachment size (50 MiB). Files exceeding this
/// limit are surfaced as [`ironclaw_types::MessageKind::System`] with a
/// `reason: "too_large"` note instead of being read into memory.
pub const DEFAULT_MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;

/// Fully resolved Delta Chat channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaChatConfig {
    /// Account id within the deltachat-rpc-server store.
    pub account_id: u64,
    /// Path / name of the `deltachat-rpc-server` binary.
    pub rpc_server_bin: String,
    /// Extra command-line arguments passed when spawning the subprocess.
    pub extra_args: Vec<String>,
    /// Backoff between failed `get_next_event` calls, in milliseconds.
    pub event_poll_ms: u64,
    /// When `true` (default), incoming messages whose `file` field points
    /// at a blob path are read off disk and surfaced as
    /// [`ironclaw_types::MessageKind::Chat`] events with their bytes
    /// available under `content.attachment.bytes_path`.
    ///
    /// When `false`, the adapter falls back to the legacy behaviour of
    /// surfacing the raw `file` path (under `content.attachment.path`)
    /// without reading the bytes.
    pub attachment_download: bool,
    /// Optional shared blob directory. When set, the adapter reads blobs
    /// directly from `<blob_dir>/<basename(file)>` (useful when the agent
    /// container does not share the deltachat blob store with the host).
    /// When unset, blobs are read at the path the server reported.
    pub blob_dir: Option<String>,
    /// Refuse to read any blob larger than this many bytes. Defaults to
    /// [`DEFAULT_MAX_ATTACHMENT_BYTES`]. Oversized files surface as
    /// [`ironclaw_types::MessageKind::System`] with a `reason: "too_large"`.
    pub max_attachment_bytes: u64,
}

impl DeltaChatConfig {
    /// Parse a JSON config value into a [`DeltaChatConfig`].
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        let obj = value.as_object().ok_or_else(|| {
            AdapterError::BadRequest("deltachat config must be a JSON object".into())
        })?;

        let account_id = match obj.get("account_id") {
            Some(Value::Number(n)) => n.as_u64().ok_or_else(|| {
                AdapterError::BadRequest(
                    "deltachat account_id must be a non-negative integer".into(),
                )
            })?,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "deltachat account_id must be a number".into(),
                ));
            }
            None => {
                return Err(AdapterError::BadRequest(
                    "deltachat account_id is required".into(),
                ));
            }
        };

        let rpc_server_bin = match obj.get("rpc_server_bin") {
            None | Some(Value::Null) => DEFAULT_RPC_SERVER_BIN.to_owned(),
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "deltachat rpc_server_bin must not be empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "deltachat rpc_server_bin must be a string".into(),
                ));
            }
        };

        let extra_args = match obj.get("extra_args") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(arr)) => {
                let mut out = Vec::with_capacity(arr.len());
                for v in arr {
                    let s = v.as_str().ok_or_else(|| {
                        AdapterError::BadRequest("deltachat extra_args must be strings".into())
                    })?;
                    out.push(s.to_owned());
                }
                out
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "deltachat extra_args must be an array of strings".into(),
                ));
            }
        };

        let event_poll_ms = match obj.get("event_poll_ms") {
            None | Some(Value::Null) => DEFAULT_EVENT_POLL_MS,
            Some(Value::Number(n)) => n.as_u64().ok_or_else(|| {
                AdapterError::BadRequest(
                    "deltachat event_poll_ms must be a non-negative integer".into(),
                )
            })?,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "deltachat event_poll_ms must be a number".into(),
                ));
            }
        };

        let attachment_download = parse_attachment_download(obj.get("attachment_download"))?;
        let blob_dir = parse_blob_dir(obj.get("blob_dir"))?;
        let max_attachment_bytes = parse_max_attachment_bytes(obj.get("max_attachment_bytes"))?;

        Ok(Self {
            account_id,
            rpc_server_bin,
            extra_args,
            event_poll_ms,
            attachment_download,
            blob_dir,
            max_attachment_bytes,
        })
    }
}

fn parse_attachment_download(value: Option<&Value>) -> Result<bool, AdapterError> {
    match value {
        None | Some(Value::Null) => Ok(true),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(AdapterError::BadRequest(
            "deltachat attachment_download must be a boolean".into(),
        )),
    }
}

fn parse_blob_dir(value: Option<&Value>) -> Result<Option<String>, AdapterError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
        Some(Value::String(_)) => Err(AdapterError::BadRequest(
            "deltachat blob_dir must not be empty".into(),
        )),
        Some(_) => Err(AdapterError::BadRequest(
            "deltachat blob_dir must be a string".into(),
        )),
    }
}

fn parse_max_attachment_bytes(value: Option<&Value>) -> Result<u64, AdapterError> {
    match value {
        None | Some(Value::Null) => Ok(DEFAULT_MAX_ATTACHMENT_BYTES),
        Some(Value::Number(n)) => n.as_u64().ok_or_else(|| {
            AdapterError::BadRequest(
                "deltachat max_attachment_bytes must be a non-negative integer".into(),
            )
        }),
        Some(_) => Err(AdapterError::BadRequest(
            "deltachat max_attachment_bytes must be a number".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_minimal_config() {
        let cfg = DeltaChatConfig::from_value(&json!({ "account_id": 1 })).unwrap();
        assert_eq!(cfg.account_id, 1);
        assert_eq!(cfg.rpc_server_bin, DEFAULT_RPC_SERVER_BIN);
        assert!(cfg.extra_args.is_empty());
        assert_eq!(cfg.event_poll_ms, DEFAULT_EVENT_POLL_MS);
        assert!(cfg.attachment_download);
        assert!(cfg.blob_dir.is_none());
        assert_eq!(cfg.max_attachment_bytes, DEFAULT_MAX_ATTACHMENT_BYTES);
    }

    #[test]
    fn parses_full_config() {
        let cfg = DeltaChatConfig::from_value(&json!({
            "account_id": 42,
            "rpc_server_bin": "/usr/local/bin/deltachat-rpc-server",
            "extra_args": ["--verbose", "--dbdir=/tmp/dc"],
            "event_poll_ms": 500
        }))
        .unwrap();
        assert_eq!(cfg.account_id, 42);
        assert_eq!(cfg.rpc_server_bin, "/usr/local/bin/deltachat-rpc-server");
        assert_eq!(
            cfg.extra_args,
            vec!["--verbose".to_owned(), "--dbdir=/tmp/dc".to_owned()]
        );
        assert_eq!(cfg.event_poll_ms, 500);
    }

    #[test]
    fn missing_account_id_errors() {
        let err = DeltaChatConfig::from_value(&json!({})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("account_id")));
    }

    #[test]
    fn non_object_config_errors() {
        let err = DeltaChatConfig::from_value(&json!("nope")).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn account_id_non_number_errors() {
        let err = DeltaChatConfig::from_value(&json!({"account_id": "one"})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn account_id_negative_errors() {
        let err = DeltaChatConfig::from_value(&json!({"account_id": -1})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rpc_server_bin_null_uses_default() {
        let cfg = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "rpc_server_bin": null
        }))
        .unwrap();
        assert_eq!(cfg.rpc_server_bin, DEFAULT_RPC_SERVER_BIN);
    }

    #[test]
    fn rpc_server_bin_empty_errors() {
        let err = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "rpc_server_bin": ""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("rpc_server_bin")));
    }

    #[test]
    fn rpc_server_bin_wrong_type_errors() {
        let err = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "rpc_server_bin": 7
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn extra_args_null_defaults_empty() {
        let cfg = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "extra_args": null
        }))
        .unwrap();
        assert!(cfg.extra_args.is_empty());
    }

    #[test]
    fn extra_args_must_be_array() {
        let err = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "extra_args": "one"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn extra_args_entries_must_be_strings() {
        let err = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "extra_args": [7]
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn event_poll_ms_null_defaults() {
        let cfg = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "event_poll_ms": null
        }))
        .unwrap();
        assert_eq!(cfg.event_poll_ms, DEFAULT_EVENT_POLL_MS);
    }

    #[test]
    fn event_poll_ms_negative_errors() {
        let err = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "event_poll_ms": -1
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn event_poll_ms_non_number_errors() {
        let err = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "event_poll_ms": "fast"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn clone_and_eq_roundtrip() {
        let cfg = DeltaChatConfig::from_value(&json!({"account_id": 3})).unwrap();
        let copy = cfg.clone();
        assert_eq!(cfg, copy);
    }

    #[test]
    fn debug_format_renders() {
        let cfg = DeltaChatConfig::from_value(&json!({"account_id": 1})).unwrap();
        assert!(format!("{cfg:?}").contains("DeltaChatConfig"));
    }

    #[test]
    fn default_constants_have_expected_values() {
        assert_eq!(DEFAULT_RPC_SERVER_BIN, "deltachat-rpc-server");
        assert_eq!(DEFAULT_EVENT_POLL_MS, 200);
        assert_eq!(DEFAULT_MAX_ATTACHMENT_BYTES, 50 * 1024 * 1024);
    }

    #[test]
    fn attachment_download_defaults_true_and_can_be_disabled() {
        let cfg = DeltaChatConfig::from_value(&json!({"account_id": 1})).unwrap();
        assert!(cfg.attachment_download);
        let off = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "attachment_download": false
        }))
        .unwrap();
        assert!(!off.attachment_download);
    }

    #[test]
    fn attachment_download_null_uses_default() {
        let cfg = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "attachment_download": null
        }))
        .unwrap();
        assert!(cfg.attachment_download);
    }

    #[test]
    fn attachment_download_non_bool_errors() {
        let err = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "attachment_download": "yes"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn blob_dir_optional_string_parses() {
        let cfg = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "blob_dir": "/var/data/dc"
        }))
        .unwrap();
        assert_eq!(cfg.blob_dir.as_deref(), Some("/var/data/dc"));
    }

    #[test]
    fn blob_dir_empty_errors() {
        let err = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "blob_dir": ""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn blob_dir_null_keeps_none() {
        let cfg = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "blob_dir": null
        }))
        .unwrap();
        assert!(cfg.blob_dir.is_none());
    }

    #[test]
    fn blob_dir_wrong_type_errors() {
        let err = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "blob_dir": 7
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn max_attachment_bytes_can_be_overridden() {
        let cfg = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "max_attachment_bytes": 1024
        }))
        .unwrap();
        assert_eq!(cfg.max_attachment_bytes, 1024);
    }

    #[test]
    fn max_attachment_bytes_negative_errors() {
        let err = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "max_attachment_bytes": -1
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn max_attachment_bytes_non_number_errors() {
        let err = DeltaChatConfig::from_value(&json!({
            "account_id": 1, "max_attachment_bytes": "huge"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }
}
