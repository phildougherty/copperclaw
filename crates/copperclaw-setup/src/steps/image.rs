//! Step 4 — container image build.
//!
//! Calls [`copperclaw_container_rt::detect`] to pick a runtime then asks it
//! to build a minimal image. The detection call is async; we drive it on
//! the current Tokio runtime via [`tokio::task::block_in_place`] so the
//! synchronous [`Step`] trait stays simple.
//!
//! Before falling through to a local build the step attempts to pull a
//! pre-published image from GHCR — this collapses cold-start from 1-2
//! minutes (full `docker build` of a Debian-slim layer) to ~10s for the
//! common case. Set `COPPERCLAW_SETUP_NO_PULL=1` to skip the pull attempt
//! entirely (useful for reproducible local builds or air-gapped hosts).

use crate::config::SetupConfig;
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::{Step, StepError, StepResult};
use copperclaw_container_rt::{ExtraFile, ImageBuildSpec};
use copperclaw_types::{ImageProfile, PinnedBinary, PinnedBinaryTarget};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

/// Repo used for the host's session base image.
pub const DEFAULT_REPO: &str = "copperclaw/session";

/// Default base image used for the container build step.
///
/// `trixie` (Debian 13) ships glibc 2.41, which covers any
/// reasonably-recent host build of `copperclaw-runner`. `bookworm`
/// (Debian 12) has glibc 2.36, which is too old for runners built
/// against glibc 2.39+ — the symptom is a `version GLIBC_2.39 not
/// found` error on container start.
pub const DEFAULT_BASE_IMAGE: &str = "debian:trixie-slim";

/// GHCR repository the CI workflow publishes the session base image to.
///
/// TODO(team-c): once the repository slug is configurable per fork or
/// per organisation, accept this via an env var or setup-config field
/// rather than hardcoding the upstream slug.
pub const DEFAULT_PULL_REGISTRY: &str = "ghcr.io/phildougherty/copperclaw/session";

/// Env var that disables the pre-pull entirely. Truthy = skip.
pub const ENV_NO_PULL: &str = "COPPERCLAW_SETUP_NO_PULL";

/// Env var that overrides the pull registry slug.
pub const ENV_PULL_REGISTRY: &str = "COPPERCLAW_SETUP_PULL_REGISTRY";

/// Docker label the published image carries, mirroring the LABEL in
/// `container/Dockerfile`. Used to verify the pulled image matches
/// the locally-expected fingerprint before adopting it.
pub const FINGERPRINT_LABEL: &str = "copperclaw.fingerprint";

/// Timeout for the `docker pull` attempt. Kept short so an offline
/// host or a slow registry falls back to a local build quickly.
pub const PULL_TIMEOUT: Duration = Duration::from_secs(60);

/// Step implementation.
#[derive(Debug, Default)]
pub struct ImageBuildStep;

impl Step for ImageBuildStep {
    fn name(&self) -> &'static str {
        "image"
    }

    fn description(&self) -> &'static str {
        "Build the copperclaw container image"
    }

    fn run(
        &self,
        cfg: &mut SetupConfig,
        prompt: &dyn Prompt,
        _state: &mut SetupState,
    ) -> Result<StepResult, StepError> {
        let opt_in = prompt.confirm("BUILD_IMAGE", "Build the container image now?", true)?;
        if !opt_in {
            return Ok(StepResult::noop(
                "skipping container image build (user declined)",
            ));
        }
        if !cfg.env_report.has_container_runtime() {
            return Ok(StepResult::noop(
                "no container runtime detected on PATH; skipping image build",
            ));
        }

        // Ask once which toolchain profile to bake into the base image.
        // Default `minimal` (secure-by-default tenet); `prototyping` adds the
        // warm web-prototyping bundle. Persisted on `cfg` so a re-run is
        // idempotent — the same answer produces the same fingerprint/tag and
        // the build is a no-op.
        let profile = resolve_image_profile(prompt)?;
        cfg.image_profile = profile.as_str().to_string();

        let mut spec = default_spec(profile)?;
        // M20 Q1: bake any pinned prebuilt binaries the profile needs (today
        // just `ruff` — no trixie apt package exists for it, verified at
        // branch time; see `copperclaw_types::image::RUFF_PINNED_BINARY`).
        // No-op for `Minimal` (empty list) and for a `Prototyping` re-run
        // once the fetch is cached, so this stays idempotent.
        bake_pinned_binaries(
            &mut spec,
            profile,
            &RealPinnedBinaryFetcher,
            &pinned_binary_cache_dir(&cfg.data_dir),
        )?;
        let target_tag = spec.image_tag();
        let fingerprint = spec.fingerprint();
        let mut messages = Vec::new();

        // Attempt the pre-pull unless the operator opted out.
        let docker = RealDockerCli;
        match try_pull(&docker, &fingerprint, &target_tag, &resolve_pull_registry()) {
            PullOutcome::Adopted => {
                messages.push(format!(
                    "pulled pre-built image from registry: {target_tag}"
                ));
                cfg.image_tag.clone_from(&target_tag);
                return Ok(StepResult {
                    messages,
                    config_changed: true,
                });
            }
            PullOutcome::Skipped(reason) => {
                messages.push(format!("skipping registry pull: {reason}"));
            }
            PullOutcome::Failed(reason) => {
                messages.push(format!("pulling failed, building locally: {reason}"));
            }
        }

        let outcome = run_build(&spec)?;
        cfg.image_tag.clone_from(&outcome.tag);
        let verb = if outcome.was_cached {
            "reused"
        } else {
            "built"
        };
        messages.push(format!("{verb} image: {}", outcome.tag));
        Ok(StepResult {
            messages,
            config_changed: true,
        })
    }
}

