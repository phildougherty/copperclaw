//! Container spawn pipeline: `maybe_spawn`, `build_spec`, image rebuild + backoff.

use super::prompt::{set_memory_dir_perms, write_memory_unavailable_marker};
use super::{ContainerManager, ManagerError};
use ironclaw_container_rt::{ContainerSpec, ImageBuildSpec, Mount, ResourceLimits, RtError};
use ironclaw_db::session::SessionPaths;
use ironclaw_db::tables::{container_configs, sessions};
use ironclaw_types::{AgentGroupId, Session};
use tracing::{info, warn};

/// Default poll cadence. The router debounces inbound, so once a
/// message has settled in `messages_in` we want to spawn fast — this
/// is the tail latency between "user typed" and "container starts
/// running". 1s feels right for a local cli loop; the sweep loop runs
/// at 60s and handles the slower lifecycle work.
pub const POLL_INTERVAL_MS: u64 = 1000;

/// Path inside the container where the session dir is mounted.
pub const CONTAINER_SESSION_DIR: &str = "/data";

/// Filename of the runner-config JSON written into the session dir.
pub const RUNNER_CONFIG_FILENAME: &str = "runner.json";

/// Path inside the container where the runner binary lives. Must
/// match the path baked into the session image at build time.
pub const CONTAINER_RUNNER_PATH: &str = "/usr/local/bin/ironclaw-runner";

/// Skill names belonging to the Phase E coding bundle. Filtered out
/// of the resolved skill set when `container_configs.coding_enabled`
/// is false. Acts as a cap on `SkillsSelector::All`; explicit
/// selector lists are honoured as-is (the operator picked the names
/// deliberately).
pub const CODING_SKILL_NAMES: &[&str] =
    &["coding-task", "git-commit", "code-review", "testing"];

/// Default idle window before the manager stops a running container.
/// 300s (5 min) matches the OpenBSD-of-claw-agents "conservative
/// defaults" tenet — long enough to avoid thrashing on quiet groups,
/// short enough that an unattended host doesn't burn memory.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;

/// Default heartbeat-staleness threshold. The runner refreshes its
/// `.heartbeat` file's mtime every ~1s as part of the poll loop; if
/// the host hasn't seen a refresh for this long, the runner is
/// presumed dead and the container is reset for respawn.
pub const DEFAULT_HEARTBEAT_STALE_SECS: u64 = 60;

/// Grace period passed to `runtime.stop` on idle / crash transitions.
/// 5s is enough for the runner to flush an in-flight HTTP call.
pub const DEFAULT_STOP_GRACE_SECS: u64 = 5;

