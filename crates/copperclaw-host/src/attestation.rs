//! Image + runner-binary attestation: record digests at spawn, check at boot.
//!
//! Session image tags are sha256 *content-addressed* (`...:sha256-<fp>`),
//! which guards against accidental drift but is not a signature. Attestation
//! adds two real things on top:
//!
//! 1. **Spawn-time recording.** Every container spawn records, in the central
//!    `audit_log`, the image tag plus the runtime-reported image content
//!    digest plus the host runner-binary digest. An operator can later read
//!    the attestation rows (`cclaw attestation list`) and see exactly which
//!    image and runner each session ran — a tamper-evident record.
//!
//! 2. **Boot-time digest check.** Against an expected (recorded) digest, the
//!    host compares the live image digest the runtime reports for the
//!    configured tag. A real mismatch (the image behind the tag changed)
//!    is surfaced; a missing baseline or a runtime that can't report a digest
//!    is reported honestly as `no-baseline` / `unknown`, never a false alarm.
//!
//! The comparison logic itself is the pure
//! [`copperclaw_container_rt::compare_digests`] core (unit-tested in that
//! crate); this module is the host-side glue that gathers the inputs and lands
//! the audit row.

use chrono::Utc;
use copperclaw_container_rt::{ContainerRuntime, DigestComparison, compare_digests};
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::audit_log::{self, AuditEntry};
use copperclaw_types::{AgentGroupId, SessionId};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

/// The audit command string under which spawn-time attestation rows are
/// recorded. Distinct from operator mutations so `cclaw audit list` /
/// dashboards can filter for it.
pub const ATTESTATION_COMMAND: &str = "container.attestation";

/// The gathered attestation facts for one spawn. Pure data — produced by
/// [`gather`] (which touches the runtime) and rendered to an [`AuditEntry`] by
/// [`Self::to_audit_entry`] (pure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attestation {
    /// The image tag the container was spawned from.
    pub image_tag: String,
    /// Content digest the runtime reports for `image_tag` (`sha256:<hex>`), or
    /// `None` when the runtime couldn't surface one.
    pub image_digest: Option<String>,
    /// Sha256 of the host runner binary (lowercase hex), or `None` when the
    /// path was absent/unreadable.
    pub runner_digest: Option<String>,
    /// The session this spawn belongs to.
    pub session_id: SessionId,
    /// The owning agent group.
    pub agent_group_id: AgentGroupId,
}

impl Attestation {
    /// Render the attestation facts to an append-only [`AuditEntry`].
    ///
    /// `caller_kind` is `"host"` (the host spawns containers, not an agent).
    /// The `args` JSON carries the digests so the audit row is self-describing
    /// when read back via `cclaw audit list`.
    #[must_use]
    pub fn to_audit_entry(&self) -> AuditEntry {
        let args = serde_json::json!({
            "image_tag": self.image_tag,
            "image_digest": self.image_digest,
            "runner_digest": self.runner_digest,
            "session_id": self.session_id.as_uuid().to_string(),
        })
        .to_string();
        AuditEntry {
            ts: Utc::now(),
            caller_kind: "host".to_string(),
            caller_session: Some(self.session_id.as_uuid().to_string()),
            caller_agent_group: Some(self.agent_group_id.as_uuid().to_string()),
            command: ATTESTATION_COMMAND.to_string(),
            args,
            result: "ok".to_string(),
            error_code: None,
            error_message: None,
            latency_ms: 0,
        }
    }
}

/// Compute the sha256 of the host runner binary, lowercase hex. Returns `None`
/// when the path is absent/unreadable — best-effort, so a missing host binary
/// just records a `None` runner digest rather than failing the spawn.
#[must_use]
pub fn runner_binary_digest(runner_path: Option<&std::path::Path>) -> Option<String> {
    let path = runner_path?;
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(format!("{:x}", hasher.finalize()))
}

/// Gather the attestation facts for a spawn by asking the runtime for the
/// image digest and hashing the host runner binary.
///
/// Never fails: a runtime error fetching the digest degrades to
/// `image_digest = None` (logged) rather than aborting — attestation is a
/// record, not a gate, at spawn time.
pub async fn gather(
    runtime: &dyn ContainerRuntime,
    image_tag: &str,
    runner_path: Option<&std::path::Path>,
    session_id: SessionId,
    agent_group_id: AgentGroupId,
) -> Attestation {
    let image_digest = match runtime.image_digest(image_tag).await {
        Ok(d) => d,
        Err(err) => {
            warn!(
                image = %image_tag,
                ?err,
                "attestation: could not read image digest from runtime; recording none"
            );
            None
        }
    };
    Attestation {
        image_tag: image_tag.to_string(),
        image_digest,
        runner_digest: runner_binary_digest(runner_path),
        session_id,
        agent_group_id,
    }
}

