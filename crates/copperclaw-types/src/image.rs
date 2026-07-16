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
/// baseline. `chromium` doubles as a headless-browser render fallback; the
/// three `fonts-*` packages give deny-default-egress deployments real UI
/// fonts instead of browser fallback serif/sans-serif. Kept sorted so a
/// rendered Dockerfile / fingerprint is deterministic.
///
/// Exact trixie package names verified at M20-Q1 branch time (2026-07-16,
/// `packages.debian.org/trixie/<name>`): `fonts-inter` (4.1+ds-1),
/// `fonts-jetbrains-mono` (2.304+ds-5), and `fonts-noto-color-emoji`
/// (2.051-0+deb13u1) all exist under exactly these names — no substitution
/// needed for the font trio.
pub const PROTOTYPING_APT_PACKAGES: &[&str] = &[
    "chromium",
    "fonts-inter",
    "fonts-jetbrains-mono",
    "fonts-noto-color-emoji",
    "sqlite3",
    "zip",
];

/// Extra global npm packages the `prototyping` profile pre-seeds via the
/// existing `npm install -g` mechanism. Sorted for determinism.
pub const PROTOTYPING_NPM_PACKAGES: &[&str] = &[
    "create-vite",
    "eslint",
    "prettier",
    "tailwindcss",
    "typescript",
    "vite",
];

/// A prebuilt binary a profile bakes via a pinned, content-addressed fetch
/// rather than an apt package — used for tools with no package on the base
/// image's distro. `copperclaw-setup`'s image-build step is today's only
/// consumer: it has the host-process access (`curl` / `tar` / `sha256sum`)
/// needed to fetch, verify, and unpack a tarball at image-*build* time
/// (containers have no apt egress at *runtime*, but the host running
/// `copperclaw-setup` does — same as `install.sh`'s own release-tarball
/// fetch). Pinned by an exact upstream version tag plus a per-architecture
/// sha256 of the release tarball, so the fetch is reproducible and
/// offline-verifiable — it never floats to "latest".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedBinary {
    /// Human-readable tool name; also used as the fetch cache-key prefix.
    pub name: &'static str,
    /// Exact upstream release tag being pinned.
    pub version: &'static str,
    /// Absolute path the extracted binary is installed to inside the image.
    pub dest_path: &'static str,
    /// One entry per supported host architecture (matched against
    /// `std::env::consts::ARCH`, e.g. `"x86_64"` / `"aarch64"`).
    pub targets: &'static [PinnedBinaryTarget],
}

/// One architecture's release asset for a [`PinnedBinary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedBinaryTarget {
    /// `std::env::consts::ARCH` value this asset targets.
    pub arch: &'static str,
    /// Download URL for the release tarball (`.tar.gz`).
    pub url: &'static str,
    /// Expected sha256 (lowercase hex) of the tarball at `url`.
    pub sha256: &'static str,
    /// Path of the binary inside the extracted tarball, relative to the
    /// extraction root.
    pub archive_member: &'static str,
}

/// Exact upstream `ruff` release pinned by the `prototyping` profile.
///
/// Verified at M20-Q1 branch time (2026-07-16, `packages.debian.org`
/// search): trixie packages no `ruff` (only unrelated `python-ruffus*` /
/// `python3-scruffy` substring matches) — the trixie-apt-package branch of
/// the Q1 card does not apply, so this pins a prebuilt binary instead.
/// Both sha256 values were verified against the actual downloaded release
/// assets (not just trusted from the upstream `.sha256` files) before being
/// committed here.
pub const RUFF_PINNED_BINARY: PinnedBinary = PinnedBinary {
    name: "ruff",
    version: "0.15.22",
    dest_path: "/usr/local/bin/ruff",
    targets: &[
        PinnedBinaryTarget {
            arch: "x86_64",
            url: "https://github.com/astral-sh/ruff/releases/download/0.15.22/ruff-x86_64-unknown-linux-gnu.tar.gz",
            sha256: "d535a4be6504146e757eff67b992f11a293a7a108be22e2a5898b32c32565996",
            archive_member: "ruff-x86_64-unknown-linux-gnu/ruff",
        },
        PinnedBinaryTarget {
            arch: "aarch64",
            url: "https://github.com/astral-sh/ruff/releases/download/0.15.22/ruff-aarch64-unknown-linux-gnu.tar.gz",
            sha256: "54ec426d839d7cea1096e9ea1c5486fd2f3df62ee6cfd71dc090b18f99bebd90",
            archive_member: "ruff-aarch64-unknown-linux-gnu/ruff",
        },
    ],
};

