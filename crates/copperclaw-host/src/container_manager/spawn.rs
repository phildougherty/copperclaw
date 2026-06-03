//! Container spawn pipeline: `maybe_spawn`, `build_spec`, image rebuild + backoff.

use super::prompt::{
    set_memory_dir_perms, write_memory_unavailable_marker, write_memory_unavailable_marker_str,
};
use super::{ContainerManager, ManagerError};
use copperclaw_container_rt::{ContainerSpec, ImageBuildSpec, Mount, ResourceLimits, RtError};
use copperclaw_db::session::SessionPaths;
use copperclaw_db::tables::{container_configs, sessions};
use copperclaw_types::{AgentGroupId, Session};
use tracing::{info, warn};

/// Default poll cadence. The router debounces inbound, so once a
/// message has settled in `messages_in` we want to spawn fast — this
/// is the tail latency between "user typed" and "container starts
/// running". 1s feels right for a local cli loop; the sweep loop runs
/// at 60s and handles the slower lifecycle work.
pub const POLL_INTERVAL_MS: u64 = 1000;

/// Path inside the container where the session dir is mounted.
pub const CONTAINER_SESSION_DIR: &str = "/data";

/// Path inside a `create_agent` sibling's container where the PARENT
/// agent's session dir is mounted read-only. Used ONLY as the fallback
/// when the parent workspace is NOT a git repo (so there's nothing to make
/// a worktree from): the sibling can still review / audit / search the
/// parent's code, but can't modify it. When the parent IS a git repo, the
/// sibling instead gets a WRITABLE worktree at [`CONTAINER_WORKSPACE_DIR`].
pub const CONTAINER_PARENT_DIR: &str = "/parent";

/// Path inside a `create_agent` sibling's container where a WRITABLE git
/// worktree of the parent's repo is mounted (branch `sib/<session-id>`).
/// The sibling edits + commits here; commits land in the parent repo's
/// shared object store, so the parent sees the branch immediately and can
/// review/merge it (`git diff main..sib/<id>`, `git merge sib/<id>`) from
/// its own `/data`. The parent's *checked-out* working tree is never
/// mounted into the sibling, so the parent's files on disk stay untouched.
pub const CONTAINER_WORKSPACE_DIR: &str = "/workspace";

/// Filename of the runner-config JSON written into the session dir.
pub const RUNNER_CONFIG_FILENAME: &str = "runner.json";

/// Path inside the container where the runner binary lives. Must
/// match the path baked into the session image at build time.
pub const CONTAINER_RUNNER_PATH: &str = "/usr/local/bin/copperclaw-runner";

/// Skill names belonging to the Phase E coding bundle. Filtered out
/// of the resolved skill set when `container_configs.coding_enabled`
/// is false. Acts as a cap on `SkillsSelector::All`; explicit
/// selector lists are honoured as-is (the operator picked the names
/// deliberately).
pub const CODING_SKILL_NAMES: &[&str] = &["coding-task", "git-commit", "code-review", "testing"];

/// Default idle window before the manager stops a running container.
/// 300s (5 min) matches the OpenBSD-of-claw-agents "conservative
/// defaults" tenet — long enough to avoid thrashing on quiet groups,
/// short enough that an unattended host doesn't burn memory.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;

/// Default heartbeat-staleness threshold. The runner refreshes its
/// `.heartbeat` file's mtime every ~1s as part of the poll loop; if
/// the host hasn't seen a refresh for this long, the runner is
/// presumed dead and the container is reset for respawn.
///
/// **Must stay ≥ 2× the runner's `DEFAULT_PROVIDER_DEADLINE_MS / 1000`**
/// (see `copperclaw-runner/src/run/mod.rs::DEFAULT_PROVIDER_DEADLINE_MS`).
/// Rationale: the runner's `HeartbeatTicker` only fires every 5s, and a
/// provider call that blocks for the full 60s deadline can leave the
/// last touch ~5s in the past by the time the deadline trips. If this
/// threshold equals the deadline, the host can mark the container stale
/// and SIGKILL it the same instant the provider call would have returned
/// `Err(DeadlineExceeded)` — losing the work and triggering a respawn
/// loop on slow models. 120s gives the runner room to fail a 60s
/// provider call cleanly (emit `usage_report`, finalise the inbound) before
/// being declared dead. The startup check in
/// [`check_heartbeat_deadline_alignment`] warns operators who tighten one
/// side without the other.
pub const DEFAULT_HEARTBEAT_STALE_SECS: u64 = 120;

/// Grace period passed to `runtime.stop` on idle / crash transitions.
/// 5s is enough for the runner to flush an in-flight HTTP call.
pub const DEFAULT_STOP_GRACE_SECS: u64 = 5;

/// Verify that the host's `heartbeat_stale_secs` leaves the runner room
/// to fail a provider call before being declared dead. The runner's
/// `HeartbeatTicker` fires every 5s and a provider attempt can block
/// for the full `provider_deadline_ms` — if the stale threshold equals
/// the deadline, the host can race the runner and SIGKILL the
/// container the same moment the provider call returns
/// `DeadlineExceeded`. We require `heartbeat_stale_secs >= 2 *
/// (provider_deadline_ms / 1000)` so the runner has its full provider
/// budget plus a turn-worth of margin to land the failure cleanly.
///
/// Returns `Ok(())` when the relationship holds. Returns a human-
/// readable warning string otherwise; callers should log it at `warn!`
/// rather than panic — an operator may have set both sides
/// deliberately. Called from `boot.rs::spawn_container_manager` at
/// startup so the warning lands once in the host log with both
/// values visible.
///
/// # Errors
///
/// Returns `Err(String)` describing the misconfiguration when the
/// safety relationship does not hold; the returned message names both
/// values and the required relationship so the operator can act
/// without spelunking the source.
pub fn check_heartbeat_deadline_alignment(
    heartbeat_stale_secs: u64,
    provider_deadline_ms: u64,
) -> Result<(), String> {
    // Integer-divide ceilingly so a deadline of e.g. 60_500ms reads as
    // 61s, not 60s — the operator is asking for "just over a minute",
    // and the safety margin should reflect that.
    let deadline_secs = provider_deadline_ms.div_ceil(1_000);
    let required = deadline_secs.saturating_mul(2);
    if heartbeat_stale_secs >= required {
        return Ok(());
    }
    Err(format!(
        "heartbeat_stale_secs ({heartbeat_stale_secs}s) < 2 x provider_deadline ({deadline_secs}s = {provider_deadline_ms}ms); \
         the host may declare a container stale and SIGKILL it before a slow provider call completes — \
         raise heartbeat_stale_secs to at least {required}s or lower the provider deadline"
    ))
}

/// Tighten a session directory to owner-only (`0700`). Best-effort and a
/// no-op on non-unix targets; a failure is logged at `warn!` rather than
/// aborting the spawn, because a slightly-too-loose dir is preferable to a
/// group going dark over a chmod hiccup (e.g. the host doesn't own the path).
/// Set explicitly on every spawn so the permission is enforced regardless of
/// the host's umask.
fn harden_dir_0700(path: &std::path::Path) {
    set_mode(path, 0o700, "session directory");
}

/// Tighten a secret-bearing session file to owner read/write (`0600`).
/// Same best-effort, umask-independent contract as [`harden_dir_0700`].
fn harden_file_0600(path: &std::path::Path) {
    set_mode(path, 0o600, "session config file");
}