/// Result of [`run_build`]. `was_cached` is `true` when the image's tag
/// already existed in the runtime's local store before the build call —
/// which means the build was a near-instant no-op rather than a real
/// `docker build`. Used to produce a more honest setup message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOutcome {
    /// Final image tag (sha256-pinned via `ImageBuildSpec::image_tag`).
    pub tag: String,
    /// Whether the tag was already present before the build call.
    pub was_cached: bool,
}

/// Path inside the image where the runner binary lives.
pub const RUNNER_PATH_IN_IMAGE: &str = "/usr/local/bin/copperclaw-runner";

/// Default minimal image spec used by the setup binary.
///
/// The runner binary is COPY'd into the image as `/usr/local/bin/copperclaw-runner`
/// so the host's container manager can `exec` it on spawn. If the
/// runner sibling cannot be located (e.g. setup is being run before
/// the workspace has been built), the step returns an error rather
/// than producing a broken image.
pub fn default_spec(profile: ImageProfile) -> Result<ImageBuildSpec, StepError> {
    let mut spec = ImageBuildSpec::new(DEFAULT_REPO, DEFAULT_BASE_IMAGE);
    // Bake the chosen image profile. `Minimal` adds nothing; `Prototyping`
    // renders the warm apt + npm bundle on top of the baseline packages, so
    // groups that opt into prototyping boot on a warm base with no per-group
    // rebuild.
    spec.image_profile = profile;
    // Baseline runtime layer: every session agent can run Python or
    // Node code it writes, hit HTTP endpoints, and clone repos out of
    // the box. Without these the agent confabulates "production-ready"
    // because it has no way to actually exercise what it produced —
    // there's no python3, no node, no curl, no git. Adds ~500MB to
    // the image but the alternative is per-spawn `install_packages`
    // churn that takes minutes per cold start.
    spec.apt_packages = DEFAULT_BASE_APT_PACKAGES
        .iter()
        .copied()
        .map(String::from)
        .collect();
    let runner_path = locate_runner_binary()?;
    let bytes = std::fs::read(&runner_path).map_err(|e| {
        StepError::Other(format!(
            "read runner binary at {}: {e}",
            runner_path.display()
        ))
    })?;
    spec.extra_files
        .push(ExtraFile::new(PathBuf::from(RUNNER_PATH_IN_IMAGE), bytes).with_mode(0o755));
    Ok(spec)
}

/// Cache directory the pinned-binary fetch (M20 Q1) keys its downloads
/// under, so re-running setup against the same install doesn't re-fetch a
/// tarball it already verified. Content-addressed by the cache-key inside
/// [`fetch_pinned_binary`] (name + version + arch + sha256), so a version
/// bump in `copperclaw_types::image` naturally invalidates the old entry
/// rather than silently reusing stale bytes.
pub fn pinned_binary_cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("cache").join("pinned-binaries")
}

/// Thin trait over the external commands needed to fetch, verify, and
/// unpack a [`PinnedBinary`]'s release tarball. Mirrors [`DockerCli`]
/// below: the real implementation shells out to `curl` / `sha256sum` /
/// `tar` (all already required on any box that can run `install.sh`, which
/// fetches + verifies + unpacks release tarballs the same way); tests
/// inject a stub so no test hits the network.
pub trait PinnedBinaryFetcher {
    /// Download `url` to `dest`.
    fn download(&self, url: &str, dest: &Path) -> Result<(), String>;
    /// Verify `path`'s sha256 digest equals `sha256_hex` (case-insensitive).
    fn verify_sha256(&self, path: &Path, sha256_hex: &str) -> Result<(), String>;
    /// Extract a `.tar.gz` archive at `archive` into `dest_dir`.
    fn extract_tar_gz(&self, archive: &Path, dest_dir: &Path) -> Result<(), String>;
}

/// Real-process implementation. Calls `curl`, `sha256sum`, and `tar` from
/// `PATH` — the same tools `install.sh` already depends on for its own
/// release-tarball fetch, so no new host prerequisite is introduced.
struct RealPinnedBinaryFetcher;

impl PinnedBinaryFetcher for RealPinnedBinaryFetcher {
    fn download(&self, url: &str, dest: &Path) -> Result<(), String> {
        let out = Command::new("curl")
            .args(["-fsSL", "--retry", "2", "-o"])
            .arg(dest)
            .arg(url)
            .output()
            .map_err(|e| format!("spawn curl: {e}"))?;
        check_status(&out, "curl download")
    }

    fn verify_sha256(&self, path: &Path, sha256_hex: &str) -> Result<(), String> {
        let out = Command::new("sha256sum")
            .arg(path)
            .output()
            .map_err(|e| format!("spawn sha256sum: {e}"))?;
        check_status(&out, "sha256sum")?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let digest = stdout.split_whitespace().next().unwrap_or("");
        if digest.eq_ignore_ascii_case(sha256_hex) {
            Ok(())
        } else {
            Err(format!(
                "checksum mismatch: expected {sha256_hex}, got {digest}"
            ))
        }
    }

    fn extract_tar_gz(&self, archive: &Path, dest_dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dest_dir)
            .map_err(|e| format!("mkdir {}: {e}", dest_dir.display()))?;
        let out = Command::new("tar")
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(dest_dir)
            .output()
            .map_err(|e| format!("spawn tar: {e}"))?;
        check_status(&out, "tar extract")
    }
}

