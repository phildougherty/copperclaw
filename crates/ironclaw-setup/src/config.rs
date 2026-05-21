//! Typed outputs of the setup steps.
//!
//! Each step produces fragments that the `SetupConfig` aggregates. The
//! struct is `serde`-friendly so it can round-trip via [`crate::state::SetupState`].

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Detected environment-tool versions / availability.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct EnvReport {
    /// Whether `docker` is on `PATH`.
    pub has_docker: bool,
    /// Whether Apple's `container` CLI is on `PATH`.
    pub has_apple_container: bool,
    /// Whether `git` is on `PATH`.
    pub has_git: bool,
    /// Whether `gh` (GitHub CLI) is on `PATH`.
    pub has_gh: bool,
    /// Whether `iclaw` (the ironclaw CLI agent) is on `PATH`.
    pub has_ncl: bool,
}

impl EnvReport {
    /// `true` when at least one container runtime is present.
    #[must_use]
    pub fn has_container_runtime(&self) -> bool {
        self.has_docker || self.has_apple_container
    }
}

/// Optional `OneCLI` Agent Vault configuration captured during setup.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OneCliConfig {
    /// Base URL of the vault service. Empty when the user declined.
    pub base_url: String,
    /// Bearer token. Persisted to `setup-state.json` so the host can re-use it.
    pub bearer_token: String,
    /// Slug used during the connectivity check.
    pub probe_slug: String,
}

/// Aggregated outputs of a full setup run.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetupConfig {
    /// Root data directory (e.g. `~/.local/share/ironclaw`).
    pub data_dir: PathBuf,
    /// Path to the central DB inside `data_dir`.
    pub central_db_path: PathBuf,
    /// Detected environment report.
    pub env_report: EnvReport,
    /// Image tag from the container-image build step. Empty when skipped.
    pub image_tag: String,
    /// Optional `OneCLI` wiring.
    pub onecli: Option<OneCliConfig>,
    /// Absolute path to the `.env` file written by the auth step.
    pub env_file: PathBuf,
    /// Extra host paths to bind-mount into agent containers.
    pub mount_paths: Vec<PathBuf>,
    /// Path to the generated systemd unit / launchd plist.
    pub service_unit_path: PathBuf,
    /// IANA timezone identifier captured from the system.
    pub timezone: String,
    /// First channel kind the user configured (e.g. `cli`).
    pub first_channel: String,
    /// `true` once the `quickstart_group` step has successfully
    /// bootstrapped a default cli agent group + wiring. Read by
    /// the first-chat step to tailor its "what to do next"
    /// instructions: when this is set we tell the user to run
    /// `iclaw chat` directly; otherwise we point them at
    /// `iclaw quickstart cli` as the manual fallback.
    #[serde(default)]
    pub quickstart_group_created: bool,
}

impl SetupConfig {
    /// Whether the `OneCLI` step ran successfully.
    #[must_use]
    pub fn onecli_configured(&self) -> bool {
        self.onecli.as_ref().is_some_and(|c| !c.base_url.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_report_default_has_no_runtime() {
        let r = EnvReport::default();
        assert!(!r.has_container_runtime());
    }

    #[test]
    fn env_report_with_docker() {
        let r = EnvReport {
            has_docker: true,
            ..EnvReport::default()
        };
        assert!(r.has_container_runtime());
    }

    #[test]
    fn env_report_with_apple_container() {
        let r = EnvReport {
            has_apple_container: true,
            ..EnvReport::default()
        };
        assert!(r.has_container_runtime());
    }

    #[test]
    fn setup_config_default_has_no_onecli() {
        let c = SetupConfig::default();
        assert!(!c.onecli_configured());
    }

    #[test]
    fn setup_config_with_onecli() {
        let c = SetupConfig {
            onecli: Some(OneCliConfig {
                base_url: "https://vault.example".into(),
                bearer_token: "t".into(),
                probe_slug: "host".into(),
            }),
            ..SetupConfig::default()
        };
        assert!(c.onecli_configured());
    }

    #[test]
    fn setup_config_onecli_empty_url_is_not_configured() {
        let c = SetupConfig {
            onecli: Some(OneCliConfig::default()),
            ..SetupConfig::default()
        };
        assert!(!c.onecli_configured());
    }

    #[test]
    fn env_report_roundtrips_via_json() {
        let r = EnvReport {
            has_docker: true,
            has_apple_container: false,
            has_git: true,
            has_gh: false,
            has_ncl: true,
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: EnvReport = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn setup_config_roundtrips_via_json() {
        let c = SetupConfig {
            data_dir: PathBuf::from("/tmp/x"),
            central_db_path: PathBuf::from("/tmp/x/data/ironclaw.db"),
            env_report: EnvReport {
                has_git: true,
                ..EnvReport::default()
            },
            image_tag: "ironclaw/session:sha256-abc".into(),
            onecli: None,
            env_file: PathBuf::from("/tmp/x/.env"),
            mount_paths: vec![PathBuf::from("/srv/data")],
            service_unit_path: PathBuf::from("/tmp/x/ironclaw.service"),
            timezone: "Etc/UTC".into(),
            first_channel: "cli".into(),
            quickstart_group_created: true,
        };
        let s = serde_json::to_string(&c).unwrap();
        let back: SetupConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(c, back);
    }
}
