//! `DockerRuntime` — Docker backend via [`bollard`] 0.18.
//!
//! Daemon I/O is unavoidable for the trait methods that hit
//! `/var/run/docker.sock`, so we keep tests focused on:
//!
//! * spec/build translation that doesn't require a daemon (covered by
//!   the `_translates_*` unit tests),
//! * the `RtError` mapping for missing/unreachable daemons (covered by
//!   the integration-style tests gated behind `docker-tests`).
//!
//! USTAR build-context packing is inlined (no `tar` workspace dep);
//! it's exercised by tests in `tar` submodule.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, LogsOptions, RemoveContainerOptions,
    StopContainerOptions,
};
use bollard::image::BuildImageOptions;
use bollard::models::{DeviceRequest, HostConfig, Mount as DockerMount, MountTypeEnum};
use bytes::Bytes;
use futures::stream::StreamExt;

use crate::build::ImageBuildSpec;
use crate::spec::{
    ContainerHandle, ContainerSpec, EgressMode, Mount, ResourceLimits, SandboxProfile,
};
use crate::{ContainerRuntime, RtError};

/// Bollard-backed Docker runtime.
pub struct DockerRuntime {
    docker: Docker,
}

impl std::fmt::Debug for DockerRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockerRuntime").finish_non_exhaustive()
    }
}

impl DockerRuntime {
    /// Connect using socket defaults (`/var/run/docker.sock` on Unix).
    pub fn connect() -> Result<Self, RtError> {
        let docker = Docker::connect_with_socket_defaults()
            .map_err(|e| RtError::Unavailable(format!("docker connect: {e}")))?;
        Ok(Self { docker })
    }

    /// Wrap an existing `Docker` client (used by tests that want to
    /// stub the daemon).
    #[must_use]
    pub fn from_docker(docker: Docker) -> Self {
        Self { docker }
    }
}

#[async_trait]
impl ContainerRuntime for DockerRuntime {
    async fn ensure_running(&self) -> Result<(), RtError> {
        self.docker
            .version()
            .await
            .map(|_| ())
            .map_err(|e| RtError::Unavailable(format!("docker version: {e}")))
    }

    async fn cleanup_orphans(&self, install_slug: &str) -> Result<(), RtError> {
        let mut filters = HashMap::new();
        filters.insert(
            "label".to_string(),
            vec![format!("copperclaw.install={install_slug}")],
        );
        let opts = ListContainersOptions::<String> {
            all: true,
            filters,
            ..Default::default()
        };
        let containers = self
            .docker
            .list_containers(Some(opts))
            .await
            .map_err(|e| RtError::Container(format!("list orphans: {e}")))?;

        for c in containers {
            let Some(id) = c.id else {
                continue;
            };
            let rm = RemoveContainerOptions {
                force: true,
                v: true,
                ..Default::default()
            };
            self.docker
                .remove_container(&id, Some(rm))
                .await
                .map_err(|e| RtError::Container(format!("remove orphan {id}: {e}")))?;
        }
        Ok(())
    }

    async fn spawn(&self, spec: ContainerSpec) -> Result<ContainerHandle, RtError> {
        let name = spec.name.clone();
        let cfg = container_config(&spec);
        let create = self
            .docker
            .create_container(
                Some(CreateContainerOptions::<String> {
                    name: name.clone(),
                    platform: None,
                }),
                cfg,
            )
            .await
            .map_err(|e| RtError::Container(format!("create container {name}: {e}")))?;

        self.docker
            .start_container::<String>(&name, None)
            .await
            .map_err(|e| RtError::Container(format!("start container {name}: {e}")))?;

        Ok(ContainerHandle::new(create.id, name))
    }

    async fn stop(&self, name: &str, grace: Duration) -> Result<(), RtError> {
        let secs = i64::try_from(grace.as_secs()).unwrap_or(i64::MAX);
        let opts = StopContainerOptions { t: secs };
        self.docker
            .stop_container(name, Some(opts))
            .await
            .map_err(|e| RtError::Container(format!("stop container {name}: {e}")))
    }

    async fn remove(&self, name: &str) -> Result<(), RtError> {
        // Stop first (best-effort; the container may already be down)
        // then force-remove. We don't propagate the stop error because
        // a missing-container 404 from stop is precisely what we want
        // when the container has already exited — the rm call gives
        // us the same outcome either way.
        let _ = self
            .docker
            .stop_container(name, Some(StopContainerOptions { t: 2 }))
            .await;
        let opts = RemoveContainerOptions {
            force: true,
            ..Default::default()
        };
        // 404 = already gone — fold into success.
        let res = self.docker.remove_container(name, Some(opts)).await;
        match res {
            Ok(())
            | Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(()),
            Err(e) => Err(RtError::Container(format!("remove container {name}: {e}"))),
        }
    }

