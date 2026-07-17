//! DB-integrity check (M21 O2 — "find corruption instead of skipping it").
//!
//! Before this card, a corrupt per-session database made every per-session
//! sweep check (`stuck`, `processing`, `recurrence`, …) log-and-swallow
//! that session on *every* pass, forever: no metric, no escalation, no
//! operator signal — the session was silently dead. Per decision (f) of
//! the M21 plan the sweep now proactively runs a read-only
//! `PRAGMA quick_check` on a rotating subset of the fleet each pass and,
//! on corruption, quarantines the session so it is skipped cleanly (and
//! loudly, once) rather than swallowed quietly on every pass.
//!
//! ## Quarantine sidecar — the on-disk contract (read by O1's `cclaw doctor`)
//!
//! When a session's DB fails `quick_check`, this check writes a sidecar
//! file as a **sibling of the session directory**, inside the (host-only)
//! agent-group directory. The path convention is **stable and documented**
//! because another card (O1) reads it to surface quarantines in
//! `cclaw doctor` without a new admin verb:
//!
//! ```text
//! <data_root>/sessions/<agent_group_uuid>/<session_uuid>.quarantined
//! ```
//!
//! i.e. the session-directory name ([`copperclaw_db::session::SessionPaths::root`])
//! with [`QUARANTINE_SIDECAR_NAME`] (`.quarantined`) appended, placed
//! *beside* the session directory rather than inside it.
//!
//! **Security (why a sibling, not a child).** The session directory itself
//! is bind-mounted read-write into the untrusted agent's container as
//! `/data`. A marker *inside* that directory could be forged by the agent
//! (a simple `touch /data/.quarantined`) to opt its own session out of all
//! host sweep supervision — including the S2 stuck-container restart
//! actuator — silently. The parent agent-group directory is never mounted
//! into any container, so a sibling marker there is host-authoritative: the
//! sandboxed agent cannot create, delete, or forge it.
//!
//! The file **contents** are a single line of UTF-8 JSON:
//!
//! ```json
//! {"reason":"quick_check","db":"outbound.db","detail":"database disk image is malformed","detected_at":"2026-07-17T12:00:00Z"}
//! ```
//!
//! - `reason`  — always `"quick_check"` today (the only detector).
//! - `db`      — which per-session file failed: `"inbound.db"` or
//!   `"outbound.db"`.
//! - `detail`  — the `quick_check` problem text (or open error).
//! - `detected_at` — RFC 3339 UTC timestamp of detection.
//!
//! A reader that only needs "is this session quarantined?" can treat the
//! mere *existence* of the file as authoritative and ignore the body. The
//! set of JSON keys is additive-only from here — O1 must tolerate unknown
//! keys.
//!
//! The sidecar is a plain file, so quarantine **survives a host restart**
//! for free: the next sweep sees the file and excludes the session again
//! with no in-memory state to rebuild (decision (f) — no migration).

use crate::error::SweepError;
use crate::service::SessionRoot;
use chrono::{DateTime, Utc};
use copperclaw_db::integrity::{QuickCheckOutcome, quick_check};
use copperclaw_types::{AgentGroupId, SessionId};

/// Suffix appended to the session-directory name to form the quarantine
/// sidecar, written as a SIBLING of the session directory (inside the
/// host-only agent-group directory, never the container-writable session
/// dir itself). See the module docs for the full path + contents contract
/// and the security rationale for the sibling placement.
pub const QUARANTINE_SIDECAR_NAME: &str = ".quarantined";

/// Number of rotation slots the per-session integrity check cycles
/// through. Each sweep pass checks only the sessions whose stable slot
/// equals the current pass's slot, so a healthy DB pays exactly one
/// `quick_check` per full rotation (every `INTEGRITY_ROTATION_SLOTS`
/// passes), NOT one per pass. With the 60 s [`crate::SWEEP_POLL_MS`] this
/// is a full-fleet integrity sweep once per hour.
pub const INTEGRITY_ROTATION_SLOTS: u64 = 60;

/// One corruption finding from a per-session `quick_check`. Collected into
/// [`crate::service::SweepReport::integrity_quarantined`] and used to write
/// the sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrityFinding {
    /// Session whose DB failed the check.
    pub session_id: SessionId,
    /// Which per-session file failed: `"inbound.db"` or `"outbound.db"`.
    pub db: &'static str,
    /// The `quick_check` problem detail (or the open error).
    pub detail: String,
}

