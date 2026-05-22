//! `ironclaw-host` — orchestrator library used by the `ironclaw` binary.
//!
//! The binary in [`main`] is a thin clap-driven shell over the entry points
//! exposed here. Splitting the boot logic into a library makes the host
//! testable from integration tests without spawning a real process.
//!
//! See `PLAN.md` § 6 T3 for the boot sequence and § A2 for the `iclaw`
//! command surface this crate serves over the local Unix socket.

#![forbid(unsafe_code)]

pub mod boot;
pub mod channels_init;
pub mod config;
pub mod container_manager;
pub mod context;
pub mod daemon;
pub mod handlers;
pub mod image_health;
pub mod orphans;
pub mod sessions;
pub mod socket;

pub use boot::{run_host, BootError};
pub use container_manager::{ContainerManager, ManagerConfig, SkillsMode};
pub use config::{ChannelInit, HostConfig, HostConfigError};
pub use context::HostContext;
pub use sessions::FsSessionRoot;
pub use socket::{dispatch_request, CommandHandler, DispatchTable, HandlerCtx};

#[cfg(test)]
pub(crate) mod tests {
    //! Crate-wide test helpers — most importantly a no-op `ContainerRuntime`
    //! impl so we can exercise the boot sequence without a live Docker/Apple
    //! Container daemon. The real runtime's mock is `pub(crate)` so we ship
    //! our own.

    use async_trait::async_trait;
    use ironclaw_container_rt::{
        ContainerHandle, ContainerRuntime, ContainerSpec, ImageBuildSpec, RtError,
    };
    use std::sync::Mutex;
    use std::time::Duration;

    /// No-op runtime. Records the last orphan-cleanup slug and every
    /// spawn call's container name so tests can assert against them.
    #[derive(Default)]
    pub struct NoopRuntime {
        pub last_orphan_slug: Mutex<Option<String>>,
        pub fail_next: Mutex<Option<RtError>>,
        pub spawn_log: Mutex<Vec<String>>,
    }

    impl NoopRuntime {
        /// Pre-load an error returned by the next call (regardless of method).
        #[must_use]
        pub fn fail_with(self, err: RtError) -> Self {
            *self.fail_next.lock().unwrap() = Some(err);
            self
        }

        /// Snapshot of the last `cleanup_orphans` slug.
        pub fn last_orphan_slug(&self) -> Option<String> {
            self.last_orphan_slug.lock().unwrap().clone()
        }

        /// Snapshot of every spawn call's container name, in order.
        pub fn spawn_calls(&self) -> Vec<String> {
            self.spawn_log.lock().unwrap().clone()
        }
    }

    impl std::fmt::Debug for NoopRuntime {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("NoopRuntime").finish_non_exhaustive()
        }
    }

    #[async_trait]
    impl ContainerRuntime for NoopRuntime {
        async fn ensure_running(&self) -> Result<(), RtError> {
            if let Some(err) = self.fail_next.lock().unwrap().take() {
                return Err(err);
            }
            Ok(())
        }

        async fn cleanup_orphans(&self, slug: &str) -> Result<(), RtError> {
            *self.last_orphan_slug.lock().unwrap() = Some(slug.to_owned());
            if let Some(err) = self.fail_next.lock().unwrap().take() {
                return Err(err);
            }
            Ok(())
        }

        async fn spawn(&self, spec: ContainerSpec) -> Result<ContainerHandle, RtError> {
            self.spawn_log.lock().unwrap().push(spec.name.clone());
            if let Some(err) = self.fail_next.lock().unwrap().take() {
                return Err(err);
            }
            Ok(ContainerHandle::new(
                format!("noop-{}-id", spec.name),
                spec.name,
            ))
        }

        async fn stop(&self, _name: &str, _grace: Duration) -> Result<(), RtError> {
            if let Some(err) = self.fail_next.lock().unwrap().take() {
                return Err(err);
            }
            Ok(())
        }

        async fn build_image(&self, spec: ImageBuildSpec) -> Result<String, RtError> {
            if let Some(err) = self.fail_next.lock().unwrap().take() {
                return Err(err);
            }
            Ok(spec.image_tag())
        }
    }

    #[tokio::test]
    async fn noop_runtime_records_orphan_slug() {
        let rt = NoopRuntime::default();
        rt.cleanup_orphans("slug").await.unwrap();
        assert_eq!(rt.last_orphan_slug().as_deref(), Some("slug"));
    }

    #[tokio::test]
    async fn noop_runtime_one_shot_failure() {
        let rt = NoopRuntime::default().fail_with(RtError::Unavailable("x".into()));
        assert!(rt.ensure_running().await.is_err());
        rt.ensure_running().await.unwrap();
    }

    #[tokio::test]
    async fn noop_runtime_spawn_stop_build_succeed() {
        let rt = NoopRuntime::default();
        let h = rt.spawn(ContainerSpec::new("c", "img")).await.unwrap();
        assert_eq!(h.name, "c");
        rt.stop("c", Duration::from_secs(1)).await.unwrap();
        let tag = rt
            .build_image(ImageBuildSpec::new("r", "debian:12-slim"))
            .await
            .unwrap();
        assert!(tag.starts_with("r:"));
    }

    #[test]
    fn noop_runtime_debug_renders() {
        let rt = NoopRuntime::default();
        assert!(format!("{rt:?}").contains("NoopRuntime"));
    }
}
