//! `AppleContainerRuntime` — shells out to the macOS `container` CLI.
//!
//! Apple's container runtime ships a single `container` binary that
//! accepts subcommands very close to docker's. We translate
//! `ContainerSpec` / `ImageBuildSpec` into argv vectors via pure
//! functions (testable without the binary present) and exec them
//! through `tokio::process::Command`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

use crate::build::ImageBuildSpec;
use crate::spec::{ContainerHandle, ContainerSpec, Mount};
use crate::{ContainerRuntime, RtError};

/// Path to the `container` CLI binary. Default `container`; can be
/// overridden for tests via [`AppleContainerRuntime::with_binary`].
const DEFAULT_BIN: &str = "container";

/// Apple-Container backend.
#[derive(Debug, Clone)]
pub struct AppleContainerRuntime {
    binary: String,
}

impl Default for AppleContainerRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl AppleContainerRuntime {
    /// Construct using the default `container` binary on `$PATH`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            binary: DEFAULT_BIN.to_string(),
        }
    }

    /// Override the binary path (test/diagnostic use).
    #[must_use]
    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    /// Detection probe: returns `Ok(())` if `container --version`
    /// resolves and exits 0.
    pub async fn probe(&self) -> Result<(), RtError> {
        let out = Command::new(&self.binary)
            .arg("--version")
            .output()
            .await
            .map_err(|e| RtError::Unavailable(format!("apple container probe: {e}")))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(RtError::Unavailable(format!(
                "container --version exited {}",
                out.status
            )))
        }
    }

    /// Binary path used by this runtime (for diagnostics/tests).
    #[must_use]
    pub fn binary(&self) -> &str {
        &self.binary
    }
}

#[async_trait]
impl ContainerRuntime for AppleContainerRuntime {
    async fn ensure_running(&self) -> Result<(), RtError> {
        self.probe().await
    }