/// Which rotation slot a session falls in — a stable function of the
/// session id, so a session lands in the same slot every pass and the
/// fleet is partitioned deterministically across the rotation. Using the
/// UUID's low bits keeps the distribution even without hashing.
pub fn rotation_slot(session_id: &SessionId, slots: u64) -> u64 {
    debug_assert!(slots > 0, "rotation slot count must be positive");
    let slots = slots.max(1);
    // v7 UUIDs carry a timestamp in the high bits and randomness in the
    // low bits; reduce the whole 128-bit value modulo `slots` so
    // freshly-created sessions scatter across slots rather than clumping
    // by creation time. The remainder is `< slots <= u64::MAX`, so the
    // narrowing back to u64 is always exact.
    let rem = session_id.as_uuid().as_u128() % u128::from(slots);
    u64::try_from(rem).unwrap_or(0)
}

/// Absolute path of a session's quarantine sidecar — a SIBLING of the
/// session directory (`<session_dir>.quarantined`) in the host-only
/// agent-group directory, deliberately NOT a child of the
/// container-writable session directory. See [`sidecar_path_for`].
pub fn sidecar_path(
    root: &dyn SessionRoot,
    agent_group_id: &AgentGroupId,
    session_id: &SessionId,
) -> std::path::PathBuf {
    sidecar_path_for(&root.session_paths(agent_group_id, session_id).root)
}

/// The quarantine sidecar path for a given session directory: the session
/// directory's own path with [`QUARANTINE_SIDECAR_NAME`] appended, so the
/// marker is a sibling in the parent (agent-group) directory. The parent is
/// never bind-mounted into a container, so the sandboxed agent cannot forge,
/// delete, or observe it — unlike the session directory, which IS its
/// writable `/data`.
pub fn sidecar_path_for(session_dir: &std::path::Path) -> std::path::PathBuf {
    let mut name = session_dir.file_name().unwrap_or_default().to_os_string();
    name.push(QUARANTINE_SIDECAR_NAME);
    match session_dir.parent() {
        Some(parent) => parent.join(name),
        None => std::path::PathBuf::from(name),
    }
}

/// True if the session already carries a quarantine sidecar and must be
/// excluded from all sweep work.
pub fn is_quarantined(
    root: &dyn SessionRoot,
    agent_group_id: &AgentGroupId,
    session_id: &SessionId,
) -> bool {
    sidecar_path(root, agent_group_id, session_id).exists()
}

/// Run a read-only `quick_check` on the session's per-session databases
/// and, on the first corrupt DB, write the quarantine sidecar.
///
/// Returns `Ok(Some(finding))` when the session was quarantined this call,
/// `Ok(None)` when both databases are healthy (or simply not yet
/// materialised — a missing file is not corruption). `Err` is reserved for
/// a failure to *write* the sidecar after detecting corruption; the caller
/// logs and retries on the next rotation.
///
/// Checks `outbound.db` first (the container is its writer, and a
/// bind-mount is the likeliest corruption source) then `inbound.db`.
pub fn check_and_quarantine(
    root: &dyn SessionRoot,
    agent_group_id: &AgentGroupId,
    session_id: &SessionId,
    now: DateTime<Utc>,
) -> Result<Option<IntegrityFinding>, SweepError> {
    let paths = root.session_paths(agent_group_id, session_id);
    for (db_name, db_path) in [
        ("outbound.db", &paths.outbound_db),
        ("inbound.db", &paths.inbound_db),
    ] {
        // M1 metric wish: integrity_quick_check_total{scope="session",
        // outcome} — increment per db probed by Healthy / Missing / Corrupt.
        match quick_check(db_path) {
            QuickCheckOutcome::Healthy | QuickCheckOutcome::Missing => {}
            QuickCheckOutcome::Corrupt(detail) => {
                write_sidecar(&sidecar_path_for(&paths.root), db_name, &detail, now)?;
                // M1 metric wish: integrity_quarantines_total — increment
                // once per session quarantined.
                return Ok(Some(IntegrityFinding {
                    session_id: *session_id,
                    db: db_name,
                    detail,
                }));
            }
        }
    }
    Ok(None)
}

