//! Step 4 — container image build.
//!
//! Calls [`ironclaw_container_rt::detect`] to pick a runtime then asks it
//! to build a minimal image. The detection call is async; we drive it on
//! the current Tokio runtime via [`tokio::task::block_in_place`] so the
//! synchronous [`Step`] trait stays simple.

use crate::config::SetupConfig;
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::{Step, StepError, StepResult};
use ironclaw_container_rt::ImageBuildSpec;

/// Repo used for the host's session base image.
pub const DEFAULT_REPO: &str = "ironclaw/session";

/// Default base image used for the container build step.
pub const DEFAULT_BASE_IMAGE: &str = "debian:12-slim";

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

        let spec = default_spec();
        let outcome = run_build(&spec)?;
        cfg.image_tag.clone_from(&outcome.tag);
        let verb = if outcome.was_cached { "reused" } else { "built" };
        Ok(StepResult::ok(format!("{verb} image: {}", outcome.tag)))
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

/// Default minimal image spec used by the setup binary.
#[must_use]
pub fn default_spec() -> ImageBuildSpec {
    ImageBuildSpec::new(DEFAULT_REPO, DEFAULT_BASE_IMAGE)
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

    #[test]
    fn default_spec_matches_repo_and_base() {
        let spec = default_spec();
        assert_eq!(spec.repo, DEFAULT_REPO);
        assert_eq!(spec.base_image, DEFAULT_BASE_IMAGE);
    }

    #[test]
    fn default_spec_image_tag_is_stable() {
        let a = default_spec();
        let b = default_spec();
        assert_eq!(a.image_tag(), b.image_tag());
    }

    #[test]
    fn default_spec_is_image_build_spec() {
        let _: ImageBuildSpec = default_spec();
    }

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
        // surface a friendly error rather than panic.
        let err = run_build(&default_spec()).unwrap_err();
        assert!(matches!(err, StepError::Other(_)));
    }
}
