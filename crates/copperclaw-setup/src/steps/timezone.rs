//! Step 10 — detect (or read) the system timezone.
//!
//! Walks the well-known sources and falls back to `Etc/UTC` when none yield
//! anything sensible.

use crate::config::SetupConfig;
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::{Step, StepError, StepResult};
use std::path::Path;

/// Step implementation.
#[derive(Debug, Default)]
pub struct TimezoneStep;

impl Step for TimezoneStep {
    fn name(&self) -> &'static str {
        "timezone"
    }

    fn description(&self) -> &'static str {
        "Detect the system timezone"
    }

    fn run(
        &self,
        cfg: &mut SetupConfig,
        prompt: &dyn Prompt,
        _state: &mut SetupState,
    ) -> Result<StepResult, StepError> {
        let detected = detect_timezone().unwrap_or_else(|| "Etc/UTC".to_string());
        let answer = prompt.input("TIMEZONE", "Timezone (IANA name)", Some(&detected))?;
        cfg.timezone.clone_from(&answer);
        Ok(StepResult::ok(format!("timezone: {answer}")))
    }
}

/// Probe the system for a timezone identifier. Returns `None` when none of
/// the supported sources are present.
#[must_use]
pub fn detect_timezone() -> Option<String> {
    if let Some(tz) = std::env::var_os("TZ") {
        let s = tz.to_string_lossy().trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    if let Ok(link) = std::fs::read_link("/etc/localtime") {
        if let Some(name) = tz_name_from_localtime(&link) {
            return Some(name);
        }
    }
    if let Ok(body) = std::fs::read_to_string("/etc/timezone") {
        let trimmed = body.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    None
}

/// Extract an IANA name from a path like `/usr/share/zoneinfo/Europe/Paris`.
#[must_use]
pub fn tz_name_from_localtime(path: &Path) -> Option<String> {
    let s = path.to_string_lossy();
    let marker = "zoneinfo/";
    s.find(marker).map(|i| s[i + marker.len()..].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;
    use std::path::PathBuf;

    #[test]
    fn tz_name_from_localtime_extracts_after_zoneinfo() {
        let p = PathBuf::from("/usr/share/zoneinfo/Europe/Paris");
        assert_eq!(tz_name_from_localtime(&p), Some("Europe/Paris".to_string()));
    }

    #[test]
    fn tz_name_from_localtime_handles_alternate_root() {
        let p = PathBuf::from("../zoneinfo/UTC");
        assert_eq!(tz_name_from_localtime(&p), Some("UTC".to_string()));
    }

    #[test]
    fn tz_name_from_localtime_returns_none_when_unrelated() {
        let p = PathBuf::from("/var/lib/something/else");
        assert!(tz_name_from_localtime(&p).is_none());
    }

    #[test]
    fn detect_timezone_does_not_panic() {
        let _ = detect_timezone();
    }

    #[test]
    fn step_uses_detected_when_no_override() {
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        // No scripted answer for TIMEZONE — Scripted falls back to default.
        let prompt = Scripted::new();
        let res = TimezoneStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(res.config_changed);
        assert!(!cfg.timezone.is_empty());
    }

    #[test]
    fn step_accepts_explicit_answer() {
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("TIMEZONE", "America/Los_Angeles");
        let _ = TimezoneStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert_eq!(cfg.timezone, "America/Los_Angeles");
    }

    #[test]
    fn step_metadata() {
        let s = TimezoneStep;
        assert_eq!(s.name(), "timezone");
        assert!(!s.description().is_empty());
        assert!(s.is_skippable());
    }
}