    async fn build_image(&self, spec: ImageBuildSpec) -> Result<String, RtError> {
        // install_packages containment: refuse to dispatch a build whose
        // build-args / labels carry a credential-shaped name. This is the
        // structural guarantee that the broker token / provider key never
        // rides into a package build's env (where a malicious postinstall
        // could read it). Clean specs (the default install always produces
        // one) pass through unchanged.
        spec.assert_no_credentials()
            .map_err(|v| RtError::Container(v.to_string()))?;
        let image_tag = spec.image_tag();
        let context_bytes = build_context_tar(&spec)?;
        // Apply the containment network mode to the build's RUN steps. Bridge
        // (the default) leaves the daemon default; None is a real `--network=none`
        // cut (no postinstall egress at all).
        let networkmode: String = spec
            .containment
            .network
            .networkmode()
            .unwrap_or("")
            .to_string();
        // Non-secret build-args only — the credential guard above already
        // rejected anything credential-shaped.
        let buildargs: HashMap<String, String> = spec.build_args.iter().cloned().collect();
        let opts = BuildImageOptions::<String> {
            dockerfile: "Dockerfile".into(),
            t: image_tag.clone(),
            rm: true,
            networkmode,
            buildargs,
            ..Default::default()
        };
        let mut stream = self
            .docker
            .build_image(opts, None, Some(Bytes::from(context_bytes)));
        while let Some(item) = stream.next().await {
            let info = item.map_err(|e| RtError::Container(format!("build: {e}")))?;
            if let Some(err) = info.error {
                return Err(RtError::Container(format!("build error: {err}")));
            }
        }
        Ok(image_tag)
    }

    async fn image_exists(&self, tag: &str) -> Result<bool, RtError> {
        match self.docker.inspect_image(tag).await {
            Ok(_) => Ok(true),
            // `inspect_image` returns a 404 wrapped in `DockerResponseServerError`
            // for missing images. Anything else is a real runtime failure.
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(false),
            Err(e) => Err(RtError::Container(format!("inspect image {tag}: {e}"))),
        }
    }

    async fn image_digest(&self, tag: &str) -> Result<Option<String>, RtError> {
        // `.Id` is the content-addressable digest (sha256:<hex>) computed from
        // the image config + layer digests — the exact thing attestation wants
        // to pin. A 404 means the image isn't present locally ⇒ `None`.
        match self.docker.inspect_image(tag).await {
            Ok(info) => Ok(info.id),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(None),
            Err(e) => Err(RtError::Container(format!(
                "inspect image {tag} for digest: {e}"
            ))),
        }
    }

    async fn logs(&self, name: &str, tail: u32) -> Result<String, RtError> {
        // bollard streams `LogOutput` chunks for both stdout and stderr.
        // We want the tail of the combined stream as plain UTF-8 — the
        // host writes the result to a `crash-<utc>.log` file, so any
        // non-UTF-8 bytes are best lossy-decoded rather than dropped.
        let opts = LogsOptions::<String> {
            stdout: true,
            stderr: true,
            tail: tail.to_string(),
            follow: false,
            ..Default::default()
        };
        let mut stream = self.docker.logs(name, Some(opts));
        let mut buf = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => {
                    buf.push_str(&String::from_utf8_lossy(chunk.as_ref()));
                }
                Err(e) => {
                    return Err(RtError::Container(format!("fetch logs {name}: {e}")));
                }
            }
        }
        Ok(buf)
    }
}

// ---- pure translation helpers ------------------------------------------

/// Translate a [`ContainerSpec`] into a bollard [`Config`].
///
/// Resource limits from [`ContainerSpec::resource_limits`] are mapped:
/// - `cpus` → `HostConfig::nano_cpus` (cpus × 10⁹)
/// - `memory_mb` → `HostConfig::memory` (MiB × 2²⁰)
/// - `pids_limit` → `HostConfig::pids_limit`
///
/// Egress posture ([`ContainerSpec::egress_mode`]) maps to the
/// `HostConfig::network_mode` chosen by [`egress_network_mode`]: under
/// [`EgressMode::DenyDefault`] with an empty allow-list the container is cut
/// off the network entirely (`"none"`, a real bollard-enforced deny). Any
/// other combination leaves networking on the default bridge — per-host
/// filtering of a non-empty allow-list is deferred to a future netns +
/// nftables pass and is NOT enforced here. The allow-list is carried on the
/// spec for that later pass and for operator visibility.
pub(crate) fn container_config(spec: &ContainerSpec) -> Config<String> {
    let env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let labels: HashMap<String, String> = spec.labels.clone();

    let mut mounts: Vec<DockerMount> = spec.mounts.iter().map(translate_mount).collect();

    // Phase 0a v2 DNS filtering: pin /etc/resolv.conf to the host-rendered
    // filtering-resolver config (read-only) so a deny-default container's stub
    // resolver can only reach the filtering resolver. Only present when the
    // host set it (deny-default), so default spawns are unaffected. Appended
    // last so it can't be shadowed by an earlier mount at the same target.
    if let Some(resolv_src) = spec.resolv_conf_source.as_deref() {
        mounts.push(DockerMount {
            target: Some("/etc/resolv.conf".to_string()),
            source: Some(resolv_src.to_string()),
            typ: Some(MountTypeEnum::BIND),
            read_only: Some(true),
            ..Default::default()
        });
    }

    let extra_hosts: Vec<String> = spec
        .extra_hosts
        .iter()
        .map(|(host, ip)| format!("{host}:{ip}"))
        .collect();

    let mut host_config = host_config_with_limits(
        &spec.resource_limits,
        mounts,
        extra_hosts,
        spec.gpu_passthrough,
        egress_network_mode(spec.egress_mode, &spec.egress_allow),
    );

    // Phase 5a stronger-sandbox security floor. Only present when the caller
    // attached a sandbox profile (the headless-browser child container);
    // default spawns leave `host_config` untouched. The profile's `runtime`
    // is expected to already be the RESOLVED runtime from
    // [`crate::select_sandbox_runtime`] — the host probes availability and
    // narrows the request before building the spec, so a microVM runtime that
    // isn't installed never reaches `runtime:` here.
    if let Some(profile) = spec.sandbox.as_ref() {
        apply_sandbox_security(&mut host_config, profile);
    }

    Config {
        image: Some(spec.image.clone()),
        env: if env.is_empty() { None } else { Some(env) },
        entrypoint: if spec.entrypoint.is_empty() {
            None
        } else {
            Some(spec.entrypoint.clone())
        },
        user: spec.user.clone(),
        working_dir: spec.working_dir.clone(),
        labels: if labels.is_empty() {
            None
        } else {
            Some(labels)
        },
        host_config: Some(host_config),
        ..Default::default()
    }
}

