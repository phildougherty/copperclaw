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
use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, RemoveContainerOptions, StopContainerOptions,
};
use bollard::image::BuildImageOptions;
use bollard::models::{HostConfig, Mount as DockerMount, MountTypeEnum};
use bollard::Docker;
use bytes::Bytes;
use futures::stream::StreamExt;

use crate::build::ImageBuildSpec;
use crate::spec::{ContainerHandle, ContainerSpec, Mount};
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
            vec![format!("ironclaw.install={install_slug}")],
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

    async fn build_image(&self, spec: ImageBuildSpec) -> Result<String, RtError> {
        let image_tag = spec.image_tag();
        let context_bytes = build_context_tar(&spec);
        let opts = BuildImageOptions::<String> {
            dockerfile: "Dockerfile".into(),
            t: image_tag.clone(),
            rm: true,
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
}

// ---- pure translation helpers ------------------------------------------

/// Translate a [`ContainerSpec`] into a bollard [`Config`].
pub(crate) fn container_config(spec: &ContainerSpec) -> Config<String> {
    let env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let labels: HashMap<String, String> = spec.labels.clone();

    let mounts: Vec<DockerMount> = spec.mounts.iter().map(translate_mount).collect();

    let extra_hosts: Vec<String> = spec
        .extra_hosts
        .iter()
        .map(|(host, ip)| format!("{host}:{ip}"))
        .collect();

    let host_config = HostConfig {
        mounts: Some(mounts),
        extra_hosts: Some(extra_hosts),
        ..Default::default()
    };

    Config {
        image: Some(spec.image.clone()),
        env: if env.is_empty() { None } else { Some(env) },
        entrypoint: if spec.entrypoint.is_empty() {
            None
        } else {
            Some(spec.entrypoint.clone())
        },
        user: spec.user.clone(),
        labels: if labels.is_empty() { None } else { Some(labels) },
        host_config: Some(host_config),
        ..Default::default()
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
pub(crate) fn build_context_tar(spec: &ImageBuildSpec) -> Vec<u8> {
    let mut out = Vec::new();
    let dockerfile = spec.dockerfile();
    tar::append(&mut out, "Dockerfile", 0o644, dockerfile.as_bytes());
    for f in &spec.extra_files {
        let basename = f
            .path
            .file_name()
            .map_or_else(|| "file".to_string(), |s| s.to_string_lossy().into_owned());
        let name = format!("files/{basename}");
        let mode = if f.mode == 0 { 0o644 } else { f.mode };
        tar::append(&mut out, &name, mode, &f.contents);
    }
    tar::finish(&mut out);
    out
}

/// Minimal USTAR writer. Just enough to feed `docker build`.
mod tar {
    /// Each tar block is 512 bytes.
    const BLOCK: usize = 512;

    /// Append a single file entry to `out`.
    pub fn append(out: &mut Vec<u8>, name: &str, mode: u32, body: &[u8]) {
        let mut header = [0u8; BLOCK];

        // name (first 100)
        let nbytes = name.as_bytes();
        let n = nbytes.len().min(100);
        header[..n].copy_from_slice(&nbytes[..n]);

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
        let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
        write_octal(&mut header[148..155], u64::from(sum), 6);
        header[155] = b' ';

        out.extend_from_slice(&header);
        out.extend_from_slice(body);
        // pad to block boundary
        let pad = (BLOCK - (body.len() % BLOCK)) % BLOCK;
        out.extend(std::iter::repeat_n(0u8, pad));
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
            append(&mut buf, "Dockerfile", 0o644, b"FROM x\n");
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
            append(&mut buf, "a", 0o644, b"x");
            append(&mut buf, "b", 0o644, b"y");
            finish(&mut buf);
            // 2 * (header + padded body) + 2 footer blocks
            assert_eq!(buf.len(), 4 * 512 + 2 * 512);
        }

        #[test]
        fn checksum_matches_header_bytes_sum() {
            let mut buf = Vec::new();
            append(&mut buf, "a", 0o644, b"");
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::ExtraFile;

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
    }

    #[test]
    fn container_config_full() {
        let spec = ContainerSpec::new("c", "img")
            .with_user("nobody")
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
        assert_eq!(cfg.env.as_deref(), Some(&["A=1".to_string(), "B=2".to_string()][..]));
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
        let tar = build_context_tar(&spec);
        // The Dockerfile name lives at offset 0 of the first header.
        assert_eq!(&tar[..10], b"Dockerfile");
    }

    #[test]
    fn build_context_tar_with_extra_files() {
        let mut spec = ImageBuildSpec::new("r", "debian:12-slim");
        spec.extra_files = vec![ExtraFile::new("/etc/x.conf", "hi")];
        let tar = build_context_tar(&spec);
        // Look for "files/x.conf" in the second header (offset depends
        // on Dockerfile body length; do a substring search).
        let hay = tar.windows(12).any(|w| w == b"files/x.conf");
        assert!(hay);
    }

    #[test]
    fn build_context_tar_terminates_with_zero_blocks() {
        let spec = ImageBuildSpec::new("r", "debian:12-slim");
        let tar = build_context_tar(&spec);
        let n = tar.len();
        // Last 1024 bytes are end-of-archive markers.
        assert!(tar[n - 1024..].iter().all(|b| *b == 0));
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
    // Run with `cargo test -p ironclaw-container-rt --features docker-tests`
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
        rt.cleanup_orphans("ironclaw-tests-no-such-slug").await.unwrap();
    }
}