/// Shared chmod helper for the session-dir hardening. `#[cfg(unix)]`-gated;
/// on non-unix it consumes the args and returns so callers stay portable.
fn set_mode(path: &std::path::Path, mode: u32, what: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
            warn!(
                ?err,
                path = %path.display(),
                mode = format!("{mode:o}"),
                "could not tighten {what} permissions"
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode, what);
    }
}

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
        let paths = SessionPaths::new(&self.cfg.data_dir, session.agent_group_id, session.id);
        paths.ensure_dirs().map_err(ManagerError::Io)?;
        // Harden the session dir to owner-only (`0700`) explicitly rather
        // than relying on the host's umask — the dir holds the session DBs,
        // attachments, and `runner.json` (provider + model config). Set it on
        // every spawn so a dir created under a permissive umask by an older
        // host (or restored from a backup) gets tightened on the next tick.
        harden_dir_0700(&paths.root);

        if !Self::has_pending_inbound(&paths)? {
            return Ok(false);
        }

        // Budget gate. If the group has a daily_token_cap and today's
        // turns already meet/exceed it, refuse to spawn. The inbound
        // sits in the row until the cap resets at UTC midnight or the
        // operator raises it via `cclaw groups budget set`.
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
            copperclaw_metrics::inc_budget_exhausted(
                &session.agent_group_id.as_uuid().to_string(),
                copperclaw_metrics::BUDGET_GATE_DAILY_TOKENS,
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
            copperclaw_metrics::inc_budget_exhausted(
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
        let runner_cfg =
            self.runner_config_for(session, cfg_row.as_ref(), Some(paths.root.as_path()));
        let runner_json = serde_json::to_vec_pretty(&runner_cfg).map_err(ManagerError::Json)?;
        let runner_cfg_path = paths.root.join(RUNNER_CONFIG_FILENAME);
        std::fs::write(&runner_cfg_path, runner_json).map_err(ManagerError::Io)?;
        // `runner.json` carries the provider/model config the runner reads on
        // boot. Lock it to owner read/write (`0600`) explicitly, not via
        // umask, so a permissive default can't leave it group/world-readable.
        harden_file_0600(&runner_cfg_path);

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
            let base_tag = cfg
                .image_tag
                .clone()
                .unwrap_or_else(|| self.cfg.default_image_tag.clone());
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
                        "image rebuild in cooldown and no fallback tag is configured".into(),
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
                        copperclaw_metrics::inc_image_rebuild_failed();
                        base_tag
                    }
                    Err(err) => {
                        self.rebuild_backoff.record_failure(session.agent_group_id);
                        copperclaw_metrics::inc_image_rebuild_failed();
                        return Err(err);
                    }
                }
            }
        } else {
            self.cfg.default_image_tag.clone()
        };

        let spec = self.build_spec(session, &paths, &image_tag, cfg_row.as_ref())?;
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
        copperclaw_metrics::inc_containers_spawned();
        copperclaw_metrics::observe_container_spawn_seconds(spawn_elapsed);
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
        // (which has `/usr/local/bin/copperclaw-runner` baked in at setup
        // time), NOT on bare `debian:trixie-slim`. The rebuild's
        // Dockerfile only adds layers (apt / npm / labels) — it does
        // not COPY the runner binary, so building from a runnerless
        // base produces a runnerless image and every subsequent
        // `runc create` fails with "stat /usr/local/bin/copperclaw-runner:
        // no such file or directory". Caught live: an agent emitted
        // `install_packages` → host auto-rebuilt → container_configs
        // got pinned to a 413MB runnerless image → all subsequent
        // spawns wedged. Falling back to a hard-coded slim base when
        // `default_image_tag` is empty (shouldn't happen in a
        // setup-completed install, but the fallback keeps tests that
        // run with `HostConfig::default()` working).
        let base = resolve_rebuild_base(&self.cfg.default_image_tag);
        let mut build_spec = ImageBuildSpec::new("copperclaw/session", &base);
        build_spec.apt_packages.clone_from(&cfg.packages_apt);
        build_spec.npm_packages.clone_from(&cfg.packages_npm);
        let tag = self
            .runtime
            .build_image(build_spec)
            .await
            .map_err(ManagerError::Spawn)?;
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

    /// Resolve a group's effective provider string using the same
    /// precedence + alias normalisation the runner config applies
    /// (`session.agent_provider` → per-group `container_config.provider` →
    /// host `default_provider`, with `"claude"` ⇒ `"anthropic"` and an empty
    /// value ⇒ `None`). Shared by the runner-config builder and the egress
    /// resolver so the auto-injected model endpoint can never disagree with
    /// the provider the runner actually drives.
    pub(super) fn resolved_provider(
        &self,
        session: &Session,
        cfg: Option<&container_configs::ContainerConfig>,
    ) -> Option<String> {
        let raw = session
            .agent_provider
            .clone()
            .or_else(|| cfg.and_then(|c| c.provider.clone()))
            .unwrap_or_else(|| self.cfg.default_provider.clone());
        match raw.as_str() {
            "" => None,
            "claude" => Some("anthropic".to_string()),
            other => Some(other.to_string()),
        }
    }

    pub(super) fn build_spec(
        &self,
        session: &Session,
        paths: &SessionPaths,
        image_tag: &str,
        cfg: Option<&container_configs::ContainerConfig>,
    ) -> Result<ContainerSpec, ManagerError> {
        // toctou redux: validate the session-root source against the live
        // sessions root before mounting. If a path component was swapped for
        // a symlink that escapes the sessions dir (or the source otherwise
        // leaves it), the spawn is refused — the agent's `/data` is the one
        // mount it cannot run without, so a compromised source must fail the
        // spawn outright rather than degrade.
        let sessions_root = self.sessions_root();
        super::mount_guard::validate_source(&paths.root, sessions_root)
            .map_err(ManagerError::UnsafeMount)?;

        let mut spec = ContainerSpec::new(
            container_name(session.agent_group_id, session.id),
            image_tag,
        )
        .with_entrypoint(vec![CONTAINER_RUNNER_PATH.to_string()])
        // Start the agent in the writable session bind mount, and
        // anchor $HOME there too. The container runs as a non-root
        // uid with no passwd entry, so the image defaults (cwd `/`,
        // `HOME=/`) are unwritable: every tool that caches under
        // $HOME (go-build, npm, pip, cargo) dies with
        // "mkdir /.cache: permission denied", and every relative
        // write/`mkdir` fails with EACCES until the agent manually
        // cds. Pointing both at the session dir fixes both classes.
        .with_working_dir(CONTAINER_SESSION_DIR)
        .with_env("HOME", CONTAINER_SESSION_DIR)
        .with_label("copperclaw.install", self.cfg.install_slug.clone())
        .with_label("copperclaw.session", session.id.as_uuid().to_string())
        .with_label(
            "copperclaw.agent_group",
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
        // database` when cclaw tried to mutate outbound.db, and silent
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
                    // toctou redux: validate the memory source against the
                    // per-group root (its own root, distinct from the
                    // sessions root). A symlink swap here would let a
                    // group's `/data/memory` escape `groups_dir`. Memory is
                    // optional, so a failed check drops the mount + leaves
                    // the UNAVAILABLE marker rather than failing the spawn.
                    if let Err(err) = super::mount_guard::validate_source(&mem_src, groups) {
                        warn!(
                            %err,
                            path = %mem_src.display(),
                            "per-group memory source failed mount validation; skipping memory mount"
                        );
                        write_memory_unavailable_marker_str(
                            &paths.root,
                            &mem_src,
                            &err.to_string(),
                        );
                    } else {
                        set_memory_dir_perms(&mem_src);
                        spec = spec.with_mount(Mount::Bind {
                            source: mem_src.to_string_lossy().into_owned(),
                            target: format!("{CONTAINER_SESSION_DIR}/memory"),
                            read_only: false,
                        });
                    }
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

        // A `create_agent` sibling gets access to the PARENT agent's
        // workspace (writable git worktree, or read-only fallback). See
        // [`apply_parent_workspace`]. Each parent-derived bind source is
        // validated against the live sessions root before mounting; a
        // compromised parent source drops the workspace mount rather than
        // failing the sibling spawn (it can still run with an empty `/data`).
        spec = apply_parent_workspace(spec, session, paths, sessions_root);

        // The runner reads its config from this path via the
        // `--config` flag wired into the entrypoint args. ContainerSpec
        // doesn't have a dedicated args field, so we encode the flag
        // by extending the entrypoint vector.
        spec.entrypoint.extend(vec![
            "--config".to_string(),
            format!("{CONTAINER_SESSION_DIR}/{RUNNER_CONFIG_FILENAME}"),
        ]);

        // All three forwarded surfaces (anthropic key, base URL,
        // provider keys) live behind the rotatable read-lock so the
        // SIGHUP handler can swap them mid-process without a host
        // restart.
        let rotatable = self
            .rotatable
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // Credential broker (Phase 0b). When the broker is enabled we MUST NOT
        // forward the real provider key into the container env — a shell inside
        // the container could `printenv ANTHROPIC_API_KEY` and exfiltrate it.
        // Instead we mint a per-session, group-scoped, TTL-bounded, revocable
        // capability TOKEN and put it in the `ANTHROPIC_API_KEY` slot (the
        // runner reads that slot verbatim), and point `ANTHROPIC_BASE_URL` at
        // the host-side broker loopback. The broker validates the token,
        // injects the real key host-side, and forwards upstream.
        //
        // When the broker is disabled (the default), behaviour is unchanged:
        // the real key + base URL are forwarded exactly as before.
        let broker_mint = self
            .broker
            .as_ref()
            .zip(self.broker_base_url.as_deref())
            .map(|(broker, base_url)| {
                let token = broker.mint_for(
                    session.id,
                    session.agent_group_id,
                    super::broker::now_epoch_secs(),
                );
                (token, base_url)
            });
        if let Some((token, base_url)) = broker_mint {
            // Broker enabled: the token rides the ANTHROPIC_API_KEY slot and
            // the base URL points at the host-side broker loopback. The real
            // key never enters the container env.
            spec = spec
                .with_env("ANTHROPIC_API_KEY", &token)
                .with_env("ANTHROPIC_BASE_URL", base_url);
        } else {
            // Default (broker disabled): forward the real key + base URL
            // exactly as before.
            if let Some(key) = rotatable.anthropic_api_key.as_deref() {
                spec = spec.with_env("ANTHROPIC_API_KEY", key);
            }
            if let Some(base) = rotatable.anthropic_base_url.as_deref() {
                spec = spec.with_env("ANTHROPIC_BASE_URL", base);
            }
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
        }

        // Egress allow-list (Phase 0a v1 / Top 10 #6). Resolve the effective
        // list = operator-configured per-group entries UNIONed with the
        // auto-injected REAL model endpoint. The injected endpoint is
        // provider-aware (anthropic/openrouter via ANTHROPIC_BASE_URL,
        // native ollama via OLLAMA_BASE_URL, each with its real default
        // applied when the env var is unset) so deny-default can never
        // blackhole model traffic — including the common single-provider
        // Anthropic deployment with no ANTHROPIC_BASE_URL override. The
        // posture is carried regardless of mode so `cclaw doctor` / logs can
        // report it; the container runtime decides what (if anything) is
        // enforced. See [`super::egress`] and
        // `copperclaw_container_rt::EgressMode`.
        let group_allow = cfg.map_or(&[][..], |c| c.egress_allow.as_slice());
        let provider = self.resolved_provider(session, cfg);
        let ollama_base_url = rotatable
            .forward_env
            .iter()
            .find(|(k, _)| k == "OLLAMA_BASE_URL")
            .map(|(_, v)| v.as_str());
        let resolved_allow = super::egress::resolve_allow_list_for_provider(
            group_allow,
            provider.as_deref(),
            rotatable.anthropic_base_url.as_deref(),
            ollama_base_url,
        );
        if !resolved_allow.is_empty() {
            spec = spec.with_egress_allow(resolved_allow);
        }
        spec = spec.with_egress_mode(self.cfg.egress_mode);

        // Host-wide Nvidia GPU passthrough. Gated by `COPPERCLAW_CONTAINER_GPU`
        // at boot so a default install never asks Docker for a device the
        // host doesn't have. When the host has `nvidia-container-toolkit`
        // installed, the agent sees the host's GPUs via `nvidia-smi`
        // inside the container and can run CUDA/Ollama workloads
        // directly.
        if self.cfg.gpu_passthrough {
            spec = spec.with_gpu_passthrough(true);
        }

        Ok(spec)
    }
}