/// Pinned binaries the `prototyping` profile bakes. Sorted for determinism
/// (today just `ruff`; grow this list rather than adding ad-hoc fetches).
pub const PROTOTYPING_PINNED_BINARIES: &[PinnedBinary] = &[RUFF_PINNED_BINARY];

/// Which toolchain profile a group's session image is baked with.
///
/// `Minimal` (the default) is the secure-by-default baseline: only the
/// language runtimes + diagnostic tools every agent needs. `Prototyping`
/// adds a warm web-prototyping toolchain — `sqlite3`, headless `chromium`,
/// `zip`, UI fonts (`fonts-inter`, `fonts-jetbrains-mono`,
/// `fonts-noto-color-emoji`), global `vite` / `create-vite` / `typescript`
/// / `eslint` / `prettier` / `tailwindcss`, and a pinned `ruff` binary — so
/// the first "build me a web app" doesn't burn its opening minutes
/// bootstrapping and a lint/typecheck verify stage is possible at all.
/// Because containers have no apt egress at runtime, what a prototype
/// needs is baked in rather than un-installable. Growing this bundle adds
/// roughly 100-170MB to the prototyping image (the three font packages,
/// the four new global npm packages, and the ~11MB `ruff` binary; the
/// pre-existing `chromium` line item still dominates the total).
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

    /// Prebuilt binaries this profile bakes via a pinned fetch (tools with
    /// no apt package on the base image's distro — see [`PinnedBinary`]).
    #[must_use]
    pub fn extra_pinned_binaries(self) -> &'static [PinnedBinary] {
        match self {
            ImageProfile::Minimal => &[],
            ImageProfile::Prototyping => PROTOTYPING_PINNED_BINARIES,
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
        assert!(ImageProfile::Minimal.extra_pinned_binaries().is_empty());
    }

    #[test]
    fn prototyping_bakes_expected_bundle() {
        assert_eq!(
            ImageProfile::Prototyping.extra_apt_packages(),
            &[
                "chromium",
                "fonts-inter",
                "fonts-jetbrains-mono",
                "fonts-noto-color-emoji",
                "sqlite3",
                "zip"
            ]
        );
        assert_eq!(
            ImageProfile::Prototyping.extra_npm_packages(),
            &[
                "create-vite",
                "eslint",
                "prettier",
                "tailwindcss",
                "typescript",
                "vite"
            ]
        );
    }

    #[test]
    fn prototyping_bakes_pinned_ruff_binary() {
        let binaries = ImageProfile::Prototyping.extra_pinned_binaries();
        assert_eq!(binaries.len(), 1);
        let ruff = binaries[0];
        assert_eq!(ruff.name, "ruff");
        assert_eq!(ruff.dest_path, "/usr/local/bin/ruff");
        // Every declared target's sha256 is well-formed (64 lowercase hex
        // chars) and its archive_member is rooted under an
        // architecture-named directory matching its own `arch` field —
        // catches a copy-paste target mismatch at compile-adjacent test
        // time rather than at first real fetch.
        assert!(!ruff.targets.is_empty());
        for target in ruff.targets {
            assert_eq!(target.sha256.len(), 64);
            assert!(
                target
                    .sha256
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "sha256 must be lowercase hex: {}",
                target.sha256
            );
            assert!(target.archive_member.ends_with("/ruff"));
            assert!(target.url.contains(ruff.version));
        }
        // At least the two most common container-host architectures are
        // covered so a fresh prototyping build doesn't fail on either.
        let arches: Vec<&str> = ruff.targets.iter().map(|t| t.arch).collect();
        assert!(arches.contains(&"x86_64"));
        assert!(arches.contains(&"aarch64"));
    }
}
