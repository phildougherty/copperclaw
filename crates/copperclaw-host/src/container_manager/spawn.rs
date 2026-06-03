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

        // Phase 0a v2 (Part B): the privileged nftables apply for the new
        // session's network namespace. This is the deferred-but-implemented
        // runtime path: the ruleset is constructed + persisted in
        // `build_spec`/`apply_dns_filter` (pure + tested), and applied here
        // only under deny-default. The apply enters the session's OWN netns via
        // the container's host-visible PID (`nsenter -t <pid> -n nft -f -`),
        // which the runtime now surfaces on the spawn handle (`handle.host_pid`).
        // It needs `CAP_NET_ADMIN` and a resolvable PID; if the PID is absent
        // (runtime couldn't report it) or the privileged apply fails (no
        // CAP_NET_ADMIN, `nft`/`nsenter` missing) it logs the honest deferred
        // status and degrades to the carried policy + DNS-filter confinement
        // rather than failing the spawn — never a faked "applied". Default
        // (allow-all) spawns never reach this.
        if self.cfg.egress_mode == copperclaw_container_rt::EgressMode::DenyDefault {
            apply_session_nftables(session, &paths, handle.host_pid);
        }

        // Successful spawn: clear any prior failure record so a future
        // crash doesn't immediately trip the apology threshold.
        self.spawn_tracker.record_success(session.id);
        sessions::mark_container_running(&self.central, session.id).map_err(ManagerError::Db)?;
        copperclaw_metrics::inc_containers_spawned();
        copperclaw_metrics::observe_container_spawn_seconds(spawn_elapsed);

        // Image + runner attestation (Phase 6 supply-chain). Record, in the
        // append-only audit log, the image tag + the runtime-reported image
        // content digest + the host runner-binary digest for this spawn — a
        // tamper-evident record of exactly which third-party-buildable image
        // and which runner each session ran. Best-effort: a runtime digest
        // error or a DB error degrades to a partial/absent record and is
        // logged, never failing the spawn (attestation is a record, not a
        // gate, at spawn time). The default install is unaffected in behaviour
        // — it just gains an extra audit row per spawn.
        let runner_path = crate::image_health::default_host_runner_path();
        let attestation = crate::attestation::gather(
            self.runtime.as_ref(),
            &image_tag,
            runner_path.as_deref(),
            session.id,
            session.agent_group_id,
        )
        .await;
        crate::attestation::record(&self.central, &attestation);

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
        // install_packages containment (Phase 6 supply-chain). The apt/npm
        // install + any package postinstall scripts run during this build are
        // third-party code. We dispatch the build under an explicit containment
        // posture: `deny_broker_token` is on (the default), so the build can
        // carry NO credential-shaped build-arg or label — the per-session
        // broker capability token is minted only at container SPAWN into the
        // container env and is structurally absent from the build, so a
        // malicious postinstall can never receive it. We do NOT thread any
        // credential into `build_spec.build_args`. Network stays on the bridge
        // because apt/npm need real package-registry egress; the containment is
        // "no broker token + no broker-loopback capability", not the
        // unrestricted-egress regime the running agent would otherwise have.
        build_spec =
            build_spec.with_containment(copperclaw_container_rt::BuildContainment::default());
        // Defense-in-depth: refuse to dispatch a build that somehow acquired a
        // credential-shaped input. Clean by construction here; the assert makes
        // a future regression (someone adding a build-arg) fail loudly rather
        // than leaking a secret into a package postinstall.
        if let Err(violation) = build_spec.assert_no_credentials() {
            return Err(ManagerError::Spawn(
                copperclaw_container_rt::RtError::Container(violation.to_string()),
            ));
        }
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

        // Multi-key provider rotation (M16 Phase 4): when the group's fallback
        // chain selected a chain-entry key whose `api_key_env` is a NON-default
        // name (e.g. `ANTHROPIC_API_KEY_B`), `runner.json` names that var but
        // the runner can only read it if its *value* is actually in the
        // container env. Resolution is the pure [`resolve_multi_key_secret`]
        // (testable under `forbid(unsafe_code)`); the host process env is the
        // injected fallback lookup. A `None` result means nothing to inject
        // (default key, no-auth provider, already-forwarded, or unset). Read-
        // only re-select — no fold/audit (those ran when `runner.json` was
        // assembled).
        let selected_key_env = self.selected_api_key_env(session, chrono::Utc::now());
        match resolve_multi_key_secret(
            selected_key_env.as_deref(),
            &rotatable.forward_env,
            |name| std::env::var(name).ok(),
        ) {
            MultiKeySecret::Inject { name, value } => {
                spec = spec.with_env(&name, &value);
            }
            MultiKeySecret::Missing { name } => {
                warn!(
                    agent_group = %session.agent_group_id.as_uuid(),
                    key_env = %name,
                    "provider failover selected a multi-key api_key_env that is unset \
                     or empty in the host env; the runner will have the var name but \
                     no value — set it in the install .env"
                );
            }
            MultiKeySecret::None => {}
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
            spec = spec.with_egress_allow(resolved_allow.clone());
        }
        spec = spec.with_egress_mode(self.cfg.egress_mode);

        // Phase 0a v2 (Part A): under deny-default, pin the container's
        // /etc/resolv.conf to a host-controlled filtering resolver that
        // answers ONLY the effective allow-list and NXDOMAINs everything else
        // — so a deny-default session can't exfiltrate via DNS labels to an
        // arbitrary resolver. The per-session resolv.conf + dnsmasq filter
        // config are written into the session dir and the resolv.conf is bound
        // read-only into the container. Gated on deny-default so default
        // (allow-all) spawns are untouched. Best-effort: a write failure logs
        // and drops the pin rather than failing the spawn (the agent's model
        // traffic still flows; the operator sees the warning).
        if self.cfg.egress_mode == copperclaw_container_rt::EgressMode::DenyDefault {
            spec = wire_dns_filter(spec, session, paths, &resolved_allow);
        }

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

