//! Boot-time image health check.
//!
//! The host's container manager spawns a per-session container from a
//! pre-built session image. Operators who edit the runner crate run
//! `./rebuild.sh`, which installs new host binaries AND rebakes the
//! session image so the new runner ends up inside the container.
//!
//! If the operator (or a deploy automation) refreshes the host
//! binaries but skips the image rebuild, every session ends up handed
//! to a stale runner. Users see silence; no logs, no apology, just
//! nothing. To prevent that, `copperclaw run` performs a boot-time
//! health check on the configured image before the container manager
//! starts:
//!
//! 1. Image exists locally (`docker image inspect`).
//! 2. Image carries the runner binary at the expected path AND that
//!    binary is executable (`docker run --rm --entrypoint ls -l`).
//! 3. Image's fingerprint label matches the host's runner sha256.
//!    A mismatch is a WARN, not a degrade — fingerprints legitimately
//!    differ across architectures and build flavours.
//!
//! Each check is bounded by a short per-call deadline and the whole
//! probe is bounded by an outer 10s timeout so a wedged docker daemon
//! can never block boot indefinitely.
//!
//! When any of steps 1 or 2 fail (or the outer deadline trips), the
//! host enters degraded mode: it still boots (so the admin socket
//! works and `cclaw doctor` is reachable), but the container manager
//! refuses to spawn new sessions and every session with a pending
//! chat inbound gets a one-time apology row routed back to the
//! originating channel.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use copperclaw_db::central::CentralDb;
use copperclaw_db::session::{SessionPaths, open_inbound, open_outbound};
use copperclaw_db::tables::{messages_in, messages_out, sessions};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{info, warn};

/// The label name an `copperclaw-setup`-built image carries with the
/// runner fingerprint. Must match the `LABEL` written by
/// `container/Dockerfile` (and re-asserted by
/// `copperclaw_setup::steps::image::FINGERPRINT_LABEL`).
pub const FINGERPRINT_LABEL: &str = "copperclaw.fingerprint";

/// Path inside the image where the runner binary is expected to live.
/// Must match `copperclaw_host::container_manager::CONTAINER_RUNNER_PATH`.
pub const RUNNER_PATH_IN_IMAGE: &str = "/usr/local/bin/copperclaw-runner";

/// Per-call timeout for the `docker run --entrypoint ls` step. Kept
/// short — the call only needs to start a container and have it
/// run `ls`, which should complete in <1s on a healthy daemon.
pub const BINARY_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Outer deadline for the entire health-check pipeline. If any step
/// hangs past this point, the host enters degraded mode rather than
/// blocking boot.
pub const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Why the host has entered degraded mode. Each variant maps to a
/// distinct value of the `reason` label on the
/// `copperclaw_degraded_state` Prometheus gauge.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum HealthDegradedReason {
    /// `docker image inspect <tag>` failed (404 or daemon error).
    /// The image isn't in the local store — operator likely forgot to
    /// run `./rebuild.sh` after updating the runner crate.
    #[error("session image {tag} not found locally")]
    ImageNotFound {
        /// The tag that was probed.
        tag: String,
    },
    /// The image exists but does not have the runner binary baked in
    /// at `/usr/local/bin/copperclaw-runner`.
    #[error("session image {tag} is missing {path}")]
    RunnerBinaryMissing {
        /// The tag that was probed.
        tag: String,
        /// The expected runner path.
        path: String,
    },
    /// The runner binary is present but not executable. Probably a
    /// hand-rolled image build that copied the binary without setting
    /// the `+x` bit.
    #[error("session image {tag} has {path} but it is not executable")]
    RunnerBinaryNotExecutable {
        /// The tag that was probed.
        tag: String,
        /// The runner path.
        path: String,
    },
    /// The overall health-check pipeline didn't complete inside
    /// [`HEALTH_CHECK_TIMEOUT`]. Typically a wedged docker daemon.
    #[error("image health check timed out after {0:?}")]
    Timeout(Duration),
    /// A docker subprocess failed to spawn or returned a non-zero
    /// status with output that doesn't match the recognised "missing
    /// file" pattern. Treated as a degrade so the host doesn't
    /// silently accept an unknown daemon state.
    #[error("image health check failed: {0}")]
    Failed(String),
}