/// Download, verify, and extract one [`PinnedBinaryTarget`] via `fetcher`,
/// returning the extracted binary's bytes. Isolated from
/// [`fetch_pinned_binary`]'s caching/cleanup so it stays independently
/// testable.
fn fetch_and_extract(
    fetcher: &dyn PinnedBinaryFetcher,
    target: &PinnedBinaryTarget,
    binary_name: &str,
    scratch: &Path,
) -> Result<Vec<u8>, StepError> {
    let archive_path = scratch.join(format!("{binary_name}.tar.gz"));
    fetcher
        .download(target.url, &archive_path)
        .map_err(|e| StepError::Other(format!("download {}: {e}", target.url)))?;
    fetcher
        .verify_sha256(&archive_path, target.sha256)
        .map_err(|e| StepError::Other(format!("verify {binary_name} checksum: {e}")))?;
    let extract_dir = scratch.join("extracted");
    fetcher
        .extract_tar_gz(&archive_path, &extract_dir)
        .map_err(|e| StepError::Other(format!("extract {binary_name}: {e}")))?;
    let member_path = extract_dir.join(target.archive_member);
    std::fs::read(&member_path).map_err(|e| {
        StepError::Other(format!(
            "read extracted {binary_name} at {}: {e}",
            member_path.display()
        ))
    })
}

/// Fetch (or read from cache), verify, and unpack a [`PinnedBinary`] for
/// the current host architecture, returning its raw executable bytes ready
/// to embed as an [`ExtraFile`].
///
/// The target architecture is `std::env::consts::ARCH` — the machine
/// running `copperclaw-setup`, which is also the machine `docker build`
/// runs on, so this matches the image's actual architecture for a local
/// (non-cross-arch) build. Caches the verified bytes under `cache_dir`
/// keyed by name + version + arch + sha256 so re-running setup (idempotent
/// per the E2/Q1 precedent) doesn't re-download every time; bumping
/// [`PinnedBinary::version`] (or its sha256) naturally invalidates the old
/// cache entry rather than silently reusing stale bytes.
pub fn fetch_pinned_binary(
    fetcher: &dyn PinnedBinaryFetcher,
    cache_dir: &Path,
    binary: &PinnedBinary,
) -> Result<Vec<u8>, StepError> {
    let arch = std::env::consts::ARCH;
    let target = binary
        .targets
        .iter()
        .find(|t| t.arch == arch)
        .ok_or_else(|| {
            StepError::Other(format!(
                "no pinned `{}` build for host architecture `{arch}`",
                binary.name
            ))
        })?;

    let cache_path = cache_dir.join(format!(
        "{}-{}-{arch}-{}",
        binary.name, binary.version, target.sha256
    ));
    if let Ok(bytes) = std::fs::read(&cache_path) {
        return Ok(bytes);
    }

    std::fs::create_dir_all(cache_dir).map_err(|e| {
        StepError::Other(format!(
            "create pinned-binary cache dir {}: {e}",
            cache_dir.display()
        ))
    })?;

    // Scratch dir for this one fetch, cleaned up on every exit path
    // (success or failure) so a crashed prior run never leaves stale
    // partial state that a retry could misread.
    let scratch = cache_dir.join(format!(".fetch-{}-{}", std::process::id(), binary.name));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch)
        .map_err(|e| StepError::Other(format!("create scratch dir {}: {e}", scratch.display())))?;

    let result = fetch_and_extract(fetcher, target, binary.name, &scratch);
    let _ = std::fs::remove_dir_all(&scratch);
    let bytes = result?;

    // Best-effort cache write: a failure here just means the next run
    // re-fetches; it must never fail the build over cache I/O.
    let _ = std::fs::write(&cache_path, &bytes);
    Ok(bytes)
}

/// Fold `profile`'s pinned-binary bundle (M20 Q1 — today just `ruff`) into
/// `spec.extra_files`, fetching + verifying + unpacking each one via
/// `fetcher`. No-op for a profile with none (`Minimal` today).
///
/// Each embedded file participates in `ImageBuildSpec::fingerprint()`
/// exactly like the runner binary and any other `extra_files` entry
/// already do, so the base image's tag changes when a pinned binary's
/// pinned version changes — no separate fingerprint plumbing needed here.
pub fn bake_pinned_binaries(
    spec: &mut ImageBuildSpec,
    profile: ImageProfile,
    fetcher: &dyn PinnedBinaryFetcher,
    cache_dir: &Path,
) -> Result<(), StepError> {
    for binary in profile.extra_pinned_binaries() {
        let bytes = fetch_pinned_binary(fetcher, cache_dir, binary)?;
        spec.extra_files
            .push(ExtraFile::new(PathBuf::from(binary.dest_path), bytes).with_mode(0o755));
    }
    Ok(())
}

/// Prompt key the base-image profile question is asked under
/// (`COPPERCLAW_SETUP_IMAGE_PROFILE` in headless mode).
pub const IMAGE_PROFILE_KEY: &str = "IMAGE_PROFILE";

/// Ask (once) which toolchain profile to bake into the base image.
///
/// Defaults to [`ImageProfile::Minimal`] (secure-by-default tenet). An
/// unknown answer degrades to `minimal` rather than erroring — a typo must
/// never silently produce the *heavier* image. Idempotent: the same answer
/// always maps to the same profile.
pub fn resolve_image_profile(prompt: &dyn Prompt) -> Result<ImageProfile, StepError> {
    let answer = prompt
        .input(
            IMAGE_PROFILE_KEY,
            "Base image profile (minimal | prototyping)",
            Some(ImageProfile::Minimal.as_str()),
        )
        .map_err(|e| StepError::Other(format!("image profile prompt: {e}")))?;
    Ok(ImageProfile::parse(answer.trim()).unwrap_or(ImageProfile::Minimal))
}

