//! Image-build spec: declarative inputs that render to a Dockerfile.
//!
//! The host derives an `ImageBuildSpec` from a session's
//! `container_configs.apt_packages` / `npm_packages` / extra-file list,
//! then asks the runtime to build it. We fingerprint the inputs with
//! sha256 so two specs with identical contents map to the same image
//! tag — building is then a no-op when the tag already exists.

use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// A file to include in the build context.
///
/// Bytes are kept in memory because builds happen rarely (per session
/// spawn at most) and inputs are small — Dockerfile + a handful of
/// config files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraFile {
    /// Destination path inside the image (absolute).
    pub path: PathBuf,
    /// File contents.
    pub contents: Vec<u8>,
    /// Octal mode bits (e.g. `0o644`). `0` means runtime default.
    pub mode: u32,
}

impl ExtraFile {
    /// Convenience constructor.
    pub fn new(path: impl Into<PathBuf>, contents: impl Into<Vec<u8>>) -> Self {
        Self {
            path: path.into(),
            contents: contents.into(),
            mode: 0,
        }
    }

    /// Builder: set the file mode.
    #[must_use]
    pub fn with_mode(mut self, mode: u32) -> Self {
        self.mode = mode;
        self
    }
}

/// Networking mode for the `RUN` steps of an image build (apt / npm install
/// and any package postinstall scripts).
///
/// The default (`Bridge`) is Docker's normal build networking — package
/// registries are reachable, which `apt-get install` / `npm install` need.
/// `None` cuts the build off the network entirely (a real, daemon-enforced
/// deny — useful for builds that copy in pre-fetched artifacts and must run no
/// network during install). The host's `install_packages` containment uses
/// `Bridge` (real registries needed) but pairs it with the broker-token denial
/// below, so a malicious postinstall has constrained — not unrestricted —
/// network and no broker capability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BuildNetwork {
    /// Docker's default build networking (bridge). Registries reachable.
    #[default]
    Bridge,
    /// No network during `RUN` steps (`--network=none`). A real deny.
    None,
    /// Host networking during build (`--network=host`). Reserved; not used by
    /// the default install — present so an operator with a private registry on
    /// the host loopback can opt in deliberately.
    Host,
}

impl BuildNetwork {
    /// The bollard `networkmode` string for this mode, or `None` to leave the
    /// daemon default (which is `Bridge`).
    #[must_use]
    pub fn networkmode(self) -> Option<&'static str> {
        match self {
            BuildNetwork::Bridge => None,
            BuildNetwork::None => Some("none"),
            BuildNetwork::Host => Some("host"),
        }
    }
}

/// Containment posture for an image build. The defaults keep the historical
/// behaviour (bridge network, no credentials in build-args). The host sets
/// `deny_broker_token` for `install_packages` builds so a malicious package
/// postinstall can never receive the per-session broker capability token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildContainment {
    /// Network mode for build `RUN` steps.
    pub network: BuildNetwork,
    /// When true, [`assert_no_credentials`] is enforced before the build is
    /// dispatched: the spec must carry NO build-arg or label that looks like a
    /// credential (broker token, provider API key, etc.). This is the
    /// structural guarantee that the broker token never reaches a package
    /// postinstall. Default true.
    pub deny_broker_token: bool,
}

impl Default for BuildContainment {
    fn default() -> Self {
        Self {
            network: BuildNetwork::Bridge,
            deny_broker_token: true,
        }
    }
}