/// Share the PARENT agent's workspace into a `create_agent` sibling.
///
/// A sibling (recorded with a parent `source_session_id`) starts with an
/// empty `/data`. The parent lives under a different agent group, so we
/// locate it by scanning `<data>/sessions/*/<parent_session_id>` — session
/// ids are unique UUIDs. Best-effort: if the parent can't be located, the
/// spec is returned unchanged.
///
/// If the parent is currently working inside a GIT REPO, the sibling gets
/// a WRITABLE worktree of THAT repo on its own branch (`sib/<id>`) at
/// `/workspace` — it can edit and commit, and the parent reviews/merges the
/// branch from its own checkout (the worktree shares the repo's object
/// store). The repo's `.git` is mounted at its IDENTICAL host-absolute path
/// so the worktree's `gitdir:` pointer resolves inside the container with no
/// rewriting; the parent's checked-out files are never mounted, so they
/// stay physically untouched.
///
/// "The repo the parent is working in" is resolved from the parent's
/// persisted shell cwd (see [`resolve_parent_repo_root`]) — a single session
/// commonly holds several projects (each its own subdir repo) across
/// `/clear`, so we worktree whichever one the parent is cd'd into rather
/// than assuming `/data` itself is the repo. Otherwise (no repo at/above the
/// cwd, and none at the workspace root) we fall back to the READ-ONLY
/// `/parent` mount of the whole workspace so the sibling can still review.
fn apply_parent_workspace(
    mut spec: ContainerSpec,
    session: &Session,
    paths: &SessionPaths,
    sessions_root: &std::path::Path,
) -> ContainerSpec {
    let Some(parent_sid) = session.source_session_id else {
        return spec;
    };
    let Some(sessions_dir) = paths.root.parent().and_then(|p| p.parent()) else {
        return spec;
    };
    let Some(parent_root) = find_session_root(sessions_dir, &parent_sid.as_uuid().to_string())
    else {
        return spec;
    };

    let sibling_sid = session.id.as_uuid().to_string();
    let worktree = resolve_parent_repo_root(&parent_root)
        .and_then(|repo_root| provision_parent_worktree(&repo_root, &sibling_sid));
    if let Some(wt) = worktree {
        // toctou redux: both parent-derived sources are host-controlled
        // paths under the sessions root. Validate each before mounting; a
        // swapped-symlink component (or an escape) drops the whole writable
        // workspace rather than handing dockerd a compromised source. The
        // sibling can still run with an empty `/data`.
        if let Err(err) = super::mount_guard::validate_source(&wt.git_dir, sessions_root) {
            warn!(%err, path = %wt.git_dir.display(), "parent .git source failed mount validation; skipping workspace");
            return spec;
        }
        if let Err(err) = super::mount_guard::validate_source(&wt.worktree_dir, sessions_root) {
            warn!(%err, path = %wt.worktree_dir.display(), "parent worktree source failed mount validation; skipping workspace");
            return spec;
        }
        let git_dir = wt.git_dir.to_string_lossy().into_owned();
        spec = spec
            // Shared `.git` at its host-absolute path (RW: the worktree's
            // commits write to this object store).
            .with_mount(Mount::Bind {
                source: git_dir.clone(),
                target: git_dir,
                read_only: false,
            })
            // The writable worktree checkout itself.
            .with_mount(Mount::Bind {
                source: wt.worktree_dir.to_string_lossy().into_owned(),
                target: CONTAINER_WORKSPACE_DIR.to_string(),
                read_only: false,
            })
            .with_env("COPPERCLAW_WORKSPACE", CONTAINER_WORKSPACE_DIR)
            .with_env("COPPERCLAW_WORKSPACE_BRANCH", &wt.branch);
    } else {
        // Read-only `/parent` fallback. Still validate the source: a
        // read-only bind of a symlink-swapped path can still leak the
        // contents of an escaped target into the container.
        if let Err(err) = super::mount_guard::validate_source(&parent_root, sessions_root) {
            warn!(%err, path = %parent_root.display(), "parent workspace source failed mount validation; skipping /parent mount");
            return spec;
        }
        spec = spec.with_mount(Mount::Bind {
            source: parent_root.to_string_lossy().into_owned(),
            target: CONTAINER_PARENT_DIR.to_string(),
            read_only: true,
        });
    }
    spec
}