/// Phase 0a v2 (Part A + B): wire the per-session DNS filter into `spec`
/// under deny-default.
///
/// Writes the pinned `/etc/resolv.conf` + dnsmasq filter config into the
/// session dir and binds the resolv.conf read-only into the container, so
/// the session can only resolve the effective allow-list and reaches no
/// arbitrary resolver. Also constructs the per-session nftables apply plan
/// and writes the ruleset to the session dir for the privileged apply (the
/// runtime path; see [`dns_filter_runtime_status`]).
///
/// Best-effort: a write failure logs at `warn!` and leaves the spec
/// without the resolv.conf pin rather than failing the spawn — model
/// traffic still flows on the carried allow-list, and the operator sees
/// the warning in the host log + `cclaw doctor`.
fn wire_dns_filter(
    mut spec: ContainerSpec,
    session: &Session,
    paths: &SessionPaths,
    resolved_allow: &[String],
) -> ContainerSpec {
    // Forward the allowed names to whatever the host's own resolver uses,
    // dropping loopback to avoid a forwarding loop. Empty → the filter
    // sidecar uses its system default.
    let host_resolv = std::fs::read_to_string("/etc/resolv.conf").ok();
    let upstreams = super::egress::filter_upstreams(host_resolv.as_deref());
    let table = super::egress::nft_table_name(&spec.name);
    let plan = super::egress::build_dns_filter_plan(table, resolved_allow, &upstreams);

    let resolv_path = paths.root.join(super::egress::RESOLV_CONF_FILENAME);
    let dnsmasq_path = paths.root.join(super::egress::DNSMASQ_CONF_FILENAME);
    let nft_path = paths.root.join(EGRESS_NFT_FILENAME);

    // Persist the filter config + ruleset. The resolv.conf is the one that
    // must land for the pin to take effect; the others are inputs to the
    // (deferred) privileged apply + filter sidecar.
    if let Err(err) = std::fs::write(&resolv_path, plan.resolv_conf.as_bytes()) {
        warn!(
            session = %session.id.as_uuid(),
            ?err,
            path = %resolv_path.display(),
            "could not write pinned resolv.conf; deny-default DNS pin skipped this spawn"
        );
        return spec;
    }
    if let Err(err) = std::fs::write(&dnsmasq_path, plan.dnsmasq_conf.as_bytes()) {
        warn!(session = %session.id.as_uuid(), ?err, "could not write dnsmasq filter config");
    }
    if let Err(err) = std::fs::write(&nft_path, plan.nft.ruleset.as_bytes()) {
        warn!(session = %session.id.as_uuid(), ?err, "could not write nftables ruleset");
    }

    spec = spec.with_resolv_conf_source(resolv_path.to_string_lossy().into_owned());

    info!(
        session = %session.id.as_uuid(),
        allow_names = plan.resolver.allow_names.len(),
        dns_filter = "pinned",
        nft_status = dns_filter_runtime_status().as_str(),
        "deny-default DNS filter wired; resolv.conf pinned to filtering resolver"
    );

    spec
}

/// Filename of the per-session constructed nftables ruleset, written into the
/// session dir for the privileged netns apply (the deferred runtime path).
pub const EGRESS_NFT_FILENAME: &str = "egress.nft";

