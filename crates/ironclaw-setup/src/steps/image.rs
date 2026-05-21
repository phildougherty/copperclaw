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
        let tag = run_build(&spec)?;
        cfg.image_tag.clone_from(&tag);
        Ok(StepResult::ok(format!("built image: {tag}")))
    }
}

/// Default minimal image spec used by the setup binary.
#[must_use]
pub fn default_spec() -> ImageBuildSpec {
    ImageBuildSpec::new(DEFAULT_REPO, DEFAULT_BASE_IMAGE)
}

/// Drive the async build on the current Tokio runtime, returning the
/// resulting image tag (or a friendly error when no runtime is reachable).
pub fn run_build(spec: &ImageBuildSpec) -> Result<String, StepError> {
    let spec = spec.clone();
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| StepError::Other("no Tokio runtime available for image build".into()))?;
    let result: Result<String, StepError> = tokio::task::block_in_place(|| {
        handle.block_on(async move {
            let rt = ironclaw_container_rt::detect()
                .await
                .map_err(|e| StepError::Other(format!("detect runtime: {e}")))?;
            let tag = rt
                .build_image(spec)
                .await
                .map_err(|e| StepError::Other(format!("build image: {e}")))?;
            Ok(tag)
        })
    });
    result
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