    async fn cleanup_orphans(&self, install_slug: &str) -> Result<(), RtError> {
        let args = list_orphans_args(install_slug);
        let out = Command::new(&self.binary)
            .args(&args)
            .output()
            .await
            .map_err(|e| RtError::Container(format!("list orphans: {e}")))?;
        if !out.status.success() {
            return Err(RtError::Container(format!(
                "container ls failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        let ids = parse_orphan_ids(&String::from_utf8_lossy(&out.stdout));
        for id in ids {
            let rm = Command::new(&self.binary)
                .args(remove_args(&id))
                .output()
                .await
                .map_err(|e| RtError::Container(format!("remove orphan {id}: {e}")))?;
            if !rm.status.success() {
                return Err(RtError::Container(format!(
                    "container rm {id} failed: {}",
                    String::from_utf8_lossy(&rm.stderr)
                )));
            }
        }
        Ok(())
    }

    async fn spawn(&self, spec: ContainerSpec) -> Result<ContainerHandle, RtError> {
        // Apple Container runtime does not support per-container resource
        // limits or egress allow-lists.  Per the "errors over silent
        // fallback" tenet we refuse to spawn rather than silently
        // ignoring the requested capability.
        if !spec.resource_limits.is_empty() {
            return Err(RtError::Unsupported(
                "resource_limits (cpus/memory_mb/pids_limit) are not supported \
                 by the Apple Container runtime; use Docker or clear the limits"
                    .into(),
            ));
        }
        if !spec.egress_allow.is_empty() {
            return Err(RtError::Unsupported(
                "egress_allow is not supported by the Apple Container runtime; \
                 use Docker or clear the egress allow-list"
                    .into(),
            ));
        }
        let args = run_args(&spec);
        let out = Command::new(&self.binary)
            .args(&args)
            .output()
            .await
            .map_err(|e| RtError::Container(format!("container run: {e}")))?;
        if !out.status.success() {
            return Err(RtError::Container(format!(
                "container run failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(ContainerHandle::new(id, spec.name))
    }

    async fn stop(&self, name: &str, grace: Duration) -> Result<(), RtError> {
        let args = stop_args(name, grace);
        let out = Command::new(&self.binary)
            .args(&args)
            .output()
            .await
            .map_err(|e| RtError::Container(format!("container stop: {e}")))?;
        if !out.status.success() {
            return Err(RtError::Container(format!(
                "container stop {name} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(())
    }

    async fn build_image(&self, spec: ImageBuildSpec) -> Result<String, RtError> {
        let tag = spec.image_tag();
        let context_dir = write_build_context(&spec)?;
        let args = build_args(&context_dir, &tag);
        let out = Command::new(&self.binary)
            .args(&args)
            .output()
            .await
            .map_err(|e| RtError::Container(format!("container build: {e}")))?;
        // Best-effort cleanup; failure to remove is logged-not-fatal.
        let _ = std::fs::remove_dir_all(&context_dir);
        if !out.status.success() {
            return Err(RtError::Container(format!(
                "container build failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(tag)
    }

    async fn image_exists(&self, tag: &str) -> Result<bool, RtError> {
        // `container image inspect <tag>` exits non-zero with the image
        // missing from the local store. We swallow stderr — distinguishing
        // "not found" from a real failure is best-effort; the build that
        // follows will surface any real runtime issue.
        let out = Command::new(&self.binary)
            .args(image_inspect_args(tag))
            .output()
            .await
            .map_err(|e| RtError::Container(format!("container image inspect: {e}")))?;
        Ok(out.status.success())
    }
}

// ---- pure arg builders (unit-tested without the binary) ----------------

/// `container ls --label ironclaw.install=<slug> --quiet --all`
pub(crate) fn list_orphans_args(install_slug: &str) -> Vec<String> {
    vec![
        "ls".into(),
        "--all".into(),
        "--quiet".into(),
        "--filter".into(),
        format!("label=ironclaw.install={install_slug}"),
    ]
}

/// Parse `container ls --quiet` output into ids (one per line, trimmed,
/// blank lines skipped).
pub(crate) fn parse_orphan_ids(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// `container rm --force <id>`
pub(crate) fn remove_args(id: &str) -> Vec<String> {
    vec!["rm".into(), "--force".into(), id.into()]
}

/// `container stop --time <secs> <name>`
pub(crate) fn stop_args(name: &str, grace: Duration) -> Vec<String> {
    // saturate at u32::MAX seconds; CLI expects an integer.
    let secs = grace.as_secs().min(u64::from(u32::MAX));
    vec![
        "stop".into(),
        "--time".into(),
        secs.to_string(),
        name.into(),
    ]
}

/// `container run` argv for a spec.
///
/// We always pass `--detach`, name, image, labels, env, mounts,
/// extra-hosts, user, and entrypoint in a deterministic order so the
/// snapshot tests stay stable.
pub(crate) fn run_args(spec: &ContainerSpec) -> Vec<String> {
    let mut a = vec!["run".into(), "--detach".into(), "--name".into(), spec.name.clone()];

    if let Some(user) = &spec.user {
        a.push("--user".into());
        a.push(user.clone());
    }

    // Labels: emit sorted by key so the argv is deterministic.
    let labels: BTreeMap<&String, &String> = spec.labels.iter().collect();
    for (k, v) in labels {
        a.push("--label".into());
        a.push(format!("{k}={v}"));
    }

    for (k, v) in &spec.env {
        a.push("--env".into());
        a.push(format!("{k}={v}"));
    }

    for (host, ip) in &spec.extra_hosts {
        a.push("--add-host".into());
        a.push(format!("{host}:{ip}"));
    }

    for m in &spec.mounts {
        a.push("--mount".into());
        a.push(mount_arg(m));
    }

    a.push(spec.image.clone());

    for piece in &spec.entrypoint {
        a.push(piece.clone());
    }

    a
}

/// Render a single mount as the `--mount type=...,...` value.
pub(crate) fn mount_arg(m: &Mount) -> String {
    match m {
        Mount::Bind {
            source,
            target,
            read_only,
        } => {
            let mut s = format!("type=bind,source={source},target={target}");
            if *read_only {
                s.push_str(",readonly");
            }
            s
        }
        Mount::Volume {
            name,
            target,
            read_only,
        } => {
            let mut s = format!("type=volume,source={name},target={target}");
            if *read_only {
                s.push_str(",readonly");
            }
            s
        }
        Mount::Tmpfs { target, size_bytes } => {
            if *size_bytes == 0 {
                format!("type=tmpfs,target={target}")
            } else {
                format!("type=tmpfs,target={target},tmpfs-size={size_bytes}")
            }
        }
    }
}

/// `container build --tag <tag> <context-dir>`
pub(crate) fn build_args(context_dir: &std::path::Path, tag: &str) -> Vec<String> {
    vec![
        "build".into(),
        "--tag".into(),
        tag.into(),
        context_dir.to_string_lossy().into_owned(),
    ]
}

/// `container image inspect <tag>` — exits non-zero when the image is
/// not in the local store.
pub(crate) fn image_inspect_args(tag: &str) -> Vec<String> {
    vec!["image".into(), "inspect".into(), tag.into()]
}

/// Materialize an `ImageBuildSpec` into a freshly-created temp directory
/// shaped like a build context (Dockerfile + `files/...`).
pub(crate) fn write_build_context(spec: &ImageBuildSpec) -> Result<PathBuf, RtError> {
    let mut dir = std::env::temp_dir();
    let unique = format!("ironclaw-build-{}", spec.fingerprint());
    dir.push(unique);
    std::fs::create_dir_all(&dir)?;

    let dockerfile = dir.join("Dockerfile");
    let mut f = std::fs::File::create(&dockerfile)?;
    f.write_all(spec.dockerfile().as_bytes())?;

    if !spec.extra_files.is_empty() {
        let files_dir = dir.join("files");
        std::fs::create_dir_all(&files_dir)?;
        for ef in &spec.extra_files {
            let basename = ef
                .path
                .file_name()
                .map_or_else(|| "file".to_string(), |s| s.to_string_lossy().into_owned());
            let mut out = std::fs::File::create(files_dir.join(basename))?;
            out.write_all(&ef.contents)?;
        }
    }

    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::ExtraFile;
    use std::collections::HashMap;

    #[test]
    fn list_orphans_args_carries_label_filter() {
        let a = list_orphans_args("default");
        assert_eq!(a[0], "ls");
        assert!(a.contains(&"--quiet".to_string()));
        assert!(a.contains(&"--all".to_string()));
        assert!(a.iter().any(|s| s == "label=ironclaw.install=default"));
    }

    #[test]
    fn remove_args_is_force_rm() {
        assert_eq!(remove_args("abc"), vec!["rm", "--force", "abc"]);
    }

    #[test]
    fn stop_args_uses_grace_seconds() {
        let a = stop_args("c1", Duration::from_secs(30));
        assert_eq!(a, vec!["stop", "--time", "30", "c1"]);
    }

    #[test]
    fn stop_args_saturates_huge_durations() {
        // pick something larger than u32::MAX seconds
        let big = Duration::from_secs(u64::from(u32::MAX) + 1);
        let a = stop_args("c", big);
        assert_eq!(a[2], u32::MAX.to_string());
    }

    #[test]
    fn parse_orphan_ids_skips_blank_lines() {
        assert_eq!(parse_orphan_ids("abc\n\ndef\n"), vec!["abc", "def"]);
    }

    #[test]
    fn parse_orphan_ids_empty_input() {
        assert!(parse_orphan_ids("").is_empty());
    }

    #[test]
    fn mount_arg_bind_rw() {
        let s = mount_arg(&Mount::Bind {
            source: "/h".into(),
            target: "/c".into(),
            read_only: false,
        });
        assert_eq!(s, "type=bind,source=/h,target=/c");
    }

    #[test]
    fn mount_arg_bind_ro() {
        let s = mount_arg(&Mount::Bind {
            source: "/h".into(),
            target: "/c".into(),
            read_only: true,
        });
        assert!(s.ends_with(",readonly"));
        assert!(s.starts_with("type=bind,source=/h,target=/c"));
    }

    #[test]
    fn mount_arg_volume_ro() {
        let s = mount_arg(&Mount::Volume {
            name: "cache".into(),
            target: "/cache".into(),
            read_only: true,
        });
        assert_eq!(s, "type=volume,source=cache,target=/cache,readonly");
    }

    #[test]
    fn mount_arg_tmpfs_zero_size() {
        let s = mount_arg(&Mount::Tmpfs {
            target: "/tmp".into(),
            size_bytes: 0,
        });
        assert_eq!(s, "type=tmpfs,target=/tmp");
    }

    #[test]
    fn mount_arg_tmpfs_sized() {
        let s = mount_arg(&Mount::Tmpfs {
            target: "/tmp".into(),
            size_bytes: 1024,
        });
        assert_eq!(s, "type=tmpfs,target=/tmp,tmpfs-size=1024");
    }

    #[test]
    fn run_args_basic() {
        let spec = ContainerSpec::new("c1", "alpine:3");
        let a = run_args(&spec);
        assert_eq!(a[0], "run");
        assert_eq!(a[1], "--detach");
        assert!(a.windows(2).any(|w| w == ["--name", "c1"]));
        assert_eq!(a.last(), Some(&"alpine:3".to_string()));
    }

    #[test]
    fn run_args_labels_sorted() {
        let mut labels = HashMap::new();
        labels.insert("b".to_string(), "2".to_string());
        labels.insert("a".to_string(), "1".to_string());
        let spec = ContainerSpec {
            labels,
            ..ContainerSpec::new("c", "img")
        };
        let a = run_args(&spec);
        // Find label positions.
        let pos_a = a.iter().position(|x| x == "a=1").unwrap();
        let pos_b = a.iter().position(|x| x == "b=2").unwrap();
        assert!(pos_a < pos_b, "labels not emitted in sorted order");
    }

    #[test]
    fn run_args_env_extra_hosts_mounts_entrypoint_user() {
        let spec = ContainerSpec::new("c", "img")
            .with_user("1000:1000")
            .with_env("FOO", "bar")
            .with_extra_host("api.local", "10.0.0.5")
            .with_mount(Mount::Bind {
                source: "/h".into(),
                target: "/c".into(),
                read_only: true,
            })
            .with_entrypoint(vec!["/bin/sh".into(), "-c".into(), "echo hi".into()]);
        let a = run_args(&spec);
        assert!(a.windows(2).any(|w| w == ["--user", "1000:1000"]));
        assert!(a.windows(2).any(|w| w == ["--env", "FOO=bar"]));
        assert!(a.windows(2).any(|w| w == ["--add-host", "api.local:10.0.0.5"]));
        assert!(a.iter().any(|s| s.starts_with("type=bind")));
        // entrypoint pieces come after image.
        let image_pos = a.iter().position(|s| s == "img").unwrap();
        assert_eq!(&a[image_pos + 1..], &["/bin/sh", "-c", "echo hi"]);
    }

    #[test]
    fn build_args_format() {
        let dir = std::path::Path::new("/tmp/x");
        let a = build_args(dir, "ironclaw:abc");
        assert_eq!(a, vec!["build", "--tag", "ironclaw:abc", "/tmp/x"]);
    }

    #[test]
    fn write_build_context_creates_dockerfile_and_files() {
        let mut spec = ImageBuildSpec::new("test", "debian:12-slim");
        spec.extra_files = vec![ExtraFile::new("/etc/x.conf", "hello")];
        let dir = write_build_context(&spec).unwrap();
        let df = std::fs::read_to_string(dir.join("Dockerfile")).unwrap();
        assert!(df.contains("FROM debian:12-slim"));
        let copied = std::fs::read_to_string(dir.join("files").join("x.conf")).unwrap();
        assert_eq!(copied, "hello");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_build_context_no_files_skips_files_dir() {
        let spec = ImageBuildSpec::new("test", "debian:12-slim");
        let dir = write_build_context(&spec).unwrap();
        assert!(dir.join("Dockerfile").exists());
        assert!(!dir.join("files").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn new_uses_default_binary() {
        let rt = AppleContainerRuntime::new();
        assert_eq!(rt.binary(), DEFAULT_BIN);
    }

    #[test]
    fn with_binary_overrides_path() {
        let rt = AppleContainerRuntime::with_binary("/usr/local/bin/container");
        assert_eq!(rt.binary(), "/usr/local/bin/container");
    }

    #[test]
    fn default_matches_new() {
        let a = AppleContainerRuntime::default();
        let b = AppleContainerRuntime::new();
        assert_eq!(a.binary(), b.binary());
    }

    #[tokio::test]
    async fn probe_missing_binary_is_unavailable() {
        let rt = AppleContainerRuntime::with_binary("/no/such/binary-9b3f");
        let err = rt.probe().await.unwrap_err();
        assert!(matches!(err, RtError::Unavailable(_)));
    }

    #[tokio::test]
    async fn ensure_running_delegates_to_probe() {
        let rt = AppleContainerRuntime::with_binary("/no/such/binary-9b3f");
        let err = rt.ensure_running().await.unwrap_err();
        assert!(matches!(err, RtError::Unavailable(_)));
    }

    #[tokio::test]
    async fn cleanup_orphans_missing_binary_returns_container_error() {
        let rt = AppleContainerRuntime::with_binary("/no/such/binary-9b3f");
        let err = rt.cleanup_orphans("slug").await.unwrap_err();
        assert!(matches!(err, RtError::Container(_)));
    }

    #[tokio::test]
    async fn spawn_missing_binary_returns_container_error() {
        let rt = AppleContainerRuntime::with_binary("/no/such/binary-9b3f");
        let err = rt.spawn(ContainerSpec::new("c", "img")).await.unwrap_err();
        assert!(matches!(err, RtError::Container(_)));
    }

    #[tokio::test]
    async fn stop_missing_binary_returns_container_error() {
        let rt = AppleContainerRuntime::with_binary("/no/such/binary-9b3f");
        let err = rt.stop("c", Duration::from_secs(1)).await.unwrap_err();
        assert!(matches!(err, RtError::Container(_)));
    }

    #[tokio::test]
    async fn build_image_missing_binary_returns_container_error() {
        let rt = AppleContainerRuntime::with_binary("/no/such/binary-9b3f");
        let spec = ImageBuildSpec::new("r", "debian:12-slim");
        let err = rt.build_image(spec).await.unwrap_err();
        assert!(matches!(err, RtError::Container(_)));
    }

    #[tokio::test]
    async fn spawn_rejects_resource_limits() {
        let rt = AppleContainerRuntime::with_binary("/no/such/binary-9b3f");
        let spec = ContainerSpec::new("c", "img")
            .with_resource_limits(crate::ResourceLimits {
                cpus: Some(1.0),
                ..Default::default()
            });
        let err = rt.spawn(spec).await.unwrap_err();
        assert!(
            matches!(err, RtError::Unsupported(_)),
            "expected Unsupported, got {err:?}"
        );
    }

    #[tokio::test]
    async fn spawn_rejects_egress_allow() {
        let rt = AppleContainerRuntime::with_binary("/no/such/binary-9b3f");
        let spec = ContainerSpec::new("c", "img")
            .with_egress_allow(vec!["api.example.com:443".into()]);
        let err = rt.spawn(spec).await.unwrap_err();
        assert!(
            matches!(err, RtError::Unsupported(_)),
            "expected Unsupported, got {err:?}"
        );
    }

    #[tokio::test]
    async fn spawn_without_limits_falls_through_to_binary() {
        // No resource limits, no egress allow-list: the check passes and
        // we try to exec the (missing) binary → Container error, not Unsupported.
        let rt = AppleContainerRuntime::with_binary("/no/such/binary-9b3f");
        let spec = ContainerSpec::new("c", "img");
        let err = rt.spawn(spec).await.unwrap_err();
        assert!(
            matches!(err, RtError::Container(_)),
            "expected Container (missing binary), got {err:?}"
        );
    }
}