impl HealthDegradedReason {
    /// The `reason` label value used for the `copperclaw_degraded_state`
    /// gauge.
    #[must_use]
    pub fn metric_label(&self) -> &'static str {
        match self {
            Self::ImageNotFound { .. } => copperclaw_metrics::DEGRADED_REASON_IMAGE_NOT_FOUND,
            Self::RunnerBinaryMissing { .. } => {
                copperclaw_metrics::DEGRADED_REASON_RUNNER_BINARY_MISSING
            }
            Self::RunnerBinaryNotExecutable { .. } => {
                copperclaw_metrics::DEGRADED_REASON_RUNNER_BINARY_NOT_EXECUTABLE
            }
            Self::Timeout(_) => copperclaw_metrics::DEGRADED_REASON_HEALTH_CHECK_TIMEOUT,
            Self::Failed(_) => copperclaw_metrics::DEGRADED_REASON_HEALTH_CHECK_FAILED,
        }
    }
}

/// Trait abstracting the docker CLI surface needed by
/// [`check_image_health`]. Lives behind a trait so tests can inject a
/// stub without spawning real processes. Async so the production
/// implementation can shell out via `tokio::process::Command` and
/// honour the outer 10s budget.
#[async_trait]
pub trait ImageProbe: Send + Sync {
    /// Returns Ok(true) when the image identified by `tag` is present
    /// locally, Ok(false) when the daemon explicitly reports a 404
    /// (the "missing" case), and Err on a transport error.
    async fn image_exists(&self, tag: &str) -> Result<bool, String>;
    /// Returns Some(value) when the supplied label is present on the
    /// image, None when the label is absent, and Err on a transport
    /// error.
    async fn image_label(&self, tag: &str, label: &str) -> Result<Option<String>, String>;
    /// Run `[ -x <path> ]` inside a one-shot container against the
    /// image. Returns Ok(BinaryCheck::Executable) when the path is
    /// executable, Ok(BinaryCheck::NotExecutable) when present but
    /// not executable, Ok(BinaryCheck::Missing) when absent, and Err
    /// on a transport / daemon error.
    async fn check_binary(&self, tag: &str, path: &str) -> Result<BinaryCheck, String>;

    /// Return the image's content digest (Docker's `.Id`, a `sha256:<hex>`
    /// string) for `tag`, `None` when the image is absent, or `Err` on a
    /// transport error. Used by the boot-time attestation digest check. The
    /// default impl returns `Ok(None)` so a probe that can't surface a digest
    /// degrades to "unknown" rather than a false mismatch.
    async fn image_digest(&self, tag: &str) -> Result<Option<String>, String> {
        let _ = tag;
        Ok(None)
    }
}

/// Outcome of the runner-binary probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryCheck {
    /// File exists at the requested path AND has the execute bit
    /// set for the running user.
    Executable,
    /// File exists but is not executable.
    NotExecutable,
    /// File does not exist in the image at the requested path.
    Missing,
}

/// Production [`ImageProbe`] that shells out to the local `docker`
/// CLI via `tokio::process::Command`. Errors are stringified so the
/// trait signatures stay small; the caller turns the string into a
/// [`HealthDegradedReason`].
///
/// Each call is wrapped in a per-call `tokio::time::timeout` so a
/// wedged docker daemon (e.g. a hung `docker run` in the binary
/// check) trips the outer [`HEALTH_CHECK_TIMEOUT`] before
/// monopolising the boot thread.
#[derive(Debug, Default)]
pub struct DockerImageProbe;

#[async_trait]
impl ImageProbe for DockerImageProbe {
    async fn image_exists(&self, tag: &str) -> Result<bool, String> {
        let mut cmd = Command::new("docker");
        cmd.arg("image")
            .arg("inspect")
            .arg(tag)
            // `--format` keeps the output tiny — we don't actually
            // parse it, the exit status is what matters.
            .arg("--format")
            .arg("{{.Id}}")
            .stderr(Stdio::piped())
            .stdout(Stdio::piped());
        let out = cmd
            .output()
            .await
            .map_err(|e| format!("spawn docker image inspect: {e}"))?;
        if out.status.success() {
            return Ok(true);
        }
        // Docker's stderr for a missing image contains `No such image:`.
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("No such image") {
            return Ok(false);
        }
        Err(format!(
            "docker image inspect {tag} failed: {}",
            stderr.trim()
        ))
    }