/// Decide the bollard `HostConfig::network_mode` for an egress posture.
///
/// Returns `Some("none")` only under [`EgressMode::DenyDefault`] with an
/// empty allow-list — the single case bollard can enforce as a hard egress
/// cut through `create_container`. Every other case returns `None` (Docker's
/// default bridge), because bollard cannot express per-host L3/L4 filtering;
/// honoring a non-empty allow-list is deferred to the netns + nftables pass.
#[must_use]
pub(crate) fn egress_network_mode(mode: EgressMode, allow: &[String]) -> Option<String> {
    match mode {
        EgressMode::DenyDefault if allow.is_empty() => Some("none".to_string()),
        EgressMode::DenyDefault | EgressMode::AllowAll => None,
    }
}

/// Build the `HostConfig` applying any resource limits. `network_mode` is the
/// pre-decided egress posture (see [`egress_network_mode`]); `None` leaves the
/// runtime default bridge.
pub(crate) fn host_config_with_limits(
    limits: &ResourceLimits,
    mounts: Vec<DockerMount>,
    extra_hosts: Vec<String>,
    gpu_passthrough: bool,
    network_mode: Option<String>,
) -> HostConfig {
    // Docker's CPU quota is in nano-CPUs (1 CPU = 1_000_000_000 nano-CPUs).
    // The f64->i64 truncation is intentional: fractional nano-CPUs are meaningless.
    // Wrap for memory_mb and pids_limit: practical values are well below i64::MAX.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let nano_cpus = limits.cpus.map(|c| (c * 1_000_000_000.0) as i64);
    // Memory is in bytes.
    #[allow(clippy::cast_possible_wrap)]
    let memory = limits.memory_mb.map(|mb| (mb * 1024 * 1024) as i64);
    #[allow(clippy::cast_possible_wrap)]
    let pids_limit = limits.pids_limit.map(|p| p as i64);

    // `--gpus all` equivalent. `count=-1` means "every device the
    // driver enumerates" — Nvidia's runtime hooks resolve this at
    // container start. Requires `nvidia-container-toolkit` on the host.
    let device_requests = if gpu_passthrough {
        Some(vec![DeviceRequest {
            driver: Some("nvidia".to_string()),
            count: Some(-1),
            capabilities: Some(vec![vec!["gpu".to_string()]]),
            ..Default::default()
        }])
    } else {
        None
    };

    HostConfig {
        mounts: Some(mounts),
        extra_hosts: Some(extra_hosts),
        nano_cpus,
        memory,
        pids_limit,
        device_requests,
        network_mode,
        ..Default::default()
    }
}

/// Apply a [`SandboxProfile`] onto a bollard [`HostConfig`], translating the
/// stronger-sandbox knobs into the engine's security primitives (Phase 5a).
///
/// Pure mutation, so it is unit-tested without a daemon. What each field maps
/// to:
///
/// * `runtime` → `HostConfig::runtime` *only* when the resolved
///   [`SandboxRuntime`] names an external OCI runtime
///   ([`SandboxRuntime::oci_runtime_name`]). [`SandboxRuntime::HardenedRunc`]
///   and [`SandboxRuntime::Default`] leave `runtime: None` (engine default) —
///   `HardenedRunc` hardens via the security options below rather than by
///   swapping the runtime binary.
/// * `cap_drop_all` → `cap_drop: ["ALL"]`.
/// * `no_new_privileges` → `security_opt: ["no-new-privileges:true"]`.
/// * `seccomp_profile` → `security_opt: ["seccomp=<name>"]` when named; absent
///   means the engine's (already restrictive) default seccomp profile.
/// * `userns_remap` → `userns_mode: "<label>"` (Docker reads the daemon-side
///   subordinate-uid map for the label).
pub(crate) fn apply_sandbox_security(host: &mut HostConfig, profile: &SandboxProfile) {
    // Select the OCI runtime binary only for runtimes that need one. The
    // resolved profile should already have narrowed an unavailable microVM
    // runtime down to what the host has — see `select_sandbox_runtime`.
    if let Some(rt_name) = profile.runtime.oci_runtime_name() {
        host.runtime = Some(rt_name.to_string());
    }

    if profile.cap_drop_all {
        host.cap_drop = Some(vec!["ALL".to_string()]);
    }

    let mut security_opt: Vec<String> = Vec::new();
    if profile.no_new_privileges {
        security_opt.push("no-new-privileges:true".to_string());
    }
    if let Some(name) = profile.seccomp_profile.as_deref() {
        security_opt.push(format!("seccomp={name}"));
    }
    if !security_opt.is_empty() {
        host.security_opt = Some(security_opt);
    }

    if let Some(label) = profile.userns_remap.as_deref() {
        host.userns_mode = Some(label.to_string());
    }
}

