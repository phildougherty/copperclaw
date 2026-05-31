//! Signal channel configuration parsed from `ChannelSetup::config`.
//!
//! Required fields: `account` (the e164 the daemon is registered as,
//! e.g. `"+15551234567"`).
//!
//! Optional fields:
//!   - `signal_cli_bin` (default `"signal-cli"`) — path to the signal-cli
//!     binary. Used by the factory when spawning the daemon subprocess.
//!   - `extra_args` (default empty) — extra CLI arguments inserted before
//!     `daemon` when starting the subprocess.
//!   - `restart_on_exit` (default `true`) — when set, the adapter attempts
//!     a one-shot restart of the subprocess on the next call after it
//!     dies. The adapter never loops — exactly one retry per failure.

use copperclaw_channels_core::AdapterError;
use serde_json::Value;

/// Default signal-cli binary name (resolved via `PATH`).
pub const DEFAULT_SIGNAL_CLI_BIN: &str = "signal-cli";

/// Default value of `restart_on_exit`.
pub const DEFAULT_RESTART_ON_EXIT: bool = true;

/// Fully resolved Signal channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalConfig {
    /// The e164 phone number the signal-cli daemon is registered as.
    pub account: String,
    /// Path to the `signal-cli` binary.
    pub signal_cli_bin: String,
    /// Extra CLI arguments inserted before `daemon`.
    pub extra_args: Vec<String>,
    /// Whether to attempt a one-shot restart when the subprocess dies.
    pub restart_on_exit: bool,
}

impl SignalConfig {
    /// Parse a JSON config value into a [`SignalConfig`].
    ///
    /// The expected shape:
    ///
    /// ```json
    /// {
    ///   "account": "+15551234567",
    ///   "signal_cli_bin": "/usr/local/bin/signal-cli",
    ///   "extra_args": ["--config", "/etc/signal-cli"],
    ///   "restart_on_exit": true
    /// }
    /// ```
    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        let obj = value.as_object().ok_or_else(|| {
            AdapterError::BadRequest("signal config must be a JSON object".into())
        })?;