/// Everything needed to render and tag a built image.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageBuildSpec {
    /// Image name prefix, e.g. `copperclaw/session`. The fingerprint is
    /// appended as the tag.
    pub repo: String,
    /// Base image (`FROM`).
    pub base_image: String,
    /// apt packages installed via `apt-get install -y`.
    pub apt_packages: Vec<String>,
    /// Global npm packages installed via `npm install -g`.
    pub npm_packages: Vec<String>,
    /// Extra files copied into the image.
    pub extra_files: Vec<ExtraFile>,
    /// Extra labels to apply (in addition to `copperclaw.fingerprint`).
    pub labels: Vec<(String, String)>,
    /// Build-args passed to the build. Deliberately NOT a credential channel:
    /// [`BuildContainment::deny_broker_token`] rejects any credential-shaped
    /// entry here. Kept so legitimate non-secret build-args (e.g. a mirror
    /// URL) remain possible.
    pub build_args: Vec<(String, String)>,
    /// Containment posture: build network mode + broker-token denial. NOT part
    /// of the [`Self::fingerprint`] (it's a runtime build posture, not
    /// Dockerfile content — two specs that differ only in containment still
    /// produce the same image), so adding it leaves existing tags unchanged.
    pub containment: BuildContainment,
}

/// Whether a build-arg / label name looks like a credential that must never be
/// threaded into an image build's environment (where a package postinstall
/// could read it). Case-insensitive substring match on the well-known secret
/// markers plus the copperclaw broker prefix.
#[must_use]
pub fn looks_like_credential(name: &str) -> bool {
    const MARKERS: &[&str] = &[
        "broker",
        "token",
        "api_key",
        "apikey",
        "secret",
        "password",
        "passwd",
        "anthropic",
        "credential",
        "bearer",
        "private_key",
    ];
    let lower = name.to_ascii_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}

/// Error returned when a build spec violates its containment policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainmentViolation {
    /// The offending build-arg / label name.
    pub name: String,
}

impl std::fmt::Display for ContainmentViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "credential-shaped build input `{}` is not permitted in a contained image build \
             (a package postinstall could read it); the broker token must never reach a build",
            self.name
        )
    }
}

impl std::error::Error for ContainmentViolation {}

impl ImageBuildSpec {
    /// Start a new spec.
    pub fn new(repo: impl Into<String>, base_image: impl Into<String>) -> Self {
        Self {
            repo: repo.into(),
            base_image: base_image.into(),
            ..Self::default()
        }
    }

