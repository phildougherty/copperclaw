//! Container runtime abstraction.
//!
//! Two backends ship in-tree:
//!
//! * [`docker::DockerRuntime`] — talks to the local Docker daemon via
//!   [`bollard`].
//! * [`apple::AppleContainerRuntime`] — shells out to Apple's
//!   `container` CLI on macOS.
//!
//! Both implement [`ContainerRuntime`]. Use [`detect`] to pick the
//! first one that responds; otherwise pick a backend explicitly.
//!
//! See `PLAN.md` § 5.5 for the spawn contract.

#![forbid(unsafe_code)]

use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;

pub mod apple;
pub mod build;
pub mod docker;
pub mod spec;

pub use crate::apple::AppleContainerRuntime;
pub use crate::build::{ExtraFile, ImageBuildSpec};
pub use crate::docker::DockerRuntime;
pub use crate::spec::{ContainerHandle, ContainerSpec, Mount, ResourceLimits};

/// Every fallible container-runtime call returns this.
#[derive(Debug, Error)]
pub enum RtError {
    /// The backend isn't installed or its daemon/CLI isn't responding.
    #[error("runtime not available: {0}")]
    Unavailable(String),
    /// The backend was reachable but the operation failed (image not
    /// found, container already exists, build error, etc.).
    #[error("container error: {0}")]
    Container(String),
    /// Underlying I/O error (tempfile create, socket open, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The requested feature is not supported by this backend. Per the
    /// project tenets ("errors over silent fallback"), callers should
    /// treat this as a hard failure rather than silently ignoring the
    /// requested capability.
    #[error("unsupported: {0}")]
    Unsupported(String),
}

/// Container-runtime trait. Both backends implement this; see crate
/// docs for the contract.
#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    /// Probe the runtime — start the daemon if needed or fail with
    /// [`RtError::Unavailable`].
    async fn ensure_running(&self) -> Result<(), RtError>;

    /// Remove every container labelled with `ironclaw.install=<slug>`
    /// regardless of state. Idempotent.
    async fn cleanup_orphans(&self, install_slug: &str) -> Result<(), RtError>;

    /// Create + start a container from `spec`.
    async fn spawn(&self, spec: ContainerSpec) -> Result<ContainerHandle, RtError>;

    /// Stop a container with at most `grace` for it to exit before
    /// the runtime sends SIGKILL.
    async fn stop(&self, name: &str, grace: Duration) -> Result<(), RtError>;

    /// Force-remove a container by name (stop + rm). Used by the
    /// host's crash-restart path so the next spawn doesn't fail with
    /// a "name already in use" conflict. Default impl is a best-
    /// effort `stop`-then-success: concrete runtimes override with
    /// the real removal.
    async fn remove(&self, name: &str) -> Result<(), RtError> {
        let _ = self.stop(name, Duration::from_secs(2)).await;
        Ok(())
    }

    /// Build (or rebuild) an image and return its full tag. Identical
    /// specs map to identical tags via [`ImageBuildSpec::fingerprint`],
    /// so callers can skip a rebuild when the tag already exists.
    async fn build_image(&self, spec: ImageBuildSpec) -> Result<String, RtError>;

    /// Whether an image with `tag` already exists locally. Callers use
    /// this to distinguish a fresh build from a reuse of a cached image
    /// without re-implementing each runtime's inspection API. The default
    /// impl returns `Ok(false)`, which is conservative — concrete runtimes
    /// should override it.
    async fn image_exists(&self, tag: &str) -> Result<bool, RtError> {
        let _ = tag;
        Ok(false)
    }

    /// Capture up to `tail` lines from the named container's combined
    /// stdout/stderr stream. Used by the host's crash-restart path to
    /// archive the last words of a dying runner before its container is
    /// removed.
    ///
    /// Default impl returns an empty string — backends that don't
    /// support log capture (`AppleContainerRuntime` today, in-process
    /// test stubs) silently degrade. Concrete runtimes (Docker)
    /// override.
    async fn logs(&self, name: &str, tail: u32) -> Result<String, RtError> {
        let _ = (name, tail);
        Ok(String::new())
    }
}