/// Resolve which git repo a `create_agent` sibling's worktree should be cut
/// from, given the parent's on-disk session root (`/data` on the host side).
///
/// A single session is reused for many projects over time (the operator
/// `/clear`s context between them, but `/data` persists), so "the repo" is
/// whichever project the parent is *currently working in*: we read the
/// parent's persisted shell cwd from `.shell_state`, map the container path
/// (`/data/<proj>`) back to the host, and walk UP to the nearest enclosing
/// `.git` — bounded at `parent_root` so we never escape the session dir.
/// Falls back to a repo at the workspace root, then `None` (no repo →
/// read-only `/parent`).
fn resolve_parent_repo_root(parent_root: &std::path::Path) -> Option<std::path::PathBuf> {
    if let Some(rel) = parent_shell_cwd_relative(parent_root) {
        let start = if rel.as_os_str().is_empty() {
            parent_root.to_path_buf()
        } else {
            parent_root.join(rel)
        };
        let mut dir: &std::path::Path = &start;
        loop {
            if dir.join(".git").is_dir() {
                return Some(dir.to_path_buf());
            }
            if dir == parent_root {
                break;
            }
            match dir.parent() {
                // Stay within the session dir; never walk above `/data`.
                Some(p) if p.starts_with(parent_root) => dir = p,
                _ => break,
            }
        }
    }
    // No cwd signal (or its repo wasn't found): a repo at the workspace root.
    if parent_root.join(".git").is_dir() {
        return Some(parent_root.to_path_buf());
    }
    None
}

/// The parent's current shell working directory, expressed RELATIVE to the
/// session root (so `/data/fairway-focus` → `fairway-focus`, and `/data`
/// itself → the empty path). Returns `None` when there's no `.shell_state`,
/// no parseable cwd, or the cwd isn't under the container session dir (e.g.
/// the agent `cd`'d to `/tmp`). The `shell` tool persists this after each
/// command.
fn parent_shell_cwd_relative(parent_root: &std::path::Path) -> Option<std::path::PathBuf> {
    let state = std::fs::read_to_string(parent_root.join(".shell_state")).ok()?;
    let cwd = parse_shell_pwd(&state)?;
    let rel = cwd
        .strip_prefix(CONTAINER_SESSION_DIR)?
        .trim_start_matches('/');
    Some(std::path::PathBuf::from(rel))
}