impl ContainerManager {
    /// Try to spawn a container for `session`. Returns `Ok(true)` when
    /// a container was actually spawned (i.e. pending work was found
    /// and the runtime call succeeded), `Ok(false)` when there was
    /// nothing pending, and `Err(...)` for real failures.
    ///
    /// Refuses with [`ManagerError::HostDegraded`] when
    /// [`Self::is_degraded`] is `true`. This is the boot-time image
    /// health gate's runtime tail: when the host couldn't verify
    /// the session image at boot, it refuses to silently spawn
    /// containers that might be using a stale runner.
    #[allow(clippy::too_many_lines)] // spawn flow has several gates that are clearer inline
    pub async fn maybe_spawn(&self, session: &Session) -> Result<bool, ManagerError> {
        if self.is_degraded() {
            return Err(ManagerError::HostDegraded);
        }
        let paths = SessionPaths::new(
            &self.cfg.data_dir,
            session.agent_group_id,
            session.id,
        );
        paths.ensure_dirs().map_err(ManagerError::Io)?;

        if !Self::has_pending_inbound(&paths)? {
            return Ok(false);
        }

        // Budget gate. If the group has a daily_token_cap and today's
        // turns already meet/exceed it, refuse to spawn. The inbound
        // sits in the row until the cap resets at UTC midnight or the
        // operator raises it via `iclaw groups budget set`.
        //
        // The user gets one in-channel reply per agent group per
        // notice window — without that, a chat goes silent and the
        // user has no idea why. See `maybe_post_budget_exhausted` for
        // the dedup window + the message text.
        if self.is_over_budget(session)? {
            warn!(
                session = %session.id.as_uuid(),
                agent_group = %session.agent_group_id.as_uuid(),
                "daily token budget exhausted; spawn deferred"
            );
            // Fires once per refusal, before the dedup window check —
            // operators can alert on the spike independent of how many
            // reply notices actually went out.
            ironclaw_metrics::inc_budget_exhausted(
                &session.agent_group_id.as_uuid().to_string(),
                ironclaw_metrics::BUDGET_GATE_DAILY_TOKENS,
            );
            if let Err(err) = self.maybe_post_budget_exhausted(session, &paths) {
                // Notification failure is non-fatal; the spawn is
                // still deferred and the warning above is in the log.
                warn!(?err, "could not post budget-exhausted reply");
            }
            return Ok(false);
        }

        // Rate-limit gate. If the group has a per-minute or per-hour
        // LLM-call cap and the trailing window count already meets/exceeds
        // it, refuse to spawn and post a one-per-window in-channel reply.
        if let Some((msg, gate_label)) = self.rate_limit_message(session)? {
            warn!(
                session = %session.id.as_uuid(),
                agent_group = %session.agent_group_id.as_uuid(),
                gate = gate_label,
                "rate limit reached; spawn deferred"
            );
            ironclaw_metrics::inc_budget_exhausted(
                &session.agent_group_id.as_uuid().to_string(),
                gate_label,
            );
            if let Err(err) = self.maybe_post_rate_limit_reply(session, &paths, &msg) {
                warn!(?err, "could not post rate-limit reply");
            }
            return Ok(false);
        }

        let cfg_row = container_configs::get(&self.central, session.agent_group_id)
            .map_err(ManagerError::Db)?;
        let runner_cfg = self.runner_config_for(session, cfg_row.as_ref(), Some(paths.root.as_path()));
        let runner_json = serde_json::to_vec_pretty(&runner_cfg).map_err(ManagerError::Json)?;
        std::fs::write(paths.root.join(RUNNER_CONFIG_FILENAME), runner_json)
            .map_err(ManagerError::Io)?;

        // Image rebuild gate (Top 10 #1 / M13).  Compute the fingerprint of
        // the rebuild-relevant config fields.  If the stored fingerprint
        // differs from the current config (or is absent), rebuild the image
        // before spawning.  The new tag + fingerprint are persisted back to
        // `container_configs` so subsequent spawns reuse the cached image.
        //
        // Failure handling: if the rebuild itself fails (bad apt name,
        // network blip during `apt-get update`, etc.) we fall back to the
        // last-known-good `image_tag` so the session still spawns. The
        // fingerprint is NOT updated on fallback, so the next spawn retries
        // the rebuild — the operator can fix the broken config without the
        // group going dark in the meantime. When there is no fallback tag
        // (first-ever build, no cached image), the error propagates and the
        // session stays Stopped for the next tick to retry.
        let image_tag = if let Some(ref cfg) = cfg_row {
            let live_fp = container_configs::compute_fingerprint(cfg);
            let stored_fp = cfg.config_fingerprint.as_deref().unwrap_or("");
            let base_tag = cfg.image_tag.clone().unwrap_or_else(|| self.cfg.default_image_tag.clone());
            if stored_fp == live_fp {
                base_tag
            } else if let Some(cooldown_remaining) =
                self.rebuild_backoff.in_cooldown(session.agent_group_id)
            {
                // Don't attempt a fresh rebuild while this group is in
                // cooldown after recent rebuild failures. Spawn on the
                // last-known-good image; the operator (or the agent
                // itself, via in-container `shell npm install`) can
                // unblock by fixing the config and waiting out the
                // backoff window, or the backoff expires naturally.
                if base_tag.is_empty() {
                    return Err(ManagerError::Spawn(RtError::Container(
                        "image rebuild in cooldown and no fallback tag is configured"
                            .into(),
                    )));
                }
                warn!(
                    agent_group = %session.agent_group_id.as_uuid(),
                    cooldown_remaining_secs = cooldown_remaining.as_secs(),
                    fallback_tag = %base_tag,
                    "image rebuild in cooldown after prior failures; spawning on last-known-good tag"
                );
                base_tag
            } else {
                match self.rebuild_image(session.agent_group_id, cfg).await {
                    Ok(new_tag) => {
                        self.rebuild_backoff.record_success(session.agent_group_id);
                        new_tag
                    }
                    Err(err) if !base_tag.is_empty() => {
                        let next_backoff =
                            self.rebuild_backoff.record_failure(session.agent_group_id);
                        warn!(
                            agent_group = %session.agent_group_id.as_uuid(),
                            fallback_tag = %base_tag,
                            next_backoff_secs = next_backoff.as_secs(),
                            ?err,
                            "image rebuild failed; spawning on last-known-good tag and backing off"
                        );
                        ironclaw_metrics::inc_image_rebuild_failed();
                        base_tag
                    }
                    Err(err) => {
                        self.rebuild_backoff.record_failure(session.agent_group_id);
                        ironclaw_metrics::inc_image_rebuild_failed();
                        return Err(err);
                    }
                }
            }
        } else {
            self.cfg.default_image_tag.clone()
        };

        let spec = self.build_spec(session, &paths, &image_tag, cfg_row.as_ref());
        // Defensive: if a previous container with this name lingered
        // (e.g. host crashed mid-cycle, orphan cleanup missed it,
        // crash-restart's remove() raced the spawn) the create call
        // would 409. Best-effort remove is a no-op when nothing
        // matches, so it's cheap to always do.
        let _ = self.runtime.remove(&spec.name).await;
        // Reset the heartbeat file so the new container starts with
        // a clean slate. Without this, the previous container's
        // last (now-stale) mtime persists and the manager would
        // crash-restart the new spawn before its runner could write
        // its first heartbeat.
        let _ = std::fs::remove_file(&paths.heartbeat);
        let spawn_started = std::time::Instant::now();
        let handle = match self.runtime.spawn(spec).await {
            Ok(h) => h,
            Err(err) => {
                // Bump the spawn-attempt tracker so the sweep's apology
                // check can decide whether to notify the user that the
                // container can't come up. The tracker is process-local
                // and cleared on a successful spawn below.
                let attempts = self.spawn_tracker.record_failure(session.id);
                warn!(
                    session = %session.id.as_uuid(),
                    attempts,
                    ?err,
                    "runtime spawn failed; bumped spawn-attempt counter",
                );
                return Err(ManagerError::Spawn(err));
            }
        };
        let spawn_elapsed = spawn_started.elapsed().as_secs_f64();
        // Successful spawn: clear any prior failure record so a future
        // crash doesn't immediately trip the apology threshold.
        self.spawn_tracker.record_success(session.id);
        sessions::mark_container_running(&self.central, session.id).map_err(ManagerError::Db)?;
        ironclaw_metrics::inc_containers_spawned();
        ironclaw_metrics::observe_container_spawn_seconds(spawn_elapsed);
        info!(
            session = %session.id.as_uuid(),
            container = %handle.id,
            image = %image_tag,
            "spawned session container"
        );
        Ok(true)
    }

