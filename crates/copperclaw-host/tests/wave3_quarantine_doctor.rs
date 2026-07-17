//! M21 Wave-3 X-rider: the O2 -> O1 cross-card seam.
//!
//! O2 (`copperclaw-host-sweep`) detects a corrupt per-session DB, writes a
//! quarantine sidecar, and excludes the session from every subsequent sweep
//! pass. O1 (`copperclaw-cclaw` `doctor`) reads that sidecar to surface a
//! `db-integrity` FAIL — WITHOUT a shared type: the on-disk artifact (its
//! path convention + JSON keys) is the ONLY contract between the two crates,
//! and they live in different lanes. Each card's own tests pin its own half
//! against the *documented* contract (O2's writer in
//! `checks/integrity.rs::corrupt_db_is_detected_and_quarantined`; O1's
//! reader in `lib.rs::db_integrity_check_clean_quarantine_and_corrupt_central`),
//! but nothing crosses the seam — a drift in the path shape or the key set
//! between the two hand-written literals would pass both card suites and
//! still leave a corrupt session invisible to `cclaw doctor`.
//!
//! This test closes that gap from a neutral crate that sees both lanes:
//!
//! 1. It runs the REAL [`SweepService`] over a genuinely-corrupted
//!    `outbound.db` and drives detection -> quarantine -> exclusion end to
//!    end (the same machinery `run_host`'s sweep loop runs), and
//! 2. It asserts the artifact O2 actually produced satisfies O1's DOCUMENTED
//!    reader contract exactly — the sibling path
//!    `<data_root>/sessions/<agent_group>/<session_uuid>.quarantined` that
//!    O1's `scan_quarantined_sessions` walks, and the `detail` key it
//!    surfaces — reproducing O1's scan over O2's real bytes.
//!
//! The doctor-command invocation over a real sidecar is NOT reachable from a
//! cross-crate test (O1's `doctor` data-root override is `#[cfg(test)]`,
//! internal to `copperclaw-cclaw`), so the reader -> FAIL/`fix:` mapping
//! itself stays pinned by O1's own unit test; this test pins that O1 is
//! reading exactly what O2 writes. See `fixtures/README-m21-wave3.md`.

use std::sync::Arc;

use copperclaw_db::central::CentralDb;
use copperclaw_db::session::{SessionPaths, open_inbound, open_outbound};
use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
use copperclaw_db::tables::sessions::{
    CreateSession, create as create_session, mark_container_running,
};
use copperclaw_host_sweep::SweepService;
use copperclaw_host_sweep::checks::integrity::{
    INTEGRITY_ROTATION_SLOTS, QUARANTINE_SIDECAR_NAME, is_quarantined, rotation_slot, sidecar_path,
};
use copperclaw_host_sweep::service::FilesystemSessionRoot;
use copperclaw_types::Session;

/// Seed one active (container-running) session in a fresh in-memory central
/// DB and return it.
fn seed_session(central: &CentralDb) -> Session {
    let ag = create_ag(
        central,
        CreateAgentGroup {
            name: "wave3".into(),
            folder: "wave3".into(),
            agent_provider: None,
        },
    )
    .unwrap();
    let sess = create_session(
        central,
        CreateSession {
            agent_group_id: ag.id,
            messaging_group_id: None,
            thread_id: None,
            agent_provider: None,
            source_session_id: None,
        },
    )
    .unwrap();
    mark_container_running(central, sess.id).unwrap();
    copperclaw_db::tables::sessions::get(central, sess.id).unwrap()
}

/// Materialise both per-session DBs, then overwrite `outbound.db` (dropping
/// its WAL sidecars) with bytes that are not a valid `SQLite` database — the
/// same corruption O2's own tests inject, reproduced here because
/// `copperclaw-host-sweep`'s `test_support` is `pub(crate)`.
fn corrupt_outbound_db(paths: &SessionPaths) {
    paths.ensure_dirs().unwrap();
    // `open_*` create the files + run migrations, so the session dir is a
    // real, healthy pair before we corrupt one of them.
    let _ = open_outbound(paths).unwrap();
    let _ = open_inbound(paths).unwrap();
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = paths.outbound_db.clone().into_os_string();
        sidecar.push(suffix);
        let _ = std::fs::remove_file(std::path::PathBuf::from(sidecar));
    }
    std::fs::write(
        &paths.outbound_db,
        b"not a sqlite database at all -- corrupted for the wave-3 x-rider",
    )
    .unwrap();
}