/// Extract `$PWD` from a dumped bash environment (`.shell_state`). Matches
/// the `... PWD="<value>"` declaration while skipping `OLDPWD` (the `PWD`
/// substring there is preceded by an alphanumeric, so it's rejected).
fn parse_shell_pwd(state: &str) -> Option<String> {
    for line in state.lines() {
        let Some(idx) = line.find("PWD=") else {
            continue;
        };
        // Reject `OLDPWD=` (and any other `*PWD=`): the real var name is
        // bounded by a non-alphanumeric (the `declare -x ` space) on its left.
        if idx > 0 && line.as_bytes()[idx - 1].is_ascii_alphanumeric() {
            continue;
        }
        let val = line[idx + 4..].trim().trim_matches('"');
        if !val.is_empty() {
            return Some(val.to_string());
        }
    }
    None
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

pub(super) fn container_name(_agent: AgentGroupId, session: copperclaw_types::SessionId) -> String {
    format!("copperclaw-{}", session.as_uuid())
}

/// Locate a session's on-disk root by scanning `<sessions_dir>/*/<session_id>`.
/// Session ids are unique UUIDs, so the first agent-group subdir containing
/// one is the right session. Returns `None` if not found (e.g. the parent
/// was deleted). Used to mount a `create_agent` sibling's parent workspace.
fn find_session_root(
    sessions_dir: &std::path::Path,
    session_id: &str,
) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(sessions_dir).ok()?.flatten() {
        let candidate = entry.path().join(session_id);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// The host paths a `create_agent` sibling needs mounted to operate a
/// writable git worktree of its parent's repo.
struct ParentWorktree {
    /// The parent repo's `.git` directory (shared object store + refs).
    /// Mounted into the sibling at this same host-absolute path.
    git_dir: std::path::PathBuf,
    /// The worktree checkout dir (the sibling's writable `/workspace`).
    worktree_dir: std::path::PathBuf,
    /// The branch the worktree is checked out on (`sib/<session-id>`).
    branch: String,
}

/// Run `git -C <repo> <args...>`, returning `true` on a clean exit.
/// Best-effort: any spawn failure (git absent, etc.) reports `false`.
fn git_ok(repo: &std::path::Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Provision (or reuse) a git worktree of `repo_root` for a sibling.
///
/// `repo_root` is the project repo resolved by [`resolve_parent_repo_root`]
/// (the workspace root, or a project subdir the parent is working in).
/// Returns `None` when it isn't a usable git repo (no `.git` dir, no commit
/// yet, or git is unavailable), in which case the caller falls back to the
/// read-only `/parent` mount.
///
/// Idempotent: a container can re-spawn within a session (crash/restart),
/// so an existing worktree dir is reused rather than re-created.
fn provision_parent_worktree(
    repo_root: &std::path::Path,
    sibling_sid: &str,
) -> Option<ParentWorktree> {
    let git_dir = repo_root.join(".git");
    // Needs a real repo with at least one commit to seed a worktree from.
    if !git_dir.is_dir() || !git_ok(repo_root, &["rev-parse", "--verify", "HEAD"]) {
        return None;
    }

    // Keep the worktree scratch dir out of the repo's `git status` — it
    // lives inside the repo's working tree, so without this every parent
    // status call would show `.copperclaw/` as untracked. Best-effort;
    // worktrees share `info/exclude` via the common dir.
    exclude_copperclaw_dir(&git_dir);

    let worktree_dir = repo_root.join(".copperclaw").join("wt").join(sibling_sid);
    let branch = format!("sib/{sibling_sid}");

    if !worktree_dir.exists() {
        // `git worktree add` creates the leaf, but make sure the parents
        // exist so it can't fail on a missing `.copperclaw/wt`.
        let _ = std::fs::create_dir_all(worktree_dir.parent()?);
        let wt = worktree_dir.to_str()?;
        // Fresh branch off HEAD; if the branch already exists (dir was
        // removed but the branch lingered) attach to it instead.
        let added = git_ok(repo_root, &["worktree", "add", "-b", &branch, wt, "HEAD"])
            || git_ok(repo_root, &["worktree", "add", wt, &branch]);
        if !added {
            return None;
        }
    }

    Some(ParentWorktree {
        git_dir,
        worktree_dir,
        branch,
    })
}

/// Append `.copperclaw/` to the repo's `.git/info/exclude` if it's not
/// already listed, so sibling worktree scratch never shows up as untracked
/// in the parent's `git status`. Best-effort and silent on any IO error.
fn exclude_copperclaw_dir(git_dir: &std::path::Path) {
    let info = git_dir.join("info");
    if std::fs::create_dir_all(&info).is_err() {
        return;
    }
    let exclude = info.join("exclude");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|l| l.trim() == ".copperclaw/") {
        return;
    }
    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(".copperclaw/\n");
    let _ = std::fs::write(&exclude, next);
}

/// Choose the base image for a per-group rebuild. Returns the install's
/// `default_image_tag` (which has `/usr/local/bin/copperclaw-runner`
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
const REBUILD_BACKOFF_INITIAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Ceiling for the rebuild backoff per group. 30 min mirrors the
/// delivery loop's `ABSOLUTE_CEILING_MS`: after this much time has
/// passed without operator action, retrying the build won't have
/// gotten cheaper, but unblocking the group does have value.
const REBUILD_BACKOFF_CEILING: std::time::Duration = std::time::Duration::from_secs(1_800);

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
    use copperclaw_db::central::CentralDb;
    use copperclaw_db::session::open_inbound;
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::messages_in;
    use copperclaw_db::tables::sessions::{CreateSession, create as create_session};
    use copperclaw_types::{ContainerStatus, SessionId};
    use std::path::PathBuf;

    /// `harden_dir_0700` sets a session directory to owner-only `0700`
    /// regardless of the mode it was created with — proving the perms are
    /// enforced explicitly, not left to the host umask.
    #[cfg(unix)]
    #[test]
    fn harden_dir_sets_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sess");
        std::fs::create_dir(&dir).unwrap();
        // Start deliberately loose.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        harden_dir_0700(&dir);
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "session dir must be tightened to 0700, got {mode:o}"
        );
    }

    /// `harden_file_0600` locks a secret-bearing session file to owner
    /// read/write `0600`, again independent of the creating umask.
    #[cfg(unix)]
    #[test]
    fn harden_file_sets_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runner.json");
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        harden_file_0600(&path);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "session config file must be tightened to 0600, got {mode:o}"
        );
    }

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
            assert!(
                next >= prev,
                "backoff should not regress: prev={prev:?} next={next:?}"
            );
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
        assert!(
            bo.in_cooldown(b).is_none(),
            "other group must be unaffected"
        );
    }

    /// Regression: image rebuilds must base off the install's default
    /// image (which has the runner binary), not bare `debian:trixie-slim`.
    /// Caught live: agent emitted `install_packages` → host rebuilt
    /// against debian-slim → resulting image had apt packages but no
    /// `/usr/local/bin/copperclaw-runner` → every `runc create` failed.
    #[test]
    fn rebuild_base_prefers_default_image_tag() {
        assert_eq!(
            resolve_rebuild_base("copperclaw/session:sha256-abc123"),
            "copperclaw/session:sha256-abc123"
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
            default_image_tag: "copperclaw/session:test".into(),
            default_provider: "anthropic".into(),
            default_model: "claude-sonnet-4-6".into(),
            default_effort: None,
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
            egress_mode: copperclaw_container_rt::EgressMode::AllowAll,
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
                source_session_id: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn container_name_is_deterministic_and_uuid_shaped() {
        let s = SessionId(uuid::Uuid::nil());
        let ag = AgentGroupId::new();
        let name = container_name(ag, s);
        assert_eq!(name, "copperclaw-00000000-0000-0000-0000-000000000000");
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
        let spec = mgr
            .build_spec(&session, &paths, "copperclaw/session:abc", None)
            .unwrap();
        // Image
        assert_eq!(spec.image, "copperclaw/session:abc");
        // Entrypoint includes the runner path + --config arg
        assert_eq!(spec.entrypoint[0], CONTAINER_RUNNER_PATH);
        assert_eq!(spec.entrypoint[1], "--config");
        assert!(spec.entrypoint[2].ends_with(RUNNER_CONFIG_FILENAME));
        // Mount the session root at /data
        let bind = spec
            .mounts
            .iter()
            .find_map(|m| match m {
                Mount::Bind {
                    source,
                    target,
                    read_only,
                } => Some((source, target, read_only)),
                _ => None,
            })
            .unwrap();
        assert_eq!(bind.1, CONTAINER_SESSION_DIR);
        assert!(!*bind.2);
        // Env carries both API key and base URL.
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "sk-test")
        );
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v.contains("openrouter"))
        );
        // Labels for orphan cleanup.
        assert_eq!(
            spec.labels.get("copperclaw.install").map(String::as_str),
            Some("test")
        );
        assert!(spec.labels.contains_key("copperclaw.session"));
        assert!(spec.labels.contains_key("copperclaw.agent_group"));
    }

    #[test]
    fn build_spec_with_broker_disabled_still_forwards_real_key() {
        // Default path (no broker): behaviour is unchanged — the real key is
        // forwarded. This is the regression guard for "default still works".
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "img", None).unwrap();
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "sk-test"),
            "broker disabled must forward the real key"
        );
    }

    #[test]
    fn build_spec_with_broker_enabled_emits_token_not_master_key() {
        // Phase 0b core security assertion: when the broker is enabled,
        // build_spec must NOT put the master key in the container env. The
        // ANTHROPIC_API_KEY slot must hold a per-session capability TOKEN, and
        // ANTHROPIC_BASE_URL must point at the broker loopback.
        use super::super::broker::{BrokerConfig, BrokerState, Revocations};

        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let cfg = BrokerConfig::resolve(true, Some("sk-test"), None, Some(3600)).unwrap();
        let broker = std::sync::Arc::new(BrokerState::new(cfg));
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        )
        .with_broker(
            std::sync::Arc::clone(&broker),
            "http://127.0.0.1:48080".into(),
        );

        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr
            .build_spec(&session, &paths, "copperclaw/session:abc", None)
            .unwrap();

        let api_key = spec
            .env
            .iter()
            .find(|(k, _)| k == "ANTHROPIC_API_KEY")
            .map(|(_, v)| v.clone())
            .expect("ANTHROPIC_API_KEY must be set");
        // The master key must NEVER appear in the container env.
        assert_ne!(
            api_key, "sk-test",
            "broker enabled: master key must not be forwarded into the container"
        );
        assert!(
            !spec.env.iter().any(|(_, v)| v == "sk-test"),
            "master key must not appear in ANY env var"
        );
        // The slot holds a real capability token shaped `cct1.payload.sig`.
        assert!(
            api_key.starts_with("cct1."),
            "expected a capability token, got {api_key}"
        );
        // And the token validates against the broker's keyring with the right
        // session + group claims.
        let revs = Revocations::new();
        let claims = broker
            .keyring
            .validate(&api_key, super::super::broker::now_epoch_secs(), &revs)
            .expect("minted token must validate");
        assert_eq!(claims.session_id, session.id);
        assert_eq!(claims.agent_group_id, session.agent_group_id);

        // Base URL points at the broker loopback, not the operator upstream.
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == "http://127.0.0.1:48080"),
            "ANTHROPIC_BASE_URL must point at the broker loopback"
        );
    }

    #[test]
    // Linux-only: asserts the `/proc/self`-based uid detection in
    // `host_uid_gid`. On macOS (also `unix`) there's no `/proc`, so the
    // function returns `None` by design and `spec.user` is unset — nothing
    // to assert. Gating to `linux` keeps the macOS CI runner green.
    #[cfg(target_os = "linux")]
    fn build_spec_runs_container_as_host_user() {
        use std::os::unix::fs::MetadataExt;
        // Regression: containers used to inherit the image's default
        // USER (root), so every file the agent wrote via the bind mount
        // landed on the host as root-owned. cclaw could not modify
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
        let spec = mgr.build_spec(&session, &paths, "img", None).unwrap();
        let user = spec.user.as_deref().expect("spec.user should be set");
        let (uid_str, gid_str) = user.split_once(':').expect("user is uid:gid");
        let uid: u32 = uid_str.parse().expect("uid is numeric");
        let gid: u32 = gid_str.parse().expect("gid is numeric");
        let meta = std::fs::metadata("/proc/self").unwrap();
        assert_eq!(uid, meta.uid());
        assert_eq!(gid, meta.gid());
    }

    #[test]
    fn build_spec_sets_writable_home_and_workdir() {
        // Regression: the container runs as a non-root uid with no
        // passwd entry, so cwd and $HOME both defaulted to `/`
        // (unwritable). `go build` died on "mkdir /.cache: permission
        // denied" — the agent never saw its compile errors and claimed
        // a non-compiling program was "built". Both must point at the
        // writable session bind mount.
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "img", None).unwrap();
        assert_eq!(spec.working_dir.as_deref(), Some(CONTAINER_SESSION_DIR));
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "HOME" && v == CONTAINER_SESSION_DIR),
            "HOME must be the writable session dir, got env {:?}",
            spec.env
        );
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
        let spec = mgr.build_spec(&session, &paths, "img", None).unwrap();
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "TAVILY_API_KEY" && v == "tav-secret")
        );
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "EXA_API_KEY" && v == "exa-secret")
        );
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
        let spec = mgr.build_spec(&session, &paths, "img", None).unwrap();
        assert!(spec.env.iter().all(|(k, _)| k != "ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn build_spec_mounts_parent_workspace_read_only_for_siblings() {
        // Non-git parent workspace -> legacy read-only /parent fallback.
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        // Parent session dir on disk, under a DIFFERENT agent group.
        let parent_group = AgentGroupId::new();
        let parent_sid = copperclaw_types::SessionId::new();
        let parent_root = SessionPaths::new(tmp.path(), parent_group, parent_sid).root;
        std::fs::create_dir_all(&parent_root).unwrap();
        // A sibling session records the parent via source_session_id.
        let mut sibling = fixture_session(&db);
        sibling.source_session_id = Some(parent_sid);
        let paths = SessionPaths::new(tmp.path(), sibling.agent_group_id, sibling.id);
        let spec = mgr.build_spec(&sibling, &paths, "img", None).unwrap();
        let parent_mount = spec.mounts.iter().find_map(|m| match m {
            Mount::Bind {
                source,
                target,
                read_only,
            } if target == CONTAINER_PARENT_DIR => Some((source.clone(), *read_only)),
            _ => None,
        });
        let (src, ro) = parent_mount.expect("/parent mount present for sibling");
        assert_eq!(src, parent_root.to_string_lossy().to_string());
        assert!(ro, "parent workspace must be mounted read-only");
    }

    #[test]
    fn build_spec_no_parent_mount_for_root_session() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        // fixture_session has source_session_id = None (a root session).
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "img", None).unwrap();
        assert!(
            !spec
                .mounts
                .iter()
                .any(|m| matches!(m, Mount::Bind { target, .. } if target == CONTAINER_PARENT_DIR)),
            "root session must not get a /parent mount"
        );
    }

    /// `true` if a usable `git` is on PATH. Worktree tests are skipped
    /// (pass vacuously) on the rare box without git so CI never flakes.
    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Initialise a git repo with one commit at `root`.
    fn init_repo_with_commit(root: &std::path::Path) {
        let run = |args: &[&str]| {
            assert!(
                super::git_ok(root, args),
                "git {args:?} failed in test repo"
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(root.join("README.md"), "hi\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-q", "-m", "init"]);
    }

    #[test]
    fn build_spec_mounts_writable_worktree_for_git_parent() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        // A git-repo parent workspace under a different agent group.
        let parent_group = AgentGroupId::new();
        let parent_sid = copperclaw_types::SessionId::new();
        let parent_root = SessionPaths::new(tmp.path(), parent_group, parent_sid).root;
        std::fs::create_dir_all(&parent_root).unwrap();
        init_repo_with_commit(&parent_root);

        let mut sibling = fixture_session(&db);
        sibling.source_session_id = Some(parent_sid);
        let paths = SessionPaths::new(tmp.path(), sibling.agent_group_id, sibling.id);
        let spec = mgr.build_spec(&sibling, &paths, "img", None).unwrap();

        // No read-only /parent — the writable worktree replaces it.
        assert!(
            !spec
                .mounts
                .iter()
                .any(|m| matches!(m, Mount::Bind { target, .. } if target == CONTAINER_PARENT_DIR)),
            "git parent must not get the read-only /parent fallback"
        );

        // /workspace is the worktree checkout, read-write.
        let ws = spec
            .mounts
            .iter()
            .find_map(|m| match m {
                Mount::Bind {
                    source,
                    target,
                    read_only,
                } if target == CONTAINER_WORKSPACE_DIR => Some((source.clone(), *read_only)),
                _ => None,
            })
            .expect("/workspace mount present");
        assert!(!ws.1, "/workspace must be read-write");
        let expected_wt = parent_root
            .join(".copperclaw")
            .join("wt")
            .join(sibling.id.as_uuid().to_string());
        assert_eq!(ws.0, expected_wt.to_string_lossy());
        assert!(
            expected_wt.join("README.md").is_file(),
            "worktree checked out"
        );

        // The shared .git is mounted RW at its identical host-absolute path
        // so the worktree's `gitdir:` pointer resolves in-container.
        let git_src = parent_root.join(".git").to_string_lossy().into_owned();
        let git_mount = spec.mounts.iter().find_map(|m| match m {
            Mount::Bind {
                source,
                target,
                read_only,
            } if *source == git_src => Some((target.clone(), *read_only)),
            _ => None,
        });
        let (git_target, git_ro) = git_mount.expect(".git mount present");
        assert_eq!(git_target, git_src, ".git target must equal its host path");
        assert!(
            !git_ro,
            ".git must be read-write (commits write objects here)"
        );

        // The branch was created off the parent's HEAD.
        let branch = format!("sib/{}", sibling.id.as_uuid());
        assert!(
            super::git_ok(&parent_root, &["rev-parse", "--verify", &branch]),
            "branch {branch} must exist in the parent repo"
        );
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "COPPERCLAW_WORKSPACE_BRANCH" && *v == branch)
        );
    }

    #[test]
    fn build_spec_worktree_provisioning_is_idempotent() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let parent_group = AgentGroupId::new();
        let parent_sid = copperclaw_types::SessionId::new();
        let parent_root = SessionPaths::new(tmp.path(), parent_group, parent_sid).root;
        std::fs::create_dir_all(&parent_root).unwrap();
        init_repo_with_commit(&parent_root);

        let mut sibling = fixture_session(&db);
        sibling.source_session_id = Some(parent_sid);
        let paths = SessionPaths::new(tmp.path(), sibling.agent_group_id, sibling.id);

        // A second spawn (container restart within a session) must reuse the
        // existing worktree, not error out.
        let spec1 = mgr.build_spec(&sibling, &paths, "img", None).unwrap();
        let spec2 = mgr.build_spec(&sibling, &paths, "img", None).unwrap();
        let has_workspace = |s: &ContainerSpec| {
            s.mounts.iter().any(
                |m| matches!(m, Mount::Bind { target, .. } if target == CONTAINER_WORKSPACE_DIR),
            )
        };
        assert!(has_workspace(&spec1) && has_workspace(&spec2));

        // `.copperclaw/` is excluded so it doesn't pollute parent git status.
        let exclude =
            std::fs::read_to_string(parent_root.join(".git/info/exclude")).unwrap_or_default();
        assert!(exclude.lines().any(|l| l.trim() == ".copperclaw/"));
    }

    #[test]
    fn build_spec_worktree_sourced_from_parent_current_project_subdir() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let parent_group = AgentGroupId::new();
        let parent_sid = copperclaw_types::SessionId::new();
        let parent_root = SessionPaths::new(tmp.path(), parent_group, parent_sid).root;
        std::fs::create_dir_all(&parent_root).unwrap();
        // /data is NOT a repo; the project lives in a subdir that IS — the
        // multi-project-per-session shape (several projects under /data).
        let project = parent_root.join("fairway-focus");
        std::fs::create_dir_all(&project).unwrap();
        init_repo_with_commit(&project);
        // The parent's shell is cd'd into the project (container path).
        std::fs::write(
            parent_root.join(".shell_state"),
            "declare -x OLDPWD=\"/data\"\ndeclare -x PWD=\"/data/fairway-focus\"\n",
        )
        .unwrap();

        let mut sibling = fixture_session(&db);
        sibling.source_session_id = Some(parent_sid);
        let paths = SessionPaths::new(tmp.path(), sibling.agent_group_id, sibling.id);
        let spec = mgr.build_spec(&sibling, &paths, "img", None).unwrap();

        // Shared .git + worktree come from the SUBDIR repo, not /data root.
        let git_src = project.join(".git").to_string_lossy().into_owned();
        assert!(
            spec.mounts.iter().any(|m| matches!(m,
                Mount::Bind { source, target, .. } if *source == git_src && *target == git_src)),
            "shared .git must be the current project's repo"
        );
        let ws = spec
            .mounts
            .iter()
            .find_map(|m| match m {
                Mount::Bind { source, target, .. } if target == CONTAINER_WORKSPACE_DIR => {
                    Some(source.clone())
                }
                _ => None,
            })
            .expect("/workspace mount present");
        assert!(
            ws.starts_with(&*project.to_string_lossy()),
            "worktree must live under the project repo: {ws}"
        );
        // A writable repo was found, so no read-only /parent fallback.
        assert!(
            !spec
                .mounts
                .iter()
                .any(|m| matches!(m, Mount::Bind { target, .. } if target == CONTAINER_PARENT_DIR))
        );
        // The branch lives in the SUBDIR repo.
        let branch = format!("sib/{}", sibling.id.as_uuid());
        assert!(super::git_ok(&project, &["rev-parse", "--verify", &branch]));
    }

    #[test]
    fn parse_shell_pwd_picks_pwd_not_oldpwd() {
        let state = "declare -x OLDPWD=\"/data\"\n\
                     declare -x PWD=\"/data/proj\"\n\
                     declare -x HOME=\"/data\"\n";
        assert_eq!(super::parse_shell_pwd(state).as_deref(), Some("/data/proj"));
        assert_eq!(super::parse_shell_pwd("declare -x HOME=\"/data\"\n"), None);
    }

    #[test]
    fn resolve_parent_repo_root_walks_up_from_cwd() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        let proj = root.join("proj");
        std::fs::create_dir_all(proj.join("src/deep")).unwrap();
        init_repo_with_commit(&proj);
        // cwd deep inside the repo resolves to the repo root.
        std::fs::write(
            root.join(".shell_state"),
            "declare -x PWD=\"/data/proj/src/deep\"\n",
        )
        .unwrap();
        assert_eq!(super::resolve_parent_repo_root(&root), Some(proj));
        // cwd in a non-repo subdir, and /data isn't a repo -> None (read-only).
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::write(
            root.join(".shell_state"),
            "declare -x PWD=\"/data/other\"\n",
        )
        .unwrap();
        assert_eq!(super::resolve_parent_repo_root(&root), None);
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
        let spec = mgr.build_spec(&session, &paths, "img", None).unwrap();
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
        let spec = mgr.build_spec(&session, &paths, "img", None).unwrap();
        let has_memory = spec.mounts.iter().any(|m| match m {
            Mount::Bind { target, .. } => target == &format!("{CONTAINER_SESSION_DIR}/memory"),
            _ => false,
        });
        assert!(
            !has_memory,
            "memory mount must not appear without groups_dir"
        );
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
            surface_thinking: false,
            tool_profile: None,
            updated_at: chrono::Utc::now(),
        };
        let spec = mgr.build_spec(&session, &paths, "img", Some(&cfg)).unwrap();
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
            surface_thinking: false,
            tool_profile: None,
            updated_at: chrono::Utc::now(),
        };
        let spec = mgr.build_spec(&session, &paths, "img", Some(&cfg)).unwrap();
        // The model endpoint (derived from the cfg's openrouter base URL) is
        // auto-injected FIRST so deny-default can't blackhole model traffic,
        // then the operator's per-group entries are unioned on top.
        assert_eq!(
            spec.egress_allow,
            vec!["openrouter.ai:443", "api.example.com:443", "db.local:5432"]
        );
    }

    /// Auto-injection (migration safety): even with an empty per-group
    /// allow-list, the resolved list carries the model endpoint derived from
    /// the base URL so a deny-default spawn can never blackhole the agent's
    /// own provider traffic.
    #[test]
    fn build_spec_auto_injects_model_endpoint_when_group_allow_empty() {
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
            surface_thinking: false,
            tool_profile: None,
            updated_at: chrono::Utc::now(),
        };
        let spec = mgr.build_spec(&session, &paths, "img", Some(&cfg)).unwrap();
        // manager_cfg's base URL is https://openrouter.ai/api/v1.
        assert_eq!(spec.egress_allow, vec!["openrouter.ai:443"]);
    }

    /// Migration-safety: a deny-default group with the DEFAULT (unset)
    /// Anthropic base URL and an empty per-group allow-list must STILL reach
    /// `api.anthropic.com` — the common single-provider deployment was the one
    /// the old "only inject when `ANTHROPIC_BASE_URL` is set" code black-holed.
    #[test]
    fn build_spec_deny_default_with_unset_anthropic_base_injects_api_anthropic() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.egress_mode = copperclaw_container_rt::EgressMode::DenyDefault;
        // No ANTHROPIC_BASE_URL override; default provider stays "anthropic".
        cfg.anthropic_base_url = None;
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "img", None).unwrap();
        assert_eq!(
            spec.egress_mode,
            copperclaw_container_rt::EgressMode::DenyDefault
        );
        // The empty group allow-list does NOT black-hole the model: the
        // real default Anthropic endpoint is auto-injected.
        assert_eq!(spec.egress_allow, vec!["api.anthropic.com:443"]);
    }

    /// Migration-safety (ollama path): a group pinned to `provider=ollama`
    /// gets the `OLLAMA_BASE_URL` host injected — NOT `api.anthropic.com` — so
    /// deny-default reaches the local model endpoint. Exercises the real
    /// spawn path reading `OLLAMA_BASE_URL` from the rotatable `forward_env`.
    #[test]
    fn build_spec_deny_default_ollama_group_injects_ollama_host() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.egress_mode = copperclaw_container_rt::EgressMode::DenyDefault;
        // ANTHROPIC_BASE_URL is set but must be IGNORED for an ollama group.
        cfg.anthropic_base_url = Some("https://api.anthropic.com".into());
        // OLLAMA_BASE_URL is forwarded the same way boot.rs wires it.
        cfg.forward_env = vec![(
            "OLLAMA_BASE_URL".to_string(),
            "http://172.17.0.1:11434".to_string(),
        )];
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let group_cfg = container_configs::ContainerConfig {
            agent_group_id: session.agent_group_id,
            provider: Some("ollama".into()),
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
            surface_thinking: false,
            tool_profile: None,
            updated_at: chrono::Utc::now(),
        };
        let spec = mgr
            .build_spec(&session, &paths, "img", Some(&group_cfg))
            .unwrap();
        assert_eq!(spec.egress_allow, vec!["172.17.0.1:11434"]);
    }

    /// Opt-in default: the host ships allow-all, so an unconfigured manager
    /// stamps `EgressMode::AllowAll` on the spec and the legacy spawn path
    /// (default bridge, advisory allow-list) is unchanged.
    #[test]
    fn build_spec_egress_mode_defaults_to_allow_all() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "img", None).unwrap();
        assert_eq!(
            spec.egress_mode,
            copperclaw_container_rt::EgressMode::AllowAll
        );
    }

    /// When the operator opts in to deny-default, the spec carries the
    /// posture AND the auto-injected model endpoint — together these are
    /// what the runtime + the later nftables pass enforce against.
    #[test]
    fn build_spec_deny_default_mode_carries_posture_and_model_endpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.egress_mode = copperclaw_container_rt::EgressMode::DenyDefault;
        cfg.anthropic_base_url = Some("https://api.anthropic.com".into());
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "img", None).unwrap();
        assert_eq!(
            spec.egress_mode,
            copperclaw_container_rt::EgressMode::DenyDefault
        );
        assert_eq!(spec.egress_allow, vec!["api.anthropic.com:443"]);
    }

    /// toctou redux: if a component of the session-root mount source is
    /// swapped for a symlink between the host computing the path and the
    /// mount, `build_spec` must REFUSE (return `ManagerError::UnsafeMount`)
    /// rather than hand dockerd a source that resolves outside the data dir.
    ///
    /// Residual (documented, not testable here): dockerd re-resolves the
    /// source path in its own process when it performs the bind, so this
    /// closes the host-side TOCTOU window but cannot eliminate a swap that
    /// races dockerd's own resolution.
    #[cfg(unix)]
    #[test]
    fn build_spec_refuses_session_mount_with_swapped_symlink_component() {
        let tmp = tempfile::tempdir().unwrap();
        // Canonicalize so the tmp prefix carries no incidental symlink that
        // would trip the scan before we plant ours (macOS /tmp etc.).
        let data_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(data_dir.clone()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(&data_dir, session.agent_group_id, session.id);

        // Plant the attack: replace the agent-group dir component of the
        // session-root path with a symlink pointing OUTSIDE the data dir.
        // The real session dir would be data_dir/sessions/<ag>/<sess>; we
        // make data_dir/sessions/<ag> a symlink to an unrelated dir.
        let sessions = data_dir.join("sessions");
        let ag_component = sessions.join(session.agent_group_id.as_uuid().to_string());
        std::fs::create_dir_all(&sessions).unwrap();
        let escape_target = data_dir.join("escape-target");
        std::fs::create_dir_all(&escape_target).unwrap();
        std::os::unix::fs::symlink(&escape_target, &ag_component).unwrap();

        let err = mgr
            .build_spec(&session, &paths, "img", None)
            .expect_err("a symlink-swapped mount source component must be refused");
        assert!(
            matches!(err, ManagerError::UnsafeMount(_)),
            "expected UnsafeMount, got {err:?}"
        );
    }

    /// Sanity counterpart: a clean session-root source (no swapped symlink)
    /// validates and `build_spec` succeeds — the new gate doesn't reject
    /// the normal spawn path.
    #[test]
    fn build_spec_accepts_clean_session_mount_source() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(data_dir.clone()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(&data_dir, session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        assert!(mgr.build_spec(&session, &paths, "img", None).is_ok());
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
                id: copperclaw_types::MessageId::new(),
                kind: copperclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "hi"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("stdin".into()),
                channel_type: Some(copperclaw_types::ChannelType::new("cli")),
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();

        mgr.tick().await.unwrap();

        // Session should now be marked running.
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Running));
        // The noop runtime records the spawn call.
        assert!(
            runtime
                .spawn_calls()
                .iter()
                .any(|name| { name.contains(&session.id.as_uuid().to_string()) })
        );
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
        let spec = mgr.build_spec(&session, &paths, "img", None).unwrap();
        // The bind mount is skipped (no source could be prepared).
        let has_memory_mount = spec.mounts.iter().any(|m| match m {
            Mount::Bind { target, .. } => target == &format!("{CONTAINER_SESSION_DIR}/memory"),
            _ => false,
        });
        assert!(
            !has_memory_mount,
            "memory mount must not appear when source prep failed"
        );
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
        let _spec = mgr.build_spec(&session, &paths, "img", None).unwrap();
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
                id: copperclaw_types::MessageId::new(),
                kind: copperclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "hi"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("stdin".into()),
                channel_type: Some(copperclaw_types::ChannelType::new("cli")),
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
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
        std::fs::write(
            &env_path,
            "ANTHROPIC_API_KEY=rotated-key\nEXA_API_KEY=exa-1\n",
        )
        .unwrap();
        let changed = mgr.reload_env(Some(&env_path));
        assert!(
            changed.contains(&"ANTHROPIC_API_KEY".to_string()),
            "{changed:?}"
        );
        assert!(changed.contains(&"EXA_API_KEY".to_string()), "{changed:?}");
        // RwLock now reflects the rotation.
        let r = mgr.rotatable.read().unwrap();
        assert_eq!(r.anthropic_api_key.as_deref(), Some("rotated-key"));
        assert!(
            r.forward_env
                .iter()
                .any(|(k, v)| k == "EXA_API_KEY" && v == "exa-1")
        );
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

    /// The defaults shipped in this module must already satisfy the
    /// startup alignment check — otherwise every host boot would emit
    /// the warn line. If a future contributor lowers the heartbeat
    /// threshold or raises the provider deadline default without
    /// updating the other, this test catches it before the operator
    /// does.
    #[test]
    fn defaults_satisfy_heartbeat_deadline_alignment() {
        // Reach across crates here intentionally: the runner crate
        // owns the provider-deadline default, the host crate owns the
        // heartbeat threshold, and the safety check is the load-
        // bearing contract between them.
        let runner_default = copperclaw_runner::DEFAULT_PROVIDER_DEADLINE_MS;
        check_heartbeat_deadline_alignment(DEFAULT_HEARTBEAT_STALE_SECS, runner_default)
            .expect("shipped defaults must already align");
    }

    /// The boundary case — heartbeat exactly equal to `2 * deadline` —
    /// is the minimum acceptable configuration and must pass without
    /// a warn.
    #[test]
    fn alignment_check_passes_at_exact_2x_boundary() {
        // 30s deadline → require 60s heartbeat.
        assert!(check_heartbeat_deadline_alignment(60, 30_000).is_ok());
        // 60s deadline → require 120s heartbeat (matches the new
        // shipped defaults).
        assert!(check_heartbeat_deadline_alignment(120, 60_000).is_ok());
    }

    /// The misconfigured case fires an Err containing both values so
    /// the operator can act on the warn line without reading source.
    /// This is the regression guard for the original race: a 60s
    /// heartbeat against a 60s provider deadline.
    #[test]
    fn alignment_check_warns_when_heartbeat_lt_2x_deadline() {
        // Original (pre-fix) configuration: same value on both sides.
        let err = check_heartbeat_deadline_alignment(60, 60_000)
            .expect_err("60s heartbeat vs 60s deadline must trip the check");
        assert!(
            err.contains("60s"),
            "warning must show heartbeat value: {err}"
        );
        assert!(
            err.contains("60000ms") || err.contains("60_000"),
            "warning must show deadline ms value: {err}"
        );
        assert!(
            err.contains("120"),
            "warning must name the required minimum (2x = 120s): {err}"
        );
    }

    /// A sub-second deadline (only reachable in tests today, but the
    /// check should still be sensible) ceils to 1s so the required
    /// minimum is 2s, not 0.
    #[test]
    fn alignment_check_ceils_sub_second_deadlines() {
        // 500ms deadline → div_ceil to 1s → require 2s heartbeat.
        assert!(check_heartbeat_deadline_alignment(2, 500).is_ok());
        assert!(check_heartbeat_deadline_alignment(1, 500).is_err());
    }

    /// Operator-supplied extreme values must not panic the check.
    /// The `saturating_mul` guards against u64 overflow when the
    /// operator pins the deadline at the upper validator bound and
    /// the heartbeat at some other large number.
    #[test]
    fn alignment_check_does_not_overflow_on_large_values() {
        // u64::MAX deadline would overflow without saturating_mul.
        // Heartbeat is also u64::MAX so the relationship holds at
        // saturation.
        assert!(check_heartbeat_deadline_alignment(u64::MAX, u64::MAX).is_ok());
        // A more realistic extreme: the validator-capped 300s
        // deadline against a tiny heartbeat — must Err, not panic.
        assert!(check_heartbeat_deadline_alignment(10, 300_000).is_err());
    }
}