    /// Build the image for an agent group whose config has changed.
    ///
    /// Uses the `ImageBuildSpec` machinery to produce a sha256-tagged image
    /// from `packages_apt` / `packages_npm` (`mcp_servers` and skills are
    /// runtime config, not image config — they don't affect the Dockerfile).
    /// After a successful build the new tag + fingerprint are written back to
    /// `container_configs` so future spawns can reuse the cached image.
    pub(super) async fn rebuild_image(
        &self,
        agent_group_id: AgentGroupId,
        cfg: &container_configs::ContainerConfig,
    ) -> Result<String, ManagerError> {
        // Base the per-group rebuild on the install's default image
        // (which has `/usr/local/bin/ironclaw-runner` baked in at setup
        // time), NOT on bare `debian:trixie-slim`. The rebuild's
        // Dockerfile only adds layers (apt / npm / labels) — it does
        // not COPY the runner binary, so building from a runnerless
        // base produces a runnerless image and every subsequent
        // `runc create` fails with "stat /usr/local/bin/ironclaw-runner:
        // no such file or directory". Caught live: an agent emitted
        // `install_packages` → host auto-rebuilt → container_configs
        // got pinned to a 413MB runnerless image → all subsequent
        // spawns wedged. Falling back to a hard-coded slim base when
        // `default_image_tag` is empty (shouldn't happen in a
        // setup-completed install, but the fallback keeps tests that
        // run with `HostConfig::default()` working).
        let base = resolve_rebuild_base(&self.cfg.default_image_tag);
        let mut build_spec = ImageBuildSpec::new("ironclaw/session", &base);
        build_spec.apt_packages.clone_from(&cfg.packages_apt);
        build_spec.npm_packages.clone_from(&cfg.packages_npm);
        let tag = self.runtime.build_image(build_spec).await.map_err(ManagerError::Spawn)?;
        let live_fp = container_configs::compute_fingerprint(cfg);
        container_configs::set_image_tag_and_fingerprint(
            &self.central,
            agent_group_id,
            &tag,
            &live_fp,
        )
        .map_err(ManagerError::Db)?;
        info!(
            agent_group = %agent_group_id.as_uuid(),
            image = %tag,
            "rebuilt image for config change"
        );
        Ok(tag)
    }

    pub(super) fn build_spec(
        &self,
        session: &Session,
        paths: &SessionPaths,
        image_tag: &str,
        cfg: Option<&container_configs::ContainerConfig>,
    ) -> ContainerSpec {
        let mut spec = ContainerSpec::new(container_name(session.agent_group_id, session.id), image_tag)
            .with_entrypoint(vec![CONTAINER_RUNNER_PATH.to_string()])
            .with_label("ironclaw.install", self.cfg.install_slug.clone())
            .with_label("ironclaw.session", session.id.as_uuid().to_string())
            .with_label(
                "ironclaw.agent_group",
                session.agent_group_id.as_uuid().to_string(),
            )
            .with_mount(Mount::Bind {
                source: paths.root.to_string_lossy().into_owned(),
                target: CONTAINER_SESSION_DIR.to_string(),
                read_only: false,
            });

        // Run the container as the host operator's UID:GID so anything
        // the agent writes through the bind mount (session_state,
        // skills.json, agent_todos.json, /data/memory/*) lands on the
        // host as files the operator can read, edit, and remove without
        // sudo. Previously the container ran as the image's USER
        // (typically root); every file required root to clean up,
        // surfaced as `Error: stepping, attempt to write a readonly
        // database` when iclaw tried to mutate outbound.db, and silent
        // EACCES on cleanup commands. Detection reads /proc/self
        // (Linux-only — matches the deployment target).
        if let Some((uid, gid)) = host_uid_gid() {
            spec = spec.with_user(format!("{uid}:{gid}"));
        }

        // Per-agent-group memory mount. Lives outside the session dir
        // so the same memory file is visible to every session of the
        // group — an agent can write a memory entry in one chat and
        // read it back from another. Created lazily here so the
        // operator doesn't need to provision the directory before the
        // first spawn. Mounted read-write because the agent is the one
        // writing to it. Disabled when `groups_dir` is unset — without
        // a per-group root there's nowhere to anchor the source.
        if let Some(groups) = self.cfg.groups_dir.as_deref() {
            let mem_src = groups
                .join(session.agent_group_id.as_uuid().to_string())
                .join("memory");
            match std::fs::create_dir_all(&mem_src) {
                Ok(()) => {
                    set_memory_dir_perms(&mem_src);
                    spec = spec.with_mount(Mount::Bind {
                        source: mem_src.to_string_lossy().into_owned(),
                        target: format!("{CONTAINER_SESSION_DIR}/memory"),
                        read_only: false,
                    });
                }
                Err(err) => {
                    warn!(
                        ?err,
                        path = %mem_src.display(),
                        "could not prepare per-group memory dir; falling back to session-local memory with UNAVAILABLE marker"
                    );
                    write_memory_unavailable_marker(&paths.root, &mem_src, &err);
                }
            }
        }

        // The runner reads its config from this path via the
        // `--config` flag wired into the entrypoint args. ContainerSpec
        // doesn't have a dedicated args field, so we encode the flag
        // by extending the entrypoint vector.
        spec.entrypoint
            .extend(vec!["--config".to_string(), format!("{CONTAINER_SESSION_DIR}/{RUNNER_CONFIG_FILENAME}")]);

        // All three forwarded surfaces (anthropic key, base URL,
        // provider keys) live behind the rotatable read-lock so the
        // SIGHUP handler can swap them mid-process without a host
        // restart.
        let rotatable = self
            .rotatable
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(key) = rotatable.anthropic_api_key.as_deref() {
            spec = spec.with_env("ANTHROPIC_API_KEY", key);
        }
        if let Some(base) = rotatable.anthropic_base_url.as_deref() {
            spec = spec.with_env("ANTHROPIC_BASE_URL", base);
        }
        // Operator-configured forwards (search API keys, etc.). Skip
        // empty values — an unset env var should not appear in the
        // container env at all. After a SIGHUP rotation that
        // *removes* a key, the missing entry is correctly dropped.
        for (k, v) in &rotatable.forward_env {
            if !v.is_empty() {
                spec = spec.with_env(k, v);
            }
        }

        // Per-group resource limits (Top 10 #8 / M13).
        if let Some(c) = cfg {
            match ResourceLimits::from_json(&c.resource_limits) {
                Ok(limits) if !limits.is_empty() => {
                    spec = spec.with_resource_limits(limits);
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(
                        agent_group = %session.agent_group_id.as_uuid(),
                        error = %e,
                        "ignoring invalid resource_limits JSON; spawning without caps"
                    );
                }
            }
            // Egress allow-list (Top 10 #6 / M13).
            if !c.egress_allow.is_empty() {
                spec = spec.with_egress_allow(c.egress_allow.clone());
            }
        }

        // Host-wide Nvidia GPU passthrough. Gated by `IRONCLAW_CONTAINER_GPU`
        // at boot so a default install never asks Docker for a device the
        // host doesn't have. When the host has `nvidia-container-toolkit`
        // installed, the agent sees the host's GPUs via `nvidia-smi`
        // inside the container and can run CUDA/Ollama workloads
        // directly.
        if self.cfg.gpu_passthrough {
            spec = spec.with_gpu_passthrough(true);
        }

        spec
    }
}

