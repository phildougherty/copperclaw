//! Parser for [`EmacsConfig`] — the JSON shape the host hands the factory.
//!
//! Schema (all fields optional; defaults apply):
//!
//! ```json
//! {
//!   "client_bin":             "emacsclient",
//!   "socket_name":            "copperclaw",
//!   "socket_dir":             "/run/user/1000/emacs",
//!   "poll_interval_ms":       500,
//!   "inbound_queue_sexp":     "(copperclaw-pop-inbound)",
//!   "outbound_sexp_template": "(copperclaw-deliver ${BUFFER_JSON} ${TEXT_JSON})",
//!   "default_buffer":         "*copperclaw*"
//! }
//! ```
//!
//! `socket_name` is passed to emacsclient as `-s <name>`. `socket_dir` is
//! passed as `--socket-name <dir>/<name>` joined together when both are set,
//! or `--socket-name <dir>` if only the directory is set. Most users will
//! pick one or the other, not both.

use copperclaw_channels_core::AdapterError;
use serde_json::Value;
use std::path::PathBuf;

/// Default `emacsclient` binary name. Resolved against `PATH` at spawn time.
pub const DEFAULT_CLIENT_BIN: &str = "emacsclient";

/// Default poll interval (milliseconds) between `copperclaw-pop-inbound`
/// invocations.
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 500;

/// Default elisp form evaluated to fetch the next inbound message.
///
/// The user's Emacs init is expected to define this function:
///
/// ```elisp
/// (defun copperclaw-pop-inbound ()
///   "Pop the next queued message for copperclaw, or return nil."
///   ;; returns nil OR
///   ;;   `(("buffer" . ,buf) ("text" . ,text) ("sender" . ,sender))
///   )
/// ```
pub const DEFAULT_INBOUND_QUEUE_SEXP: &str = "(copperclaw-pop-inbound)";

/// Default elisp template evaluated to deliver an outbound message. The
/// adapter substitutes `${BUFFER_JSON}` with a JSON-encoded string (the
/// target buffer name) and `${TEXT_JSON}` with a JSON-encoded string (the
/// message body) before passing the result to `emacsclient -e`.
///
/// The user's Emacs init is expected to define:
///
/// ```elisp
/// (defun copperclaw-deliver (buffer text)
///   "Append TEXT to BUFFER."
///   (with-current-buffer (get-buffer-create buffer)
///     (goto-char (point-max))
///     (insert text "\n")))
/// ```
pub const DEFAULT_OUTBOUND_SEXP_TEMPLATE: &str =
    "(copperclaw-deliver ${BUFFER_JSON} ${TEXT_JSON})";

/// Default Emacs buffer that receives outbound messages when the host does
/// not specify a `platform_id`.
pub const DEFAULT_BUFFER: &str = "*copperclaw*";

/// Parsed Emacs channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmacsConfig {
    /// Path or basename of the `emacsclient` binary.
    pub client_bin: String,
    /// Optional Emacs server name (`-s <socket_name>`).
    pub socket_name: Option<String>,
    /// Optional Emacs server socket directory (`--socket-name <path>`).
    pub socket_dir: Option<PathBuf>,
    /// Milliseconds between poll attempts.
    pub poll_interval_ms: u64,
    /// Elisp form evaluated to fetch the next inbound message.
    pub inbound_queue_sexp: String,
    /// Elisp template with `${BUFFER_JSON}` / `${TEXT_JSON}` placeholders.
    pub outbound_sexp_template: String,
    /// Default Emacs buffer used when `platform_id` is empty.
    pub default_buffer: String,
}

impl Default for EmacsConfig {
    fn default() -> Self {
        Self {
            client_bin: DEFAULT_CLIENT_BIN.to_owned(),
            socket_name: None,
            socket_dir: None,
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            inbound_queue_sexp: DEFAULT_INBOUND_QUEUE_SEXP.to_owned(),
            outbound_sexp_template: DEFAULT_OUTBOUND_SEXP_TEMPLATE.to_owned(),
            default_buffer: DEFAULT_BUFFER.to_owned(),
        }
    }
}

impl EmacsConfig {
    /// Parse from the host-provided JSON blob.
    ///
    /// - `Value::Null` and an empty object both produce [`Self::default`].
    /// - Unknown fields are rejected with [`AdapterError::BadRequest`].
    /// - Type mismatches are rejected with [`AdapterError::BadRequest`].
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let obj = value.as_object().ok_or_else(|| {
            AdapterError::BadRequest("emacs config must be a JSON object".into())
        })?;
        let mut cfg = Self::default();
        for (key, val) in obj {
            match key.as_str() {
                "client_bin" => {
                    cfg.client_bin = take_string(key, val)?;
                }
                "socket_name" => {
                    cfg.socket_name = take_opt_string(key, val)?;
                }
                "socket_dir" => {
                    cfg.socket_dir = take_opt_string(key, val)?.map(PathBuf::from);
                }
                "poll_interval_ms" => {
                    cfg.poll_interval_ms = take_u64(key, val)?;
                }
                "inbound_queue_sexp" => {
                    cfg.inbound_queue_sexp = take_string(key, val)?;
                }
                "outbound_sexp_template" => {
                    cfg.outbound_sexp_template = take_string(key, val)?;
                }
                "default_buffer" => {
                    cfg.default_buffer = take_string(key, val)?;
                }
                other => {
                    return Err(AdapterError::BadRequest(format!(
                        "emacs config: unknown field `{other}`"
                    )));
                }
            }
        }
        Ok(cfg)
    }
}