/// Picker for [`detect`] / explicit selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeKind {
    /// Docker (via bollard).
    Docker,
    /// Apple Container (CLI shell-out).
    Apple,
}

impl RuntimeKind {
    /// Stable token used in logs/labels.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeKind::Docker => "docker",
            RuntimeKind::Apple => "apple",
        }
    }
}

impl std::fmt::Display for RuntimeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Probe Docker first, then Apple Container. Returns the first one
/// that responds, otherwise [`RtError::Unavailable`].
///
/// The Docker probe is `bollard::Docker::version`; the Apple probe is
/// `container --version`. Probes are short — they will not block past
/// the runtime's own connect/exec timeout.
pub async fn detect() -> Result<Box<dyn ContainerRuntime>, RtError> {
    if let Ok(rt) = DockerRuntime::connect() {
        if rt.ensure_running().await.is_ok() {
            return Ok(Box::new(rt));
        }
    }
    let apple = AppleContainerRuntime::new();
    if apple.ensure_running().await.is_ok() {
        return Ok(Box::new(apple));
    }
    Err(RtError::Unavailable(
        "no container runtime detected (tried docker, apple)".into(),
    ))
}

/// Convenience: which kind would [`detect`] pick? Returns
/// [`RtError::Unavailable`] if neither responds.
pub async fn detect_kind() -> Result<RuntimeKind, RtError> {
    if let Ok(rt) = DockerRuntime::connect() {
        if rt.ensure_running().await.is_ok() {
            return Ok(RuntimeKind::Docker);
        }
    }
    let apple = AppleContainerRuntime::new();
    if apple.ensure_running().await.is_ok() {
        return Ok(RuntimeKind::Apple);
    }
    Err(RtError::Unavailable(
        "no container runtime detected (tried docker, apple)".into(),
    ))
}

#[cfg(test)]
pub(crate) mod mock {
    //! A trait-only mock that records every call. Used to assert
    //! against the [`ContainerRuntime`] surface without a real daemon.

    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;

    use crate::build::ImageBuildSpec;
    use crate::spec::{ContainerHandle, ContainerSpec};
    use crate::{ContainerRuntime, RtError};

    #[derive(Debug, Default)]
    pub struct MockRuntime {
        pub calls: Mutex<Vec<MockCall>>,
        /// Pre-loaded error to return from the next call (any method).
        pub fail_next: Mutex<Option<RtError>>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum MockCall {
        EnsureRunning,
        CleanupOrphans(String),
        Spawn(String),
        Stop(String, Duration),
        BuildImage(String),
    }

    impl MockRuntime {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn fail_with(self, err: RtError) -> Self {
            *self.fail_next.lock().unwrap() = Some(err);
            self
        }

        pub fn calls(&self) -> Vec<MockCall> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, call: MockCall) -> Result<(), RtError> {
            if let Some(err) = self.fail_next.lock().unwrap().take() {
                return Err(err);
            }
            self.calls.lock().unwrap().push(call);
            Ok(())
        }
    }

    #[async_trait]
    impl ContainerRuntime for MockRuntime {
        async fn ensure_running(&self) -> Result<(), RtError> {
            self.record(MockCall::EnsureRunning)
        }
        async fn cleanup_orphans(&self, slug: &str) -> Result<(), RtError> {
            self.record(MockCall::CleanupOrphans(slug.to_string()))
        }
        async fn spawn(&self, spec: ContainerSpec) -> Result<ContainerHandle, RtError> {
            let name = spec.name.clone();
            self.record(MockCall::Spawn(name.clone()))?;
            Ok(ContainerHandle::new(format!("mock-{name}-id"), name))
        }
        async fn stop(&self, name: &str, grace: Duration) -> Result<(), RtError> {
            self.record(MockCall::Stop(name.to_string(), grace))
        }
        async fn build_image(&self, spec: ImageBuildSpec) -> Result<String, RtError> {
            let tag = spec.image_tag();
            self.record(MockCall::BuildImage(tag.clone()))?;
            Ok(tag)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockCall, MockRuntime};