/// Container name format. Uses the session id (which is a UUID) so
/// names are globally unique and DNS-safe.
/// Resolve the host process's effective UID/GID for the container's
/// `--user` flag. Reads `/proc/self`'s ownership via the standard
/// `MetadataExt` interface so we avoid the `unsafe` `libc::geteuid`
/// call (the workspace forbids unsafe). Linux-only — returns `None` on
/// systems without `/proc`, in which case the caller falls back to the
/// image's default USER and the operator gets the legacy root-owned
/// bind-mount behaviour.
pub(super) fn host_uid_gid() -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata("/proc/self").ok()?;
    Some((meta.uid(), meta.gid()))
}

pub(super) fn container_name(_agent: AgentGroupId, session: ironclaw_types::SessionId) -> String {
    format!("ironclaw-{}", session.as_uuid())
}

/// Choose the base image for a per-group rebuild. Returns the install's
/// `default_image_tag` (which has `/usr/local/bin/ironclaw-runner`
/// baked in at setup time) when set; falls back to `debian:trixie-slim`
/// otherwise (only reachable from tests that construct a `HostConfig`
/// without going through setup). The rebuild Dockerfile only adds layers
/// on top — it doesn't COPY the runner — so basing on a runnerless image
/// produces an unspawnable container.
#[must_use]
pub fn resolve_rebuild_base(default_image_tag: &str) -> String {
    if default_image_tag.is_empty() {
        "debian:trixie-slim".to_string()
    } else {
        default_image_tag.to_string()
    }
}

/// Initial backoff after the first rebuild failure for an agent group.
/// Doubles per subsequent failure up to [`REBUILD_BACKOFF_CEILING`].
const REBUILD_BACKOFF_INITIAL: std::time::Duration =
    std::time::Duration::from_secs(60);

/// Ceiling for the rebuild backoff per group. 30 min mirrors the
/// delivery loop's `ABSOLUTE_CEILING_MS`: after this much time has
/// passed without operator action, retrying the build won't have
/// gotten cheaper, but unblocking the group does have value.
const REBUILD_BACKOFF_CEILING: std::time::Duration =
    std::time::Duration::from_secs(1_800);

/// Per-agent-group cooldown table for image rebuilds. Wraps a
/// `Mutex<HashMap>` because the spawn path is already async + holds
/// the broader manager state; trading the lock contention for not
/// having to thread a watch / arc-swap is the right call here. All
/// methods are short, lock-free I/O free.
pub struct RebuildBackoff {
    inner: std::sync::Mutex<std::collections::HashMap<AgentGroupId, RebuildBackoffEntry>>,
}

#[derive(Debug, Clone, Copy)]
struct RebuildBackoffEntry {
    /// Number of consecutive failures observed.
    consecutive_failures: u32,
    /// Earliest moment at which the next rebuild attempt is allowed.
    next_attempt_at: std::time::Instant,
}

impl Default for RebuildBackoff {
    fn default() -> Self {
        Self::new()
    }
}