        let account = match obj.get("account") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "signal account must not be empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "signal account must be a string".into(),
                ));
            }
            None => {
                return Err(AdapterError::BadRequest(
                    "signal account is required".into(),
                ));
            }
        };
        if !account.starts_with('+') {
            return Err(AdapterError::BadRequest(format!(
                "signal account must be an e164 starting with `+`, got `{account}`"
            )));
        }

        let signal_cli_bin = match obj.get("signal_cli_bin") {
            None | Some(Value::Null) => DEFAULT_SIGNAL_CLI_BIN.to_owned(),
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::String(_)) => {
                return Err(AdapterError::BadRequest(
                    "signal signal_cli_bin must not be empty".into(),
                ));
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "signal signal_cli_bin must be a string".into(),
                ));
            }
        };

        let extra_args = match obj.get("extra_args") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(arr)) => {
                let mut out = Vec::with_capacity(arr.len());
                for v in arr {
                    let s = v.as_str().ok_or_else(|| {
                        AdapterError::BadRequest(
                            "signal extra_args entries must be strings".into(),
                        )
                    })?;
                    out.push(s.to_owned());
                }
                out
            }
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "signal extra_args must be an array".into(),
                ));
            }
        };

        let restart_on_exit = match obj.get("restart_on_exit") {
            None | Some(Value::Null) => DEFAULT_RESTART_ON_EXIT,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "signal restart_on_exit must be a boolean".into(),
                ));
            }
        };

        Ok(Self {
            account,
            signal_cli_bin,
            extra_args,
            restart_on_exit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_minimal_config() {
        let cfg = SignalConfig::from_value(&json!({"account": "+15551234567"})).unwrap();
        assert_eq!(cfg.account, "+15551234567");
        assert_eq!(cfg.signal_cli_bin, DEFAULT_SIGNAL_CLI_BIN);
        assert!(cfg.extra_args.is_empty());
        assert!(cfg.restart_on_exit);
    }

    #[test]
    fn parses_full_config() {
        let cfg = SignalConfig::from_value(&json!({
            "account": "+15550001234",
            "signal_cli_bin": "/usr/local/bin/signal-cli",
            "extra_args": ["--config", "/etc/sc"],
            "restart_on_exit": false
        }))
        .unwrap();
        assert_eq!(cfg.account, "+15550001234");
        assert_eq!(cfg.signal_cli_bin, "/usr/local/bin/signal-cli");
        assert_eq!(cfg.extra_args, vec!["--config", "/etc/sc"]);
        assert!(!cfg.restart_on_exit);
    }

    #[test]
    fn rejects_non_object() {
        let err = SignalConfig::from_value(&json!("nope")).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn missing_account_errors() {
        let err = SignalConfig::from_value(&json!({})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("account")));
    }

    #[test]
    fn empty_account_errors() {
        let err = SignalConfig::from_value(&json!({"account": ""})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("empty")));
    }

    #[test]
    fn non_string_account_errors() {
        let err = SignalConfig::from_value(&json!({"account": 7})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn account_without_plus_errors() {
        let err = SignalConfig::from_value(&json!({"account": "15551234567"})).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("e164")));
    }

    #[test]
    fn signal_cli_bin_null_defaults() {
        let cfg = SignalConfig::from_value(&json!({
            "account": "+15551234567",
            "signal_cli_bin": null
        }))
        .unwrap();
        assert_eq!(cfg.signal_cli_bin, DEFAULT_SIGNAL_CLI_BIN);
    }

    #[test]
    fn signal_cli_bin_empty_errors() {
        let err = SignalConfig::from_value(&json!({
            "account": "+15551234567",
            "signal_cli_bin": ""
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn signal_cli_bin_wrong_type_errors() {
        let err = SignalConfig::from_value(&json!({
            "account": "+15551234567",
            "signal_cli_bin": 9
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn extra_args_null_defaults_to_empty() {
        let cfg = SignalConfig::from_value(&json!({
            "account": "+15551234567",
            "extra_args": null
        }))
        .unwrap();
        assert!(cfg.extra_args.is_empty());
    }

    #[test]
    fn extra_args_must_be_array() {
        let err = SignalConfig::from_value(&json!({
            "account": "+15551234567",
            "extra_args": "x"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn extra_args_entries_must_be_strings() {
        let err = SignalConfig::from_value(&json!({
            "account": "+15551234567",
            "extra_args": [7]
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn restart_on_exit_null_defaults() {
        let cfg = SignalConfig::from_value(&json!({
            "account": "+15551234567",
            "restart_on_exit": null
        }))
        .unwrap();
        assert_eq!(cfg.restart_on_exit, DEFAULT_RESTART_ON_EXIT);
    }

    #[test]
    fn restart_on_exit_wrong_type_errors() {
        let err = SignalConfig::from_value(&json!({
            "account": "+15551234567",
            "restart_on_exit": "yes"
        }))
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn clone_and_eq_roundtrip() {
        let cfg = SignalConfig::from_value(&json!({"account": "+15551234567"})).unwrap();
        let copy = cfg.clone();
        assert_eq!(cfg, copy);
    }

    #[test]
    fn debug_format_renders() {
        let cfg = SignalConfig::from_value(&json!({"account": "+15551234567"})).unwrap();
        let s = format!("{cfg:?}");
        assert!(s.contains("SignalConfig"));
    }

    #[test]
    fn defaults_constants() {
        assert_eq!(DEFAULT_SIGNAL_CLI_BIN, "signal-cli");
        // Document the intended default; comparing against a runtime-bound
        // local sidesteps clippy::assertions_on_constants.
        let expected_default = true;
        assert_eq!(DEFAULT_RESTART_ON_EXIT, expected_default);
    }
}