/// Phase 0a v2 (Part B) runtime path: apply the per-session nftables ruleset
/// written by [`wire_dns_filter`] into the session's own network namespace.
///
/// The ruleset (constructed + tested purely) lives at `<session>/egress.nft`.
/// Applying it requires `CAP_NET_ADMIN` and must target the session's OWN
/// network namespace (not the host's) — we enter it via the container's
/// host-visible PID (`host_pid`, surfaced by the runtime on the spawn handle)
/// with `nsenter -t <pid> -n nft -f -`, feeding the ruleset on stdin. We do NOT
/// apply to the host netns: that would be both wrong (it would filter the host)
/// and unsafe.
///
/// What now runs vs. what is still deferred:
///
///   * **PID present + `nft` available + `CAP_NET_ADMIN`** → the privileged
///     netns apply actually runs and the ruleset is loaded into the session
///     netns ([`NftApplyOutcome::Applied`]).
///   * **No PID** (the runtime couldn't report one, e.g. the Apple runtime or
///     a non-running container) → [`NftApplyOutcome::DeferredNoTarget`]: there
///     is nothing to `nsenter` into, so the apply can't run. Honest deferral.
///   * **`nft` missing / non-Linux** → [`NftApplyOutcome::DeferredToolMissing`]
///     / [`NftApplyOutcome::DeferredUnsupported`].
///   * **apply attempted but the privileged exec failed** (no `CAP_NET_ADMIN`,
///     `nsenter` missing, nft load error) → [`NftApplyOutcome::DeferredApplyFailed`].
///     We NEVER report `Applied` on a failed exec.
///
/// In every deferred case the deny-default DNS filter + the empty-allow
/// `network_mode: none` cut remain the live enforcement; only the L3/L4
/// nftables filtering of a non-empty list is what's deferred. Returns the
/// outcome so callers/tests can assert exactly what happened. Never fails the
/// spawn.
fn apply_session_nftables(
    session: &Session,
    paths: &SessionPaths,
    host_pid: Option<i32>,
) -> NftApplyOutcome {
    let nft_path = paths.root.join(EGRESS_NFT_FILENAME);
    let sid = session.id.as_uuid();

    // Capability probe first: no `nft` / non-Linux short-circuits to an honest
    // deferred status regardless of whether we have a PID.
    match dns_filter_runtime_status() {
        NftRuntimeStatus::ToolMissing => {
            warn!(
                session = %sid,
                "deny-default: `nft` not found on PATH — L3/L4 egress filtering not applied; DNS filtering + empty-allow network cut still enforced"
            );
            return NftApplyOutcome::DeferredToolMissing;
        }
        NftRuntimeStatus::Unsupported => {
            warn!(
                session = %sid,
                "deny-default: nftables unsupported on this platform — L3/L4 egress filtering not applied"
            );
            return NftApplyOutcome::DeferredUnsupported;
        }
        NftRuntimeStatus::Available => {}
    }

    // `nft` is available. We still need the container's host-visible PID to
    // target the session netns. Without it there is no `nsenter` target — the
    // ruleset stays constructed-and-deferred (the pre-PID-handoff behaviour).
    let Some(pid) = host_pid else {
        info!(
            session = %sid,
            ruleset = %nft_path.display(),
            "deny-default nftables ruleset constructed; netns apply deferred — runtime did not surface a container PID to target (`nsenter -t <pid>`)"
        );
        return NftApplyOutcome::DeferredNoTarget;
    };

    // We have a PID and `nft`. Attempt the privileged netns apply. This is the
    // step that genuinely needs CAP_NET_ADMIN at runtime; a failure (missing
    // capability, `nsenter` absent, nft load error) is reported honestly as
    // deferred — never faked as applied.
    let ruleset = match std::fs::read_to_string(&nft_path) {
        Ok(rs) => rs,
        Err(err) => {
            warn!(
                session = %sid,
                ?err,
                path = %nft_path.display(),
                "deny-default: could not read constructed nftables ruleset; netns apply deferred"
            );
            return NftApplyOutcome::DeferredApplyFailed;
        }
    };

    match run_nsenter_nft_apply(pid, &ruleset) {
        Ok(()) => {
            info!(
                session = %sid,
                pid,
                "deny-default nftables ruleset applied to session netns (nsenter -t <pid> -n nft -f -)"
            );
            NftApplyOutcome::Applied
        }
        Err(err) => {
            warn!(
                session = %sid,
                pid,
                %err,
                ruleset = %nft_path.display(),
                "deny-default: privileged netns nftables apply failed (needs CAP_NET_ADMIN + nsenter); L3/L4 filtering deferred — DNS filtering + empty-allow network cut still enforced"
            );
            NftApplyOutcome::DeferredApplyFailed
        }
    }
}

/// Build the argv for the privileged per-session nftables apply: enter the
/// container's network namespace by its host-visible PID and load the ruleset
/// from stdin. Pure (no exec) so it is unit-tested without privileges.
///
/// `nsenter -t <pid> -n nft -f -` — `-t <pid>` targets the process, `-n`
/// enters only its network namespace (we deliberately do NOT enter mount/pid
/// namespaces — we only want to apply rules to the session's netns), and
/// `nft -f -` reads the ruleset on stdin (so the allow-list hosts never hit the
/// process table / arg-length caps).
#[must_use]
pub fn nsenter_nft_argv(pid: i32) -> Vec<String> {
    vec![
        "nsenter".to_string(),
        "-t".to_string(),
        pid.to_string(),
        "-n".to_string(),
        "nft".to_string(),
        "-f".to_string(),
        "-".to_string(),
    ]
}

/// Execute the privileged netns nftables apply, feeding `ruleset` on stdin.
///
/// Linux-only — the apply targets a Linux network namespace via `nsenter`. On
/// non-Linux this is unreachable (the capability probe returns `Unsupported`
/// first), but we keep a portable stub so the crate builds everywhere. Returns
/// `Err(String)` (a human-readable reason) on any failure — spawn-failure,
/// non-zero exit (the common no-`CAP_NET_ADMIN` case), or a stdin-write error
/// — so the caller can report an honest deferred status. Never returns `Ok` on
/// a non-zero exit, so a permissions failure can never masquerade as applied.
#[cfg(target_os = "linux")]
fn run_nsenter_nft_apply(pid: i32, ruleset: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let argv = nsenter_nft_argv(pid);
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn `{}`: {e}", argv.join(" ")))?;

    // Feed the ruleset on stdin, then drop the handle so `nft` sees EOF.
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "child stdin unavailable".to_string())?;
        stdin
            .write_all(ruleset.as_bytes())
            .map_err(|e| format!("write ruleset to nft stdin: {e}"))?;
    }

    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait for nft: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(format!(
            "nft exited {} (stderr: {})",
            out.status
                .code()
                .map_or_else(|| "signal".to_string(), |c| c.to_string()),
            stderr.trim()
        ))
    }
}

/// Non-Linux stub: the privileged netns apply is a Linux facility. The
/// capability probe returns [`NftRuntimeStatus::Unsupported`] before this is
/// reached, so this is dead on non-Linux but keeps the crate portable.
#[cfg(not(target_os = "linux"))]
fn run_nsenter_nft_apply(_pid: i32, _ruleset: &str) -> Result<(), String> {
    Err("nftables netns apply unsupported on this platform".to_string())
}