/// Write the quarantine sidecar at `sidecar` (see the module docs for the
/// format). `sidecar` is the host-only sibling path from
/// [`sidecar_path_for`]; its parent (the agent-group directory) already
/// exists in every real deployment, but we `create_dir_all` it defensively
/// so the write never fails on a fresh tree.
fn write_sidecar(
    sidecar: &std::path::Path,
    db: &str,
    detail: &str,
    now: DateTime<Utc>,
) -> Result<(), SweepError> {
    if let Some(parent) = sidecar.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::json!({
        "reason": "quick_check",
        "db": db,
        "detail": detail,
        "detected_at": now.to_rfc3339(),
    });
    // `to_string` (not pretty) keeps the sidecar a single line.
    let line =
        serde_json::to_string(&body).unwrap_or_else(|_| "{\"reason\":\"quick_check\"}".into());
    std::fs::write(sidecar, line)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MemSessionRoot, corrupt_session_db, seed_running_session};
    use chrono::TimeZone;
    use copperclaw_db::central::CentralDb;
    use std::collections::HashSet;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 17, 12, 0, 0).unwrap()
    }

    #[test]
    fn rotation_slot_is_stable_and_in_range() {
        let s = SessionId::new();
        let a = rotation_slot(&s, INTEGRITY_ROTATION_SLOTS);
        let b = rotation_slot(&s, INTEGRITY_ROTATION_SLOTS);
        assert_eq!(a, b, "slot must be stable for a session");
        assert!(a < INTEGRITY_ROTATION_SLOTS);
    }

    #[test]
    fn rotation_covers_every_session_within_n_passes() {
        // Every session must be checked in exactly one slot across a full
        // rotation of INTEGRITY_ROTATION_SLOTS passes.
        let sessions: Vec<SessionId> = (0..200).map(|_| SessionId::new()).collect();
        let mut checked: HashSet<SessionId> = HashSet::new();
        for pass in 0..INTEGRITY_ROTATION_SLOTS {
            let slot = pass % INTEGRITY_ROTATION_SLOTS;
            for s in &sessions {
                if rotation_slot(s, INTEGRITY_ROTATION_SLOTS) == slot {
                    assert!(
                        checked.insert(*s),
                        "session checked more than once per rotation"
                    );
                }
            }
        }
        for s in &sessions {
            assert!(checked.contains(s), "session never checked within N passes");
        }
    }

    #[test]
    fn healthy_session_is_not_quarantined() {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let session = seed_running_session(&central);
        // Materialise healthy per-session DBs.
        let _ = root
            .outbound_pool(&session.agent_group_id, &session.id)
            .unwrap();
        let _ = root
            .inbound_pool(&session.agent_group_id, &session.id)
            .unwrap();

        let finding =
            check_and_quarantine(&root, &session.agent_group_id, &session.id, now()).unwrap();
        assert!(finding.is_none());
        assert!(!is_quarantined(&root, &session.agent_group_id, &session.id));
    }

    #[test]
    fn missing_dbs_are_not_corruption() {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let session = seed_running_session(&central);
        // No DBs materialised at all.
        let finding =
            check_and_quarantine(&root, &session.agent_group_id, &session.id, now()).unwrap();
        assert!(
            finding.is_none(),
            "a session with no DB files is not corrupt"
        );
    }

    #[test]
    fn corrupt_db_is_detected_and_quarantined() {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let session = seed_running_session(&central);
        corrupt_session_db(&root, &session, "outbound.db");

        let finding = check_and_quarantine(&root, &session.agent_group_id, &session.id, now())
            .unwrap()
            .expect("corruption must be detected");
        assert_eq!(finding.db, "outbound.db");
        assert!(is_quarantined(&root, &session.agent_group_id, &session.id));

        // Sidecar is a single line of the documented JSON shape.
        let path = sidecar_path(&root, &session.agent_group_id, &session.id);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(!body.contains('\n'), "sidecar must be one line");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["reason"], "quick_check");
        assert_eq!(v["db"], "outbound.db");
        assert!(v["detail"].as_str().is_some());
        assert!(v["detected_at"].as_str().is_some());
    }

    /// Security regression (review finding, MEDIUM): the quarantine marker
    /// must live OUTSIDE the session directory, because that directory is
    /// bind-mounted read-write into the untrusted agent's container as
    /// `/data`. A marker the agent can write would let it opt its own
    /// session out of all host sweep supervision (incl. the S2 stuck
    /// actuator) with a bare `touch /data/.quarantined`. So: the sidecar is
    /// a sibling, and a marker forged INSIDE the session dir is NOT honored.
    #[test]
    fn quarantine_marker_lives_outside_the_container_writable_session_dir() {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let session = seed_running_session(&central);
        let _ = root
            .outbound_pool(&session.agent_group_id, &session.id)
            .unwrap();
        let session_dir = root
            .session_paths(&session.agent_group_id, &session.id)
            .root;
        let sidecar = sidecar_path(&root, &session.agent_group_id, &session.id);

        // Sibling of the session dir, never a child of it.
        assert!(
            !sidecar.starts_with(&session_dir),
            "quarantine marker must not live inside the container-writable session dir: {sidecar:?}",
        );
        assert_eq!(
            sidecar.parent(),
            session_dir.parent(),
            "marker must be a sibling in the (host-only) agent-group dir",
        );

        // An agent forging `.quarantined` inside its own /data must NOT be
        // honored as a host quarantine.
        std::fs::write(session_dir.join(QUARANTINE_SIDECAR_NAME), "{}").unwrap();
        assert!(
            !is_quarantined(&root, &session.agent_group_id, &session.id),
            "a marker inside the container-writable session dir must not count as quarantined",
        );
    }
}