/// Translate one [`Mount`] into a bollard [`DockerMount`].
pub(crate) fn translate_mount(m: &Mount) -> DockerMount {
    match m {
        Mount::Bind {
            source,
            target,
            read_only,
        } => DockerMount {
            target: Some(target.clone()),
            source: Some(source.clone()),
            typ: Some(MountTypeEnum::BIND),
            read_only: Some(*read_only),
            ..Default::default()
        },
        Mount::Volume {
            name,
            target,
            read_only,
        } => DockerMount {
            target: Some(target.clone()),
            source: Some(name.clone()),
            typ: Some(MountTypeEnum::VOLUME),
            read_only: Some(*read_only),
            ..Default::default()
        },
        Mount::Tmpfs { target, size_bytes } => {
            let tmpfs_options = if *size_bytes == 0 {
                None
            } else {
                Some(bollard::models::MountTmpfsOptions {
                    size_bytes: i64::try_from(*size_bytes).ok(),
                    mode: None,
                    ..Default::default()
                })
            };
            DockerMount {
                target: Some(target.clone()),
                typ: Some(MountTypeEnum::TMPFS),
                tmpfs_options,
                ..Default::default()
            }
        }
    }
}

/// Pack the build context (Dockerfile + extra files) into a USTAR
/// tarball that bollard can POST to `/build`.
pub(crate) fn build_context_tar(spec: &ImageBuildSpec) -> Result<Vec<u8>, RtError> {
    let mut out = Vec::new();
    let dockerfile = spec.dockerfile();
    tar::append(&mut out, "Dockerfile", 0o644, dockerfile.as_bytes())
        .map_err(|e| RtError::Container(format!("tar Dockerfile: {e}")))?;
    for f in &spec.extra_files {
        let basename = f
            .path
            .file_name()
            .map_or_else(|| "file".to_string(), |s| s.to_string_lossy().into_owned());
        let name = format!("files/{basename}");
        let mode = if f.mode == 0 { 0o644 } else { f.mode };
        tar::append(&mut out, &name, mode, &f.contents)
            .map_err(|e| RtError::Container(format!("tar {name}: {e}")))?;
    }
    tar::finish(&mut out);
    Ok(out)
}

/// Minimal USTAR writer. Just enough to feed `docker build`.
mod tar {
    /// Each tar block is 512 bytes.
    const BLOCK: usize = 512;

    /// Errors from the USTAR writer. These bubble up out of
    /// [`super::build_context_tar`] as [`crate::RtError::Container`]
    /// so callers see an actionable failure instead of a silently
    /// truncated path (which would surface later as an opaque image
    /// build error or, worse, a path collision).
    #[derive(Debug)]
    pub enum TarError {
        /// Path is longer than the USTAR maximum (256 bytes total —
        /// `name` ≤ 100 + `prefix` ≤ 155 + the `/` separator).
        PathTooLong { path: String, len: usize },
        /// Path is 101..=256 bytes but cannot be split into a
        /// `name`/`prefix` pair satisfying the USTAR field-width
        /// limits. Happens e.g. with a single 200-byte basename, or
        /// when the only viable split point lands in the middle of a
        /// multi-byte UTF-8 sequence (we only split at ASCII `/`).
        NoValidSplit { path: String },
    }