/// Outcome of the per-spawn privileged nftables apply
/// ([`apply_session_nftables`]). Distinct from [`NftRuntimeStatus`] (the
/// host-process capability probe doctor surfaces): this records what actually
/// happened for THIS session's apply, so the host log + tests can assert that a
/// deferral is reported honestly and an apply is only ever claimed when the
/// privileged exec genuinely succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NftApplyOutcome {
    /// The ruleset was loaded into the session netns (`nsenter … nft -f -`
    /// exited 0). The only outcome that claims L3/L4 enforcement.
    Applied,
    /// `nft` is available but the runtime surfaced no container PID, so there
    /// was no netns to target. Ruleset constructed; apply deferred.
    DeferredNoTarget,
    /// The privileged apply was attempted (PID + `nft` present) but the exec
    /// failed — typically no `CAP_NET_ADMIN`, or `nsenter` missing. NEVER an
    /// applied claim.
    DeferredApplyFailed,
    /// `nft` is not installed; the ruleset is constructed but cannot be applied.
    DeferredToolMissing,
    /// The platform doesn't support nftables (non-Linux).
    DeferredUnsupported,
}

impl NftApplyOutcome {
    /// Stable token for logs / JSON surfaces.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NftApplyOutcome::Applied => "applied",
            NftApplyOutcome::DeferredNoTarget => "deferred-no-target",
            NftApplyOutcome::DeferredApplyFailed => "deferred-apply-failed",
            NftApplyOutcome::DeferredToolMissing => "deferred-tool-missing",
            NftApplyOutcome::DeferredUnsupported => "deferred-unsupported",
        }
    }

    /// Whether the ruleset was genuinely loaded into the session netns. `false`
    /// for every deferred variant — the honest signal that L3/L4 filtering is
    /// NOT active and only DNS filtering + the empty-allow cut apply.
    #[must_use]
    pub fn is_applied(self) -> bool {
        matches!(self, NftApplyOutcome::Applied)
    }
}

/// Report whether the privileged nftables apply path can run in this host
/// process. The rule construction is always done (pure + tested); the *apply*
/// needs `CAP_NET_ADMIN` against the session netns. This probe lets `cclaw
/// doctor` show whether deny-default's L3/L4 filtering is actually enforced or
/// only constructed-and-deferred. Linux-only; non-Linux always reports the
/// deferred status (nftables is a Linux facility).
#[must_use]
pub fn dns_filter_runtime_status() -> NftRuntimeStatus {
    #[cfg(target_os = "linux")]
    {
        // `nft` on PATH is necessary for the apply; CAP_NET_ADMIN is checked
        // by attempting a privileged no-op only at the actual apply site (we
        // don't probe it here to avoid a syscall on every spawn). Presence of
        // the binary is the cheap, side-effect-free signal doctor surfaces.
        if which_nft().is_some() {
            NftRuntimeStatus::Available
        } else {
            NftRuntimeStatus::ToolMissing
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        NftRuntimeStatus::Unsupported
    }
}

/// Whether the per-session nftables apply (the deferred runtime path) can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NftRuntimeStatus {
    /// `nft` is on PATH and the platform supports it — the privileged apply
    /// can be attempted at spawn (subject to `CAP_NET_ADMIN`).
    Available,
    /// `nft` is not installed; the ruleset is constructed but cannot be
    /// applied. Deny-default still enforces DNS filtering + the empty-allow
    /// `network_mode: none` cut.
    ToolMissing,
    /// The platform doesn't support nftables (non-Linux). Ruleset construction
    /// is exercised by tests; application is not applicable here.
    Unsupported,
}

impl NftRuntimeStatus {
    /// Stable token for logs / JSON surfaces.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NftRuntimeStatus::Available => "available",
            NftRuntimeStatus::ToolMissing => "tool-missing",
            NftRuntimeStatus::Unsupported => "unsupported",
        }
    }
}

/// Locate `nft` on `PATH` without spawning it. Splits `$PATH` and checks each
/// entry for an `nft` file. Returns the first match. Linux-only helper for
/// [`dns_filter_runtime_status`].
#[cfg(target_os = "linux")]
fn which_nft() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("nft");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
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

/// Outcome of resolving the secret value for a provider-failover-selected
/// multi-key `api_key_env`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MultiKeySecret {
    /// Inject `name=value` into the container env.
    Inject { name: String, value: String },
    /// The key was selected but has no resolvable value — warn; the runner
    /// will have the name but no secret.
    Missing { name: String },
    /// Nothing to do (no selection, the default `ANTHROPIC_API_KEY` slot which
    /// is wired elsewhere, or a key already present in `forward_env`).
    None,
}