/// Apt packages installed in every session image by default. The list
/// is deliberately conservative: language runtimes the agent will
/// reach for, plus the network + build tools they need to be useful.
/// Operators can extend per-group via `cclaw groups config
/// add-package`. Heavyweight things (clang, rustc, postgres-client,
/// docker.io) intentionally NOT included — those belong in per-group
/// add-ons.
pub const DEFAULT_BASE_APT_PACKAGES: &[&str] = &[
    // Network + cert basics.
    "ca-certificates",
    "curl",
    "wget",
    // Source control.
    "git",
    // Python runtime + package management + venv support for safe
    // per-session installs without polluting the system-wide site.
    "python3",
    "python3-pip",
    "python3-venv",
    // Node runtime + npm. Pre-installed so the agent doesn't reach
    // for `install_packages` and trigger a full image rebuild before
    // it can `node app.js`.
    "nodejs",
    "npm",
    // GNU build chain — required by many pip installs that compile
    // C extensions on first use. Heavier than the rest but ubiquitous.
    "build-essential",
    // Useful diagnostic / inspection tools the agent reaches for when
    // troubleshooting its own output.
    "jq",
    "less",
    "procps",
    // Code navigation: ripgrep for fast content search, universal-ctags
    // for cross-language definition lookup. Containers have no
    // Debian-repo egress at runtime (`apt-get update` exits 100), so
    // these must ship in the baseline rather than install on demand.
    "ripgrep",
    "universal-ctags",
];

/// Find the `copperclaw-runner` binary that should be baked into the
/// session image.
///
/// Resolution order:
/// 1. `COPPERCLAW_RUNNER_BIN` env var — explicit override (CI, packaging).
/// 2. Sibling of the currently running executable
///    (`std::env::current_exe()` parent + `copperclaw-runner`).
/// 3. Anywhere on `PATH` — last resort, useful when copperclaw-setup is
///    installed system-wide.
pub fn locate_runner_binary() -> Result<PathBuf, StepError> {
    if let Some(explicit) = std::env::var_os("COPPERCLAW_RUNNER_BIN") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Ok(p);
        }
        return Err(StepError::Other(format!(
            "COPPERCLAW_RUNNER_BIN points at {} which does not exist",
            p.display()
        )));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let candidate = parent.join("copperclaw-runner");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    if let Some(p) = which_runner() {
        return Ok(p);
    }
    Err(StepError::Other(
        "could not locate `copperclaw-runner` binary — \
         set COPPERCLAW_RUNNER_BIN or place it next to copperclaw-setup"
            .into(),
    ))
}

fn which_runner() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("copperclaw-runner");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[allow(dead_code)]
fn _path_unused(_: &Path) {}

/// Outcome of the optional pre-build pull attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullOutcome {
    /// Image was pulled, its label matched, and it was re-tagged
    /// locally as the fingerprint tag. The local build can be skipped.
    Adopted,
    /// Pull wasn't attempted (env opt-out, etc.). Fall through to build.
    Skipped(String),
    /// Pull attempted but failed (network, 404, fingerprint mismatch).
    /// Fall through to build.
    Failed(String),
}

/// Thin trait over the docker CLI so the pull path can be unit-tested
/// without spawning real processes.
pub trait DockerCli {
    /// Pull the supplied reference. Returns Ok on success.
    fn pull(&self, reference: &str, timeout: Duration) -> Result<(), String>;
    /// Return the value of the supplied label, or None if missing.
    fn label(&self, reference: &str, label: &str) -> Result<Option<String>, String>;
    /// Apply an extra tag to an image.
    fn tag(&self, source: &str, target: &str) -> Result<(), String>;
}

/// Real-process docker CLI implementation. Calls `docker` from PATH.
struct RealDockerCli;

impl DockerCli for RealDockerCli {
    fn pull(&self, reference: &str, _timeout: Duration) -> Result<(), String> {
        // `timeout` is advisory for the trait contract; we shell out to
        // `docker pull` which has no built-in deadline. The buildx
        // version supports `--progress=plain`; we keep flags minimal so
        // the call works across docker-cli versions.
        let out = Command::new("docker")
            .arg("pull")
            .arg(reference)
            .output()
            .map_err(|e| format!("spawn docker pull: {e}"))?;
        check_status(&out, "docker pull")
    }

    fn label(&self, reference: &str, label: &str) -> Result<Option<String>, String> {
        // `docker inspect -f` keeps the output to a single line so we
        // can compare it directly. Missing label expands to `<no value>`.
        let format_arg = format!("{{{{ index .Config.Labels \"{label}\" }}}}");
        let out = Command::new("docker")
            .arg("inspect")
            .arg("-f")
            .arg(format_arg)
            .arg(reference)
            .output()
            .map_err(|e| format!("spawn docker inspect: {e}"))?;
        check_status(&out, "docker inspect")?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() || s == "<no value>" {
            Ok(None)
        } else {
            Ok(Some(s))
        }
    }

    fn tag(&self, source: &str, target: &str) -> Result<(), String> {
        let out = Command::new("docker")
            .arg("tag")
            .arg(source)
            .arg(target)
            .output()
            .map_err(|e| format!("spawn docker tag: {e}"))?;
        check_status(&out, "docker tag")
    }
}

