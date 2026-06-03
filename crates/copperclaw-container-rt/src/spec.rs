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
            Mount::Bind { target, .. }
            | Mount::Volume { target, .. }
            | Mount::Tmpfs { target, .. } => target,
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

/// Egress posture for a spawned container.
///
/// `AllowAll` (the default) preserves the legacy behaviour: the container
/// joins Docker's default bridge with full outbound NAT and the
/// [`ContainerSpec::egress_allow`] list is purely advisory.
///
/// `DenyDefault` is the opt-in first cut of a default-deny network posture.
/// What the Docker runtime **genuinely enforces** for `DenyDefault` today:
///
/// * When the resolved allow-list is **empty**, the container is spawned
///   with `network_mode: "none"` — a real, bollard-enforced full network
///   cut (no interfaces beyond loopback). This is the only posture in which
///   bollard alone can guarantee egress is blocked.
/// * When the allow-list is **non-empty** (the normal production case, since
///   the host auto-injects the model endpoint so the agent can always reach
///   its provider), bollard cannot express per-host L3/L4 filtering through
///   `create_container`. The container therefore keeps its default-bridge
///   networking, and the allow-list is recorded on the spec + surfaced to
///   the operator (`cclaw doctor`, host log) so the gap is visible.
///
/// Phase 0a **v2** strengthens this with two opt-in layers, both gated on
/// `DenyDefault` so default spawns are unaffected:
///
/// * **DNS filtering** ([`crate::dns`]): the container's `/etc/resolv.conf` is
///   pinned ([`ContainerSpec::resolv_conf_source`]) to a host-controlled
///   filtering resolver that answers ONLY the effective allow-listed names and
///   NXDOMAINs everything else — so a deny-default session can't exfiltrate via
///   DNS labels to an arbitrary resolver.
/// * **nftables** ([`crate::nftables`]): a per-session ruleset (drop-all egress
///   except the allow-listed `host:port` set + the filtering resolver) is
///   constructed for application to the session's network namespace. The rule
///   *construction* is pure + tested; the *application* needs `CAP_NET_ADMIN`
///   at spawn and is the runtime path.
///
/// What remains **deferred to the runtime** (constructed + tested here, but
/// requiring `CAP_NET_ADMIN` / a live netns to actually apply): the privileged
/// `nft -f` load against the session netns, and standing up the filter-resolver
/// sidecar. Callers must not treat a non-empty-allow-list `DenyDefault` spawn
/// as a hard L3/L4 boundary unless the host confirms the nftables apply
/// succeeded — until then the carried ruleset + pinned resolv.conf are the
/// policy and the DNS-level confinement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EgressMode {
    /// Full outbound networking on the default bridge. The allow-list is
    /// advisory only.
    #[default]
    AllowAll,
    /// Opt-in default-deny posture. See the type-level docs for exactly
    /// what bollard enforces vs. what is deferred.
    DenyDefault,
}

impl EgressMode {
    /// Stable lower-case token for logs / JSON surfaces.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EgressMode::AllowAll => "allow-all",
            EgressMode::DenyDefault => "deny-default",
        }
    }
}