/// Pure resolver for the multi-key rotation secret (M16 Phase 4). Decides what
/// (if anything) `build_spec` must inject for a chain-selected `api_key_env`.
///
/// Env-free so it is testable under the workspace's `forbid(unsafe_code)`
/// (which makes `std::env::set_var` unavailable): `lookup` is the injected
/// process-env reader, supplied by `build_spec` as `std::env::var`.
///
/// Rules:
/// * `None` selection / the default `ANTHROPIC_API_KEY` name ⇒ `None` (the
///   default slot is wired by `build_spec` directly, possibly as a broker
///   token — never clobber it here).
/// * a name already in `forward_env` ⇒ `None` (the forward loop injected it).
/// * otherwise resolve the value via `lookup`; non-empty ⇒ `Inject`,
///   unset/empty ⇒ `Missing`.
pub(super) fn resolve_multi_key_secret(
    selected: Option<&str>,
    forward_env: &[(String, String)],
    lookup: impl Fn(&str) -> Option<String>,
) -> MultiKeySecret {
    let Some(name) = selected else {
        return MultiKeySecret::None;
    };
    if name == "ANTHROPIC_API_KEY" || forward_env.iter().any(|(k, _)| k == name) {
        return MultiKeySecret::None;
    }
    match lookup(name).filter(|v| !v.is_empty()) {
        Some(value) => MultiKeySecret::Inject {
            name: name.to_string(),
            value,
        },
        None => MultiKeySecret::Missing {
            name: name.to_string(),
        },
    }
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
    fn build_spec_and_runner_config_agree_on_broker_endpoint() {
        // HIGH-bug regression (end-to-end coherence): the container ENV
        // (`ANTHROPIC_BASE_URL`, written by build_spec) and the on-disk
        // `runner.json` (`api_base_url`, written by runner_config_for) MUST
        // name the SAME endpoint when the broker is enabled — the broker
        // loopback. The runner prefers runner.json's `api_base_url`, so if the
        // two ever disagree the file silently wins and the broker is bypassed.
        // This pins the single source of truth across both writers.
        use super::super::broker::{BrokerConfig, BrokerState};

        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        // Operator base URL is set (the OpenRouter deployment the bug fired on)
        // via manager_cfg; the broker holds the real upstream host-side.
        let broker_cfg = BrokerConfig::resolve(true, Some("sk-test"), None, Some(3600)).unwrap();
        let broker = std::sync::Arc::new(BrokerState::new(broker_cfg));
        let loopback = "http://127.0.0.1:48080";
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        )
        .with_broker(std::sync::Arc::clone(&broker), loopback.into());

        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr
            .build_spec(&session, &paths, "copperclaw/session:abc", None)
            .unwrap();
        let env_base = spec
            .env
            .iter()
            .find(|(k, _)| k == "ANTHROPIC_BASE_URL")
            .map(|(_, v)| v.clone())
            .expect("ANTHROPIC_BASE_URL must be set");

        let rc = mgr.runner_config_for(&session, None, None);
        let file_base = rc
            .api_base_url
            .expect("runner.json api_base_url must be set");

        assert_eq!(env_base, loopback, "container env base must be the broker");
        assert_eq!(
            file_base, loopback,
            "runner.json base must be the broker (not the operator URL)"
        );
        assert_eq!(
            env_base, file_base,
            "env ANTHROPIC_BASE_URL and runner.json api_base_url must agree"
        );

        // And the key slot in the container env is a capability token, not the
        // real key — so the runner authenticates to the broker, which swaps in
        // the real key host-side.
        let api_key = spec
            .env
            .iter()
            .find(|(k, _)| k == "ANTHROPIC_API_KEY")
            .map(|(_, v)| v.clone())
            .expect("ANTHROPIC_API_KEY must be set");
        assert!(api_key.starts_with("cct1."), "expected a capability token");
        assert!(
            !spec.env.iter().any(|(_, v)| v == "sk-test"),
            "real key must not appear in any container env var"
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

    // --- Phase 0a v2: deny-default DNS filtering wiring --------------------

    #[test]
    fn build_spec_allow_all_does_not_pin_resolv_conf() {
        // Default (allow-all) spawns must be entirely unaffected: no
        // resolv.conf pin, no filter files written.
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()), // egress_mode = AllowAll
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let spec = mgr.build_spec(&session, &paths, "img", None).unwrap();
        assert!(
            spec.resolv_conf_source.is_none(),
            "allow-all must not pin resolv.conf"
        );
        assert!(
            !paths
                .root
                .join(super::super::egress::RESOLV_CONF_FILENAME)
                .exists(),
            "allow-all must not write a resolv.conf"
        );
    }

    #[test]
    fn build_spec_deny_default_pins_resolv_conf_and_writes_filter_files() {
        // Under deny-default, build_spec writes the per-session resolv.conf +
        // dnsmasq filter + nftables ruleset and pins the resolv.conf into the
        // container.
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.egress_mode = copperclaw_container_rt::EgressMode::DenyDefault;
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let spec = mgr.build_spec(&session, &paths, "img", None).unwrap();

        // resolv.conf is pinned to the written host file.
        let resolv_path = paths.root.join(super::super::egress::RESOLV_CONF_FILENAME);
        assert_eq!(
            spec.resolv_conf_source.as_deref(),
            Some(resolv_path.to_string_lossy().as_ref())
        );
        // The file exists, pins a single nameserver, and carries no search.
        let resolv = std::fs::read_to_string(&resolv_path).unwrap();
        assert!(resolv.contains("nameserver 127.0.0.1\n"));
        assert!(!resolv.contains("search "));

        // The dnsmasq filter answers the model endpoint and NXDOMAINs the rest.
        // The manager fixture sets ANTHROPIC_BASE_URL to openrouter, so the
        // injected model endpoint is openrouter.ai (NOT api.anthropic.com).
        let dnsmasq =
            std::fs::read_to_string(paths.root.join(super::super::egress::DNSMASQ_CONF_FILENAME))
                .unwrap();
        assert!(
            dnsmasq.contains("server=/openrouter.ai/"),
            "dnsmasq must allow the injected model endpoint: {dnsmasq}"
        );
        assert!(dnsmasq.contains("address=/#/\n"));

        // The nftables ruleset was constructed + persisted with a drop policy.
        let nft = std::fs::read_to_string(paths.root.join(EGRESS_NFT_FILENAME)).unwrap();
        assert!(nft.contains("policy drop;"));
        assert!(nft.contains("ip daddr 127.0.0.1 udp dport 53 accept"));
        // The name endpoint becomes a port-gated rule with the dns-filtered note.
        assert!(
            nft.contains("tcp dport 443 accept comment \"allow openrouter.ai (dns-filtered)\""),
            "ruleset must gate the model endpoint port: {nft}"
        );
    }

    #[test]
    fn apply_session_nftables_never_fails_spawn_and_reports_outcome() {
        // The privileged apply is best-effort: it returns an outcome and never
        // errors, regardless of whether `nft`/CAP_NET_ADMIN are present.
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.egress_mode = copperclaw_container_rt::EgressMode::DenyDefault;
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        // Build the spec so the ruleset file exists, then apply with a PID.
        let _ = mgr.build_spec(&session, &paths, "img", None).unwrap();
        let outcome = apply_session_nftables(&session, &paths, Some(424_242));
        // The outcome is one of the honest values — and on CI (no
        // CAP_NET_ADMIN / no `nft`) it must NOT claim `Applied`.
        assert!(matches!(
            outcome,
            NftApplyOutcome::Applied
                | NftApplyOutcome::DeferredNoTarget
                | NftApplyOutcome::DeferredApplyFailed
                | NftApplyOutcome::DeferredToolMissing
                | NftApplyOutcome::DeferredUnsupported
        ));
        // In the CI sandbox the apply can never genuinely succeed (PID 424242
        // is not a real container, and there's no CAP_NET_ADMIN), so the
        // outcome must be a deferral, never a faked `Applied`.
        assert!(
            !outcome.is_applied(),
            "apply against a bogus PID without CAP_NET_ADMIN must defer, not fake success: {outcome:?}"
        );
    }

    #[test]
    fn apply_session_nftables_no_pid_defers_when_nft_available() {
        // With `nft` available but NO container PID surfaced by the runtime,
        // there is no netns to target — the outcome must be the honest
        // `DeferredNoTarget`, never `Applied`. When `nft` is absent (CI), the
        // tool-missing deferral wins first; either way it is never applied.
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.egress_mode = copperclaw_container_rt::EgressMode::DenyDefault;
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let _ = mgr.build_spec(&session, &paths, "img", None).unwrap();
        let outcome = apply_session_nftables(&session, &paths, None);
        assert!(!outcome.is_applied());
        match dns_filter_runtime_status() {
            // nft present: the PID-less path is specifically DeferredNoTarget.
            NftRuntimeStatus::Available => {
                assert_eq!(outcome, NftApplyOutcome::DeferredNoTarget);
            }
            // nft absent / unsupported: the capability probe defers first.
            NftRuntimeStatus::ToolMissing => {
                assert_eq!(outcome, NftApplyOutcome::DeferredToolMissing);
            }
            NftRuntimeStatus::Unsupported => {
                assert_eq!(outcome, NftApplyOutcome::DeferredUnsupported);
            }
        }
    }

    #[test]
    fn nsenter_nft_argv_targets_session_netns_and_reads_stdin() {
        // The privileged apply must enter ONLY the netns of the given PID and
        // load the ruleset from stdin (`-f -`).
        assert_eq!(
            nsenter_nft_argv(4242),
            vec![
                "nsenter".to_string(),
                "-t".to_string(),
                "4242".to_string(),
                "-n".to_string(),
                "nft".to_string(),
                "-f".to_string(),
                "-".to_string(),
            ]
        );
    }

    #[test]
    fn nft_apply_outcome_as_str_and_is_applied() {
        assert_eq!(NftApplyOutcome::Applied.as_str(), "applied");
        assert!(NftApplyOutcome::Applied.is_applied());
        for o in [
            NftApplyOutcome::DeferredNoTarget,
            NftApplyOutcome::DeferredApplyFailed,
            NftApplyOutcome::DeferredToolMissing,
            NftApplyOutcome::DeferredUnsupported,
        ] {
            assert!(!o.is_applied(), "{o:?} must not claim applied");
            assert!(o.as_str().starts_with("deferred-"));
        }
    }

    #[test]
    fn nft_runtime_status_as_str_is_stable() {
        assert_eq!(NftRuntimeStatus::Available.as_str(), "available");
        assert_eq!(NftRuntimeStatus::ToolMissing.as_str(), "tool-missing");
        assert_eq!(NftRuntimeStatus::Unsupported.as_str(), "unsupported");
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

    #[tokio::test]
    async fn deny_default_spawn_threads_host_pid_into_egress_apply_path() {
        // End-to-end: under deny-default, a spawn whose runtime surfaces a
        // host-visible PID drives the privileged nftables apply path with that
        // PID as the netns target. The runtime (NoopRuntime) reports a PID on
        // the spawn handle; `maybe_spawn` forwards `handle.host_pid` into
        // `apply_session_nftables`. We assert the spawn completed (deny-default
        // path ran end-to-end) and the per-session ruleset was constructed +
        // persisted for the apply to load. The apply itself can't genuinely
        // succeed in CI (bogus PID, no CAP_NET_ADMIN) — and crucially it does
        // NOT fail the spawn (honest deferral, never faked success).
        use copperclaw_container_rt::ContainerRuntime as _;
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        // Runtime surfaces a host PID on every spawn handle.
        let runtime =
            std::sync::Arc::new(crate::tests::NoopRuntime::default().with_host_pid(98765));
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.egress_mode = copperclaw_container_rt::EgressMode::DenyDefault;
        let mgr = ContainerManager::new(db.clone(), runtime.clone(), cfg);
        let session = fixture_session(&db);
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

        // The deny-default apply path runs with the surfaced PID and never
        // fails the spawn.
        let spawned = mgr.maybe_spawn(&session).await.unwrap();
        assert!(spawned, "deny-default spawn should succeed");

        // The handle the runtime returned carries the PID the apply path
        // consumed — proving the runtime → handle.host_pid wiring is live.
        let handle = runtime
            .spawn(ContainerSpec::new("probe", "img"))
            .await
            .unwrap();
        assert_eq!(
            handle.host_pid,
            Some(98765),
            "runtime must surface the host PID the deny-default apply targets"
        );

        // The per-session ruleset was constructed + persisted for the privileged
        // `nsenter -t <pid> -n nft -f -` load to read.
        let nft = std::fs::read_to_string(paths.root.join(EGRESS_NFT_FILENAME)).unwrap();
        assert!(nft.contains("policy drop;"));

        // And the spawn itself was recorded + the session marked running — the
        // apply is best-effort and never blocks the spawn.
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Running));
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

    /// Phase 6 supply-chain: a successful spawn records an attestation row in
    /// the central audit log carrying the runtime-reported image digest.
    #[tokio::test]
    async fn spawn_records_attestation_digest_in_audit_log() {
        use copperclaw_db::tables::audit_log;

        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        // Runtime reports a content digest for the image it spawns.
        let runtime = std::sync::Arc::new(
            crate::tests::NoopRuntime::default().with_image_digest("sha256:deadbeefcafe"),
        );
        let mgr = ContainerManager::new(
            db.clone(),
            runtime.clone(),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
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

        let spawned = mgr.maybe_spawn(&session).await.unwrap();
        assert!(spawned, "expected a spawn with pending inbound");
        assert_eq!(runtime.spawn_calls().len(), 1, "exactly one spawn");

        // The attestation row is present and carries the image digest.
        let rows = audit_log::list_recent(&db, chrono::Utc::now() - chrono::Duration::hours(1), 50)
            .unwrap();
        let att = rows
            .iter()
            .find(|r| r.command == crate::attestation::ATTESTATION_COMMAND)
            .expect("attestation audit row must exist after spawn");
        assert_eq!(att.caller_kind, "host");
        assert_eq!(
            att.caller_session.as_deref(),
            Some(session.id.as_uuid().to_string().as_str())
        );
        let args: serde_json::Value = serde_json::from_str(&att.args).unwrap();
        assert_eq!(args["image_digest"], "sha256:deadbeefcafe");
        assert!(
            args["image_tag"].as_str().is_some_and(|s| !s.is_empty()),
            "image_tag must be recorded"
        );
    }

    // ── install_packages containment ───────────────────────────────

    /// Phase 6 supply-chain: the apt/npm install build dispatched by
    /// `rebuild_image` must run broker-token-denied. Even with the credential
    /// broker ENABLED on the manager, the build spec handed to the runtime
    /// carries the deny-broker-token containment posture and NO credential
    /// build-arg — so a malicious package postinstall cannot receive the
    /// per-session broker capability token (the token is minted only at
    /// container spawn, never threaded into a build).
    #[tokio::test]
    async fn install_packages_build_is_broker_token_denied() {
        use super::super::broker::{BrokerConfig, BrokerState};

        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = std::sync::Arc::new(crate::tests::NoopRuntime::default());
        // Broker ENABLED — the worst case for a token leak into a build.
        let broker_cfg =
            BrokerConfig::resolve(true, Some("sk-real-key"), None, Some(3600)).unwrap();
        let broker = std::sync::Arc::new(BrokerState::new(broker_cfg));
        let mgr = ContainerManager::new(
            db.clone(),
            runtime.clone(),
            manager_cfg(tmp.path().to_path_buf()),
        )
        .with_broker(broker, "http://127.0.0.1:48080".into());

        let session = fixture_session(&db);
        // Config with agent-requested packages (the install_packages outcome).
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
            packages_apt: vec!["cowsay".into()],
            packages_npm: vec!["left-pad".into()],
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
        // rebuild_image writes the new tag back to container_configs, so the
        // row must exist first.
        container_configs::upsert(
            &db,
            container_configs::UpsertContainerConfig {
                agent_group_id: session.agent_group_id,
                provider: None,
                model: None,
                effort: None,
                image_tag: None,
                assistant_name: None,
                max_messages_per_prompt: None,
                skills: container_configs::SkillsSelector::All,
                mcp_servers: serde_json::json!({}),
                packages_apt: vec!["cowsay".into()],
                packages_npm: vec!["left-pad".into()],
                additional_mounts: serde_json::json!([]),
                cli_scope: container_configs::CliScope::Group,
                config_fingerprint: None,
                egress_allow: vec![],
                resource_limits: serde_json::json!({}),
                coding_enabled: false,
                surface_thinking: false,
                tool_profile: None,
            },
        )
        .unwrap();

        let _tag = mgr
            .rebuild_image(session.agent_group_id, &cfg)
            .await
            .expect("rebuild should succeed");

        let specs = runtime.build_specs();
        assert_eq!(specs.len(), 1, "exactly one build dispatched");
        let built = &specs[0];
        // 1. The build packages match the agent's request.
        assert_eq!(built.apt_packages, vec!["cowsay".to_string()]);
        assert_eq!(built.npm_packages, vec!["left-pad".to_string()]);
        // 2. Containment denies the broker token.
        assert!(
            built.containment.deny_broker_token,
            "install_packages build must deny the broker token"
        );
        // 3. NO build-arg / label is credential-shaped (the broker token, the
        //    real key, etc. never ride into the build), even with broker on.
        assert!(
            built.assert_no_credentials().is_ok(),
            "build spec must carry no credential-shaped input"
        );
        assert!(
            built.build_args.is_empty(),
            "no build-args threaded into the package build"
        );
        // 4. The real key must not appear anywhere in the build spec's inputs.
        let serialized = format!("{built:?}");
        assert!(
            !serialized.contains("sk-real-key"),
            "the real provider key must not leak into the build spec"
        );
        assert!(
            !serialized.contains("cct1."),
            "no broker capability token may leak into the build spec"
        );
    }

    /// Phase 6 supply-chain: an OAuth token stored host-side for an external
    /// MCP server must NEVER be forwarded into the container env. The token
    /// lives only in the central `mcp_oauth_tokens` table; `build_spec` builds
    /// the container env from the rotatable config + broker, and never reads
    /// the OAuth store — so a shell inside the container can't `printenv` the
    /// token.
    #[test]
    fn oauth_token_stored_host_side_never_enters_container_env() {
        use copperclaw_db::tables::mcp_oauth_tokens;

        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);

        // Store an OAuth token for this group's "github" MCP server.
        let secret = "oauth-access-SECRET-do-not-leak";
        mcp_oauth_tokens::upsert(
            &db,
            &mcp_oauth_tokens::UpsertMcpOAuthToken {
                agent_group_id: session.agent_group_id,
                server_name: "github".into(),
                access_token: secret.into(),
                refresh_token: Some("refresh-SECRET".into()),
                token_type: "Bearer".into(),
                scope: Some("repo".into()),
                expires_at: None,
            },
        )
        .unwrap();

        // It is durably stored host-side.
        let stored = mcp_oauth_tokens::get(&db, session.agent_group_id, "github")
            .unwrap()
            .expect("token stored host-side");
        assert_eq!(stored.access_token, secret);

        // But it must not appear anywhere in the spawned container's env.
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "img", None).unwrap();
        for (k, v) in &spec.env {
            assert!(
                !v.contains("SECRET"),
                "OAuth token must not be in container env (var {k})"
            );
        }
        assert!(
            !spec
                .env
                .iter()
                .any(|(k, _)| k.contains("OAUTH") || k.eq_ignore_ascii_case("GITHUB_TOKEN")),
            "no OAuth-bearing env var may be injected into the container"
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

    // ---- M16 Phase 4: multi-key secret injection (MEDIUM 3) ----

    #[test]
    fn resolve_multi_key_secret_rules() {
        let fwd = vec![("TAVILY_API_KEY".to_string(), "tv".to_string())];
        // No selection -> nothing to inject.
        assert_eq!(
            resolve_multi_key_secret(None, &fwd, |_| None),
            MultiKeySecret::None
        );
        // The default slot is wired elsewhere (and may be a broker token) —
        // never inject it here even if a chain pins it by name.
        assert_eq!(
            resolve_multi_key_secret(Some("ANTHROPIC_API_KEY"), &fwd, |_| Some("x".into())),
            MultiKeySecret::None
        );
        // Already forwarded by the operator -> the forward loop handles it.
        assert_eq!(
            resolve_multi_key_secret(Some("TAVILY_API_KEY"), &fwd, |_| Some("x".into())),
            MultiKeySecret::None
        );
        // A non-default selected key whose value resolves -> inject name+value.
        assert_eq!(
            resolve_multi_key_secret(Some("ANTHROPIC_API_KEY_B"), &fwd, |n| {
                (n == "ANTHROPIC_API_KEY_B").then(|| "sk-secondary".to_string())
            }),
            MultiKeySecret::Inject {
                name: "ANTHROPIC_API_KEY_B".into(),
                value: "sk-secondary".into(),
            }
        );
        // Selected but unset/empty in the host env -> Missing (warn at spawn).
        assert_eq!(
            resolve_multi_key_secret(Some("ANTHROPIC_API_KEY_B"), &fwd, |_| None),
            MultiKeySecret::Missing {
                name: "ANTHROPIC_API_KEY_B".into(),
            }
        );
        assert_eq!(
            resolve_multi_key_secret(Some("ANTHROPIC_API_KEY_B"), &fwd, |_| Some(String::new())),
            MultiKeySecret::Missing {
                name: "ANTHROPIC_API_KEY_B".into(),
            }
        );
    }

    /// End-to-end: a chain that selects a NON-default key env (`api_key_env =
    /// ANTHROPIC_API_KEY_B`) must produce a container env that actually carries
    /// that var's *value* — `runner.json` names it, so without the value the
    /// runner has a name pointing at nothing. The value here is sourced via the
    /// rotatable `forward_env` (the SIGHUP-rotatable surface), which is what an
    /// operator sets the secondary key through; that exercises the same
    /// `selected_api_key_env` selection the live path runs.
    #[test]
    fn build_spec_injects_selected_multi_key_secret_value() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        // Carry the secondary key's value the way an operator would (.env ->
        // rotatable forward_env). build_spec must surface it for the container.
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.forward_env = vec![(
            "ANTHROPIC_API_KEY_B".to_string(),
            "sk-secondary".to_string(),
        )];
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        // Single-entry chain whose only key names the secondary env var, so the
        // selector picks ANTHROPIC_API_KEY_B with no degrade needed.
        copperclaw_db::tables::provider_profiles::set_chain(
            &db,
            session.agent_group_id,
            &serde_json::json!([
                {"provider": "anthropic", "model": "claude-sonnet-4-6",
                 "keys": [{"id": "k2", "api_key_env": "ANTHROPIC_API_KEY_B"}]}
            ]),
            &serde_json::json!({}),
            None,
            chrono::Utc::now(),
        )
        .unwrap();
        // Confirm the selection actually resolves to the secondary key env.
        assert_eq!(
            mgr.selected_api_key_env(&session, chrono::Utc::now())
                .as_deref(),
            Some("ANTHROPIC_API_KEY_B"),
        );
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "img", None).unwrap();
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "ANTHROPIC_API_KEY_B" && v == "sk-secondary"),
            "selected multi-key api_key_env value must reach the container env: {:?}",
            spec.env
        );
    }
}