    async fn image_label(&self, tag: &str, label: &str) -> Result<Option<String>, String> {
        let format_arg = format!("{{{{ index .Config.Labels \"{label}\" }}}}");
        let mut cmd = Command::new("docker");
        cmd.arg("image")
            .arg("inspect")
            .arg("--format")
            .arg(format_arg)
            .arg(tag)
            .stderr(Stdio::piped())
            .stdout(Stdio::piped());
        let out = cmd
            .output()
            .await
            .map_err(|e| format!("spawn docker image inspect: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "docker image inspect {tag} for label {label} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() || s == "<no value>" {
            Ok(None)
        } else {
            Ok(Some(s))
        }
    }

    async fn check_binary(&self, tag: &str, path: &str) -> Result<BinaryCheck, String> {
        // Use `/bin/ls -l <path>` rather than `[ -x <path> ]` so the
        // image only needs a working `ls`. `--entrypoint` overrides
        // the image's CMD; `--rm` keeps the host clean.
        //
        // Exit codes:
        //   0 → path exists; we then parse `ls -l` to decide whether
        //       the execute bit is set.
        //   non-zero → typically "No such file or directory" from ls.
        //
        // Wrapped in `tokio::time::timeout(BINARY_CHECK_TIMEOUT, ...)`
        // so a hung daemon trips the outer `HEALTH_CHECK_TIMEOUT`.
        let mut cmd = Command::new("docker");
        cmd.arg("run")
            .arg("--rm")
            .arg("--entrypoint")
            .arg("/bin/ls")
            .arg(tag)
            .arg("-l")
            .arg(path)
            .kill_on_drop(true)
            .stderr(Stdio::piped())
            .stdout(Stdio::piped());
        let out = match timeout(BINARY_CHECK_TIMEOUT, cmd.output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(format!("spawn docker run: {e}")),
            Err(_) => {
                // The docker subprocess is killed on drop because of
                // `kill_on_drop(true)` above.
                return Err(format!(
                    "docker run --entrypoint ls timed out after {BINARY_CHECK_TIMEOUT:?}"
                ));
            }
        };
        if !out.status.success() {
            // `ls` emits `No such file or directory` to stderr when
            // the path is missing — that's the explicit Missing case.
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("No such file or directory") {
                return Ok(BinaryCheck::Missing);
            }
            return Err(format!(
                "docker run --entrypoint ls failed (status {}): {}",
                out.status,
                stderr.trim()
            ));
        }
        // Parse `ls -l` output for the execute bit. Sample line:
        //   -rwxr-xr-x 1 root root 1234 May 21 12:00 /usr/local/bin/copperclaw-runner
        let stdout = String::from_utf8_lossy(&out.stdout);
        let first_line = stdout.lines().next().unwrap_or("");
        let perms = first_line.split_whitespace().next().unwrap_or("");
        if perms.len() < 10 {
            // ls output didn't match the format we expect. Treat as
            // Failed rather than Missing — the file is present, but
            // we can't classify it.
            return Err(format!(
                "could not parse `ls -l` output for {path}: {first_line}"
            ));
        }
        // Owner execute bit is char 3 (after the leading file-type
        // char and the rw chars). It's `x` (or `s` for setuid) when
        // set, `-` when clear.
        let owner_exec = perms.chars().nth(3).unwrap_or('-');
        if owner_exec == 'x' || owner_exec == 's' {
            Ok(BinaryCheck::Executable)
        } else {
            Ok(BinaryCheck::NotExecutable)
        }
    }

    async fn image_digest(&self, tag: &str) -> Result<Option<String>, String> {
        // `.Id` is the content-addressable digest (sha256:<hex>) — the same
        // value `docker image inspect` reports and the runtime backend reads.
        let mut cmd = Command::new("docker");
        cmd.arg("image")
            .arg("inspect")
            .arg(tag)
            .arg("--format")
            .arg("{{.Id}}")
            .stderr(Stdio::piped())
            .stdout(Stdio::piped());
        let out = cmd
            .output()
            .await
            .map_err(|e| format!("spawn docker image inspect for digest: {e}"))?;
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            return Ok(if s.is_empty() { None } else { Some(s) });
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("No such image") {
            return Ok(None);
        }
        Err(format!(
            "docker image inspect {tag} for digest failed: {}",
            stderr.trim()
        ))
    }
}

/// Compute the sha256 of the host's runner binary, in lowercase hex.
/// Returns `None` when the path is absent or unreadable — the
/// fingerprint comparison is best-effort (we only emit a warn on
/// mismatch), so a missing host binary just suppresses the warn.
pub fn host_runner_fingerprint(runner_path: Option<&Path>) -> Option<String> {
    let path = runner_path?;
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    Some(format!("{digest:x}"))
}

/// Best-effort guess at the host's running runner binary. Checks the
/// sibling of the current executable first, then `/usr/local/bin/copperclaw-runner`,
/// then `$HOME/.local/bin/copperclaw-runner`. Returns the first match.
///
/// TODO(team-cc): expose this via `HostConfig` once the install
/// layout grows an explicit "runner-binary path" field. The
/// best-effort version below is enough for the boot warn.
#[must_use]
pub fn default_host_runner_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let candidate = parent.join("copperclaw-runner");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    for candidate in [
        "/usr/local/bin/copperclaw-runner",
        "/usr/bin/copperclaw-runner",
    ] {
        let p = Path::new(candidate);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let candidate = PathBuf::from(home).join(".local/bin/copperclaw-runner");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Run the health-check pipeline against `image_tag`. Returns Ok on
/// success (image is healthy enough to run); returns Err with the
/// [`HealthDegradedReason`] that caused the failure.
///
/// `host_runner_fp` is the host's runner binary fingerprint. When
/// `Some`, a mismatch against the image's `copperclaw.fingerprint`
/// label emits a WARN but does NOT degrade — fingerprints can
/// legitimately differ across architectures and build flavours.
/// When `None`, the fingerprint compare is skipped.
///
/// The whole pipeline is bounded by [`HEALTH_CHECK_TIMEOUT`] — a
/// wedged docker daemon never blocks boot for more than 10s.
pub async fn check_image_health(
    probe: &dyn ImageProbe,
    image_tag: &str,
    host_runner_fp: Option<&str>,
) -> Result<(), HealthDegradedReason> {
    // Outer deadline keeps a wedged daemon from blocking boot.
    // Individual probe calls are synchronous (the production probe
    // shells out to `docker` via `std::process::Command`); the
    // expectation is that each call completes in <1s on a healthy
    // daemon. The `BINARY_CHECK_TIMEOUT` constant documents the
    // per-step budget but is also enforced inline by the production
    // probe's own subprocess timeout (see `DockerImageProbe::check_binary`).
    let tag = image_tag.to_string();
    let host_fp = host_runner_fp.map(str::to_owned);

    let work = async {
        // 1. Image exists.
        match probe.image_exists(&tag).await {
            Ok(true) => {}
            Ok(false) => {
                return Err(HealthDegradedReason::ImageNotFound { tag: tag.clone() });
            }
            Err(e) => {
                return Err(HealthDegradedReason::Failed(format!(
                    "image exists check: {e}"
                )));
            }
        }

        // 2. Runner binary is present + executable.
        match probe.check_binary(&tag, RUNNER_PATH_IN_IMAGE).await {
            Ok(BinaryCheck::Executable) => {}
            Ok(BinaryCheck::Missing) => {
                return Err(HealthDegradedReason::RunnerBinaryMissing {
                    tag: tag.clone(),
                    path: RUNNER_PATH_IN_IMAGE.to_string(),
                });
            }
            Ok(BinaryCheck::NotExecutable) => {
                return Err(HealthDegradedReason::RunnerBinaryNotExecutable {
                    tag: tag.clone(),
                    path: RUNNER_PATH_IN_IMAGE.to_string(),
                });
            }
            Err(e) => {
                return Err(HealthDegradedReason::Failed(format!(
                    "runner binary check: {e}"
                )));
            }
        }

        // 3. Fingerprint compare. WARN-only — never degrades.
        if let Some(host_fp) = host_fp.as_deref() {
            match probe.image_label(&tag, FINGERPRINT_LABEL).await {
                Ok(Some(image_fp)) => {
                    if image_fp != host_fp {
                        warn!(
                            tag = %tag,
                            expected = %host_fp,
                            image_has = %image_fp,
                            "image runner may be stale; expected fingerprint {host_fp}, image has {image_fp}",
                        );
                    }
                }
                Ok(None) => {
                    warn!(
                        tag = %tag,
                        "image is missing the {FINGERPRINT_LABEL} label; cannot compare runner fingerprints",
                    );
                }
                Err(e) => {
                    warn!(
                        tag = %tag,
                        ?e,
                        "could not read {FINGERPRINT_LABEL} from image; skipping fingerprint compare",
                    );
                }
            }
        }

        Ok(())
    };

    match timeout(HEALTH_CHECK_TIMEOUT, work).await {
        Ok(res) => res,
        Err(_) => Err(HealthDegradedReason::Timeout(HEALTH_CHECK_TIMEOUT)),
    }
}

/// Env var an operator pins with the EXPECTED image content digest
/// (`sha256:<hex>` or bare hex) so the boot-time attestation digest check has
/// a baseline to compare the live image against. Unset (the default) ⇒ no
/// baseline ⇒ the check reports `no-baseline` and does nothing — behaviour is
/// unchanged until an operator opts in.
pub const EXPECTED_IMAGE_DIGEST_ENV: &str = "COPPERCLAW_EXPECTED_IMAGE_DIGEST";

/// Boot-time attestation digest check (real comparison, opt-in baseline).
///
/// Fetches the live content digest the daemon reports for `image_tag` via the
/// [`ImageProbe`] and compares it against `expected_digest` using the pure
/// [`copperclaw_container_rt::compare_digests`] core. A
/// [`copperclaw_container_rt::DigestComparison::Mismatch`] is the security
/// signal — the image behind the tag changed from the recorded baseline; it is
/// logged loudly. `NoBaseline` / `Unknown` are reported quietly and are not
/// failures, so the default install (no pinned digest) is unaffected.
///
/// Returns the comparison so the boot path / tests can assert on it. Never
/// fails boot.
pub async fn check_boot_image_digest(
    probe: &dyn ImageProbe,
    image_tag: &str,
    expected_digest: Option<&str>,
) -> copperclaw_container_rt::DigestComparison {
    use copperclaw_container_rt::{DigestComparison, compare_digests};
    let observed = match probe.image_digest(image_tag).await {
        Ok(d) => d,
        Err(e) => {
            warn!(tag = %image_tag, error = %e, "boot attestation: could not read live image digest");
            None
        }
    };
    let comparison = compare_digests(observed.as_deref(), expected_digest);
    match comparison {
        DigestComparison::Match => {
            info!(tag = %image_tag, "boot attestation: image digest matches pinned baseline");
        }
        DigestComparison::Mismatch => {
            warn!(
                tag = %image_tag,
                observed = observed.as_deref().unwrap_or("none"),
                expected = expected_digest.unwrap_or("none"),
                "BOOT ATTESTATION MISMATCH: image content digest behind the tag changed from the pinned baseline ({EXPECTED_IMAGE_DIGEST_ENV})"
            );
        }
        DigestComparison::NoBaseline => {
            info!(
                tag = %image_tag,
                "boot attestation: no pinned baseline digest ({EXPECTED_IMAGE_DIGEST_ENV} unset); skipping digest comparison"
            );
        }
        DigestComparison::Unknown => {
            info!(tag = %image_tag, "boot attestation: daemon reported no image digest; skipping comparison");
        }
    }
    comparison
}

/// Per-session apology text written to every session with a pending
/// chat inbound when the host enters degraded mode. Plain ASCII, no
/// emojis — matches the project's "no emojis" rule.
pub const DEGRADED_APOLOGY_TEXT: &str = "The agent is temporarily degraded \
     — the container image is missing or out of date. \
     The operator has been notified.";

/// Transition the host into degraded mode after the boot-time image
/// health check has failed. The host still starts up so the admin
/// socket is reachable, but:
///
/// 1. The container manager refuses to spawn new sessions
///    (`maybe_spawn` returns [`crate::container_manager::ManagerError::HostDegraded`]).
///    The caller of this function is responsible for calling
///    `manager.set_degraded()` once the manager exists.
/// 2. For every session with a pending inbound, a one-time apology
///    row is written to `outbound.db` and routed back through the
///    channel the most recent inbound came in on. Uses the same
///    write pattern as `copperclaw_runner::run::emit_terminal_failure_apologies`.
/// 3. `copperclaw_degraded_state{reason=<reason>}` is set to 1.
/// 4. A clear startup log line is emitted (the `HOST DEGRADED:` line).
///
/// Returns the number of sessions that received an apology row.
/// Errors writing individual rows are logged + swallowed — the
/// pipeline is best-effort, and a partial write is still better
/// than no write.
pub fn enter_degraded_mode(
    central: &CentralDb,
    data_dir: &Path,
    reason: &HealthDegradedReason,
) -> usize {
    // 1. Set the metric gauge. Cheap, idempotent.
    copperclaw_metrics::set_degraded_state(reason.metric_label());

    // 2. Loud startup log. `tracing::warn` so the line stands out
    // against `info` chatter.
    warn!(
        reason = %reason,
        "HOST DEGRADED: {reason}. Run './rebuild.sh' to refresh the session image."
    );

    // 3. Per-session apology emission.
    let sessions_list = match sessions::list_active(central) {
        Ok(s) => s,
        Err(err) => {
            warn!(
                ?err,
                "degraded-mode: could not list active sessions; skipping apology fan-out"
            );
            return 0;
        }
    };
    let mut notified = 0_usize;
    for s in &sessions_list {
        let paths = SessionPaths::new(data_dir, s.agent_group_id, s.id);
        match emit_degraded_apology(&paths) {
            Ok(true) => {
                notified += 1;
                info!(
                    session = %s.id.as_uuid(),
                    agent_group = %s.agent_group_id.as_uuid(),
                    "degraded-mode: posted apology to pending-inbound session"
                );
            }
            Ok(false) => {}
            Err(err) => {
                warn!(
                    session = %s.id.as_uuid(),
                    ?err,
                    "degraded-mode: could not write apology row; continuing"
                );
            }
        }
    }
    if notified > 0 {
        info!(
            notified,
            total = sessions_list.len(),
            "degraded-mode: apology fan-out complete"
        );
    }
    notified
}

/// Write a single apology outbound row for `paths` when the session
/// has at least one pending chat inbound. Returns Ok(true) when a
/// row was written, Ok(false) when there was nothing to apologise
/// for (no chat inbound pending), and Err on a DB failure.
///
/// The apology is routed via the most recent pending chat inbound's
/// `(channel_type, platform_id, thread_id)` so the delivery loop
/// dispatches it back to the originating chat — same pattern as the
/// runner's terminal-failure apology.
fn emit_degraded_apology(paths: &SessionPaths) -> Result<bool, copperclaw_db::DbError> {
    // Open the inbound DB. If the session dir doesn't exist yet
    // (e.g. a session record without on-disk files) this returns an
    // error which we surface to the caller; the caller swallows.
    let inbound = open_inbound(paths)?;
    // The most recent pending chat inbound's routing fields are the
    // best apology target — anything older might be from a different
    // channel.
    let pending = messages_in::get_pending(&inbound, true, 50)?;
    let Some(target) = pending
        .iter()
        .find(|m| matches!(m.kind, copperclaw_types::MessageKind::Chat))
    else {
        return Ok(false);
    };
    let Some(channel_type) = &target.channel_type else {
        return Ok(false);
    };
    let Some(platform_id) = &target.platform_id else {
        return Ok(false);
    };

    let outbound = open_outbound(paths)?;
    let row = messages_out::WriteOutbound {
        id: copperclaw_types::MessageId::new(),
        in_reply_to: Some(target.id),
        timestamp: chrono::Utc::now(),
        deliver_after: None,
        recurrence: None,
        kind: copperclaw_types::MessageKind::Chat,
        platform_id: Some(platform_id.clone()),
        channel_type: Some(channel_type.clone()),
        thread_id: target.thread_id.clone(),
        content: serde_json::json!({ "text": DEGRADED_APOLOGY_TEXT }),
    };
    messages_out::insert(&outbound, &row)?;
    Ok(true)
}

#[cfg(test)]
pub(crate) mod tests {
    //! Trait-only stub for the [`ImageProbe`] surface so the tests in
    //! this crate (and the host crate) can drive `check_image_health`
    //! without spawning real `docker` processes.

    use super::*;
    use std::sync::Mutex;

    /// Stub probe whose responses are pre-loaded by the test. Records
    /// every call so tests can assert call order.
    pub struct StubProbe {
        pub image_exists_result: Mutex<Result<bool, String>>,
        pub label_result: Mutex<Result<Option<String>, String>>,
        pub binary_result: Mutex<Result<BinaryCheck, String>>,
        pub digest_result: Mutex<Result<Option<String>, String>>,
        pub calls: Mutex<Vec<String>>,
    }

    impl Default for StubProbe {
        fn default() -> Self {
            Self {
                image_exists_result: Mutex::new(Ok(true)),
                label_result: Mutex::new(Ok(Some("fp-test".to_string()))),
                binary_result: Mutex::new(Ok(BinaryCheck::Executable)),
                digest_result: Mutex::new(Ok(Some("sha256:basedigest".to_string()))),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl StubProbe {
        pub fn with_image_exists(self, r: Result<bool, String>) -> Self {
            *self.image_exists_result.lock().unwrap() = r;
            self
        }
        pub fn with_label(self, r: Result<Option<String>, String>) -> Self {
            *self.label_result.lock().unwrap() = r;
            self
        }
        pub fn with_binary(self, r: Result<BinaryCheck, String>) -> Self {
            *self.binary_result.lock().unwrap() = r;
            self
        }
        pub fn with_digest(self, r: Result<Option<String>, String>) -> Self {
            *self.digest_result.lock().unwrap() = r;
            self
        }
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ImageProbe for StubProbe {
        async fn image_exists(&self, tag: &str) -> Result<bool, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("image_exists {tag}"));
            self.image_exists_result.lock().unwrap().clone()
        }
        async fn image_label(&self, tag: &str, label: &str) -> Result<Option<String>, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("image_label {tag} {label}"));
            self.label_result.lock().unwrap().clone()
        }
        async fn check_binary(&self, tag: &str, path: &str) -> Result<BinaryCheck, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("check_binary {tag} {path}"));
            self.binary_result.lock().unwrap().clone()
        }
        async fn image_digest(&self, tag: &str) -> Result<Option<String>, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("image_digest {tag}"));
            self.digest_result.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn boot_digest_check_matches_pinned_baseline() {
        use copperclaw_container_rt::DigestComparison;
        let probe = StubProbe::default().with_digest(Ok(Some("sha256:abc".into())));
        // Pinned baseline uses the dash spelling; comparison normalises.
        let c = check_boot_image_digest(&probe, "tag", Some("sha256-ABC")).await;
        assert_eq!(c, DigestComparison::Match);
    }

    #[tokio::test]
    async fn boot_digest_check_flags_mismatch() {
        use copperclaw_container_rt::DigestComparison;
        let probe = StubProbe::default().with_digest(Ok(Some("sha256:abc".into())));
        let c = check_boot_image_digest(&probe, "tag", Some("sha256:def")).await;
        assert_eq!(c, DigestComparison::Mismatch);
        assert!(c.is_failure());
    }

    #[tokio::test]
    async fn boot_digest_check_no_baseline_is_default_safe() {
        use copperclaw_container_rt::DigestComparison;
        let probe = StubProbe::default();
        // The default install pins no digest ⇒ NoBaseline, not a failure.
        let c = check_boot_image_digest(&probe, "tag", None).await;
        assert_eq!(c, DigestComparison::NoBaseline);
        assert!(!c.is_failure());
    }

    #[tokio::test]
    async fn boot_digest_check_unknown_when_daemon_has_no_digest() {
        use copperclaw_container_rt::DigestComparison;
        let probe = StubProbe::default().with_digest(Ok(None));
        let c = check_boot_image_digest(&probe, "tag", Some("sha256:abc")).await;
        assert_eq!(c, DigestComparison::Unknown);
    }

    #[tokio::test]
    async fn image_health_passes_when_image_has_runner() {
        let probe = StubProbe::default();
        let res = check_image_health(&probe, "copperclaw/session:test", Some("fp-test")).await;
        assert!(res.is_ok(), "expected Ok, got {res:?}");
        let calls = probe.calls();
        assert!(calls.iter().any(|c| c.starts_with("image_exists ")));
        assert!(calls.iter().any(|c| c.starts_with("check_binary ")));
        // Fingerprint check fired because host_runner_fp was Some.
        assert!(calls.iter().any(|c| c.starts_with("image_label ")));
    }

    #[tokio::test]
    async fn image_health_fails_when_image_missing() {
        let probe = StubProbe::default().with_image_exists(Ok(false));
        let res = check_image_health(&probe, "missing/tag:nope", None).await;
        match res {
            Err(HealthDegradedReason::ImageNotFound { tag }) => {
                assert_eq!(tag, "missing/tag:nope");
            }
            other => panic!("expected ImageNotFound, got {other:?}"),
        }
        // We must not have reached the binary probe — the image is
        // missing, there's nothing to ls inside.
        let calls = probe.calls();
        assert!(
            !calls.iter().any(|c| c.starts_with("check_binary ")),
            "binary check should be skipped: {calls:?}"
        );
    }

    #[tokio::test]
    async fn image_health_fails_when_runner_binary_absent() {
        let probe = StubProbe::default().with_binary(Ok(BinaryCheck::Missing));
        let res = check_image_health(&probe, "copperclaw/session:test", None).await;
        match res {
            Err(HealthDegradedReason::RunnerBinaryMissing { tag, path }) => {
                assert_eq!(tag, "copperclaw/session:test");
                assert_eq!(path, RUNNER_PATH_IN_IMAGE);
            }
            other => panic!("expected RunnerBinaryMissing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn image_health_fails_when_runner_binary_not_executable() {
        let probe = StubProbe::default().with_binary(Ok(BinaryCheck::NotExecutable));
        let res = check_image_health(&probe, "copperclaw/session:test", None).await;
        assert!(matches!(
            res,
            Err(HealthDegradedReason::RunnerBinaryNotExecutable { .. })
        ));
    }

    #[tokio::test]
    async fn image_health_warns_on_fingerprint_mismatch() {
        // Image carries a different fingerprint than the host runner.
        // The pipeline must still return Ok — fingerprint mismatch
        // is warn-only.
        let probe = StubProbe::default().with_label(Ok(Some("image-fp".to_string())));
        let res = check_image_health(&probe, "copperclaw/session:test", Some("host-fp")).await;
        assert!(
            res.is_ok(),
            "fingerprint mismatch must NOT degrade, got {res:?}"
        );
    }

    #[tokio::test]
    async fn image_health_skips_label_when_no_host_fingerprint() {
        let probe = StubProbe::default();
        let res = check_image_health(&probe, "tag:test", None).await;
        assert!(res.is_ok());
        // Without a host fingerprint there's nothing to compare against,
        // so the label call should not have fired.
        let calls = probe.calls();
        assert!(
            !calls.iter().any(|c| c.starts_with("image_label ")),
            "label probe should be skipped when host_fp is None: {calls:?}"
        );
    }

    #[tokio::test]
    async fn image_health_fails_on_probe_transport_error() {
        let probe = StubProbe::default().with_image_exists(Err("daemon down".to_string()));
        let res = check_image_health(&probe, "tag:test", None).await;
        match res {
            Err(HealthDegradedReason::Failed(msg)) => {
                assert!(msg.contains("daemon down"), "got: {msg}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn metric_label_covers_every_variant() {
        let cases = [
            HealthDegradedReason::ImageNotFound { tag: "x".into() },
            HealthDegradedReason::RunnerBinaryMissing {
                tag: "x".into(),
                path: "p".into(),
            },
            HealthDegradedReason::RunnerBinaryNotExecutable {
                tag: "x".into(),
                path: "p".into(),
            },
            HealthDegradedReason::Timeout(Duration::from_secs(1)),
            HealthDegradedReason::Failed("boom".into()),
        ];
        for c in &cases {
            // Each label is non-empty and snake_case ASCII.
            let lbl = c.metric_label();
            assert!(!lbl.is_empty());
            assert!(
                lbl.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_'),
                "label {lbl:?} must be snake_case ASCII"
            );
        }
    }

    #[test]
    fn host_runner_fingerprint_returns_hex_for_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake-runner");
        std::fs::write(&path, b"abc").unwrap();
        let fp = host_runner_fingerprint(Some(&path)).expect("fingerprint should compute");
        // sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(
            fp,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn host_runner_fingerprint_returns_none_for_missing_file() {
        let p = Path::new("/definitely/not/a/file/copperclaw-runner");
        assert!(host_runner_fingerprint(Some(p)).is_none());
        assert!(host_runner_fingerprint(None).is_none());
    }
}