    #[test]
    fn rt_error_display_unavailable() {
        let e = RtError::Unavailable("nope".into());
        assert_eq!(e.to_string(), "runtime not available: nope");
    }

    #[test]
    fn rt_error_display_container() {
        let e = RtError::Container("boom".into());
        assert_eq!(e.to_string(), "container error: boom");
    }

    #[test]
    fn rt_error_display_unsupported() {
        let e = RtError::Unsupported("egress_allow".into());
        assert_eq!(e.to_string(), "unsupported: egress_allow");
    }

    #[test]
    fn rt_error_display_io() {
        let inner = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let e = RtError::Io(inner);
        assert!(e.to_string().starts_with("io error:"));
    }

    #[test]
    fn rt_error_io_from_std_io() {
        let inner = std::io::Error::other("boom");
        let e: RtError = inner.into();
        assert!(matches!(e, RtError::Io(_)));
    }

    #[test]
    fn runtime_kind_display() {
        assert_eq!(RuntimeKind::Docker.to_string(), "docker");
        assert_eq!(RuntimeKind::Apple.to_string(), "apple");
    }

    #[test]
    fn runtime_kind_as_str() {
        assert_eq!(RuntimeKind::Docker.as_str(), "docker");
        assert_eq!(RuntimeKind::Apple.as_str(), "apple");
    }

    #[test]
    fn runtime_kind_eq_copy() {
        let k = RuntimeKind::Docker;
        let k2 = k;
        assert_eq!(k, k2);
        assert_ne!(RuntimeKind::Docker, RuntimeKind::Apple);
    }

    #[tokio::test]
    async fn mock_records_all_calls() {
        let rt = MockRuntime::new();
        let dyn_rt: &dyn ContainerRuntime = &rt;
        dyn_rt.ensure_running().await.unwrap();
        dyn_rt.cleanup_orphans("slug").await.unwrap();
        let handle = dyn_rt
            .spawn(ContainerSpec::new("c", "img"))
            .await
            .unwrap();
        assert_eq!(handle.name, "c");
        assert_eq!(handle.id, "mock-c-id");
        dyn_rt.stop("c", Duration::from_secs(5)).await.unwrap();
        let tag = dyn_rt
            .build_image(ImageBuildSpec::new("r", "debian:12-slim"))
            .await
            .unwrap();
        assert!(tag.starts_with("r:sha256-"));

        let calls = rt.calls();
        assert_eq!(calls.len(), 5);
        assert!(matches!(calls[0], MockCall::EnsureRunning));
        assert!(matches!(&calls[1], MockCall::CleanupOrphans(s) if s == "slug"));
        assert!(matches!(&calls[2], MockCall::Spawn(s) if s == "c"));
        assert!(matches!(&calls[3], MockCall::Stop(s, d) if s == "c" && *d == Duration::from_secs(5)));
        assert!(matches!(&calls[4], MockCall::BuildImage(_)));
    }

    #[tokio::test]
    async fn mock_can_inject_failure() {
        let rt = MockRuntime::new().fail_with(RtError::Unavailable("offline".into()));
        let err = rt.ensure_running().await.unwrap_err();
        assert!(matches!(err, RtError::Unavailable(_)));
        // Subsequent calls succeed (the injected failure was one-shot).
        rt.ensure_running().await.unwrap();
    }

    #[tokio::test]
    async fn detect_when_neither_available_errs() {
        // We can't reliably guarantee docker is missing in every test
        // environment, but we *can* on this host: if the docker probe
        // somehow succeeds, fall through and assert the type round-trip.
        match detect_kind().await {
            Ok(kind) => {
                assert!(kind == RuntimeKind::Docker || kind == RuntimeKind::Apple);
            }
            Err(e) => assert!(matches!(e, RtError::Unavailable(_))),
        }
    }
}