fn check_status(out: &Output, what: &str) -> Result<(), String> {
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{what} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Resolve the registry slug to attempt pulls from. Honours
/// `COPPERCLAW_SETUP_PULL_REGISTRY` then falls back to the default.
pub fn resolve_pull_registry() -> String {
    std::env::var(ENV_PULL_REGISTRY).unwrap_or_else(|_| DEFAULT_PULL_REGISTRY.to_string())
}

/// Attempt to pull `<registry>:sha256-<fingerprint>`, verify its label,
/// and tag it locally as `target_tag`.
///
/// Pure of the docker CLI via the [`DockerCli`] trait so tests can
/// inject a stub that returns canned outcomes.
pub fn try_pull(
    docker: &dyn DockerCli,
    fingerprint: &str,
    target_tag: &str,
    registry: &str,
) -> PullOutcome {
    if env_truthy(ENV_NO_PULL) {
        return PullOutcome::Skipped(format!("{ENV_NO_PULL} is set"));
    }
    let remote_ref = format!("{registry}:sha256-{fingerprint}");
    if let Err(e) = docker.pull(&remote_ref, PULL_TIMEOUT) {
        return PullOutcome::Failed(format!("docker pull {remote_ref}: {e}"));
    }
    let label = match docker.label(&remote_ref, FINGERPRINT_LABEL) {
        Ok(v) => v,
        Err(e) => return PullOutcome::Failed(format!("label inspect: {e}")),
    };
    match label {
        Some(v) if v == fingerprint => {}
        Some(other) => {
            return PullOutcome::Failed(format!(
                "fingerprint mismatch: expected {fingerprint}, image carried {other}"
            ));
        }
        None => {
            return PullOutcome::Failed(format!(
                "pulled image is missing the {FINGERPRINT_LABEL} label"
            ));
        }
    }
    if let Err(e) = docker.tag(&remote_ref, target_tag) {
        return PullOutcome::Failed(format!("docker tag: {e}"));
    }
    PullOutcome::Adopted
}

/// `true` if the named env var is set to a truthy value.
fn env_truthy(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Drive the async build on the current Tokio runtime, returning the
/// resulting image tag (or a friendly error when no runtime is reachable).
///
/// The runtime is asked whether the target tag already exists before the
/// build kicks off so the caller can distinguish "first install, real
/// `docker build`" from "re-running setup on a hash-stable spec".
pub fn run_build(spec: &ImageBuildSpec) -> Result<BuildOutcome, StepError> {
    let spec = spec.clone();
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| StepError::Other("no Tokio runtime available for image build".into()))?;
    tokio::task::block_in_place(|| {
        handle.block_on(async move {
            let rt = copperclaw_container_rt::detect()
                .await
                .map_err(|e| StepError::Other(format!("detect runtime: {e}")))?;
            let target_tag = spec.image_tag();
            // `image_exists` is best-effort; treat a probe failure the same
            // as "not present" rather than aborting the build.
            let was_cached = rt.image_exists(&target_tag).await.unwrap_or(false);
            let tag = rt
                .build_image(spec)
                .await
                .map_err(|e| StepError::Other(format!("build image: {e}")))?;
            Ok(BuildOutcome { tag, was_cached })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;
    use copperclaw_container_rt::ImageBuildSpec;
    use std::sync::Mutex;

    /// `default_spec` resolves the runner binary by sibling/path lookup
    /// which isn't guaranteed in a sandboxed test runner. Build a
    /// dummy `ExtraFile` inline so the per-field assertions don't
    /// depend on the host's environment.
    fn fake_default_spec() -> ImageBuildSpec {
        let mut spec = ImageBuildSpec::new(DEFAULT_REPO, DEFAULT_BASE_IMAGE);
        spec.extra_files.push(
            ExtraFile::new(
                std::path::PathBuf::from(RUNNER_PATH_IN_IMAGE),
                b"fake-runner".to_vec(),
            )
            .with_mode(0o755),
        );
        spec
    }

    /// Trait-only mock that records calls and replays canned responses.
    struct StubDocker {
        pull_result: Mutex<Result<(), String>>,
        label_result: Mutex<Result<Option<String>, String>>,
        tag_result: Mutex<Result<(), String>>,
        calls: Mutex<Vec<String>>,
    }

    impl StubDocker {
        fn new() -> Self {
            Self {
                pull_result: Mutex::new(Ok(())),
                label_result: Mutex::new(Ok(None)),
                tag_result: Mutex::new(Ok(())),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn with_pull(self, r: Result<(), String>) -> Self {
            *self.pull_result.lock().unwrap() = r;
            self
        }

        fn with_label(self, r: Result<Option<String>, String>) -> Self {
            *self.label_result.lock().unwrap() = r;
            self
        }

        fn with_tag(self, r: Result<(), String>) -> Self {
            *self.tag_result.lock().unwrap() = r;
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl DockerCli for StubDocker {
        fn pull(&self, reference: &str, _timeout: Duration) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("pull {reference}"));
            self.pull_result.lock().unwrap().clone()
        }

        fn label(&self, reference: &str, label: &str) -> Result<Option<String>, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("label {reference} {label}"));
            self.label_result.lock().unwrap().clone()
        }

        fn tag(&self, source: &str, target: &str) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("tag {source} {target}"));
            self.tag_result.lock().unwrap().clone()
        }
    }

    #[test]
    fn fake_default_spec_matches_repo_and_base() {
        let spec = fake_default_spec();
        assert_eq!(spec.repo, DEFAULT_REPO);
        assert_eq!(spec.base_image, DEFAULT_BASE_IMAGE);
    }

    #[test]
    fn fake_default_spec_image_tag_is_stable() {
        let a = fake_default_spec();
        let b = fake_default_spec();
        assert_eq!(a.image_tag(), b.image_tag());
    }

    #[test]
    fn fake_default_spec_includes_runner_binary() {
        let spec = fake_default_spec();
        let found = spec
            .extra_files
            .iter()
            .find(|f| f.path == std::path::PathBuf::from(RUNNER_PATH_IN_IMAGE));
        let Some(f) = found else {
            panic!("expected runner ExtraFile, got {:?}", spec.extra_files);
        };
        assert_eq!(f.mode, 0o755);
        assert!(!f.contents.is_empty());
    }

    // Note: locate_runner_binary's env-override path is exercised
    // indirectly by integration tests that run the actual setup
    // binary; mutating std::env inside a unit test is unsafe under
    // edition 2024 and the workspace forbids `unsafe`.

    #[test]
    fn step_metadata() {
        let s = ImageBuildStep;
        assert_eq!(s.name(), "image");
        assert!(!s.description().is_empty());
        assert!(s.is_skippable());
    }

    #[test]
    fn step_skips_when_user_declines() {
        let s = ImageBuildStep;
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("BUILD_IMAGE", "no");
        let res = s.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(!res.config_changed);
    }

    #[test]
    fn step_skips_when_no_runtime() {
        let s = ImageBuildStep;
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        // env_report.has_container_runtime() is false by default.
        let prompt = Scripted::new().with("BUILD_IMAGE", "yes");
        let res = s.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(!res.config_changed);
        assert!(
            res.messages
                .iter()
                .any(|m| m.contains("no container runtime"))
        );
    }

    // ---- image profile (M18 E2) ---------------------------------------

    #[test]
    fn resolve_image_profile_defaults_to_minimal() {
        // No scripted answer → the prompt's default (`minimal`) is used.
        let prompt = Scripted::new();
        assert_eq!(
            resolve_image_profile(&prompt).unwrap(),
            ImageProfile::Minimal
        );
    }

    #[test]
    fn resolve_image_profile_reads_prototyping() {
        let prompt = Scripted::new().with(IMAGE_PROFILE_KEY, "prototyping");
        assert_eq!(
            resolve_image_profile(&prompt).unwrap(),
            ImageProfile::Prototyping
        );
    }

    #[test]
    fn resolve_image_profile_unknown_degrades_to_minimal() {
        // A typo must never silently produce the heavier image.
        let prompt = Scripted::new().with(IMAGE_PROFILE_KEY, "kitchen-sink");
        assert_eq!(
            resolve_image_profile(&prompt).unwrap(),
            ImageProfile::Minimal
        );
    }

    #[test]
    fn resolve_image_profile_is_idempotent() {
        // The same answer maps to the same profile on repeated runs — a
        // re-run of setup doesn't drift the baked profile.
        let prompt = Scripted::new()
            .with(IMAGE_PROFILE_KEY, "prototyping")
            .with(IMAGE_PROFILE_KEY, "prototyping");
        let first = resolve_image_profile(&prompt).unwrap();
        let second = resolve_image_profile(&prompt).unwrap();
        assert_eq!(first, second);
        assert_eq!(first, ImageProfile::Prototyping);
    }

    #[test]
    fn prototyping_base_spec_bakes_the_warm_toolchain() {
        // Bake test at the setup layer: a prototyping base spec renders the
        // warm apt + npm bundle on top of the baseline packages.
        let mut spec = fake_default_spec();
        spec.apt_packages = DEFAULT_BASE_APT_PACKAGES
            .iter()
            .copied()
            .map(String::from)
            .collect();
        spec.image_profile = ImageProfile::Prototyping;
        let df = spec.dockerfile();
        for pkg in [
            "sqlite3",
            "chromium",
            "zip",
            "fonts-inter",
            "fonts-jetbrains-mono",
            "fonts-noto-color-emoji",
        ] {
            assert!(df.contains(pkg), "expected apt `{pkg}` in base bake");
        }
        for pkg in [
            "vite",
            "create-vite",
            "typescript",
            "eslint",
            "prettier",
            "tailwindcss",
        ] {
            assert!(df.contains(pkg), "expected npm `{pkg}` in base bake");
        }
        // A minimal base spec bakes none of them.
        let mut minimal = spec.clone();
        minimal.image_profile = ImageProfile::Minimal;
        let mdf = minimal.dockerfile();
        assert!(!mdf.contains("sqlite3"));
        assert!(!mdf.contains("npm install -g"));
        // The two profiles produce different image tags.
        assert_ne!(spec.image_tag(), minimal.image_tag());
    }

    #[test]
    fn step_reruns_without_runtime_are_idempotent() {
        // Re-running the step (opt-in + profile answered) with no runtime
        // must not mutate config or duplicate state — same noop both times.
        let s = ImageBuildStep;
        let prompt = Scripted::new()
            .with("BUILD_IMAGE", "yes")
            .with("BUILD_IMAGE", "yes")
            .with(IMAGE_PROFILE_KEY, "prototyping")
            .with(IMAGE_PROFILE_KEY, "prototyping");
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        let a = s.run(&mut cfg, &prompt, &mut state).unwrap();
        let cfg_after_first = cfg.clone();
        let b = s.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(!a.config_changed);
        assert!(!b.config_changed);
        assert_eq!(cfg, cfg_after_first, "re-run must not drift config");
    }

    #[test]
    fn run_build_without_runtime_errors() {
        // We can't be inside a Tokio context here; the function should
        // surface a friendly error rather than panic. Use the inline
        // dummy spec so this test is independent of binary-locate.
        let err = run_build(&fake_default_spec()).unwrap_err();
        assert!(matches!(err, StepError::Other(_)));
    }

    // ---- try_pull behaviour --------------------------------------------

    #[test]
    fn try_pull_adopts_on_matching_fingerprint() {
        let fp = "abc123";
        let target = "copperclaw/session:sha256-abc123";
        let docker = StubDocker::new().with_label(Ok(Some(fp.to_string())));
        let outcome = try_pull(&docker, fp, target, "ghcr.io/example/session");
        assert_eq!(outcome, PullOutcome::Adopted);
        let calls = docker.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls[0].starts_with("pull ghcr.io/example/session:sha256-abc123"));
        assert!(calls[1].starts_with("label ghcr.io/example/session:sha256-abc123"));
        assert!(calls[2].starts_with(
            "tag ghcr.io/example/session:sha256-abc123 copperclaw/session:sha256-abc123"
        ));
    }

    #[test]
    fn try_pull_fails_on_pull_error() {
        let docker = StubDocker::new().with_pull(Err("not found".to_string()));
        let outcome = try_pull(&docker, "fp", "target", "reg");
        match outcome {
            PullOutcome::Failed(msg) => assert!(msg.contains("not found"), "got: {msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        // Subsequent calls (label/tag) shouldn't fire after pull fails.
        let calls = docker.calls();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn try_pull_fails_on_label_mismatch() {
        let docker = StubDocker::new().with_label(Ok(Some("other".to_string())));
        let outcome = try_pull(&docker, "fp", "target", "reg");
        match outcome {
            PullOutcome::Failed(msg) => {
                assert!(msg.contains("mismatch"), "got: {msg}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        // No tag call should have happened.
        assert!(!docker.calls().iter().any(|c| c.starts_with("tag ")));
    }

    #[test]
    fn try_pull_fails_when_label_absent() {
        let docker = StubDocker::new().with_label(Ok(None));
        let outcome = try_pull(&docker, "fp", "target", "reg");
        match outcome {
            PullOutcome::Failed(msg) => {
                assert!(
                    msg.contains("missing the copperclaw.fingerprint label"),
                    "got: {msg}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn try_pull_fails_when_label_inspect_errors() {
        let docker = StubDocker::new().with_label(Err("daemon down".to_string()));
        let outcome = try_pull(&docker, "fp", "target", "reg");
        match outcome {
            PullOutcome::Failed(msg) => assert!(msg.contains("label inspect"), "got: {msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn try_pull_fails_when_tag_errors() {
        let docker = StubDocker::new()
            .with_label(Ok(Some("fp".to_string())))
            .with_tag(Err("tag failed".to_string()));
        let outcome = try_pull(&docker, "fp", "target", "reg");
        match outcome {
            PullOutcome::Failed(msg) => assert!(msg.contains("docker tag"), "got: {msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn resolve_pull_registry_defaults_when_env_missing() {
        // We can't safely mutate process env from a #[test] under edition
        // 2024 (unsafe is forbidden). Instead verify the default branch
        // by reading the constant directly.
        assert!(DEFAULT_PULL_REGISTRY.starts_with("ghcr.io/"));
    }

    // ---- pinned-binary fetch (M20 Q1) -----------------------------------

    /// Trait-only stub: no real `curl`/`sha256sum`/`tar` process ever
    /// spawns. `extract_tar_gz` writes canned bytes straight to the
    /// archive-member path the test configured, standing in for a real
    /// tarball's contents.
    struct StubFetcher {
        download_result: Result<(), String>,
        verify_result: Result<(), String>,
        extract_result: Result<(), String>,
        member_bytes: Vec<u8>,
        calls: Mutex<Vec<String>>,
    }

    impl StubFetcher {
        fn ok(member_bytes: impl Into<Vec<u8>>) -> Self {
            Self {
                download_result: Ok(()),
                verify_result: Ok(()),
                extract_result: Ok(()),
                member_bytes: member_bytes.into(),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn with_verify(mut self, r: Result<(), String>) -> Self {
            self.verify_result = r;
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl PinnedBinaryFetcher for StubFetcher {
        fn download(&self, url: &str, dest: &Path) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("download {url}"));
            self.download_result.clone()?;
            // A real download would leave archive bytes at `dest`; a
            // placeholder is enough since the stub `extract_tar_gz`
            // ignores the archive contents.
            std::fs::write(dest, b"fake-tarball").map_err(|e| e.to_string())
        }

        fn verify_sha256(&self, _path: &Path, _sha256_hex: &str) -> Result<(), String> {
            self.calls.lock().unwrap().push("verify".to_string());
            self.verify_result.clone()
        }

        fn extract_tar_gz(&self, _archive: &Path, dest_dir: &Path) -> Result<(), String> {
            self.calls.lock().unwrap().push("extract".to_string());
            self.extract_result.clone()?;
            // A real `tar` doesn't know `archive_member` either — it just
            // unpacks whatever the tarball actually nests; the caller
            // (`fetch_and_extract`) is the one that then joins
            // `target.archive_member` onto `dest_dir`. So the stub writes
            // under every plausible member path the tests use, matching
            // the real ruff tarball's `<target-triple>/ruff` shape for
            // both host architectures plus the synthetic `test_binary`
            // member — cheap, and avoids the stub needing to know which
            // `PinnedBinary` is in play.
            for member in [
                "stub-target/ruff",
                "ruff-x86_64-unknown-linux-gnu/ruff",
                "ruff-aarch64-unknown-linux-gnu/ruff",
            ] {
                let path = dest_dir.join(member);
                std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
                std::fs::write(&path, &self.member_bytes).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }

    fn test_binary() -> PinnedBinary {
        // A synthetic single-target binary keyed to the actual test-host
        // architecture (`std::env::consts::ARCH` is itself a `'static`
        // const, so no leaking/allocation is needed) so
        // `fetch_pinned_binary`'s arch match succeeds regardless of which
        // CI/dev architecture runs the suite.
        PinnedBinary {
            name: "stub-tool",
            version: "1.2.3",
            dest_path: "/usr/local/bin/stub-tool",
            targets: &[PinnedBinaryTarget {
                arch: std::env::consts::ARCH,
                url: "https://example.invalid/stub-tool.tar.gz",
                sha256: "0000000000000000000000000000000000000000000000000000000000000000",
                archive_member: "stub-target/ruff",
            }],
        }
    }

    #[test]
    fn fetch_pinned_binary_happy_path() {
        let dir = tempfile::tempdir().unwrap();
        let fetcher = StubFetcher::ok(b"binary-bytes".to_vec());
        let binary = test_binary();
        let bytes = fetch_pinned_binary(&fetcher, dir.path(), &binary).unwrap();
        assert_eq!(bytes, b"binary-bytes");
        assert_eq!(
            fetcher.calls(),
            vec![
                "download https://example.invalid/stub-tool.tar.gz",
                "verify",
                "extract"
            ]
        );
        // Scratch dir is cleaned up; only the cache file remains.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one cached file, got {entries:?}"
        );
    }

    #[test]
    fn fetch_pinned_binary_is_cached_on_second_call() {
        let dir = tempfile::tempdir().unwrap();
        let binary = test_binary();
        let fetcher = StubFetcher::ok(b"binary-bytes".to_vec());
        let first = fetch_pinned_binary(&fetcher, dir.path(), &binary).unwrap();
        assert_eq!(first, b"binary-bytes");
        assert_eq!(
            fetcher.calls().len(),
            3,
            "first call fetches over the network"
        );

        // A second fetcher that would fail if actually invoked — the cache
        // hit must short-circuit before any of its methods are called.
        let poison = StubFetcher {
            download_result: Err("must not be called".to_string()),
            verify_result: Err("must not be called".to_string()),
            extract_result: Err("must not be called".to_string()),
            member_bytes: Vec::new(),
            calls: Mutex::new(Vec::new()),
        };
        let second = fetch_pinned_binary(&poison, dir.path(), &binary).unwrap();
        assert_eq!(second, b"binary-bytes");
        assert!(
            poison.calls().is_empty(),
            "cache hit must skip the fetcher entirely"
        );
    }

    #[test]
    fn fetch_pinned_binary_fails_on_checksum_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let fetcher = StubFetcher::ok(b"binary-bytes".to_vec())
            .with_verify(Err("checksum mismatch: expected X, got Y".to_string()));
        let binary = test_binary();
        let err = fetch_pinned_binary(&fetcher, dir.path(), &binary).unwrap_err();
        match err {
            StepError::Other(msg) => assert!(msg.contains("checksum mismatch"), "got: {msg}"),
            other => panic!("expected Other, got {other:?}"),
        }
        // Nothing is cached on failure.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn fetch_pinned_binary_fails_for_unsupported_architecture() {
        let dir = tempfile::tempdir().unwrap();
        let fetcher = StubFetcher::ok(b"binary-bytes".to_vec());
        let binary = PinnedBinary {
            name: "stub-tool",
            version: "1.2.3",
            dest_path: "/usr/local/bin/stub-tool",
            targets: &[PinnedBinaryTarget {
                arch: "not-a-real-arch",
                url: "https://example.invalid/stub-tool.tar.gz",
                sha256: "0000000000000000000000000000000000000000000000000000000000000000",
                archive_member: "stub-target/ruff",
            }],
        };
        let err = fetch_pinned_binary(&fetcher, dir.path(), &binary).unwrap_err();
        match err {
            StepError::Other(msg) => assert!(msg.contains("no pinned"), "got: {msg}"),
            other => panic!("expected Other, got {other:?}"),
        }
        assert!(
            fetcher.calls().is_empty(),
            "must fail before touching the fetcher"
        );
    }

    #[test]
    fn bake_pinned_binaries_is_noop_for_minimal() {
        let mut spec = fake_default_spec();
        let fetcher = StubFetcher {
            download_result: Err("must not be called for Minimal".to_string()),
            verify_result: Err("must not be called for Minimal".to_string()),
            extract_result: Err("must not be called for Minimal".to_string()),
            member_bytes: Vec::new(),
            calls: Mutex::new(Vec::new()),
        };
        let dir = tempfile::tempdir().unwrap();
        let before = spec.extra_files.len();
        bake_pinned_binaries(&mut spec, ImageProfile::Minimal, &fetcher, dir.path()).unwrap();
        assert_eq!(
            spec.extra_files.len(),
            before,
            "Minimal adds no pinned binaries"
        );
        assert!(fetcher.calls().is_empty());
    }

    #[test]
    fn bake_pinned_binaries_embeds_ruff_for_prototyping() {
        // Uses the real `ImageProfile::Prototyping` bundle (today just
        // `ruff`) with a stub fetcher, verifying the embedded ExtraFile's
        // path/mode/content end up on the spec and render into the
        // Dockerfile via the existing generic extra_files mechanism.
        let mut spec = fake_default_spec();
        let fetcher = StubFetcher::ok(b"#!/bin/sh\necho fake-ruff".to_vec());
        let dir = tempfile::tempdir().unwrap();
        bake_pinned_binaries(&mut spec, ImageProfile::Prototyping, &fetcher, dir.path()).unwrap();
        let ruff_file = spec
            .extra_files
            .iter()
            .find(|f| f.path == PathBuf::from("/usr/local/bin/ruff"))
            .expect("expected an embedded /usr/local/bin/ruff ExtraFile");
        assert_eq!(ruff_file.mode, 0o755);
        assert_eq!(ruff_file.contents, b"#!/bin/sh\necho fake-ruff");
        let df = spec.dockerfile();
        assert!(
            df.contains("/usr/local/bin/ruff"),
            "expected ruff COPY in dockerfile"
        );
        assert!(df.contains("chmod 755 /usr/local/bin/ruff"));
    }
}
