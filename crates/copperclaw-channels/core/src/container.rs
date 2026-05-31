//! Per-channel contribution to the agent container environment.
//!
//! When an agent group is bound to one or more channels, each channel's
//! factory contributes environment variables, bind mounts, and package
//! requirements through `ContainerContribution`. The container runtime
//! merges all contributions before spawning the agent container.
//!
//! The `Mount` type lives here (and is intentionally simple) so this crate
//! does not depend on `copperclaw-container-rt`. Channel adapters describe
//! what they need; the container runtime translates `Mount` into its own
//! runtime-specific shape.

use std::path::PathBuf;

/// A bind mount that the container must include for the channel to work.
///
/// `source` is a host-side path (typically under the channel's data dir).
/// `target` is the in-container path. `read_only` defaults to `false`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub source: PathBuf,
    pub target: PathBuf,
    pub read_only: bool,
}

impl Mount {
    /// Read-write mount.
    pub fn rw(source: impl Into<PathBuf>, target: impl Into<PathBuf>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            read_only: false,
        }
    }

    /// Read-only mount.
    pub fn ro(source: impl Into<PathBuf>, target: impl Into<PathBuf>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            read_only: true,
        }
    }
}

/// Aggregate of everything a channel contributes to an agent container.
///
/// Per § 5.1 of `PLAN.md`. All fields default to empty so a channel that
/// needs nothing (e.g. the CLI channel) can use `ContainerContribution::default()`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerContribution {
    /// Environment variables to set inside the container.
    pub env: Vec<(String, String)>,
    /// Bind mounts to add to the container.
    pub mounts: Vec<Mount>,
    /// Apt packages to install at image build time.
    pub packages_apt: Vec<String>,
    /// Npm packages to install at image build time.
    pub packages_npm: Vec<String>,
}

impl ContainerContribution {
    /// Returns true when the contribution adds nothing to the container.
    pub fn is_empty(&self) -> bool {
        self.env.is_empty()
            && self.mounts.is_empty()
            && self.packages_apt.is_empty()
            && self.packages_npm.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_rw_is_writable() {
        let m = Mount::rw("/host/x", "/container/x");
        assert_eq!(m.source, PathBuf::from("/host/x"));
        assert_eq!(m.target, PathBuf::from("/container/x"));
        assert!(!m.read_only);
    }

    #[test]
    fn mount_ro_is_read_only() {
        let m = Mount::ro("/host/y", "/container/y");
        assert!(m.read_only);
    }

    #[test]
    fn mount_equality_and_clone() {
        let a = Mount::rw("/a", "/b");
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn default_is_empty() {
        let c = ContainerContribution::default();
        assert!(c.is_empty());
        assert!(c.env.is_empty());
        assert!(c.mounts.is_empty());
        assert!(c.packages_apt.is_empty());
        assert!(c.packages_npm.is_empty());
    }

    #[test]
    fn is_empty_false_when_any_field_populated() {
        let mut c = ContainerContribution::default();
        c.env.push(("KEY".into(), "v".into()));
        assert!(!c.is_empty());

        let mut c = ContainerContribution::default();
        c.mounts.push(Mount::rw("/a", "/b"));
        assert!(!c.is_empty());

        let mut c = ContainerContribution::default();
        c.packages_apt.push("curl".into());
        assert!(!c.is_empty());

        let mut c = ContainerContribution::default();
        c.packages_npm.push("axios".into());
        assert!(!c.is_empty());
    }

    #[test]
    fn debug_format_is_available() {
        let c = ContainerContribution::default();
        let _ = format!("{c:?}");
    }
}
