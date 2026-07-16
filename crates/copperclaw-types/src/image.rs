//! Per-group container image profile.
//!
//! Selects how much toolchain is baked into a session image beyond the
//! minimal secure-by-default baseline. Lives in `copperclaw-types` because
//! two crates need it without depending on each other: `copperclaw-db`
//! stores it per-group in `container_configs.image_profile` (and folds it
//! into the rebuild fingerprint), and `copperclaw-container-rt` renders the
//! profile's extra packages into the Dockerfile at build time.

use serde::{Deserialize, Serialize};

/// Extra apt packages the `prototyping` profile bakes on top of the
/// baseline. `chromium` doubles as a headless-browser render fallback.
/// Kept sorted so a rendered Dockerfile / fingerprint is deterministic.
pub const PROTOTYPING_APT_PACKAGES: &[&str] = &["chromium", "sqlite3", "zip"];

/// Extra global npm packages the `prototyping` profile pre-seeds via the
/// existing `npm install -g` mechanism. Sorted for determinism.
pub const PROTOTYPING_NPM_PACKAGES: &[&str] = &["create-vite", "vite"];

/// Which toolchain profile a group's session image is baked with.
///
/// `Minimal` (the default) is the secure-by-default baseline: only the
/// language runtimes + diagnostic tools every agent needs. `Prototyping`
/// adds a warm web-prototyping toolchain (`sqlite3`, headless `chromium`,
/// `zip`, and global `vite` / `create-vite`) so the first "build me a web
/// app" doesn't burn its opening minutes bootstrapping — and, because
/// containers have no apt egress at runtime, so what a prototype needs is
/// baked in rather than un-installable.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageProfile {
    /// Baseline image — no extra packages. Secure-by-default.
    #[default]
    Minimal,
    /// Warm web-prototyping image — adds the prototyping apt + npm bundle.
    Prototyping,
}

impl ImageProfile {
    /// Stable lowercase wire/storage string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ImageProfile::Minimal => "minimal",
            ImageProfile::Prototyping => "prototyping",
        }
    }

    /// Parse from the storage/wire string. `None` for anything unknown.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "minimal" => Some(ImageProfile::Minimal),
            "prototyping" => Some(ImageProfile::Prototyping),
            _ => None,
        }
    }

    /// Extra apt packages this profile bakes on top of the baseline.
    #[must_use]
    pub fn extra_apt_packages(self) -> &'static [&'static str] {
        match self {
            ImageProfile::Minimal => &[],
            ImageProfile::Prototyping => PROTOTYPING_APT_PACKAGES,
        }
    }

    /// Extra global npm packages this profile pre-seeds on top of the
    /// baseline.
    #[must_use]
    pub fn extra_npm_packages(self) -> &'static [&'static str] {
        match self {
            ImageProfile::Minimal => &[],
            ImageProfile::Prototyping => PROTOTYPING_NPM_PACKAGES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_minimal() {
        assert_eq!(ImageProfile::default(), ImageProfile::Minimal);
    }

    #[test]
    fn as_str_and_parse_round_trip() {
        for p in [ImageProfile::Minimal, ImageProfile::Prototyping] {
            assert_eq!(ImageProfile::parse(p.as_str()), Some(p));
        }
        assert_eq!(ImageProfile::parse("nope"), None);
    }

    #[test]
    fn serde_is_lowercase() {
        assert_eq!(
            serde_json::to_string(&ImageProfile::Prototyping).unwrap(),
            "\"prototyping\""
        );
        let p: ImageProfile = serde_json::from_str("\"minimal\"").unwrap();
        assert_eq!(p, ImageProfile::Minimal);
    }

    #[test]
    fn minimal_has_no_extra_packages() {
        assert!(ImageProfile::Minimal.extra_apt_packages().is_empty());
        assert!(ImageProfile::Minimal.extra_npm_packages().is_empty());
    }

    #[test]
    fn prototyping_bakes_expected_bundle() {
        assert_eq!(
            ImageProfile::Prototyping.extra_apt_packages(),
            &["chromium", "sqlite3", "zip"]
        );
        assert_eq!(
            ImageProfile::Prototyping.extra_npm_packages(),
            &["create-vite", "vite"]
        );
    }
}