fn take_string(key: &str, value: &Value) -> Result<String, AdapterError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        _ => Err(AdapterError::BadRequest(format!(
            "emacs config: field `{key}` must be a string"
        ))),
    }
}

fn take_opt_string(key: &str, value: &Value) -> Result<Option<String>, AdapterError> {
    match value {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        _ => Err(AdapterError::BadRequest(format!(
            "emacs config: field `{key}` must be a string or null"
        ))),
    }
}

fn take_u64(key: &str, value: &Value) -> Result<u64, AdapterError> {
    value
        .as_u64()
        .ok_or_else(|| AdapterError::BadRequest(format!(
            "emacs config: field `{key}` must be a non-negative integer"
        )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_when_null() {
        let c = EmacsConfig::from_value(&Value::Null).unwrap();
        assert_eq!(c, EmacsConfig::default());
    }

    #[test]
    fn defaults_when_empty_object() {
        let c = EmacsConfig::from_value(&json!({})).unwrap();
        assert_eq!(c, EmacsConfig::default());
    }

    #[test]
    fn full_config_parses() {
        let c = EmacsConfig::from_value(&json!({
            "client_bin": "/usr/local/bin/emacsclient",
            "socket_name": "copperclaw",
            "socket_dir": "/run/user/1000/emacs",
            "poll_interval_ms": 250,
            "inbound_queue_sexp": "(my-pop)",
            "outbound_sexp_template": "(my-send ${BUFFER_JSON} ${TEXT_JSON})",
            "default_buffer": "*work*"
        }))
        .unwrap();
        assert_eq!(c.client_bin, "/usr/local/bin/emacsclient");
        assert_eq!(c.socket_name.as_deref(), Some("copperclaw"));
        assert_eq!(c.socket_dir, Some(PathBuf::from("/run/user/1000/emacs")));
        assert_eq!(c.poll_interval_ms, 250);
        assert_eq!(c.inbound_queue_sexp, "(my-pop)");
        assert_eq!(c.outbound_sexp_template, "(my-send ${BUFFER_JSON} ${TEXT_JSON})");
        assert_eq!(c.default_buffer, "*work*");
    }

    #[test]
    fn rejects_non_object_top_level() {
        let err = EmacsConfig::from_value(&json!("nope")).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_unknown_field() {
        let err = EmacsConfig::from_value(&json!({ "frobnitz": 1 })).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("frobnitz")),
            _ => panic!("expected BadRequest"),
        }
    }

    #[test]
    fn rejects_non_string_client_bin() {
        let err = EmacsConfig::from_value(&json!({ "client_bin": 42 })).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_default_buffer() {
        let err = EmacsConfig::from_value(&json!({ "default_buffer": [] })).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_integer_poll_interval() {
        let err = EmacsConfig::from_value(&json!({ "poll_interval_ms": "fast" })).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_negative_poll_interval() {
        let err = EmacsConfig::from_value(&json!({ "poll_interval_ms": -1 })).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn socket_name_can_be_null() {
        let c = EmacsConfig::from_value(&json!({ "socket_name": null })).unwrap();
        assert!(c.socket_name.is_none());
    }

    #[test]
    fn socket_dir_can_be_null() {
        let c = EmacsConfig::from_value(&json!({ "socket_dir": null })).unwrap();
        assert!(c.socket_dir.is_none());
    }

    #[test]
    fn rejects_non_string_socket_name() {
        let err = EmacsConfig::from_value(&json!({ "socket_name": 1 })).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_socket_dir() {
        let err = EmacsConfig::from_value(&json!({ "socket_dir": 1 })).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_inbound_queue_sexp() {
        let err = EmacsConfig::from_value(&json!({ "inbound_queue_sexp": 1 })).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn rejects_non_string_outbound_template() {
        let err = EmacsConfig::from_value(&json!({ "outbound_sexp_template": 1 })).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn default_constants_match_struct() {
        let d = EmacsConfig::default();
        assert_eq!(d.client_bin, DEFAULT_CLIENT_BIN);
        assert_eq!(d.poll_interval_ms, DEFAULT_POLL_INTERVAL_MS);
        assert_eq!(d.inbound_queue_sexp, DEFAULT_INBOUND_QUEUE_SEXP);
        assert_eq!(d.outbound_sexp_template, DEFAULT_OUTBOUND_SEXP_TEMPLATE);
        assert_eq!(d.default_buffer, DEFAULT_BUFFER);
    }

    #[test]
    fn poll_interval_zero_is_accepted() {
        // Zero is valid; the run loop will just busy-poll. We do not impose
        // an arbitrary lower bound here; the operator gets what they ask
        // for.
        let c = EmacsConfig::from_value(&json!({ "poll_interval_ms": 0 })).unwrap();
        assert_eq!(c.poll_interval_ms, 0);
    }
}