    impl std::fmt::Display for TarError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::PathTooLong { path, len } => {
                    write!(f, "USTAR path exceeds 256 bytes (got {len}): {path}")
                }
                Self::NoValidSplit { path } => write!(
                    f,
                    "USTAR path cannot be split into name (<=100) + prefix (<=155): {path}"
                ),
            }
        }
    }

    impl std::error::Error for TarError {}

    /// USTAR `name` field width.
    const NAME_MAX: usize = 100;
    /// USTAR `prefix` field width (offset 345).
    const PREFIX_MAX: usize = 155;

    /// Split a long path into (`prefix`, `name`) per the USTAR spec.
    /// The final archived path is `format!("{prefix}/{name}")`.
    ///
    /// Returns the components only if both fit their respective fields
    /// (≤155, ≤100) and a split point exists at an ASCII `/`. Callers
    /// for paths ≤100 bytes should write `name` directly and skip
    /// `prefix`.
    fn split_long(path: &str) -> Option<(&str, &str)> {
        let bytes = path.as_bytes();
        if bytes.len() <= NAME_MAX {
            // Caller should not be splitting this — handled upstream.
            return Some(("", path));
        }
        // Find the LAST `/` such that the suffix after it is ≤100 and
        // the prefix before it is ≤155.
        for i in (0..bytes.len()).rev() {
            if bytes[i] != b'/' {
                continue;
            }
            let prefix = &bytes[..i];
            let name = &bytes[i + 1..];
            if prefix.len() <= PREFIX_MAX && name.len() <= NAME_MAX && !name.is_empty() {
                // Safe to slice — we only split at ASCII `/`, which is
                // never inside a multi-byte UTF-8 sequence.
                return Some((
                    std::str::from_utf8(prefix).ok()?,
                    std::str::from_utf8(name).ok()?,
                ));
            }
        }
        None
    }

    /// Append a single file entry to `out`.
    ///
    /// Returns [`TarError`] when `name` exceeds the USTAR addressable
    /// limit (256 bytes split as 155 prefix + `/` + 100 name) instead
    /// of silently truncating, which previously produced opaque
    /// build failures or path collisions.
    pub fn append(out: &mut Vec<u8>, name: &str, mode: u32, body: &[u8]) -> Result<(), TarError> {
        let mut header = [0u8; BLOCK];

        let nbytes = name.as_bytes();
        if nbytes.len() <= NAME_MAX {
            // Short path: write entirely into `name`, leave `prefix`
            // (offset 345, 155 bytes) zero-filled.
            header[..nbytes.len()].copy_from_slice(nbytes);
        } else if nbytes.len() <= NAME_MAX + 1 + PREFIX_MAX {
            // Medium path: split at the LAST `/` that fits both fields.
            let (prefix, suffix) = split_long(name).ok_or_else(|| TarError::NoValidSplit {
                path: name.to_string(),
            })?;
            header[..suffix.len()].copy_from_slice(suffix.as_bytes());
            header[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
        } else {
            return Err(TarError::PathTooLong {
                path: name.to_string(),
                len: nbytes.len(),
            });
        }

        // mode (8 bytes, octal ASCII + nul)
        write_octal(&mut header[100..108], u64::from(mode), 7);
        // uid (8), gid (8) — zero-filled (octal "0       \0")
        write_octal(&mut header[108..116], 0, 7);
        write_octal(&mut header[116..124], 0, 7);
        // size (12)
        write_octal(&mut header[124..136], body.len() as u64, 11);
        // mtime (12)
        write_octal(&mut header[136..148], 0, 11);
        // checksum placeholder = 8 spaces
        for b in &mut header[148..156] {
            *b = b' ';
        }
        // typeflag '0' = regular file
        header[156] = b'0';
        // magic + version: "ustar\0" + "00"
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");

        // checksum: sum of all bytes in header (with placeholder spaces).
        // Per USTAR, the field is six octal digits, then NUL, then space.
        // This covers the full 512-byte block, so the `prefix` bytes
        // we may have just written at offset 345 are included.
        let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
        write_octal(&mut header[148..155], u64::from(sum), 6);
        header[155] = b' ';

        out.extend_from_slice(&header);
        out.extend_from_slice(body);
        // pad to block boundary
        let pad = (BLOCK - (body.len() % BLOCK)) % BLOCK;
        out.extend(std::iter::repeat_n(0u8, pad));
        Ok(())
    }

    /// Write `value` as a zero-padded octal ASCII string of width
    /// `width`, followed by a NUL.
    fn write_octal(slot: &mut [u8], value: u64, width: usize) {
        let s = format!("{value:0width$o}");
        let bytes = s.as_bytes();
        let n = bytes.len().min(width);
        slot[..n].copy_from_slice(&bytes[..n]);
        if slot.len() > width {
            slot[width] = 0;
        }
    }

    /// Two zero blocks mark end-of-archive.
    pub fn finish(out: &mut Vec<u8>) {
        out.extend(std::iter::repeat_n(0u8, BLOCK * 2));
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn single_entry_layout() {
            let mut buf = Vec::new();
            append(&mut buf, "Dockerfile", 0o644, b"FROM x\n").unwrap();
            finish(&mut buf);
            // header (512) + body padded to 512 + 2*512 footer = 2048
            assert_eq!(buf.len(), 512 + 512 + 1024);
            // first 10 bytes = "Dockerfile"
            assert_eq!(&buf[..10], b"Dockerfile");
            // typeflag at offset 156
            assert_eq!(buf[156], b'0');
            // magic at 257
            assert_eq!(&buf[257..263], b"ustar\0");
            // body starts at 512
            assert_eq!(&buf[512..519], b"FROM x\n");
        }

        #[test]
        fn multiple_entries_pad_to_blocks() {
            let mut buf = Vec::new();
            append(&mut buf, "a", 0o644, b"x").unwrap();
            append(&mut buf, "b", 0o644, b"y").unwrap();
            finish(&mut buf);
            // 2 * (header + padded body) + 2 footer blocks
            assert_eq!(buf.len(), 4 * 512 + 2 * 512);
        }

        #[test]
        fn checksum_matches_header_bytes_sum() {
            let mut buf = Vec::new();
            append(&mut buf, "a", 0o644, b"").unwrap();
            // After write, checksum field at 148..156 should be the
            // octal sum of every byte in the header (where 148..156
            // was spaces during the computation).
            let header = &buf[..512];
            // recompute expected sum with placeholder spaces in 148..156
            let mut work = [0u8; 512];
            work.copy_from_slice(header);
            for b in &mut work[148..156] {
                *b = b' ';
            }
            let expected: u32 = work.iter().map(|b| u32::from(*b)).sum();
            // The checksum slot at 148..156 is "<6 octal digits><NUL><SPACE>"
            // per USTAR. We only parse the 6 digits.
            let recorded = std::str::from_utf8(&header[148..154]).unwrap();
            let parsed = u32::from_str_radix(recorded, 8).unwrap();
            assert_eq!(parsed, expected);
            // NUL + SPACE trailer.
            assert_eq!(header[154], 0);
            assert_eq!(header[155], b' ');
        }

        #[test]
        fn short_name_written_inline_prefix_empty() {
            // Pre-fix path (≤100 bytes): goes entirely into `name`,
            // `prefix` at offset 345..500 stays zero.
            let mut buf = Vec::new();
            let name = "files/short.txt";
            append(&mut buf, name, 0o644, b"hi").unwrap();
            assert_eq!(&buf[..name.len()], name.as_bytes());
            assert!(buf[345..500].iter().all(|&b| b == 0));
        }

        #[test]
        fn medium_name_split_into_prefix_and_name() {
            // 101..=256 bytes: must split at a `/` so the resulting
            // `name` ≤ 100 and `prefix` ≤ 155. Choose a path whose
            // last `/` lands inside the splittable window.
            let dir = "a".repeat(130);
            let base = "b".repeat(80);
            let name = format!("{dir}/{base}");
            assert!(name.len() > 100 && name.len() <= 256);

            let mut buf = Vec::new();
            append(&mut buf, &name, 0o644, b"x").unwrap();

            // `name` at offset 0..100, `prefix` at offset 345..500.
            let name_field = &buf[..100];
            let prefix_field = &buf[345..500];
            let name_str = std::str::from_utf8(&name_field[..base.len()]).unwrap();
            let prefix_str = std::str::from_utf8(&prefix_field[..dir.len()]).unwrap();
            assert_eq!(name_str, base);
            assert_eq!(prefix_str, dir);
            // Reconstruct: `prefix/name` (the readback convention).
            let reconstructed = format!("{prefix_str}/{name_str}");
            assert_eq!(reconstructed, name);
            // Padding bytes past the written content must be zero so
            // the field is properly NUL-terminated.
            assert!(name_field[base.len()..].iter().all(|&b| b == 0));
            assert!(prefix_field[dir.len()..].iter().all(|&b| b == 0));
        }

        #[test]
        fn medium_name_checksum_includes_prefix_bytes() {
            // The checksum sums the WHOLE 512-byte header. If the
            // `prefix` write isn't included, the recorded sum won't
            // match a re-computation that has those bytes filled in.
            let dir = "d".repeat(120);
            let base = "f".repeat(70);
            let name = format!("{dir}/{base}");

            let mut buf = Vec::new();
            append(&mut buf, &name, 0o644, b"").unwrap();
            let header = &buf[..512];

            let mut work = [0u8; 512];
            work.copy_from_slice(header);
            for b in &mut work[148..156] {
                *b = b' ';
            }
            let expected: u32 = work.iter().map(|b| u32::from(*b)).sum();
            let recorded = std::str::from_utf8(&header[148..154]).unwrap();
            let parsed = u32::from_str_radix(recorded, 8).unwrap();
            assert_eq!(parsed, expected);
        }

        #[test]
        fn too_long_name_returns_error() {
            // > 256 bytes: no valid USTAR encoding exists.
            let name = "x".repeat(300);
            let mut buf = Vec::new();
            let err = append(&mut buf, &name, 0o644, b"").unwrap_err();
            assert!(matches!(err, TarError::PathTooLong { .. }));
        }

        #[test]
        fn medium_name_no_split_point_returns_error() {
            // 101..=256 bytes but no `/` in the right window: e.g. a
            // single 200-byte basename. Cannot fit in `name` (max
            // 100) and there's no separator to peel off into `prefix`.
            let name = "z".repeat(200);
            let mut buf = Vec::new();
            let err = append(&mut buf, &name, 0o644, b"").unwrap_err();
            assert!(matches!(err, TarError::NoValidSplit { .. }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::ExtraFile;
    use crate::spec::SandboxRuntime;

    #[test]
    fn translates_bind_mount() {
        let dm = translate_mount(&Mount::Bind {
            source: "/h".into(),
            target: "/c".into(),
            read_only: true,
        });
        assert_eq!(dm.typ, Some(MountTypeEnum::BIND));
        assert_eq!(dm.source.as_deref(), Some("/h"));
        assert_eq!(dm.target.as_deref(), Some("/c"));
        assert_eq!(dm.read_only, Some(true));
    }

    #[test]
    fn translates_volume_mount() {
        let dm = translate_mount(&Mount::Volume {
            name: "vol".into(),
            target: "/v".into(),
            read_only: false,
        });
        assert_eq!(dm.typ, Some(MountTypeEnum::VOLUME));
        assert_eq!(dm.source.as_deref(), Some("vol"));
        assert_eq!(dm.target.as_deref(), Some("/v"));
        assert_eq!(dm.read_only, Some(false));
    }

    #[test]
    fn translates_tmpfs_mount_no_size() {
        let dm = translate_mount(&Mount::Tmpfs {
            target: "/tmp".into(),
            size_bytes: 0,
        });
        assert_eq!(dm.typ, Some(MountTypeEnum::TMPFS));
        assert!(dm.tmpfs_options.is_none());
    }

    #[test]
    fn translates_tmpfs_mount_with_size() {
        let dm = translate_mount(&Mount::Tmpfs {
            target: "/tmp".into(),
            size_bytes: 1024,
        });
        assert_eq!(dm.typ, Some(MountTypeEnum::TMPFS));
        let opts = dm.tmpfs_options.expect("tmpfs opts");
        assert_eq!(opts.size_bytes, Some(1024));
    }

    #[test]
    fn container_config_minimal() {
        let spec = ContainerSpec::new("c", "alpine:3");
        let cfg = container_config(&spec);
        assert_eq!(cfg.image.as_deref(), Some("alpine:3"));
        assert!(cfg.env.is_none());
        assert!(cfg.entrypoint.is_none());
        assert!(cfg.labels.is_none());
        let host = cfg.host_config.expect("host config");
        assert_eq!(host.extra_hosts.as_deref(), Some(&[] as &[String]));
        assert_eq!(host.mounts.as_deref(), Some(&[] as &[DockerMount]));
        // No resource limits set → all None.
        assert!(host.nano_cpus.is_none());
        assert!(host.memory.is_none());
        assert!(host.pids_limit.is_none());
    }

    #[test]
    fn container_config_resource_limits_applied() {
        let spec = ContainerSpec::new("c", "img").with_resource_limits(ResourceLimits {
            cpus: Some(1.5),
            memory_mb: Some(512),
            pids_limit: Some(256),
        });
        let cfg = container_config(&spec);
        let host = cfg.host_config.unwrap();
        // 1.5 CPUs = 1_500_000_000 nano-CPUs
        assert_eq!(host.nano_cpus, Some(1_500_000_000));
        // 512 MiB = 512 * 1024 * 1024 bytes = 536870912
        assert_eq!(host.memory, Some(512 * 1024 * 1024));
        assert_eq!(host.pids_limit, Some(256));
    }

    #[test]
    fn container_config_allow_all_leaves_network_mode_unset() {
        // Default posture (AllowAll) must not touch network_mode — the
        // legacy spawn path keeps the default bridge.
        let spec = ContainerSpec::new("c", "img");
        let cfg = container_config(&spec);
        let host = cfg.host_config.unwrap();
        assert!(host.network_mode.is_none());
    }

    #[test]
    fn container_config_deny_default_empty_allow_cuts_network() {
        // DenyDefault with no allow-list is the one bollard-enforceable
        // hard cut: network_mode "none".
        let spec = ContainerSpec::new("c", "img").with_egress_mode(EgressMode::DenyDefault);
        let cfg = container_config(&spec);
        let host = cfg.host_config.unwrap();
        assert_eq!(host.network_mode.as_deref(), Some("none"));
    }

    #[test]
    fn container_config_deny_default_with_allow_keeps_network() {
        // DenyDefault with a non-empty allow-list cannot be filtered by
        // bollard, so the container stays networked (per-host filtering is
        // deferred to the netns+nftables pass). The allow-list is carried.
        let spec = ContainerSpec::new("c", "img")
            .with_egress_mode(EgressMode::DenyDefault)
            .with_egress_allow(vec!["api.anthropic.com:443".into()]);
        let cfg = container_config(&spec);
        let host = cfg.host_config.unwrap();
        assert!(
            host.network_mode.is_none(),
            "non-empty allow-list under deny-default must NOT hard-cut the network"
        );
    }

    // ── Phase 5a stronger-sandbox security translation ───────────────────

    #[test]
    fn container_config_no_sandbox_leaves_security_unset() {
        // Default spawn: no sandbox profile, so none of the security knobs
        // are touched — behaviour unchanged.
        let spec = ContainerSpec::new("c", "img");
        let host = container_config(&spec).host_config.unwrap();
        assert!(host.runtime.is_none());
        assert!(host.cap_drop.is_none());
        assert!(host.security_opt.is_none());
        assert!(host.userns_mode.is_none());
    }

    #[test]
    fn apply_sandbox_security_hardened_runc_floor() {
        // HardenedRunc: no external runtime (engine default), but the full
        // security floor applied.
        let mut host = HostConfig::default();
        apply_sandbox_security(
            &mut host,
            &SandboxProfile::hardened(SandboxRuntime::HardenedRunc),
        );
        assert!(
            host.runtime.is_none(),
            "hardened-runc must not swap the OCI runtime"
        );
        assert_eq!(host.cap_drop.as_deref(), Some(&["ALL".to_string()][..]));
        let so = host.security_opt.unwrap();
        assert!(so.iter().any(|s| s == "no-new-privileges:true"));
        assert_eq!(host.userns_mode.as_deref(), Some("copperclaw-browser"));
    }

    #[test]
    fn apply_sandbox_security_runsc_sets_runtime() {
        let mut host = HostConfig::default();
        apply_sandbox_security(&mut host, &SandboxProfile::hardened(SandboxRuntime::Runsc));
        assert_eq!(host.runtime.as_deref(), Some("runsc"));
        // The security floor still applies on top of the gVisor runtime.
        assert_eq!(host.cap_drop.as_deref(), Some(&["ALL".to_string()][..]));
    }

    #[test]
    fn apply_sandbox_security_named_seccomp_profile() {
        let mut host = HostConfig::default();
        let profile = SandboxProfile::hardened(SandboxRuntime::HardenedRunc)
            .with_seccomp_profile("browser-strict");
        apply_sandbox_security(&mut host, &profile);
        let so = host.security_opt.unwrap();
        assert!(
            so.iter().any(|s| s == "seccomp=browser-strict"),
            "named seccomp profile must reach security_opt: {so:?}"
        );
    }

    #[test]
    fn container_config_threads_sandbox_into_host_config() {
        let spec = ContainerSpec::new("c", "img")
            .with_sandbox(SandboxProfile::hardened(SandboxRuntime::Kata));
        let host = container_config(&spec).host_config.unwrap();
        assert_eq!(host.runtime.as_deref(), Some("kata-runtime"));
        assert_eq!(host.cap_drop.as_deref(), Some(&["ALL".to_string()][..]));
        assert_eq!(host.userns_mode.as_deref(), Some("copperclaw-browser"));
    }

    #[test]
    fn container_config_no_resolv_conf_by_default() {
        // Default spawn (no DNS filter) must not add an /etc/resolv.conf mount
        // — the legacy behaviour is untouched.
        let spec = ContainerSpec::new("c", "img");
        let cfg = container_config(&spec);
        let host = cfg.host_config.unwrap();
        let mounts = host.mounts.unwrap();
        assert!(
            !mounts
                .iter()
                .any(|m| m.target.as_deref() == Some("/etc/resolv.conf")),
            "no resolv.conf mount unless DNS filtering is pinned"
        );
    }

    #[test]
    fn container_config_pins_resolv_conf_read_only_when_set() {
        // DNS filtering on: the host-rendered resolv.conf is bound read-only at
        // /etc/resolv.conf so the container can only reach the filter resolver.
        let spec = ContainerSpec::new("c", "img")
            .with_egress_mode(EgressMode::DenyDefault)
            .with_resolv_conf_source("/host/sessions/s1/resolv.conf");
        let cfg = container_config(&spec);
        let host = cfg.host_config.unwrap();
        let mounts = host.mounts.unwrap();
        let resolv = mounts
            .iter()
            .find(|m| m.target.as_deref() == Some("/etc/resolv.conf"))
            .expect("resolv.conf mount present");
        assert_eq!(
            resolv.source.as_deref(),
            Some("/host/sessions/s1/resolv.conf")
        );
        assert_eq!(resolv.read_only, Some(true));
        assert_eq!(resolv.typ, Some(MountTypeEnum::BIND));
    }

    #[test]
    fn egress_network_mode_matrix() {
        assert_eq!(egress_network_mode(EgressMode::AllowAll, &[]), None);
        assert_eq!(
            egress_network_mode(EgressMode::AllowAll, &["x:1".to_string()]),
            None
        );
        assert_eq!(
            egress_network_mode(EgressMode::DenyDefault, &[]),
            Some("none".to_string())
        );
        assert_eq!(
            egress_network_mode(EgressMode::DenyDefault, &["x:1".to_string()]),
            None
        );
    }

    #[test]
    fn container_config_partial_resource_limits() {
        // Only memory set; others absent.
        let spec = ContainerSpec::new("c", "img").with_resource_limits(ResourceLimits {
            cpus: None,
            memory_mb: Some(128),
            pids_limit: None,
        });
        let cfg = container_config(&spec);
        let host = cfg.host_config.unwrap();
        assert!(host.nano_cpus.is_none());
        assert_eq!(host.memory, Some(128 * 1024 * 1024));
        assert!(host.pids_limit.is_none());
    }

    #[test]
    fn container_config_full() {
        let spec = ContainerSpec::new("c", "img")
            .with_user("nobody")
            .with_working_dir("/data")
            .with_env("A", "1")
            .with_env("B", "2")
            .with_label("k", "v")
            .with_extra_host("api", "1.2.3.4")
            .with_mount(Mount::Bind {
                source: "/h".into(),
                target: "/c".into(),
                read_only: false,
            })
            .with_entrypoint(vec!["sh".into()]);
        let cfg = container_config(&spec);
        assert_eq!(cfg.user.as_deref(), Some("nobody"));
        assert_eq!(cfg.working_dir.as_deref(), Some("/data"));
        assert_eq!(
            cfg.env.as_deref(),
            Some(&["A=1".to_string(), "B=2".to_string()][..])
        );
        assert_eq!(cfg.entrypoint.as_deref(), Some(&["sh".to_string()][..]));
        let labels = cfg.labels.expect("labels");
        assert_eq!(labels.get("k").map(String::as_str), Some("v"));
        let host = cfg.host_config.expect("host");
        assert_eq!(
            host.extra_hosts.as_deref(),
            Some(&["api:1.2.3.4".to_string()][..])
        );
        assert_eq!(host.mounts.expect("mounts").len(), 1);
    }

    #[test]
    fn build_context_tar_contains_dockerfile() {
        let spec = ImageBuildSpec::new("r", "debian:12-slim");
        let tar = build_context_tar(&spec).unwrap();
        // The Dockerfile name lives at offset 0 of the first header.
        assert_eq!(&tar[..10], b"Dockerfile");
    }

    #[test]
    fn build_context_tar_with_extra_files() {
        let mut spec = ImageBuildSpec::new("r", "debian:12-slim");
        spec.extra_files = vec![ExtraFile::new("/etc/x.conf", "hi")];
        let tar = build_context_tar(&spec).unwrap();
        // Look for "files/x.conf" in the second header (offset depends
        // on Dockerfile body length; do a substring search).
        let hay = tar.windows(12).any(|w| w == b"files/x.conf");
        assert!(hay);
    }

    #[test]
    fn build_context_tar_terminates_with_zero_blocks() {
        let spec = ImageBuildSpec::new("r", "debian:12-slim");
        let tar = build_context_tar(&spec).unwrap();
        let n = tar.len();
        // Last 1024 bytes are end-of-archive markers.
        assert!(tar[n - 1024..].iter().all(|b| *b == 0));
    }

    #[test]
    fn build_context_tar_rejects_oversize_extra_file_name() {
        let mut spec = ImageBuildSpec::new("r", "debian:12-slim");
        // Force a basename so long that `files/<base>` > 256 bytes.
        let huge = "z".repeat(260);
        spec.extra_files = vec![ExtraFile::new(format!("/etc/{huge}"), "hi")];
        let err = build_context_tar(&spec).unwrap_err();
        match err {
            RtError::Container(msg) => assert!(msg.contains("USTAR path"), "{msg}"),
            other => panic!("expected Container, got {other:?}"),
        }
    }

    #[test]
    fn debug_impl_is_non_exhaustive() {
        // We can't construct a real Docker without a daemon, but
        // confirm the Debug impl compiles + writes the struct name.
        let s = format!("{DummyDebug:?}");
        assert!(s.contains("DockerRuntime"));
    }

    struct DummyDebug;
    impl std::fmt::Debug for DummyDebug {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("DockerRuntime").finish_non_exhaustive()
        }
    }

    // ---- daemon-gated integration tests ---------------------------------
    //
    // Run with `cargo test -p copperclaw-container-rt --features docker-tests`
    // on a host where `/var/run/docker.sock` is reachable. Skipped by
    // default so CI without a daemon stays green.

    #[cfg_attr(not(feature = "docker-tests"), ignore)]
    #[tokio::test]
    async fn ensure_running_against_daemon() {
        let rt = DockerRuntime::connect().expect("connect");
        rt.ensure_running().await.unwrap();
    }

    #[cfg_attr(not(feature = "docker-tests"), ignore)]
    #[tokio::test]
    async fn cleanup_orphans_against_daemon_is_idempotent() {
        let rt = DockerRuntime::connect().expect("connect");
        rt.cleanup_orphans("copperclaw-tests-no-such-slug")
            .await
            .unwrap();
    }
}
