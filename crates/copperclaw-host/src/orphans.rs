//! Wrapper around [`copperclaw_container_rt::ContainerRuntime::cleanup_orphans`].
//!
//! Pulled into its own module so the boot sequence can be unit-tested against
//! a `dyn ContainerRuntime` mock without going through `detect()`.

use copperclaw_container_rt::{ContainerRuntime, RtError};

/// Call `runtime.cleanup_orphans(install_slug)` and log errors at warn level.
///
/// The caller is expected to continue the boot sequence even if cleanup
/// fails — orphan cleanup is a best-effort hygiene step.
pub async fn cleanup_orphans(
    runtime: &dyn ContainerRuntime,
    install_slug: &str,
) -> Result<(), RtError> {
    runtime.cleanup_orphans(install_slug).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::NoopRuntime;

    #[tokio::test]
    async fn ok_when_runtime_succeeds() {
        let rt = NoopRuntime::default();
        cleanup_orphans(&rt, "slug").await.unwrap();
        assert_eq!(rt.last_orphan_slug().as_deref(), Some("slug"));
    }

    #[tokio::test]
    async fn surfaces_runtime_error() {
        let rt = NoopRuntime::default().fail_with(RtError::Unavailable("nope".into()));
        let err = cleanup_orphans(&rt, "slug").await.unwrap_err();
        assert!(matches!(err, RtError::Unavailable(_)));
    }
}