    /// Sha256 of all inputs that affect the rendered Dockerfile.
    ///
    /// Stable across runs: order-sensitive for vectors (the caller is
    /// expected to keep order deterministic).
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"base=");
        hasher.update(self.base_image.as_bytes());
        hasher.update(b"\napt=");
        for p in &self.apt_packages {
            hasher.update(p.as_bytes());
            hasher.update(b",");
        }
        hasher.update(b"\nnpm=");
        for p in &self.npm_packages {
            hasher.update(p.as_bytes());
            hasher.update(b",");
        }
        hasher.update(b"\nfiles=");
        for f in &self.extra_files {
            hasher.update(f.path.to_string_lossy().as_bytes());
            hasher.update(b":");
            // Hash file content via its own digest so files of varying
            // size don't collide with each other when concatenated.
            let h = Sha256::digest(&f.contents);
            hasher.update(h);
            hasher.update(b":");
            hasher.update(f.mode.to_le_bytes());
            hasher.update(b",");
        }
        hasher.update(b"\nlabels=");
        for (k, v) in &self.labels {
            hasher.update(k.as_bytes());
            hasher.update(b"=");
            hasher.update(v.as_bytes());
            hasher.update(b",");
        }
        hex::encode(hasher.finalize())
    }

    /// Image reference (`<repo>:sha256-<fingerprint>`).
    #[must_use]
    pub fn image_tag(&self) -> String {
        format!("{}:sha256-{}", self.repo, self.fingerprint())
    }

    /// Builder: apply a containment posture (network mode + broker-token
    /// denial). Used by the host's `install_packages` path.
    #[must_use]
    pub fn with_containment(mut self, containment: BuildContainment) -> Self {
        self.containment = containment;
        self
    }

    /// Enforce the spec's containment policy. When
    /// [`BuildContainment::deny_broker_token`] is set, every build-arg name and
    /// label name is checked against [`looks_like_credential`]; the first match
    /// is a [`ContainmentViolation`]. This is the structural guarantee that the
    /// broker token / provider key never rides into a package build's env where
    /// a malicious postinstall could read it.
    ///
    /// Returns `Ok(())` when the spec is clean (the default install always is —
    /// the host never threads credentials into a build).
    pub fn assert_no_credentials(&self) -> Result<(), ContainmentViolation> {
        if !self.containment.deny_broker_token {
            return Ok(());
        }
        for (k, _) in &self.build_args {
            if looks_like_credential(k) {
                return Err(ContainmentViolation { name: k.clone() });
            }
        }
        for (k, _) in &self.labels {
            if looks_like_credential(k) {
                return Err(ContainmentViolation { name: k.clone() });
            }
        }
        Ok(())
    }

    /// Render the Dockerfile text.
    ///
    /// Output is deterministic for the same inputs, which makes it
    /// snapshot-testable.
    #[must_use]
    pub fn dockerfile(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("FROM {}\n", self.base_image));

        if !self.apt_packages.is_empty() {
            out.push_str(
                "RUN apt-get update \\\n \
                 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \\\n",
            );
            for pkg in &self.apt_packages {
                out.push_str(&format!("      {pkg} \\\n"));
            }
            out.push_str(" && rm -rf /var/lib/apt/lists/*\n");
        }

        if !self.npm_packages.is_empty() {
            out.push_str("RUN npm install -g \\\n");
            let last = self.npm_packages.len() - 1;
            for (i, pkg) in self.npm_packages.iter().enumerate() {
                if i == last {
                    out.push_str(&format!("      {pkg}\n"));
                } else {
                    out.push_str(&format!("      {pkg} \\\n"));
                }
            }
        }

        for f in &self.extra_files {
            // Files land in a build-context-relative directory named
            // `files/<index>`; the backend wires those up when packing
            // the build context. We just emit the COPY directive.
            let path = f.path.to_string_lossy();
            out.push_str(&format!("COPY {} {}\n", context_path_for(f), path));
            if f.mode != 0 {
                out.push_str(&format!("RUN chmod {:o} {}\n", f.mode, path));
            }
        }

        out.push_str(&format!(
            "LABEL copperclaw.fingerprint=\"{}\"\n",
            self.fingerprint()
        ));
        for (k, v) in &self.labels {
            out.push_str(&format!("LABEL {k}=\"{v}\"\n"));
        }
        out
    }
}