/// Reproduce O1's documented reader contract (`scan_quarantined_sessions`):
/// walk `<data_root>/sessions/<agent_group>/` for `*.quarantined` sibling
/// markers, strip the suffix to recover the session uuid, and read the
/// `detail` key O1 surfaces in the `db-integrity` FAIL row. Returns
/// `(session_uuid, detail)` per quarantined session — exactly what O1 builds
/// its FAIL detail from.
fn scan_quarantined_like_doctor(data_root: &std::path::Path) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let sessions_root = data_root.join("sessions");
    let Ok(agent_dirs) = std::fs::read_dir(&sessions_root) else {
        return out;
    };
    for agent in agent_dirs.flatten() {
        let Ok(entries) = std::fs::read_dir(agent.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(session_uuid) = name.strip_suffix(QUARANTINE_SIDECAR_NAME) else {
                continue;
            };
            // Existence alone is authoritative; the body is best-effort.
            let detail = std::fs::read_to_string(entry.path())
                .ok()
                .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
                .and_then(|v| {
                    v.get("detail")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                });
            out.push((session_uuid.to_string(), detail));
        }
    }
    out
}

/// End to end: a corrupt per-session DB is detected + quarantined by a real
/// sweep pass, excluded from the next pass, and the quarantine artifact O2
/// produced satisfies O1's documented `cclaw doctor` reader contract.
#[test]
fn o2_quarantine_artifact_is_read_by_the_o1_doctor_contract_and_excluded_from_sweeps() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = tmp.path();
    let central = CentralDb::open_in_memory().unwrap();
    let session = seed_session(&central);
    let paths = SessionPaths::new(data_root, session.agent_group_id, session.id);
    corrupt_outbound_db(&paths);

    let root: Arc<dyn copperclaw_host_sweep::SessionRoot> =
        Arc::new(FilesystemSessionRoot::new(data_root.to_path_buf()));
    let sweep = SweepService::new(central.clone(), Arc::clone(&root));

    // The per-session integrity probe is a rotating one: a session is only
    // `quick_check`ed on the pass whose slot matches its stable rotation
    // slot. Drive passes up to and including that slot — the corruption is
    // quarantined on the matching pass, no sooner. (On the earlier passes
    // the corrupt DB is hit by the other per-session checks and cleanly
    // swallowed — the exact pre-O2 "silently dead" behaviour this card ends,
    // now bounded to at most one rotation.)
    let slot = rotation_slot(&session.id, INTEGRITY_ROTATION_SLOTS);
    let mut quarantine_pass = None;
    for pass in 0..=slot {
        let report = sweep.run_once().expect("sweep pass");
        if report
            .integrity_quarantined
            .iter()
            .any(|f| f.session_id == session.id)
        {
            quarantine_pass = Some((pass, report));
            break;
        }
        // Before its slot comes up the session must be neither checked nor
        // excluded — the probe genuinely rotates.
        assert!(
            !report.integrity_excluded.contains(&session.id),
            "not excluded before quarantine (pass {pass})"
        );
    }
    let (pass, report) = quarantine_pass.expect("the corrupt session must be quarantined");
    assert_eq!(pass, slot, "quarantined on exactly its rotation slot");

    let finding = report
        .integrity_quarantined
        .iter()
        .find(|f| f.session_id == session.id)
        .expect("finding for the corrupt session");
    assert_eq!(finding.db, "outbound.db", "the corrupt DB is named");
    assert!(is_quarantined(
        root.as_ref(),
        &session.agent_group_id,
        &session.id
    ));

    // ── The artifact matches O1's documented on-disk reader contract. ──
    let marker = sidecar_path(root.as_ref(), &session.agent_group_id, &session.id);
    // Sibling of the session dir, in the (host-only) agent-group dir — the
    // exact path O1's scanner walks: sessions/<ag>/<session_uuid>.quarantined.
    let expected = data_root
        .join("sessions")
        .join(session.agent_group_id.as_uuid().to_string())
        .join(format!("{}{QUARANTINE_SIDECAR_NAME}", session.id.as_uuid()));
    assert_eq!(marker, expected, "O2 sidecar path == O1 reader convention");
    assert_eq!(
        marker.parent(),
        paths.root.parent(),
        "marker is a sibling of the session dir, never a child of it"
    );

    // Reproduce O1's scan over O2's real bytes: doctor would find exactly
    // this session, with the `quick_check` detail it surfaces in the FAIL.
    let scanned = scan_quarantined_like_doctor(data_root);
    assert_eq!(scanned.len(), 1, "doctor's scan finds the one quarantine");
    assert_eq!(
        scanned[0].0,
        session.id.as_uuid().to_string(),
        "doctor recovers the session uuid from the sidecar name"
    );
    assert!(
        scanned[0].1.as_deref().is_some_and(|d| !d.is_empty()),
        "doctor reads a non-empty quick_check detail for the FAIL row: {:?}",
        scanned[0].1
    );

    // ── The next sweep pass EXCLUDES the quarantined session end to end. ──
    let after = sweep.run_once().expect("post-quarantine sweep pass");
    assert!(
        after.integrity_excluded.contains(&session.id),
        "a quarantined session is excluded from the next pass"
    );
    assert!(
        !after
            .integrity_quarantined
            .iter()
            .any(|f| f.session_id == session.id),
        "an already-quarantined session is not re-quarantined"
    );
    assert!(
        !after.integrity_checked.contains(&session.id),
        "an excluded session pays no further quick_check cost"
    );
}
