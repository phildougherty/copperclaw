//! Container spawn spec and supporting value types.
//!
//! The container-runtime contract (`PLAN.md` § 5.5) is intentionally
//! backend-agnostic: a `ContainerSpec` is the entire description of one
//! container the host wants up. Both `DockerRuntime` and
//! `AppleContainerRuntime` translate this struct into their respective
//! native call shape.

use std::collections::HashMap;

/// A bind/volume/tmpfs mount, with read-only or read-write access.
///
/// We keep this enum small and exhaustive — the three variants below
/// cover every mount the host wires up: workspace bind, named cache
/// volumes, and tmpfs `/tmp` for hot data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    /// Bind a host path into the container.
    Bind {
        /// Absolute path on the host.
        source: String,
        /// Absolute path inside the container.
        target: String,
        /// `true` mounts read-only; `false` is read-write.
        read_only: bool,
    },
    /// Mount a named (docker-managed) volume.
    Volume {
        /// Volume name, as understood by the runtime.
        name: String,
        /// Absolute path inside the container.
        target: String,
        /// `true` mounts read-only; `false` is read-write.
        read_only: bool,
    },
    /// Create a tmpfs at `target` of size `size_bytes` (zero = backend default).
    Tmpfs {
        /// Absolute path inside the container.
        target: String,
        /// Byte cap; `0` means runtime default.
        size_bytes: u64,
    },
}

impl Mount {
    /// Container-side path this mount lands at.
    #[must_use]
    pub fn target(&self) -> &str {
        match self {
            Mount::Bind { target, .. } | Mount::Volume { target, .. } | Mount::Tmpfs { target, .. } => target,
        }
    }

    /// `true` if this mount is read-only.
    ///
    /// Tmpfs mounts are always writable (read-only tmpfs is useless), so
    /// they report `false`.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        match self {
            Mount::Bind { read_only, .. } | Mount::Volume { read_only, .. } => *read_only,
            Mount::Tmpfs { .. } => false,
        }
    }

    /// Stable short token for the kind of mount this is.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Mount::Bind { .. } => "bind",
            Mount::Volume { .. } => "volume",
            Mount::Tmpfs { .. } => "tmpfs",
        }
    }
}

/// Description of a container the host wants spawned.
///
/// All fields are intentionally cheap value types so a `ContainerSpec`
/// can be built in normal code and then passed by value into
/// `ContainerRuntime::spawn`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerSpec {
    /// Container name as seen by the runtime.
    pub name: String,
    /// Image reference (`repo:tag`, fully-qualified digest, etc.).
    pub image: String,
    /// Container labels (key/value pairs the host uses to find orphans).
    pub labels: HashMap<String, String>,
    /// Environment variables, in deterministic order.
    pub env: Vec<(String, String)>,
    /// Filesystem mounts to attach.
    pub mounts: Vec<Mount>,
    /// Optional entrypoint override.
    pub entrypoint: Vec<String>,
    /// Optional `user[:group]` override.
    pub user: Option<String>,
    /// Additional `/etc/hosts` entries as `(hostname, ip)`.
    pub extra_hosts: Vec<(String, String)>,
}

impl ContainerSpec {
    /// Start a new spec with just name + image.
    pub fn new(name: impl Into<String>, image: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            image: image.into(),
            ..Self::default()
        }
    }

    /// Add a label.
    #[must_use]
    pub fn with_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    /// Append an env variable.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Append a mount.
    #[must_use]
    pub fn with_mount(mut self, mount: Mount) -> Self {
        self.mounts.push(mount);
        self
    }

    /// Set the entrypoint.
    #[must_use]
    pub fn with_entrypoint(mut self, entrypoint: Vec<String>) -> Self {
        self.entrypoint = entrypoint;
        self
    }

    /// Set the `user[:group]`.
    #[must_use]
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// Append an `/etc/hosts` entry.
    #[must_use]
    pub fn with_extra_host(mut self, host: impl Into<String>, ip: impl Into<String>) -> Self {
        self.extra_hosts.push((host.into(), ip.into()));
        self
    }
}

/// Result of a successful `spawn`.
///
/// `id` is the runtime-assigned container id (full or short — backends
/// return whichever they natively expose). `name` matches the spec name
/// and is what the host uses for subsequent `stop` calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerHandle {
    /// Runtime-assigned container identifier.
    pub id: String,
    /// Container name (same as `ContainerSpec.name`).
    pub name: String,
}

impl ContainerHandle {
    /// Construct a handle from id + name.
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_bind_accessors() {
        let m = Mount::Bind {
            source: "/host".into(),
            target: "/in".into(),
            read_only: true,
        };
        assert_eq!(m.target(), "/in");
        assert!(m.is_read_only());
        assert_eq!(m.kind(), "bind");
    }

    #[test]
    fn mount_volume_accessors() {
        let m = Mount::Volume {
            name: "cache".into(),
            target: "/cache".into(),
            read_only: false,
        };
        assert_eq!(m.target(), "/cache");
        assert!(!m.is_read_only());
        assert_eq!(m.kind(), "volume");
    }

    #[test]
    fn mount_tmpfs_is_never_read_only() {
        let m = Mount::Tmpfs {
            target: "/tmp".into(),
            size_bytes: 0,
        };
        assert_eq!(m.target(), "/tmp");
        assert!(!m.is_read_only());
        assert_eq!(m.kind(), "tmpfs");
    }

    #[test]
    fn spec_builder_chains() {
        let spec = ContainerSpec::new("c1", "alpine:3")
            .with_label("group", "g1")
            .with_env("FOO", "bar")
            .with_mount(Mount::Bind {
                source: "/h".into(),
                target: "/c".into(),
                read_only: false,
            })
            .with_entrypoint(vec!["/bin/sh".into(), "-c".into()])
            .with_user("1000:1000")
            .with_extra_host("api.local", "10.0.0.5");

        assert_eq!(spec.name, "c1");
        assert_eq!(spec.image, "alpine:3");
        assert_eq!(spec.labels.get("group"), Some(&"g1".to_string()));
        assert_eq!(spec.env, vec![("FOO".into(), "bar".into())]);
        assert_eq!(spec.mounts.len(), 1);
        assert_eq!(spec.entrypoint, vec!["/bin/sh", "-c"]);
        assert_eq!(spec.user.as_deref(), Some("1000:1000"));
        assert_eq!(
            spec.extra_hosts,
            vec![("api.local".to_string(), "10.0.0.5".to_string())]
        );
    }

    #[test]
    fn spec_default_is_empty() {
        let spec = ContainerSpec::default();
        assert!(spec.name.is_empty());
        assert!(spec.labels.is_empty());
        assert!(spec.env.is_empty());
        assert!(spec.mounts.is_empty());
        assert!(spec.entrypoint.is_empty());
        assert!(spec.user.is_none());
        assert!(spec.extra_hosts.is_empty());
    }

    #[test]
    fn handle_new() {
        let h = ContainerHandle::new("abc123", "session-1");
        assert_eq!(h.id, "abc123");
        assert_eq!(h.name, "session-1");
    }
}