/// Stronger-sandbox runtime requested for a container.
///
/// The default Docker spawn uses the host's default OCI runtime (`runc`),
/// which shares the host kernel directly. For containers that run
/// attacker-influenceable code — the headless-browser child container is the
/// motivating case (Phase 5a): it renders arbitrary, possibly-malicious web
/// pages — a *stronger* isolation boundary is wanted so a Chromium/renderer
/// RCE does not land directly on the host kernel.
///
/// This enum names the runtime the spawn should *request*. Whether the host
/// can actually honour it is **environment-dependent** and decided at runtime
/// by [`select_sandbox_runtime`]:
///
/// * [`SandboxRuntime::Runsc`] (gVisor), [`SandboxRuntime::Kata`], and
///   [`SandboxRuntime::Firecracker`] interpose a user-space kernel or a
///   microVM between the container and the host kernel. They require the
///   corresponding OCI runtime binary (`runsc` / `kata-runtime` /
///   `firecracker`-backed runtime) to be installed and registered with the
///   container engine. None of these is guaranteed present in CI or on a
///   stock host, so requesting one is **best-effort**: the selection logic
///   falls back to the next-strongest available option rather than failing
///   the spawn outright.
/// * [`SandboxRuntime::HardenedRunc`] is the always-available floor: the
///   default `runc` runtime, but spawned with a hardened seccomp profile,
///   a user-namespace remap (root-in-container ≠ root-on-host), all
///   capabilities dropped, and `no-new-privileges`. This needs no extra
///   binary, so it is the guaranteed fallback.
///
/// `Default` is the legacy posture (whatever the engine's default runtime is,
/// no extra hardening) — used only when no sandbox profile is attached, so
/// existing spawns are unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxRuntime {
    /// Engine default runtime (`runc`), no extra hardening. Legacy posture.
    #[default]
    Default,
    /// gVisor (`runsc`): a user-space kernel intercepts syscalls. Strong
    /// isolation without a full VM. Requires the `runsc` OCI runtime.
    Runsc,
    /// Kata Containers: each container runs in a lightweight VM. Requires the
    /// `kata-runtime` OCI runtime + hardware virtualisation.
    Kata,
    /// Firecracker microVM (via a Firecracker-backed OCI runtime). Requires
    /// `/dev/kvm` + the runtime shim.
    Firecracker,
    /// Default `runc`, but hardened: seccomp + userns remap + cap-drop +
    /// no-new-privileges. The always-available floor — no extra binary.
    HardenedRunc,
}

impl SandboxRuntime {
    /// Stable lower-case token for logs / JSON / labels.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxRuntime::Default => "default",
            SandboxRuntime::Runsc => "runsc",
            SandboxRuntime::Kata => "kata",
            SandboxRuntime::Firecracker => "firecracker",
            SandboxRuntime::HardenedRunc => "hardened-runc",
        }
    }

    /// The OCI runtime name the container engine must have registered for
    /// this sandbox to be honoured, if any. `None` means "the engine
    /// default runtime" (`Default` / `HardenedRunc` — the latter hardens the
    /// default runtime via security options rather than swapping it).
    #[must_use]
    pub fn oci_runtime_name(self) -> Option<&'static str> {
        match self {
            SandboxRuntime::Default | SandboxRuntime::HardenedRunc => None,
            SandboxRuntime::Runsc => Some("runsc"),
            SandboxRuntime::Kata => Some("kata-runtime"),
            SandboxRuntime::Firecracker => Some("io.containerd.firecracker.v1"),
        }
    }

    /// Whether honouring this runtime requires a binary/feature beyond the
    /// engine default. `HardenedRunc` only layers security-options onto the
    /// default runtime, so it is always available; the microVM/gVisor
    /// runtimes need their shim installed.
    #[must_use]
    pub fn needs_external_runtime(self) -> bool {
        self.oci_runtime_name().is_some()
    }

    /// Relative isolation strength, higher is stronger. Used by
    /// [`select_sandbox_runtime`] to pick the strongest *available* option no
    /// weaker than what was requested.
    #[must_use]
    pub fn strength(self) -> u8 {
        match self {
            SandboxRuntime::Default => 0,
            SandboxRuntime::HardenedRunc => 1,
            SandboxRuntime::Runsc => 2,
            // Kata and Firecracker both interpose a (micro)VM; equal strength.
            SandboxRuntime::Kata | SandboxRuntime::Firecracker => 3,
        }
    }
}

