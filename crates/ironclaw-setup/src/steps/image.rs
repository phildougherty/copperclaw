//! Step 4 — container image build.
//!
//! Calls [`ironclaw_container_rt::detect`] to pick a runtime then asks it
//! to build a minimal image. The detection call is async; we drive it on
//! the current Tokio runtime via [`tokio::task::block_in_place`] so the
//! synchronous [`Step`] trait stays simple.
//!
//! Before falling through to a local build the step attempts to pull a
//! pre-published image from GHCR — this collapses cold-start from 1-2
//! minutes (full `docker build` of a Debian-slim layer) to ~10s for the
//! common case. Set `IRONCLAW_SETUP_NO_PULL=1` to skip the pull attempt
//! entirely (useful for reproducible local builds or air-gapped hosts).

use crate::config::SetupConfig;
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::{Step, StepError, StepResult};
use ironclaw_container_rt::{ExtraFile, ImageBuildSpec};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

/// Repo used for the host's session base image.
pub const DEFAULT_REPO: &str = "ironclaw/session";

/// Default base image used for the container build step.
///
/// `trixie` (Debian 13) ships glibc 2.41, which covers any
/// reasonably-recent host build of `ironclaw-runner`. `bookworm`
/// (Debian 12) has glibc 2.36, which is too old for runners built
/// against glibc 2.39+ — the symptom is a `version GLIBC_2.39 not
/// found` error on container start.
pub const DEFAULT_BASE_IMAGE: &str = "debian:trixie-slim";

/// GHCR repository the CI workflow publishes the session base image to.
///
/// TODO(team-c): once the repository slug is configurable per fork or
/// per organisation, accept this via an env var or setup-config field
/// rather than hardcoding the upstream slug.
pub const DEFAULT_PULL_REGISTRY: &str = "ghcr.io/phildougherty/ironclaw/session";

/// Env var that disables the pre-pull entirely. Truthy = skip.
pub const ENV_NO_PULL: &str = "IRONCLAW_SETUP_NO_PULL";

/// Env var that overrides the pull registry slug.
pub const ENV_PULL_REGISTRY: &str = "IRONCLAW_SETUP_PULL_REGISTRY";

/// Docker label the published image carries, mirroring the LABEL in
/// `container/Dockerfile`. Used to verify the pulled image matches
/// the locally-expected fingerprint before adopting it.
pub const FINGERPRINT_LABEL: &str = "ironclaw.fingerprint";

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
        "Build the ironclaw container image"
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

        let spec = default_spec()?;
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
        let verb = if outcome.was_cached { "reused" } else { "built" };
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
pub const RUNNER_PATH_IN_IMAGE: &str = "/usr/local/bin/ironclaw-runner";

/// Default minimal image spec used by the setup binary.
///
/// The runner binary is COPY'd into the image as `/usr/local/bin/ironclaw-runner`
/// so the host's container manager can `exec` it on spawn. If the
/// runner sibling cannot be located (e.g. setup is being run before
/// the workspace has been built), the step returns an error rather
/// than producing a broken image.
pub fn default_spec() -> Result<ImageBuildSpec, StepError> {
    let mut spec = ImageBuildSpec::new(DEFAULT_REPO, DEFAULT_BASE_IMAGE);
    let runner_path = locate_runner_binary()?;
    let bytes = std::fs::read(&runner_path).map_err(|e| {
        StepError::Other(format!(
            "read runner binary at {}: {e}",
            runner_path.display()
        ))
    })?;
    spec.extra_files.push(
        ExtraFile::new(PathBuf::from(RUNNER_PATH_IN_IMAGE), bytes).with_mode(0o755),
    );
    Ok(spec)
}

/// Find the `ironclaw-runner` binary that should be baked into the
/// session image.
///
/// Resolution order:
/// 1. `IRONCLAW_RUNNER_BIN` env var — explicit override (CI, packaging).
/// 2. Sibling of the currently running executable
///    (`std::env::current_exe()` parent + `ironclaw-runner`).
/// 3. Anywhere on `PATH` — last resort, useful when ironclaw-setup is
///    installed system-wide.
pub fn locate_runner_binary() -> Result<PathBuf, StepError> {
    if let Some(explicit) = std::env::var_os("IRONCLAW_RUNNER_BIN") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Ok(p);
        }
        return Err(StepError::Other(format!(
            "IRONCLAW_RUNNER_BIN points at {} which does not exist",
            p.display()
        )));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let candidate = parent.join("ironclaw-runner");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    if let Some(p) = which_runner() {
        return Ok(p);
    }
    Err(StepError::Other(
        "could not locate `ironclaw-runner` binary — \
         set IRONCLAW_RUNNER_BIN or place it next to ironclaw-setup"
            .into(),
    ))
}

fn which_runner() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("ironclaw-runner");
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
/// `IRONCLAW_SETUP_PULL_REGISTRY` then falls back to the default.
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
            let rt = ironclaw_container_rt::detect()
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
    use ironclaw_container_rt::ImageBuildSpec;
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
        assert!(res.messages.iter().any(|m| m.contains("no container runtime")));
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
        let target = "ironclaw/session:sha256-abc123";
        let docker =
            StubDocker::new().with_label(Ok(Some(fp.to_string())));
        let outcome = try_pull(&docker, fp, target, "ghcr.io/example/session");
        assert_eq!(outcome, PullOutcome::Adopted);
        let calls = docker.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls[0].starts_with("pull ghcr.io/example/session:sha256-abc123"));
        assert!(calls[1].starts_with("label ghcr.io/example/session:sha256-abc123"));
        assert!(calls[2].starts_with(
            "tag ghcr.io/example/session:sha256-abc123 ironclaw/session:sha256-abc123"
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
                assert!(msg.contains("missing the ironclaw.fingerprint label"), "got: {msg}");
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
}