/// Stable in-context filename for an extra file: `files/<index>-<basename>`.
///
/// Kept pure so build-context assembly (per backend) and Dockerfile
/// rendering agree on names.
fn context_path_for(file: &ExtraFile) -> String {
    let basename = file
        .path
        .file_name()
        .map_or_else(|| "file".to_string(), |s| s.to_string_lossy().into_owned());
    format!("files/{basename}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_file_with_mode() {
        let f = ExtraFile::new("/etc/foo", "x").with_mode(0o755);
        assert_eq!(f.path, PathBuf::from("/etc/foo"));
        assert_eq!(f.contents, b"x");
        assert_eq!(f.mode, 0o755);
    }

    #[test]
    fn fingerprint_is_stable() {
        let a = ImageBuildSpec::new("copperclaw/session", "debian:12-slim");
        let b = ImageBuildSpec::new("copperclaw/session", "debian:12-slim");
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn fingerprint_changes_with_apt_packages() {
        let mut a = ImageBuildSpec::new("r", "b");
        a.apt_packages = vec!["git".into()];
        let mut b = ImageBuildSpec::new("r", "b");
        b.apt_packages = vec!["curl".into()];
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn fingerprint_changes_with_npm_packages() {
        let mut a = ImageBuildSpec::new("r", "b");
        a.npm_packages = vec!["typescript".into()];
        let mut b = ImageBuildSpec::new("r", "b");
        b.npm_packages = vec!["ts-node".into()];
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn fingerprint_changes_with_extra_file_contents() {
        let mut a = ImageBuildSpec::new("r", "b");
        a.extra_files = vec![ExtraFile::new("/etc/x", "v1")];
        let mut b = ImageBuildSpec::new("r", "b");
        b.extra_files = vec![ExtraFile::new("/etc/x", "v2")];
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn fingerprint_changes_with_labels() {
        let mut a = ImageBuildSpec::new("r", "b");
        a.labels = vec![("k".into(), "v1".into())];
        let mut b = ImageBuildSpec::new("r", "b");
        b.labels = vec![("k".into(), "v2".into())];
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn image_tag_uses_repo_and_fingerprint() {
        let spec = ImageBuildSpec::new("copperclaw/session", "debian:12-slim");
        let tag = spec.image_tag();
        assert!(tag.starts_with("copperclaw/session:sha256-"));
        assert!(tag.ends_with(&spec.fingerprint()));
    }

    // ── install_packages containment ───────────────────────────────

    #[test]
    fn default_containment_denies_broker_token_and_uses_bridge() {
        let c = BuildContainment::default();
        assert!(c.deny_broker_token);
        assert_eq!(c.network, BuildNetwork::Bridge);
        assert_eq!(c.network.networkmode(), None);
    }

    #[test]
    fn build_network_modes_map_to_docker_strings() {
        assert_eq!(BuildNetwork::Bridge.networkmode(), None);
        assert_eq!(BuildNetwork::None.networkmode(), Some("none"));
        assert_eq!(BuildNetwork::Host.networkmode(), Some("host"));
    }

    #[test]
    fn looks_like_credential_catches_secret_markers() {
        for name in [
            "ANTHROPIC_API_KEY",
            "BROKER_TOKEN",
            "X_TOKEN",
            "my_secret",
            "DB_PASSWORD",
            "Bearer",
            "service_credential",
            "PRIVATE_KEY",
            "apikey",
        ] {
            assert!(looks_like_credential(name), "{name} should be flagged");
        }
        for name in ["MIRROR_URL", "DEBIAN_FRONTEND", "TZ", "LANG", "PROXY_HOST"] {
            assert!(!looks_like_credential(name), "{name} should be allowed");
        }
    }

    #[test]
    fn assert_no_credentials_passes_for_clean_spec() {
        let spec = ImageBuildSpec::new("r", "debian:12-slim");
        // Default containment denies broker token; a clean spec is fine.
        assert!(spec.assert_no_credentials().is_ok());
    }

    #[test]
    fn assert_no_credentials_rejects_credential_build_arg() {
        let mut spec = ImageBuildSpec::new("r", "debian:12-slim");
        spec.build_args = vec![("BROKER_TOKEN".into(), "cct1.abc".into())];
        let err = spec.assert_no_credentials().unwrap_err();
        assert_eq!(err.name, "BROKER_TOKEN");
        assert!(
            err.to_string()
                .contains("broker token must never reach a build")
        );
    }

    #[test]
    fn assert_no_credentials_rejects_credential_label() {
        let mut spec = ImageBuildSpec::new("r", "debian:12-slim");
        spec.labels = vec![("anthropic_api_key".into(), "sk-x".into())];
        assert!(spec.assert_no_credentials().is_err());
    }

    #[test]
    fn assert_no_credentials_allows_non_secret_build_arg() {
        let mut spec = ImageBuildSpec::new("r", "debian:12-slim");
        spec.build_args = vec![("MIRROR_URL".into(), "http://mirror".into())];
        assert!(spec.assert_no_credentials().is_ok());
    }

    #[test]
    fn disabling_deny_broker_token_skips_the_guard() {
        // Opt-out is possible but deliberate — the default never disables it.
        let mut spec = ImageBuildSpec::new("r", "debian:12-slim");
        spec.build_args = vec![("BROKER_TOKEN".into(), "x".into())];
        spec.containment.deny_broker_token = false;
        assert!(spec.assert_no_credentials().is_ok());
    }

    #[test]
    fn containment_does_not_change_fingerprint_or_tag() {
        // Containment is a runtime build posture, not Dockerfile content — two
        // specs differing only in containment must map to the same image tag.
        let mut a = ImageBuildSpec::new("r", "debian:12-slim");
        a.apt_packages = vec!["git".into()];
        let mut b = a.clone();
        b.containment = BuildContainment {
            network: BuildNetwork::None,
            deny_broker_token: true,
        };
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.image_tag(), b.image_tag());
    }

    #[test]
    fn with_containment_builder_sets_posture() {
        let spec = ImageBuildSpec::new("r", "b").with_containment(BuildContainment {
            network: BuildNetwork::None,
            deny_broker_token: true,
        });
        assert_eq!(spec.containment.network, BuildNetwork::None);
    }

    #[test]
    fn dockerfile_empty_is_just_from_and_label() {
        let spec = ImageBuildSpec::new("r", "debian:12-slim");
        let df = spec.dockerfile();
        assert!(df.contains("FROM debian:12-slim"));
        assert!(!df.contains("apt-get"));
        assert!(!df.contains("npm install"));
        assert!(!df.contains("COPY "));
        assert!(df.contains("LABEL copperclaw.fingerprint="));
    }

    #[test]
    fn dockerfile_apt_only() {
        let mut spec = ImageBuildSpec::new("r", "debian:12-slim");
        spec.apt_packages = vec!["git".into(), "curl".into()];
        let df = spec.dockerfile();
        assert!(df.contains("apt-get update"));
        assert!(df.contains("apt-get install -y"));
        assert!(df.contains("git"));
        assert!(df.contains("curl"));
        assert!(df.contains("rm -rf /var/lib/apt/lists/*"));
        assert!(!df.contains("npm install"));
    }

    #[test]
    fn dockerfile_npm_only() {
        let mut spec = ImageBuildSpec::new("r", "node:22-bookworm-slim");
        spec.npm_packages = vec!["typescript".into(), "ts-node".into()];
        let df = spec.dockerfile();
        assert!(!df.contains("apt-get"));
        assert!(df.contains("npm install -g"));
        assert!(df.contains("typescript"));
        assert!(df.contains("ts-node"));
    }

    #[test]
    fn dockerfile_apt_plus_npm() {
        let mut spec = ImageBuildSpec::new("r", "debian:12-slim");
        spec.apt_packages = vec!["git".into()];
        spec.npm_packages = vec!["ts-node".into()];
        let df = spec.dockerfile();
        // apt block precedes npm block
        let apt_at = df.find("apt-get install").expect("apt block");
        let npm_at = df.find("npm install -g").expect("npm block");
        assert!(apt_at < npm_at);
    }

    #[test]
    fn dockerfile_extra_files_emits_copy_and_chmod() {
        let mut spec = ImageBuildSpec::new("r", "debian:12-slim");
        spec.extra_files = vec![
            ExtraFile::new("/etc/foo.conf", "k=v"),
            ExtraFile::new("/usr/local/bin/run.sh", "#!/bin/sh\necho hi\n").with_mode(0o755),
        ];
        let df = spec.dockerfile();
        assert!(df.contains("COPY files/foo.conf /etc/foo.conf"));
        assert!(df.contains("COPY files/run.sh /usr/local/bin/run.sh"));
        assert!(df.contains("RUN chmod 755 /usr/local/bin/run.sh"));
        // No chmod for the file that left mode at 0.
        assert!(!df.contains("RUN chmod 0 /etc/foo.conf"));
    }

    #[test]
    fn dockerfile_renders_labels() {
        let mut spec = ImageBuildSpec::new("r", "b");
        spec.labels = vec![("owner".into(), "copperclaw".into())];
        let df = spec.dockerfile();
        assert!(df.contains("LABEL copperclaw.fingerprint="));
        assert!(df.contains("LABEL owner=\"copperclaw\""));
    }

    #[test]
    fn context_path_handles_missing_basename() {
        // Files without a basename component land under `files/file`.
        let f = ExtraFile::new("/", "");
        assert_eq!(context_path_for(&f), "files/file");
    }

    #[test]
    fn context_path_uses_basename() {
        let f = ExtraFile::new("/etc/foo/bar.conf", "");
        assert_eq!(context_path_for(&f), "files/bar.conf");
    }
}