/// Hardening knobs applied on top of the chosen [`SandboxRuntime`].
///
/// These are the seccomp / userns / capability / privilege controls the
/// runtime translates into engine security-options. They are the *floor*:
/// even when a microVM runtime is selected, applying them costs nothing and
/// closes host-kernel attack surface for the in-VM process.
///
/// The default ([`SandboxProfile::hardened`]) is what the browser child
/// container uses: drop all caps, no-new-privileges, a userns remap, and the
/// engine's default (or a named stricter) seccomp profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxProfile {
    /// The sandbox runtime requested. Selection at spawn picks the strongest
    /// *available* runtime no weaker than this (see [`select_sandbox_runtime`]).
    pub runtime: SandboxRuntime,
    /// Drop ALL Linux capabilities (`--cap-drop=ALL`). A renderer needs none.
    pub cap_drop_all: bool,
    /// Set `no-new-privileges` so a setuid binary inside can't escalate.
    pub no_new_privileges: bool,
    /// Remap the container's user namespace so root-in-container maps to an
    /// unprivileged host uid (`--userns-remap` / `userns_mode`). `None` leaves
    /// the engine default; `Some(label)` requests a named remap range.
    pub userns_remap: Option<String>,
    /// Named seccomp profile to request. `None` uses the engine's *default*
    /// seccomp profile (already restrictive); `Some(name)` names a stricter
    /// host-supplied profile (e.g. a browser-specific allow-list).
    pub seccomp_profile: Option<String>,
}

impl Default for SandboxProfile {
    fn default() -> Self {
        Self {
            runtime: SandboxRuntime::Default,
            cap_drop_all: false,
            no_new_privileges: false,
            userns_remap: None,
            seccomp_profile: None,
        }
    }
}

impl SandboxProfile {
    /// The hardened profile used for attacker-influenceable workloads (the
    /// browser child container). Requests the named `runtime`, drops all
    /// caps, sets no-new-privileges, and remaps the user namespace to the
    /// `copperclaw-browser` range.
    #[must_use]
    pub fn hardened(runtime: SandboxRuntime) -> Self {
        Self {
            runtime,
            cap_drop_all: true,
            no_new_privileges: true,
            userns_remap: Some("copperclaw-browser".to_string()),
            seccomp_profile: None,
        }
    }

    /// Override the requested seccomp profile name.
    #[must_use]
    pub fn with_seccomp_profile(mut self, name: impl Into<String>) -> Self {
        self.seccomp_profile = Some(name.into());
        self
    }
}

/// Pick the sandbox runtime to actually spawn with, given what the operator
/// requested and which external runtimes are installed on this host.
///
/// `available` is the set of [`SandboxRuntime`] variants whose backing binary
/// the host probed and found present (the [`SandboxRuntime::HardenedRunc`]
/// floor is *always* implicitly available and need not be listed). The
/// returned runtime is the **strongest available option no weaker than the
/// request**, with the hardened-`runc` floor as the guaranteed fallback.
///
/// This is the honest core of the "microVM availability is
/// environment-dependent" story: the *selection* is pure and fully tested
/// here; the privileged spawn that consumes the result is the runtime path,
/// gated behind the opt-in flag.
#[must_use]
pub fn select_sandbox_runtime(
    requested: SandboxRuntime,
    available: &[SandboxRuntime],
) -> SandboxRuntime {
    // The default posture is never "upgraded" — if the caller didn't ask for
    // hardening, leave the legacy runtime alone.
    if requested == SandboxRuntime::Default {
        return SandboxRuntime::Default;
    }
    // If the exact request is available (or needs no external runtime), honour
    // it. Otherwise fall back to the strongest *available* external runtime
    // that is at least as strong as the request, else the hardened-runc floor.
    if !requested.needs_external_runtime() || available.contains(&requested) {
        return requested;
    }
    available
        .iter()
        .copied()
        .filter(|r| r.needs_external_runtime() && r.strength() >= requested.strength())
        .max_by_key(|r| r.strength())
        .unwrap_or(SandboxRuntime::HardenedRunc)
}

/// Per-group resource caps forwarded to the container runtime at spawn.
///
/// All fields are optional. The Docker runtime translates present fields
/// to `--cpus`, `--memory`, and `--pids-limit` respectively. The Apple
/// Container runtime returns `RtError::Unsupported` when any field is
/// `Some`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResourceLimits {
    /// CPU quota as a fraction of one CPU, e.g. `1.5` for one and a half
    /// CPUs. Maps to Docker's `--cpus` flag.
    pub cpus: Option<f64>,
    /// Memory cap in mebibytes. Maps to Docker's `--memory <N>m` flag.
    pub memory_mb: Option<u64>,
    /// Maximum number of processes the container may create. Maps to
    /// Docker's `--pids-limit` flag.
    pub pids_limit: Option<u64>,
}