/// Record the spawn-time attestation row in the central audit log.
///
/// Best-effort: a DB error is logged + swallowed (the audit hook must never
/// block a spawn), matching the dispatch-path audit insert contract.
pub fn record(central: &CentralDb, attestation: &Attestation) {
    let entry = attestation.to_audit_entry();
    match audit_log::insert(central, &entry) {
        Ok(_) => {
            info!(
                image = %attestation.image_tag,
                image_digest = attestation.image_digest.as_deref().unwrap_or("none"),
                runner_digest = attestation.runner_digest.as_deref().unwrap_or("none"),
                session = %attestation.session_id.as_uuid(),
                "recorded spawn attestation"
            );
        }
        Err(err) => {
            warn!(
                ?err,
                "attestation: could not write audit row; continuing spawn"
            );
        }
    }
}

/// Boot-time digest check: compare the live image digest the runtime reports
/// for `image_tag` against `expected_digest` (the recorded baseline, e.g. the
/// digest pinned at the last `rebuild.sh`).
///
/// Returns the [`DigestComparison`]. A [`DigestComparison::Mismatch`] is the
/// security signal — the image behind the tag changed. `NoBaseline` (no
/// expected digest configured) and `Unknown` (runtime didn't report a digest)
/// are NOT failures; the caller treats them as "nothing to assert".
///
/// This performs a real digest comparison (not a stub): it fetches the live
/// `.Id` from the runtime and runs the pure comparison. The default install
/// has no expected digest configured, so the result is `NoBaseline` and
/// behaviour is unchanged until an operator opts in by pinning one.
pub async fn check_boot_digest(
    runtime: &dyn ContainerRuntime,
    image_tag: &str,
    expected_digest: Option<&str>,
) -> DigestComparison {
    let observed = match runtime.image_digest(image_tag).await {
        Ok(d) => d,
        Err(err) => {
            warn!(
                image = %image_tag,
                ?err,
                "attestation: boot digest check could not read live image digest"
            );
            None
        }
    };
    let comparison = compare_digests(observed.as_deref(), expected_digest);
    match comparison {
        DigestComparison::Match => {
            info!(image = %image_tag, "boot attestation: image digest matches recorded baseline");
        }
        DigestComparison::Mismatch => {
            warn!(
                image = %image_tag,
                observed = observed.as_deref().unwrap_or("none"),
                expected = expected_digest.unwrap_or("none"),
                "BOOT ATTESTATION MISMATCH: image digest behind tag changed from the recorded baseline"
            );
        }
        DigestComparison::NoBaseline => {
            info!(
                image = %image_tag,
                "boot attestation: no recorded baseline digest; recording observed as baseline is the operator's call"
            );
        }
        DigestComparison::Unknown => {
            info!(
                image = %image_tag,
                "boot attestation: runtime did not report an image digest; skipping comparison"
            );
        }
    }
    comparison
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use copperclaw_container_rt::{ContainerHandle, ContainerSpec, ImageBuildSpec, RtError};
    use std::time::Duration;

    /// Minimal runtime stub: returns a pre-set image digest and records
    /// nothing else. Only the methods attestation touches are meaningful.
    struct DigestStub {
        digest: Option<String>,
        err: bool,
    }

    #[async_trait]
    impl ContainerRuntime for DigestStub {
        async fn ensure_running(&self) -> Result<(), RtError> {
            Ok(())
        }
        async fn cleanup_orphans(&self, _slug: &str) -> Result<(), RtError> {
            Ok(())
        }
        async fn spawn(&self, spec: ContainerSpec) -> Result<ContainerHandle, RtError> {
            Ok(ContainerHandle::new("id".to_string(), spec.name))
        }
        async fn stop(&self, _name: &str, _grace: Duration) -> Result<(), RtError> {
            Ok(())
        }
        async fn build_image(&self, spec: ImageBuildSpec) -> Result<String, RtError> {
            Ok(spec.image_tag())
        }
        async fn image_digest(&self, _tag: &str) -> Result<Option<String>, RtError> {
            if self.err {
                Err(RtError::Container("daemon down".into()))
            } else {
                Ok(self.digest.clone())
            }
        }
    }

    fn central() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    #[test]
    fn runner_digest_hashes_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runner");
        std::fs::write(&path, b"abc").unwrap();
        // sha256("abc")
        assert_eq!(
            runner_binary_digest(Some(&path)).as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }

    #[test]
    fn runner_digest_none_for_missing() {
        assert!(runner_binary_digest(None).is_none());
        assert!(runner_binary_digest(Some(std::path::Path::new("/no/such/runner"))).is_none());
    }

    #[tokio::test]
    async fn gather_records_image_and_runner_digests() {
        let rt = DigestStub {
            digest: Some("sha256:deadbeef".into()),
            err: false,
        };
        let dir = tempfile::tempdir().unwrap();
        let runner = dir.path().join("runner");
        std::fs::write(&runner, b"abc").unwrap();
        let att = gather(
            &rt,
            "copperclaw/session:sha256-xyz",
            Some(&runner),
            SessionId::new(),
            AgentGroupId::new(),
        )
        .await;
        assert_eq!(att.image_digest.as_deref(), Some("sha256:deadbeef"));
        assert!(att.runner_digest.is_some());
        assert_eq!(att.image_tag, "copperclaw/session:sha256-xyz");
    }

    #[tokio::test]
    async fn gather_tolerates_runtime_digest_error() {
        let rt = DigestStub {
            digest: None,
            err: true,
        };
        let att = gather(&rt, "tag", None, SessionId::new(), AgentGroupId::new()).await;
        // Runtime errored ⇒ no image digest recorded, but gather still
        // produced a record (never fails the spawn).
        assert!(att.image_digest.is_none());
        assert!(att.runner_digest.is_none());
    }

    #[test]
    fn record_writes_audit_row() {
        let db = central();
        let att = Attestation {
            image_tag: "copperclaw/session:sha256-xyz".into(),
            image_digest: Some("sha256:deadbeef".into()),
            runner_digest: Some("cafef00d".into()),
            session_id: SessionId::new(),
            agent_group_id: AgentGroupId::new(),
        };
        record(&db, &att);
        let rows =
            audit_log::list_recent(&db, Utc::now() - chrono::Duration::hours(1), 10).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.command, ATTESTATION_COMMAND);
        assert_eq!(row.caller_kind, "host");
        assert_eq!(
            row.caller_session.as_deref(),
            Some(att.session_id.as_uuid().to_string().as_str())
        );
        // The digests are carried in the args JSON.
        let args: serde_json::Value = serde_json::from_str(&row.args).unwrap();
        assert_eq!(args["image_digest"], "sha256:deadbeef");
        assert_eq!(args["runner_digest"], "cafef00d");
        assert_eq!(args["image_tag"], "copperclaw/session:sha256-xyz");
    }

    #[test]
    fn audit_entry_carries_none_digests_as_json_null() {
        let att = Attestation {
            image_tag: "tag".into(),
            image_digest: None,
            runner_digest: None,
            session_id: SessionId::new(),
            agent_group_id: AgentGroupId::new(),
        };
        let entry = att.to_audit_entry();
        let args: serde_json::Value = serde_json::from_str(&entry.args).unwrap();
        assert!(args["image_digest"].is_null());
        assert!(args["runner_digest"].is_null());
    }

    #[tokio::test]
    async fn boot_digest_match() {
        let rt = DigestStub {
            digest: Some("sha256:abc".into()),
            err: false,
        };
        let c = check_boot_digest(&rt, "tag", Some("sha256-ABC")).await;
        assert_eq!(c, DigestComparison::Match);
    }

    #[tokio::test]
    async fn boot_digest_mismatch_is_failure() {
        let rt = DigestStub {
            digest: Some("sha256:abc".into()),
            err: false,
        };
        let c = check_boot_digest(&rt, "tag", Some("sha256:def")).await;
        assert_eq!(c, DigestComparison::Mismatch);
        assert!(c.is_failure());
    }

    #[tokio::test]
    async fn boot_digest_no_baseline_is_default_safe() {
        let rt = DigestStub {
            digest: Some("sha256:abc".into()),
            err: false,
        };
        // No expected digest configured (the default install) ⇒ not a failure.
        let c = check_boot_digest(&rt, "tag", None).await;
        assert_eq!(c, DigestComparison::NoBaseline);
        assert!(!c.is_failure());
    }

    #[tokio::test]
    async fn boot_digest_unknown_when_runtime_has_no_digest() {
        let rt = DigestStub {
            digest: None,
            err: false,
        };
        let c = check_boot_digest(&rt, "tag", Some("sha256:abc")).await;
        assert_eq!(c, DigestComparison::Unknown);
        assert!(!c.is_failure());
    }
}