impl RebuildBackoff {
    /// Build an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// If the group is still in cooldown, return the remaining
    /// duration. Returns `None` when the group has no record or the
    /// cooldown has elapsed (in which case the caller should attempt
    /// the rebuild).
    #[must_use]
    pub fn in_cooldown(&self, group: AgentGroupId) -> Option<std::time::Duration> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = guard.get(&group)?;
        let now = std::time::Instant::now();
        if entry.next_attempt_at > now {
            Some(entry.next_attempt_at - now)
        } else {
            None
        }
    }

    /// Record a successful rebuild; clears any prior cooldown for
    /// this group so the next config change is attempted immediately.
    pub fn record_success(&self, group: AgentGroupId) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(&group);
    }

    /// Record a rebuild failure for this group. Increments the
    /// consecutive-failure count, computes the next exponential
    /// backoff, and returns the duration the group is now in
    /// cooldown for (useful for log messages).
    pub fn record_failure(&self, group: AgentGroupId) -> std::time::Duration {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        let entry = guard.entry(group).or_insert(RebuildBackoffEntry {
            consecutive_failures: 0,
            next_attempt_at: now,
        });
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        let exp = entry.consecutive_failures.saturating_sub(1).min(8);
        let backoff = REBUILD_BACKOFF_INITIAL
            .saturating_mul(1u32 << exp)
            .min(REBUILD_BACKOFF_CEILING);
        entry.next_attempt_at = now + backoff;
        backoff
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::{ManagerConfig, SkillsMode};
    use super::super::prompt::MEMORY_UNAVAILABLE_FILENAME;
    use super::*;
    use ironclaw_db::central::CentralDb;
    use ironclaw_db::session::open_inbound;
    use ironclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};
    use ironclaw_db::tables::messages_in;
    use ironclaw_db::tables::sessions::{create as create_session, CreateSession};
    use ironclaw_types::{ContainerStatus, SessionId};
    use std::path::PathBuf;

    /// Backoff is empty on construction: every group is allowed to
    /// rebuild immediately.
    #[test]
    fn rebuild_backoff_clear_by_default() {
        let bo = RebuildBackoff::new();
        assert!(bo.in_cooldown(AgentGroupId::new()).is_none());
    }

    /// First failure installs a cooldown approximately equal to
    /// `REBUILD_BACKOFF_INITIAL`. Subsequent calls within the
    /// cooldown window report time remaining and don't re-trigger
    /// the rebuild.
    #[test]
    fn rebuild_backoff_first_failure_installs_initial_cooldown() {
        let bo = RebuildBackoff::new();
        let ag = AgentGroupId::new();
        let dur = bo.record_failure(ag);
        assert_eq!(dur, REBUILD_BACKOFF_INITIAL);
        let remaining = bo.in_cooldown(ag).expect("group must be in cooldown");
        assert!(remaining <= REBUILD_BACKOFF_INITIAL);
    }

    /// Consecutive failures double the backoff up to the ceiling.
    #[test]
    fn rebuild_backoff_doubles_to_ceiling() {
        let bo = RebuildBackoff::new();
        let ag = AgentGroupId::new();
        let mut prev = bo.record_failure(ag);
        // Push past the doubling threshold a few times.
        for _ in 0..6 {
            let next = bo.record_failure(ag);
            assert!(next >= prev, "backoff should not regress: prev={prev:?} next={next:?}");
            assert!(next <= REBUILD_BACKOFF_CEILING);
            prev = next;
        }
        // After many failures we should be sitting at the ceiling.
        assert_eq!(prev, REBUILD_BACKOFF_CEILING);
    }

    /// A successful rebuild clears the cooldown immediately.
    #[test]
    fn rebuild_backoff_success_clears_cooldown() {
        let bo = RebuildBackoff::new();
        let ag = AgentGroupId::new();
        let _ = bo.record_failure(ag);
        assert!(bo.in_cooldown(ag).is_some());
        bo.record_success(ag);
        assert!(bo.in_cooldown(ag).is_none());
    }

    /// Per-group isolation: a failure for group A must not delay
    /// rebuilds for group B.
    #[test]
    fn rebuild_backoff_per_group_independent() {
        let bo = RebuildBackoff::new();
        let a = AgentGroupId::new();
        let b = AgentGroupId::new();
        let _ = bo.record_failure(a);
        assert!(bo.in_cooldown(a).is_some());
        assert!(bo.in_cooldown(b).is_none(), "other group must be unaffected");
    }

    /// Regression: image rebuilds must base off the install's default
    /// image (which has the runner binary), not bare `debian:trixie-slim`.
    /// Caught live: agent emitted `install_packages` → host rebuilt
    /// against debian-slim → resulting image had apt packages but no
    /// `/usr/local/bin/ironclaw-runner` → every `runc create` failed.
    #[test]
    fn rebuild_base_prefers_default_image_tag() {
        assert_eq!(
            resolve_rebuild_base("ironclaw/session:sha256-abc123"),
            "ironclaw/session:sha256-abc123"
        );
    }

    #[test]
    fn rebuild_base_falls_back_when_default_unset() {
        // Only reachable from tests constructing HostConfig::default()
        // — a setup-completed install always has the env var. The
        // fallback keeps unit tests working without producing an
        // empty `FROM` directive in the Dockerfile.
        assert_eq!(resolve_rebuild_base(""), "debian:trixie-slim");
    }

    fn manager_cfg(data_dir: PathBuf) -> ManagerConfig {
        ManagerConfig {
            install_slug: "test".into(),
            data_dir,
            default_image_tag: "ironclaw/session:test".into(),
            default_provider: "anthropic".into(),
            default_model: "claude-sonnet-4-6".into(),
            anthropic_api_key: Some("sk-test".into()),
            anthropic_base_url: Some("https://openrouter.ai/api/v1".into()),
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            heartbeat_stale_secs: DEFAULT_HEARTBEAT_STALE_SECS,
            stop_grace_secs: DEFAULT_STOP_GRACE_SECS,
            skills_dir: None,
            groups_dir: None,
            skills_mode: SkillsMode::Inline,
            gpu_passthrough: false,
            forward_env: Vec::new(),
        }
    }

    fn fixture_session(db: &CentralDb) -> Session {
        let ag = create_ag(
            db,
            CreateAgentGroup {
                name: "demo".into(),
                folder: "demo".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        create_session(
            db,
            CreateSession {
                agent_group_id: ag.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn container_name_is_deterministic_and_uuid_shaped() {
        let s = SessionId(uuid::Uuid::nil());
        let ag = AgentGroupId::new();
        let name = container_name(ag, s);
        assert_eq!(name, "ironclaw-00000000-0000-0000-0000-000000000000");
    }

    #[test]
    fn build_spec_includes_bind_label_env_entrypoint() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "ironclaw/session:abc", None);
        // Image
        assert_eq!(spec.image, "ironclaw/session:abc");
        // Entrypoint includes the runner path + --config arg
        assert_eq!(spec.entrypoint[0], CONTAINER_RUNNER_PATH);
        assert_eq!(spec.entrypoint[1], "--config");
        assert!(spec.entrypoint[2].ends_with(RUNNER_CONFIG_FILENAME));
        // Mount the session root at /data
        let bind = spec
            .mounts
            .iter()
            .find_map(|m| match m {
                Mount::Bind { source, target, read_only } => Some((source, target, read_only)),
                _ => None,
            })
            .unwrap();
        assert_eq!(bind.1, CONTAINER_SESSION_DIR);
        assert!(!*bind.2);
        // Env carries both API key and base URL.
        assert!(spec.env.iter().any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "sk-test"));
        assert!(spec.env.iter().any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v.contains("openrouter")));
        // Labels for orphan cleanup.
        assert_eq!(spec.labels.get("ironclaw.install").map(String::as_str), Some("test"));
        assert!(spec.labels.contains_key("ironclaw.session"));
        assert!(spec.labels.contains_key("ironclaw.agent_group"));
    }

    #[test]
    #[cfg(unix)]
    fn build_spec_runs_container_as_host_user() {
        use std::os::unix::fs::MetadataExt;
        // Regression: containers used to inherit the image's default
        // USER (root), so every file the agent wrote via the bind mount
        // landed on the host as root-owned. iclaw could not modify
        // outbound.db without sudo. Now build_spec sets `--user
        // <uid>:<gid>` matching the host process so files are operator-
        // owned.
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "img", None);
        let user = spec.user.as_deref().expect("spec.user should be set");
        let (uid_str, gid_str) = user.split_once(':').expect("user is uid:gid");
        let uid: u32 = uid_str.parse().expect("uid is numeric");
        let gid: u32 = gid_str.parse().expect("gid is numeric");
        let meta = std::fs::metadata("/proc/self").unwrap();
        assert_eq!(uid, meta.uid());
        assert_eq!(gid, meta.gid());
    }

    #[test]
    fn build_spec_forwards_extra_env_and_skips_empty_values() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.forward_env = vec![
            ("TAVILY_API_KEY".to_string(), "tav-secret".to_string()),
            ("EXA_API_KEY".to_string(), "exa-secret".to_string()),
            // Empty values must not appear in the container env.
            ("BRAVE_SEARCH_API_KEY".to_string(), String::new()),
        ];
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "img", None);
        assert!(spec.env.iter().any(|(k, v)| k == "TAVILY_API_KEY" && v == "tav-secret"));
        assert!(spec.env.iter().any(|(k, v)| k == "EXA_API_KEY" && v == "exa-secret"));
        assert!(
            spec.env.iter().all(|(k, _)| k != "BRAVE_SEARCH_API_KEY"),
            "empty-valued forward must be skipped"
        );
    }

    #[test]
    fn build_spec_skips_base_url_when_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.anthropic_base_url = None;
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "img", None);
        assert!(spec.env.iter().all(|(k, _)| k != "ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn build_spec_mounts_per_group_memory_when_groups_dir_set() {
        let tmp = tempfile::tempdir().unwrap();
        let groups = tmp.path().join("groups");
        std::fs::create_dir_all(&groups).unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.groups_dir = Some(groups.clone());
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "img", None);
        let memory_mount = spec.mounts.iter().find_map(|m| match m {
            Mount::Bind {
                source,
                target,
                read_only,
            } if target == &format!("{CONTAINER_SESSION_DIR}/memory") => {
                Some((source.clone(), *read_only))
            }
            _ => None,
        });
        let (src, ro) = memory_mount.expect("memory mount present");
        let expected = groups
            .join(session.agent_group_id.as_uuid().to_string())
            .join("memory");
        assert_eq!(src, expected.to_string_lossy().to_string());
        assert!(!ro);
        // Mount source dir is created lazily.
        assert!(expected.is_dir());
    }

    #[test]
    fn build_spec_skips_memory_mount_when_groups_dir_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.groups_dir = None;
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "img", None);
        let has_memory = spec.mounts.iter().any(|m| match m {
            Mount::Bind { target, .. } => target == &format!("{CONTAINER_SESSION_DIR}/memory"),
            _ => false,
        });
        assert!(!has_memory, "memory mount must not appear without groups_dir");
    }

    #[test]
    fn build_spec_applies_resource_limits_from_config() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        // Build a minimal ContainerConfig with resource limits set.
        let cfg = container_configs::ContainerConfig {
            agent_group_id: session.agent_group_id,
            provider: None,
            model: None,
            effort: None,
            image_tag: None,
            assistant_name: None,
            max_messages_per_prompt: None,
            skills: container_configs::SkillsSelector::All,
            mcp_servers: serde_json::json!({}),
            packages_apt: vec![],
            packages_npm: vec![],
            additional_mounts: serde_json::json!([]),
            cli_scope: container_configs::CliScope::Group,
            config_fingerprint: None,
            egress_allow: vec![],
            resource_limits: serde_json::json!({"cpus": "1.5", "memory_mb": 512u64}),
            coding_enabled: false,
            updated_at: chrono::Utc::now(),
        };
        let spec = mgr.build_spec(&session, &paths, "img", Some(&cfg));
        assert!(!spec.resource_limits.is_empty());
        assert!((spec.resource_limits.cpus.unwrap() - 1.5).abs() < f64::EPSILON);
        assert_eq!(spec.resource_limits.memory_mb, Some(512));
    }

    #[test]
    fn build_spec_applies_egress_allow_from_config() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let cfg = container_configs::ContainerConfig {
            agent_group_id: session.agent_group_id,
            provider: None,
            model: None,
            effort: None,
            image_tag: None,
            assistant_name: None,
            max_messages_per_prompt: None,
            skills: container_configs::SkillsSelector::All,
            mcp_servers: serde_json::json!({}),
            packages_apt: vec![],
            packages_npm: vec![],
            additional_mounts: serde_json::json!([]),
            cli_scope: container_configs::CliScope::Group,
            config_fingerprint: None,
            egress_allow: vec!["api.example.com:443".into(), "db.local:5432".into()],
            resource_limits: serde_json::json!({}),
            coding_enabled: false,
            updated_at: chrono::Utc::now(),
        };
        let spec = mgr.build_spec(&session, &paths, "img", Some(&cfg));
        assert_eq!(spec.egress_allow, vec!["api.example.com:443", "db.local:5432"]);
    }

    #[test]
    fn build_spec_empty_egress_allow_stays_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let cfg = container_configs::ContainerConfig {
            agent_group_id: session.agent_group_id,
            provider: None,
            model: None,
            effort: None,
            image_tag: None,
            assistant_name: None,
            max_messages_per_prompt: None,
            skills: container_configs::SkillsSelector::All,
            mcp_servers: serde_json::json!({}),
            packages_apt: vec![],
            packages_npm: vec![],
            additional_mounts: serde_json::json!([]),
            cli_scope: container_configs::CliScope::Group,
            config_fingerprint: None,
            egress_allow: vec![],
            resource_limits: serde_json::json!({}),
            coding_enabled: false,
            updated_at: chrono::Utc::now(),
        };
        let spec = mgr.build_spec(&session, &paths, "img", Some(&cfg));
        assert!(spec.egress_allow.is_empty());
    }

    #[tokio::test]
    async fn tick_skips_sessions_without_pending_inbound() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = std::sync::Arc::new(crate::tests::NoopRuntime::default());
        let mgr = ContainerManager::new(
            db.clone(),
            runtime.clone(),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let _session = fixture_session(&db);
        mgr.tick().await.unwrap();
        // session_status is unchanged: NoopRuntime would have been
        // called, but only if pending inbound existed.
        let sessions_list = sessions::list_active(&db).unwrap();
        for s in sessions_list {
            assert!(matches!(s.container_status, ContainerStatus::Stopped));
        }
    }

    #[tokio::test]
    async fn tick_spawns_when_inbound_has_pending_work() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = std::sync::Arc::new(crate::tests::NoopRuntime::default());
        let mgr = ContainerManager::new(
            db.clone(),
            runtime.clone(),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        // Seed pending inbound (even seq, status='pending').
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let conn = open_inbound(&paths).unwrap();
        messages_in::insert(
            &conn,
            &messages_in::WriteInbound {
                id: ironclaw_types::MessageId::new(),
                kind: ironclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "hi"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("stdin".into()),
                channel_type: Some(ironclaw_types::ChannelType::new("cli")),
                thread_id: None,
                source_session_id: None,
            },
        )
        .unwrap();

        mgr.tick().await.unwrap();

        // Session should now be marked running.
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Running));
        // The noop runtime records the spawn call.
        assert!(runtime.spawn_calls().iter().any(|name| {
            name.contains(&session.id.as_uuid().to_string())
        }));
        // Runner config got written.
        assert!(paths.root.join(RUNNER_CONFIG_FILENAME).is_file());
    }

    /// Finding 7: when the per-group memory dir cannot be created, the
    /// host drops a session-local `memory/UNAVAILABLE.md` marker so the
    /// agent inside the container can detect the degraded mount.
    #[test]
    fn build_spec_writes_memory_unavailable_marker_when_groups_dir_unwriteable() {
        let tmp = tempfile::tempdir().unwrap();
        // Point groups_dir at a path whose parent is a regular file —
        // `create_dir_all` cannot create a directory under a non-dir.
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let groups = blocker.join("groups");
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.groups_dir = Some(groups);
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let spec = mgr.build_spec(&session, &paths, "img", None);
        // The bind mount is skipped (no source could be prepared).
        let has_memory_mount = spec.mounts.iter().any(|m| match m {
            Mount::Bind { target, .. } => target == &format!("{CONTAINER_SESSION_DIR}/memory"),
            _ => false,
        });
        assert!(!has_memory_mount, "memory mount must not appear when source prep failed");
        // The marker file lands inside the session root so /data/memory
        // is still browsable from the container.
        let marker = paths.root.join("memory").join(MEMORY_UNAVAILABLE_FILENAME);
        assert!(
            marker.is_file(),
            "expected UNAVAILABLE.md marker at {}",
            marker.display()
        );
        let body = std::fs::read_to_string(&marker).unwrap();
        assert!(body.contains("Memory mount unavailable"));
    }

    /// Finding 8: the per-group memory dir is relaxed to group-writeable
    /// (`0o775`) so the operator (host uid) can clean up files the
    /// container's root user wrote into the bind without sudo.
    #[cfg(unix)]
    #[test]
    fn build_spec_per_group_memory_dir_is_group_writeable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let groups = tmp.path().join("groups");
        std::fs::create_dir_all(&groups).unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.groups_dir = Some(groups.clone());
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let _spec = mgr.build_spec(&session, &paths, "img", None);
        let mem = groups
            .join(session.agent_group_id.as_uuid().to_string())
            .join("memory");
        let mode = std::fs::metadata(&mem).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o775,
            "expected 0o775 mode bits on per-group memory dir, got {mode:o}"
        );
    }

    // ── degraded mode ──────────────────────────────────────────────

    /// Boot-time image health check failed → host enters degraded
    /// mode → `maybe_spawn` refuses with `HostDegraded`.
    #[tokio::test]
    async fn degraded_mode_refuses_spawn() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = std::sync::Arc::new(crate::tests::NoopRuntime::default());
        let mgr = ContainerManager::new(
            db.clone(),
            runtime.clone(),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        // Seed pending inbound so the gate would otherwise spawn.
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let conn = open_inbound(&paths).unwrap();
        messages_in::insert(
            &conn,
            &messages_in::WriteInbound {
                id: ironclaw_types::MessageId::new(),
                kind: ironclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "hi"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("stdin".into()),
                channel_type: Some(ironclaw_types::ChannelType::new("cli")),
                thread_id: None,
                source_session_id: None,
            },
        )
        .unwrap();

        // Flip into degraded mode programmatically (in production
        // this is done by run_host after the image health check
        // returns an Err).
        mgr.set_degraded();
        assert!(mgr.is_degraded(), "manager should report degraded");

        let err = mgr
            .maybe_spawn(&session)
            .await
            .expect_err("must refuse to spawn when degraded");
        assert!(
            matches!(err, ManagerError::HostDegraded),
            "expected HostDegraded, got {err:?}"
        );
        // The runtime must NOT have been called.
        assert!(
            runtime.spawn_calls().is_empty(),
            "no runtime spawn allowed in degraded mode"
        );
    }

    // ── SIGHUP env rotation ────────────────────────────────────────

    #[test]
    fn reload_env_updates_rotatable_and_returns_changed_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let env_path = tmp.path().join(".env");
        std::fs::write(&env_path, "ANTHROPIC_API_KEY=rotated-key\nEXA_API_KEY=exa-1\n").unwrap();
        let changed = mgr.reload_env(Some(&env_path));
        assert!(changed.contains(&"ANTHROPIC_API_KEY".to_string()), "{changed:?}");
        assert!(changed.contains(&"EXA_API_KEY".to_string()), "{changed:?}");
        // RwLock now reflects the rotation.
        let r = mgr.rotatable.read().unwrap();
        assert_eq!(r.anthropic_api_key.as_deref(), Some("rotated-key"));
        assert!(r.forward_env.iter().any(|(k, v)| k == "EXA_API_KEY" && v == "exa-1"));
    }

    #[test]
    fn reload_env_drops_keys_that_disappear() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        // Seed an EXA_API_KEY at construction time so it's in the
        // initial RotatableConfig; the env file rotation will drop it.
        cfg.forward_env = vec![("EXA_API_KEY".into(), "exa-old".into())];
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let env_path = tmp.path().join(".env");
        // New env has no EXA_API_KEY → the key should be dropped.
        std::fs::write(&env_path, "ANTHROPIC_API_KEY=sk-new\n").unwrap();
        let changed = mgr.reload_env(Some(&env_path));
        assert!(changed.contains(&"EXA_API_KEY".to_string()));
        let r = mgr.rotatable.read().unwrap();
        assert!(
            r.forward_env.iter().all(|(k, _)| k != "EXA_API_KEY"),
            "EXA_API_KEY must be dropped after rotation, got: {:?}",
            r.forward_env
        );
    }

    #[test]
    fn reload_env_with_no_file_returns_empty_changed_when_nothing_was_set() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.anthropic_api_key = None;
        cfg.anthropic_base_url = None;
        cfg.forward_env = vec![];
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let changed = mgr.reload_env(None);
        assert!(changed.is_empty(), "{changed:?}");
    }

}