impl ResourceLimits {
    /// Returns `true` when no limits are set (all fields are `None`).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cpus.is_none() && self.memory_mb.is_none() && self.pids_limit.is_none()
    }

    /// Parse from a `serde_json::Value` produced by
    /// `container_configs.resource_limits`.  Unknown keys are ignored.
    /// Returns `Err` only when a recognised key is present with an
    /// incompatible JSON type.
    pub fn from_json(v: &serde_json::Value) -> Result<Self, String> {
        let mut out = Self::default();
        if let Some(obj) = v.as_object() {
            if let Some(cv) = obj.get("cpus") {
                let s = cv
                    .as_str()
                    .ok_or_else(|| "`cpus` must be a string".to_string())?;
                out.cpus = Some(
                    s.parse::<f64>()
                        .map_err(|e| format!("`cpus` is not a valid float: {e}"))?,
                );
            }
            if let Some(mv) = obj.get("memory_mb") {
                out.memory_mb = Some(
                    mv.as_u64()
                        .ok_or_else(|| "`memory_mb` must be a non-negative integer".to_string())?,
                );
            }
            if let Some(pv) = obj.get("pids_limit") {
                out.pids_limit =
                    Some(pv.as_u64().ok_or_else(|| {
                        "`pids_limit` must be a non-negative integer".to_string()
                    })?);
            }
        }
        Ok(out)
    }
}

