//! Host-level integration: the HEARTBEAT-style condition check-in wired
//! through the real [`SweepService`] + [`FsSessionRoot`] the boot path uses.
//!
//! Proves the end-to-end path the host depends on: register a condition on
//! the sweep's shared `ConditionStore`, run a real sweep pass against an
//! on-disk session tree, and confirm a wake inbound was synthesised AND the
//! fire was audited — fired ONLY because the condition currently holds.

use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
use copperclaw_db::tables::audit_log;
use copperclaw_db::tables::sessions::{
    CreateSession, create as create_sess, mark_container_running,
};
use copperclaw_host::sessions::FsSessionRoot;
use copperclaw_host_sweep::{
    CHECKIN_AUDIT_COMMAND, Condition, ConditionContext, ConditionKind, ConditionStore, SweepService,
};
use std::sync::Arc;

fn seed_running_session(central: &CentralDb) -> copperclaw_types::Session {
    let ag = create_ag(
        central,
        CreateAgentGroup {
            name: "checkin-test".into(),
            folder: format!("checkin-{}", uuid::Uuid::new_v4()),
            agent_provider: None,
        },
    )
    .unwrap();
    let sess = create_sess(
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

fn count_inbound(root: &FsSessionRoot, sess: &copperclaw_types::Session) -> i64 {
    use copperclaw_host_sweep::SessionRoot as _;
    let mut pool = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
    pool.conn_mut()
        .query_row("SELECT COUNT(*) FROM messages_in", [], |r| r.get(0))
        .unwrap()
}

#[tokio::test]
async fn host_condition_checkin_fires_only_when_condition_holds_and_audits() {
    let tmp = tempfile::tempdir().unwrap();
    let central = CentralDb::open_in_memory().unwrap();
    let sess = seed_running_session(&central);

    let root: Arc<dyn copperclaw_host_sweep::SessionRoot> =
        Arc::new(FsSessionRoot::new(tmp.path()));
    let store = Arc::new(ConditionStore::new());
    // Build the sweep exactly like boot.rs, then share the condition store.
    let sweep =
        SweepService::new(central.clone(), root.clone()).with_condition_store(store.clone());

    // Register a flag-driven condition on the session via the SHARED store
    // the host would call (sweep.condition_store()).
    sweep.condition_store().register(Condition {
        id: "ci-1".into(),
        agent_group_id: sess.agent_group_id,
        session_id: sess.id,
        kind: ConditionKind::FlagSet {
            flag: "wake".into(),
        },
        prompt: "heartbeat: please check in".into(),
    });

    // The default production sampler builds context from pending inbound only,
    // so the flag condition is FALSE under it -> no fire on a normal pass.
    let report = sweep.run_once().unwrap();
    assert!(
        report.condition_checkins_fired.is_empty(),
        "flag not set in sampled context -> must not fire"
    );
    let fs_root = FsSessionRoot::new(tmp.path());
    assert_eq!(count_inbound(&fs_root, &sess), 0);
    assert_eq!(audit_log::count(&central).unwrap(), 0);

    // Now drive the condition directly with a holding context to prove the
    // fire path the sweep wires: rising edge -> exactly one wake + one audit.
    let held = |_: &copperclaw_types::SessionId| ConditionContext {
        flags_set: vec!["wake".into()],
        ..ConditionContext::quiet()
    };
    let fired = copperclaw_host_sweep::checks::condition_checkin::check(
        sweep.condition_store().as_ref(),
        &central,
        root.as_ref(),
        &held,
        chrono::Utc::now(),
    )
    .unwrap();
    assert_eq!(fired.len(), 1, "condition holds -> fires once");
    assert_eq!(fired[0].series_id, "ci-1");
    assert_eq!(
        count_inbound(&fs_root, &sess),
        1,
        "wake inbound synthesised"
    );

    // The fire was audited under the condition-checkin command.
    let rows = audit_log::list_recent(
        &central,
        chrono::Utc::now() - chrono::Duration::hours(1),
        10,
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].command, CHECKIN_AUDIT_COMMAND);
    assert!(rows[0].args.contains("ci-1"));
}

#[tokio::test]
async fn host_default_sweep_has_no_condition_checkins() {
    // Behaviour-unchanged guarantee: with no conditions registered, a host
    // sweep pass fires zero check-ins and writes zero check-in audits.
    let tmp = tempfile::tempdir().unwrap();
    let central = CentralDb::open_in_memory().unwrap();
    let _sess = seed_running_session(&central);
    let root: Arc<dyn copperclaw_host_sweep::SessionRoot> =
        Arc::new(FsSessionRoot::new(tmp.path()));
    let sweep = SweepService::new(central.clone(), root);
    let report = sweep.run_once().unwrap();
    assert!(report.condition_checkins_fired.is_empty());
    assert_eq!(audit_log::count(&central).unwrap(), 0);
}