/// Description of a container the host wants spawned.
///
/// All fields are intentionally cheap value types so a `ContainerSpec`
/// can be built in normal code and then passed by value into
/// `ContainerRuntime::spawn`.
#[derive(Debug, Clone, Default, PartialEq)]
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
    /// Working directory the entrypoint starts in. When `None` the
    /// runtime falls back to the image's `WORKDIR` — typically `/`,
    /// which a non-root `user` cannot write to (and which leaves
    /// `$HOME`-relative tool caches like `go-build`/`npm`/`pip`
    /// pointing at unwritable paths). The host sets this to the
    /// session's writable bind mount.
    pub working_dir: Option<String>,
    /// Additional `/etc/hosts` entries as `(hostname, ip)`.
    pub extra_hosts: Vec<(String, String)>,
    /// Per-group resource caps applied at spawn time.
    pub resource_limits: ResourceLimits,
    /// When `true`, expose every Nvidia GPU on the host to the
    /// container (`docker run --gpus all` equivalent — wires a
    /// `DeviceRequest { driver: "nvidia", count: -1, capabilities:
    /// [["gpu"]] }` into the bollard `HostConfig`). Requires the
    /// `nvidia-container-toolkit` package on the host. Default off
    /// because most agent sessions don't need GPU access and a
    /// device-request against a host without the toolkit fails the
    /// spawn outright.
    pub gpu_passthrough: bool,
    /// Resolved egress allow-list as `"host:port"` pairs.
    ///
    /// This is the *policy*, not a guarantee of enforcement — see
    /// [`EgressMode`] for exactly what the Docker runtime enforces today
    /// (a `network_mode: "none"` cut only when this list is empty under
    /// [`EgressMode::DenyDefault`]) and what is deferred to a future
    /// netns + nftables pass (per-host filtering of a non-empty list).
    /// Under [`EgressMode::AllowAll`] this list is advisory only.
    pub egress_allow: Vec<String>,
    /// Egress posture. See [`EgressMode`]. Defaults to
    /// [`EgressMode::AllowAll`] so the legacy spawn path is unchanged.
    pub egress_mode: EgressMode,
    /// Host path to a pinned `/etc/resolv.conf` to bind read-only into the
    /// container (Phase 0a v2, DNS filtering). When `Some`, the runtime adds a
    /// read-only bind mount of this file at `/etc/resolv.conf` so the
    /// container's stub resolver can only reach the host-controlled filtering
    /// resolver (which answers ONLY the effective allow-list and NXDOMAINs
    /// everything else). `None` (the default) leaves the image's resolv.conf
    /// untouched — the legacy behaviour. Set only under
    /// [`EgressMode::DenyDefault`] so default spawns are unaffected.
    pub resolv_conf_source: Option<String>,
    /// Optional stronger-sandbox profile (Phase 5a). `None` (the default)
    /// leaves the legacy spawn posture entirely unchanged — the engine
    /// default runtime, no extra security options. `Some(profile)` requests
    /// a stronger isolation runtime (gVisor / Kata / Firecracker, falling
    /// back to a hardened `runc` with seccomp + userns remap + cap-drop +
    /// no-new-privileges). Set only for attacker-influenceable workloads
    /// such as the headless-browser child container.
    ///
    /// What the Docker runtime genuinely applies from this today: the
    /// security-options floor (`cap_drop=ALL`, `no-new-privileges`, userns
    /// remap, seccomp) plus selecting the requested OCI runtime *if it is
    /// installed*. Whether a microVM/gVisor runtime is actually present is
    /// environment-dependent; [`select_sandbox_runtime`] resolves the request
    /// against what the host probed, and the privileged spawn is gated behind
    /// the browser tool's opt-in flag.
    pub sandbox: Option<SandboxProfile>,
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

    /// Set the working directory the entrypoint starts in.
    #[must_use]
    pub fn with_working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Append an `/etc/hosts` entry.
    #[must_use]
    pub fn with_extra_host(mut self, host: impl Into<String>, ip: impl Into<String>) -> Self {
        self.extra_hosts.push((host.into(), ip.into()));
        self
    }

    /// Set per-group resource limits.
    #[must_use]
    pub fn with_resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.resource_limits = limits;
        self
    }

    /// Expose all Nvidia GPUs on the host to the container. Requires
    /// `nvidia-container-toolkit` on the host. See [`Self::gpu_passthrough`].
    #[must_use]
    pub fn with_gpu_passthrough(mut self, on: bool) -> Self {
        self.gpu_passthrough = on;
        self
    }

    /// Set the egress allow-list (the resolved `host:port` policy). Under
    /// [`EgressMode::AllowAll`] this is advisory; under
    /// [`EgressMode::DenyDefault`] see [`EgressMode`] for enforcement.
    #[must_use]
    pub fn with_egress_allow(mut self, allow: Vec<String>) -> Self {
        self.egress_allow = allow;
        self
    }

    /// Set the egress posture. See [`EgressMode`].
    #[must_use]
    pub fn with_egress_mode(mut self, mode: EgressMode) -> Self {
        self.egress_mode = mode;
        self
    }

    /// Pin the container's `/etc/resolv.conf` to a host-controlled file
    /// (Phase 0a v2 DNS filtering). See [`Self::resolv_conf_source`].
    #[must_use]
    pub fn with_resolv_conf_source(mut self, source: impl Into<String>) -> Self {
        self.resolv_conf_source = Some(source.into());
        self
    }

    /// Attach a stronger-sandbox profile (Phase 5a). See [`Self::sandbox`].
    #[must_use]
    pub fn with_sandbox(mut self, profile: SandboxProfile) -> Self {
        self.sandbox = Some(profile);
        self
    }
}

/// Result of a successful `spawn`.
///
/// `id` is the runtime-assigned container id (full or short — backends
/// return whichever they natively expose). `name` matches the spec name
/// and is what the host uses for subsequent `stop` calls.
///
/// `host_pid` is the **host-visible** PID of the container's main process
/// (Docker's `State.Pid`), when the backend can surface it. This is the
/// target the privileged deny-default egress apply needs: entering the
/// session's own network namespace is `nsenter -t <host_pid> -n nft -f -`.
/// It is `None` when the backend cannot report a PID (the Apple Container
/// runtime today, the in-process test stub) or when the container reported
/// PID 0 (created-but-not-running — Docker uses 0 for a non-running
/// container, which can never be a netns target). Callers MUST treat
/// `None` as "no netns target available" and degrade honestly rather than
/// guessing a PID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerHandle {
    /// Runtime-assigned container identifier.
    pub id: String,
    /// Container name (same as `ContainerSpec.name`).
    pub name: String,
    /// Host-visible PID of the container's main process, when known. See the
    /// type-level docs — the privileged netns apply targets this PID.
    pub host_pid: Option<i32>,
}

impl ContainerHandle {
    /// Construct a handle from id + name, with no host PID surfaced.
    ///
    /// Backends that can resolve the host-visible PID should chain
    /// [`Self::with_host_pid`].
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            host_pid: None,
        }
    }

    /// Attach the host-visible PID of the container's main process.
    ///
    /// A `Some(0)` is normalised to `None`: Docker reports PID 0 for a
    /// container that is created but not running, which is never a valid
    /// `nsenter` target. Likewise any non-positive PID is rejected.
    #[must_use]
    pub fn with_host_pid(mut self, pid: Option<i32>) -> Self {
        self.host_pid = pid.filter(|p| *p > 0);
        self
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
        assert!(spec.resource_limits.is_empty());
        assert!(spec.egress_allow.is_empty());
        assert_eq!(spec.egress_mode, EgressMode::AllowAll);
        assert!(spec.resolv_conf_source.is_none());
        assert!(spec.sandbox.is_none());
    }

    // ── SandboxRuntime / SandboxProfile / selection ──────────────────────

    #[test]
    fn sandbox_runtime_as_str_stable() {
        assert_eq!(SandboxRuntime::Default.as_str(), "default");
        assert_eq!(SandboxRuntime::Runsc.as_str(), "runsc");
        assert_eq!(SandboxRuntime::Kata.as_str(), "kata");
        assert_eq!(SandboxRuntime::Firecracker.as_str(), "firecracker");
        assert_eq!(SandboxRuntime::HardenedRunc.as_str(), "hardened-runc");
    }

    #[test]
    fn sandbox_runtime_oci_name_and_external_flag() {
        // Default + hardened-runc layer on the default runtime — no external
        // binary, so no OCI runtime name.
        assert_eq!(SandboxRuntime::Default.oci_runtime_name(), None);
        assert!(!SandboxRuntime::Default.needs_external_runtime());
        assert_eq!(SandboxRuntime::HardenedRunc.oci_runtime_name(), None);
        assert!(!SandboxRuntime::HardenedRunc.needs_external_runtime());
        // The stronger runtimes each name a distinct OCI runtime and need it
        // installed.
        assert_eq!(SandboxRuntime::Runsc.oci_runtime_name(), Some("runsc"));
        assert!(SandboxRuntime::Runsc.needs_external_runtime());
        assert_eq!(
            SandboxRuntime::Kata.oci_runtime_name(),
            Some("kata-runtime")
        );
        assert!(SandboxRuntime::Kata.needs_external_runtime());
        assert!(SandboxRuntime::Firecracker.needs_external_runtime());
    }

    #[test]
    fn sandbox_runtime_strength_ordering() {
        assert!(SandboxRuntime::HardenedRunc.strength() > SandboxRuntime::Default.strength());
        assert!(SandboxRuntime::Runsc.strength() > SandboxRuntime::HardenedRunc.strength());
        assert!(SandboxRuntime::Kata.strength() > SandboxRuntime::Runsc.strength());
        assert_eq!(
            SandboxRuntime::Firecracker.strength(),
            SandboxRuntime::Kata.strength()
        );
    }

    #[test]
    fn sandbox_profile_hardened_sets_floor() {
        let p = SandboxProfile::hardened(SandboxRuntime::Runsc);
        assert_eq!(p.runtime, SandboxRuntime::Runsc);
        assert!(p.cap_drop_all);
        assert!(p.no_new_privileges);
        assert_eq!(p.userns_remap.as_deref(), Some("copperclaw-browser"));
        // Default seccomp (engine default) unless explicitly overridden.
        assert!(p.seccomp_profile.is_none());
    }

    #[test]
    fn sandbox_profile_with_seccomp_profile() {
        let p =
            SandboxProfile::hardened(SandboxRuntime::HardenedRunc).with_seccomp_profile("browser");
        assert_eq!(p.seccomp_profile.as_deref(), Some("browser"));
    }

    #[test]
    fn sandbox_profile_default_is_inert() {
        let p = SandboxProfile::default();
        assert_eq!(p.runtime, SandboxRuntime::Default);
        assert!(!p.cap_drop_all);
        assert!(!p.no_new_privileges);
        assert!(p.userns_remap.is_none());
        assert!(p.seccomp_profile.is_none());
    }

    #[test]
    fn select_default_request_never_upgrades() {
        // A Default request stays Default even if stronger runtimes exist.
        assert_eq!(
            select_sandbox_runtime(
                SandboxRuntime::Default,
                &[SandboxRuntime::Runsc, SandboxRuntime::Kata]
            ),
            SandboxRuntime::Default
        );
    }

    #[test]
    fn select_exact_request_honoured_when_available() {
        assert_eq!(
            select_sandbox_runtime(SandboxRuntime::Runsc, &[SandboxRuntime::Runsc]),
            SandboxRuntime::Runsc
        );
        assert_eq!(
            select_sandbox_runtime(
                SandboxRuntime::Kata,
                &[SandboxRuntime::Runsc, SandboxRuntime::Kata]
            ),
            SandboxRuntime::Kata
        );
    }

    #[test]
    fn select_falls_back_to_strongest_available() {
        // Asked for gVisor, only Kata installed (stronger): use Kata.
        assert_eq!(
            select_sandbox_runtime(SandboxRuntime::Runsc, &[SandboxRuntime::Kata]),
            SandboxRuntime::Kata
        );
    }

    #[test]
    fn select_falls_back_to_hardened_runc_when_nothing_external() {
        // Asked for a microVM, nothing installed: hardened-runc floor.
        assert_eq!(
            select_sandbox_runtime(SandboxRuntime::Firecracker, &[]),
            SandboxRuntime::HardenedRunc
        );
        assert_eq!(
            select_sandbox_runtime(SandboxRuntime::Runsc, &[]),
            SandboxRuntime::HardenedRunc
        );
    }

    #[test]
    fn select_does_not_downgrade_below_request_strength() {
        // Asked for Kata (strength 3); only Runsc (strength 2) available —
        // Runsc is weaker than the request, so do NOT silently use it; fall
        // to the hardened-runc floor and let the operator see the gap.
        assert_eq!(
            select_sandbox_runtime(SandboxRuntime::Kata, &[SandboxRuntime::Runsc]),
            SandboxRuntime::HardenedRunc
        );
    }

    #[test]
    fn select_hardened_runc_request_always_satisfiable() {
        // HardenedRunc needs no external binary, so the request is always met.
        assert_eq!(
            select_sandbox_runtime(SandboxRuntime::HardenedRunc, &[]),
            SandboxRuntime::HardenedRunc
        );
    }

    #[test]
    fn spec_with_sandbox_round_trips() {
        let profile = SandboxProfile::hardened(SandboxRuntime::Runsc);
        let spec = ContainerSpec::new("c", "img").with_sandbox(profile.clone());
        assert_eq!(spec.sandbox, Some(profile));
    }

    #[test]
    fn spec_with_resolv_conf_source() {
        let spec = ContainerSpec::new("c", "img").with_resolv_conf_source("/h/resolv.conf");
        assert_eq!(spec.resolv_conf_source.as_deref(), Some("/h/resolv.conf"));
    }

    #[test]
    fn spec_with_resource_limits() {
        let limits = ResourceLimits {
            cpus: Some(1.5),
            memory_mb: Some(512),
            pids_limit: Some(256),
        };
        let spec = ContainerSpec::new("c", "img").with_resource_limits(limits);
        assert_eq!(spec.resource_limits.cpus, Some(1.5));
        assert_eq!(spec.resource_limits.memory_mb, Some(512));
        assert_eq!(spec.resource_limits.pids_limit, Some(256));
        assert!(!spec.resource_limits.is_empty());
    }

    #[test]
    fn spec_with_egress_allow() {
        let spec =
            ContainerSpec::new("c", "img").with_egress_allow(vec!["api.example.com:443".into()]);
        assert_eq!(spec.egress_allow, vec!["api.example.com:443".to_string()]);
    }

    #[test]
    fn spec_egress_mode_defaults_to_allow_all() {
        let spec = ContainerSpec::new("c", "img");
        assert_eq!(spec.egress_mode, EgressMode::AllowAll);
    }

    #[test]
    fn spec_with_egress_mode() {
        let spec = ContainerSpec::new("c", "img").with_egress_mode(EgressMode::DenyDefault);
        assert_eq!(spec.egress_mode, EgressMode::DenyDefault);
    }

    #[test]
    fn egress_mode_as_str_is_stable() {
        assert_eq!(EgressMode::AllowAll.as_str(), "allow-all");
        assert_eq!(EgressMode::DenyDefault.as_str(), "deny-default");
    }

    #[test]
    fn resource_limits_empty() {
        assert!(ResourceLimits::default().is_empty());
        assert!(
            !ResourceLimits {
                cpus: Some(1.0),
                ..Default::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn resource_limits_from_json_full() {
        let v = serde_json::json!({"cpus": "1.5", "memory_mb": 512u64, "pids_limit": 256u64});
        let lim = ResourceLimits::from_json(&v).unwrap();
        assert!((lim.cpus.unwrap() - 1.5).abs() < f64::EPSILON);
        assert_eq!(lim.memory_mb, Some(512));
        assert_eq!(lim.pids_limit, Some(256));
    }

    #[test]
    fn resource_limits_from_json_empty_object() {
        let v = serde_json::json!({});
        let lim = ResourceLimits::from_json(&v).unwrap();
        assert!(lim.is_empty());
    }

    #[test]
    fn resource_limits_from_json_ignores_unknown_keys() {
        let v = serde_json::json!({"cpus": "1.0", "unknown": 99});
        let lim = ResourceLimits::from_json(&v).unwrap();
        assert!(lim.cpus.is_some());
    }

    #[test]
    fn resource_limits_from_json_bad_cpus_type() {
        let v = serde_json::json!({"cpus": 1}); // not a string
        assert!(ResourceLimits::from_json(&v).is_err());
    }

    #[test]
    fn resource_limits_from_json_bad_memory_type() {
        let v = serde_json::json!({"memory_mb": "big"}); // not an integer
        assert!(ResourceLimits::from_json(&v).is_err());
    }

    #[test]
    fn resource_limits_from_json_bad_cpus_string() {
        let v = serde_json::json!({"cpus": "not-a-float"});
        assert!(ResourceLimits::from_json(&v).is_err());
    }

    #[test]
    fn handle_new() {
        let h = ContainerHandle::new("abc123", "session-1");
        assert_eq!(h.id, "abc123");
        assert_eq!(h.name, "session-1");
        // No PID surfaced by the bare constructor — backends opt in.
        assert_eq!(h.host_pid, None);
    }

    #[test]
    fn handle_with_host_pid_keeps_positive() {
        let h = ContainerHandle::new("id", "n").with_host_pid(Some(4242));
        assert_eq!(h.host_pid, Some(4242));
    }

    #[test]
    fn handle_with_host_pid_normalises_zero_and_negatives_to_none() {
        // Docker reports PID 0 for a created-but-not-running container — never
        // a valid netns target, so it must surface as None.
        assert_eq!(
            ContainerHandle::new("id", "n")
                .with_host_pid(Some(0))
                .host_pid,
            None
        );
        assert_eq!(
            ContainerHandle::new("id", "n")
                .with_host_pid(Some(-1))
                .host_pid,
            None
        );
        assert_eq!(
            ContainerHandle::new("id", "n").with_host_pid(None).host_pid,
            None
        );
    }
}
