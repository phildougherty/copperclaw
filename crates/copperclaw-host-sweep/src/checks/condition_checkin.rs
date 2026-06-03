//! Heartbeat condition-check-in (HEARTBEAT-style).
//!
//! A *condition* check-in is a periodic main-session wake that fires ONLY
//! when a stored boolean condition currently holds — distinct from the
//! time-based [`crate::checks::scheduling`] path, which fires when a
//! `next_fire` instant has elapsed. Here there is no clock deadline: the
//! sweep evaluates each registered condition against a freshly-sampled
//! [`ConditionContext`] every tick, and a condition that *holds* synthesises
//! a `kind: task` wake inbound into its session — exactly as scheduling
//! does — but gated on the predicate, not on time.
//!
//! ## Edge-triggered, not level-triggered
//!
//! A condition that keeps holding must not re-fire every 60s and bury the
//! agent in wakes. The [`ConditionStore`] records each condition's last
//! observed truthiness; a fire happens only on a **rising edge** (the
//! condition was false-or-never-seen last tick and is true now). When the
//! condition goes false again the store re-arms it, so the next rising edge
//! fires once more. This is the HEARTBEAT contract: "tell me when X becomes
//! true," not "spam me while X is true."
//!
//! ## Audited
//!
//! Every fire writes one `audit_log` row (`command =
//! "sweep.condition_checkin"`, `result = "ok"`) so an operator can see, in
//! `cclaw audit list`, exactly which condition woke which session and when —
//! the same audit surface the socket dispatch layer uses for mutations.
//!
//! ## Scope
//!
//! This module is deliberately just the condition evaluation + the fire
//! path. It does NOT implement webhook-triggers or a durable task-flow
//! engine (explicitly out of scope). Conditions live in an in-memory
//! [`ConditionStore`] shared with the host (mirroring
//! [`crate::spawn_tracker::SpawnAttemptTracker`]); persisting them or wiring
//! a registration surface is a separate, opt-in concern. With an empty store
//! — the default until the host registers anything — this check is a no-op
//! and the sweep's behaviour is unchanged.

use crate::error::SweepError;
use crate::service::{SeriesFanout, SessionRoot};
use chrono::{DateTime, Utc};
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::audit_log::{AuditEntry, insert as insert_audit};
use copperclaw_db::tables::messages_in::{WriteInbound, insert as insert_in};
use copperclaw_types::{AgentGroupId, MessageId, MessageKind, SessionId};
use std::collections::HashMap;
use std::sync::Mutex;

/// The kind of observable a [`Condition`] tests. Kept small and concrete so
/// the predicate is a pure, exhaustively-testable function — NOT an
/// arbitrary expression engine (that would be the out-of-scope task-flow).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConditionKind {
    /// Holds when the session has at least `min` pending (unprocessed,
    /// due) inbound messages waiting. Lets a "check in when work piled up"
    /// heartbeat fire without a timer.
    PendingInboundAtLeast { min: u32 },
    /// Holds when the session's last-activity instant is older than
    /// `idle_secs` — an idle-watchdog heartbeat that nudges the agent only
    /// once it has actually gone quiet.
    IdleForAtLeastSecs { idle_secs: u64 },
    /// Holds when an operator-set flag (keyed by name) is currently true.
    /// The flag value is supplied in the [`ConditionContext`]; this lets the
    /// host expose a simple "fire when this latch is set" hook without this
    /// module owning the latch's storage.
    FlagSet { flag: String },
}

/// One stored condition bound to a session. The `id` is the audit /
/// de-dup key; `prompt` is the text delivered to the woken agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub id: String,
    pub agent_group_id: AgentGroupId,
    pub session_id: SessionId,
    pub kind: ConditionKind,
    pub prompt: String,
}

/// The sampled observable state a condition is evaluated against. Built by
/// the caller (the sweep) per session per tick. Pure data, so
/// [`Condition::holds`] is a deterministic function with no I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionContext {
    /// Count of pending (due, unprocessed) inbound messages for the session.
    pub pending_inbound: u32,
    /// Seconds since the session's last activity. `None` when unknown.
    pub idle_secs: Option<u64>,
    /// Operator flags currently set true for this session.
    pub flags_set: Vec<String>,
}

impl ConditionContext {
    /// An all-quiet context: no pending work, not known-idle, no flags.
    /// Convenient default for tests and for sessions with no signal.
    #[must_use]
    pub fn quiet() -> Self {
        Self {
            pending_inbound: 0,
            idle_secs: None,
            flags_set: Vec::new(),
        }
    }
}

impl Condition {
    /// Pure predicate: does this condition currently hold in `ctx`?
    #[must_use]
    pub fn holds(&self, ctx: &ConditionContext) -> bool {
        match &self.kind {
            ConditionKind::PendingInboundAtLeast { min } => ctx.pending_inbound >= *min,
            ConditionKind::IdleForAtLeastSecs { idle_secs } => {
                ctx.idle_secs.is_some_and(|s| s >= *idle_secs)
            }
            ConditionKind::FlagSet { flag } => ctx.flags_set.iter().any(|f| f == flag),
        }
    }
}

/// In-memory registry of conditions + their last-observed edge state.
/// Shared (behind an `Arc`) between the host (which registers conditions)
/// and the sweep (which evaluates + fires). Mirrors
/// [`crate::spawn_tracker::SpawnAttemptTracker`]: a tiny `Mutex`-guarded map,
/// no DB, default-empty so the sweep is a strict no-op until populated.
#[derive(Debug, Default)]
pub struct ConditionStore {
    /// condition id -> condition.
    conditions: Mutex<HashMap<String, Condition>>,
    /// condition id -> last observed `holds` value (the edge latch).
    last_held: Mutex<HashMap<String, bool>>,
}

impl ConditionStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a condition. Resets the edge latch so a
    /// freshly-registered condition that already holds fires on the next
    /// tick (its first observation is a rising edge from "never seen").
    pub fn register(&self, condition: Condition) {
        let id = condition.id.clone();
        self.conditions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), condition);
        self.last_held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }

    /// Remove a condition (and its latch). Returns whether it existed.
    pub fn remove(&self, id: &str) -> bool {
        self.last_held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
        self.conditions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
            .is_some()
    }

    /// Snapshot of all registered conditions for one session.
    #[must_use]
    pub fn for_session(&self, session_id: &SessionId) -> Vec<Condition> {
        self.conditions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|c| &c.session_id == session_id)
            .cloned()
            .collect()
    }

    /// Snapshot of all registered conditions.
    #[must_use]
    pub fn all(&self) -> Vec<Condition> {
        self.conditions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// Update the edge latch and report whether THIS observation is a
    /// rising edge (false-or-never-seen -> true). Only a rising edge fires.
    fn observe_edge(&self, id: &str, now_holds: bool) -> bool {
        let mut latch = self
            .last_held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was = latch.get(id).copied().unwrap_or(false);
        latch.insert(id.to_string(), now_holds);
        now_holds && !was
    }

    /// Test-only read of the current latch value.
    #[cfg(test)]
    fn latch_of(&self, id: &str) -> Option<bool> {
        self.last_held.lock().unwrap().get(id).copied()
    }
}

/// The dotted command name recorded in `audit_log` for each fire. Distinct
/// from the scheduled-task path so operators can tell condition-driven wakes
/// apart from time-driven ones.
pub const CHECKIN_AUDIT_COMMAND: &str = "sweep.condition_checkin";

/// Build the per-session context lookup the sweep evaluates against. The
/// caller supplies a closure that samples observable state for a session id;
/// this keeps the check decoupled from how the sweep gathers signals
/// (per-session DB counts, clocks, operator flags) and keeps the fire path
/// unit-testable with a stub sampler.
pub type ContextSampler<'a> = dyn Fn(&SessionId) -> ConditionContext + 'a;

/// Evaluate every registered condition against its session's sampled
/// context and fire the ones that just became true (rising edge).
///
/// "Fire" mirrors [`crate::checks::scheduling::check`]: synthesise a
/// `kind: task` wake inbound into the session's `inbound.db` so the wake
/// check transitions the container back to running on the next tick. Each
/// fire also appends an `audit_log` row.
///
/// Returns one [`SeriesFanout`] per fired condition (series id = condition
/// id) so the host can correlate the wake back to its source. A condition
/// whose predicate is currently false re-arms its latch and produces no
/// fanout. An empty store yields an empty result with zero side effects.
pub fn check(
    store: &ConditionStore,
    central: &CentralDb,
    root: &dyn SessionRoot,
    sample: &ContextSampler<'_>,
    now: DateTime<Utc>,
) -> Result<Vec<SeriesFanout>, SweepError> {
    let mut out = Vec::new();
    for condition in store.all() {
        let ctx = sample(&condition.session_id);
        let holds = condition.holds(&ctx);
        let rising = store.observe_edge(&condition.id, holds);
        if !rising {
            // Either currently false (re-armed) or still-true (already
            // fired on its rising edge — do not re-fire while it holds).
            continue;
        }
        let fanout = fire(central, root, &condition, now)?;
        out.push(fanout);
    }
    Ok(out)
}

/// Synthesise the wake inbound + audit row for one fired condition.
fn fire(
    central: &CentralDb,
    root: &dyn SessionRoot,
    condition: &Condition,
    now: DateTime<Utc>,
) -> Result<SeriesFanout, SweepError> {
    let msg_id = MessageId::new();
    let write = WriteInbound {
        id: msg_id,
        kind: MessageKind::Task,
        timestamp: now,
        content: serde_json::json!({
            "text": condition.prompt,
            "condition_id": condition.id,
            "checkin": true,
        }),
        trigger: true,
        on_wake: true,
        process_after: None,
        recurrence: None,
        series_id: Some(condition.id.clone()),
        platform_id: None,
        channel_type: None,
        thread_id: None,
        source_session_id: None,
        reply_to: None,
        is_group: None,
    };
    let mut inbound = root.inbound_pool(&condition.agent_group_id, &condition.session_id)?;
    insert_in(inbound.conn_mut(), &write)?;

    // Audit the fire. Best-effort in spirit, but we surface a DB error here
    // because an unaudited fire is a security gap — the caller logs+swallows
    // per-condition errors so one bad row never aborts the whole pass.
    insert_audit(
        central,
        &AuditEntry {
            ts: now,
            caller_kind: "host".into(),
            caller_session: Some(condition.session_id.as_uuid().to_string()),
            caller_agent_group: Some(condition.agent_group_id.as_uuid().to_string()),
            command: CHECKIN_AUDIT_COMMAND.into(),
            args: serde_json::json!({
                "condition_id": condition.id,
                "message_id": msg_id.as_uuid().to_string(),
            })
            .to_string(),
            result: "ok".into(),
            error_code: None,
            error_message: None,
            latency_ms: 0,
        },
    )?;

    Ok(SeriesFanout {
        series_id: condition.id.clone(),
        new_message_id: msg_id,
        next_fire: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MemSessionRoot, seed_running_session};
    use chrono::TimeZone;
    use copperclaw_db::tables::audit_log;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap()
    }

    fn mk_condition(sess: &copperclaw_types::Session, id: &str, kind: ConditionKind) -> Condition {
        Condition {
            id: id.into(),
            agent_group_id: sess.agent_group_id,
            session_id: sess.id,
            kind,
            prompt: "check in".into(),
        }
    }

    fn count_inbound(root: &MemSessionRoot, sess: &copperclaw_types::Session) -> i64 {
        let mut pool = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        pool.conn_mut()
            .query_row("SELECT COUNT(*) FROM messages_in", [], |r| r.get(0))
            .unwrap()
    }

    // ---- pure predicate --------------------------------------------------

    #[test]
    fn pending_inbound_predicate() {
        let c = Condition {
            id: "c".into(),
            agent_group_id: AgentGroupId::new(),
            session_id: SessionId::new(),
            kind: ConditionKind::PendingInboundAtLeast { min: 3 },
            prompt: "x".into(),
        };
        assert!(!c.holds(&ConditionContext {
            pending_inbound: 2,
            ..ConditionContext::quiet()
        }));
        assert!(c.holds(&ConditionContext {
            pending_inbound: 3,
            ..ConditionContext::quiet()
        }));
        assert!(c.holds(&ConditionContext {
            pending_inbound: 9,
            ..ConditionContext::quiet()
        }));
    }

    #[test]
    fn idle_predicate_requires_known_idle() {
        let c = Condition {
            id: "c".into(),
            agent_group_id: AgentGroupId::new(),
            session_id: SessionId::new(),
            kind: ConditionKind::IdleForAtLeastSecs { idle_secs: 300 },
            prompt: "x".into(),
        };
        // Unknown idle -> never holds.
        assert!(!c.holds(&ConditionContext::quiet()));
        assert!(!c.holds(&ConditionContext {
            idle_secs: Some(299),
            ..ConditionContext::quiet()
        }));
        assert!(c.holds(&ConditionContext {
            idle_secs: Some(300),
            ..ConditionContext::quiet()
        }));
    }

    #[test]
    fn flag_predicate() {
        let c = Condition {
            id: "c".into(),
            agent_group_id: AgentGroupId::new(),
            session_id: SessionId::new(),
            kind: ConditionKind::FlagSet {
                flag: "alarm".into(),
            },
            prompt: "x".into(),
        };
        assert!(!c.holds(&ConditionContext::quiet()));
        assert!(c.holds(&ConditionContext {
            flags_set: vec!["alarm".into()],
            ..ConditionContext::quiet()
        }));
        assert!(!c.holds(&ConditionContext {
            flags_set: vec!["other".into()],
            ..ConditionContext::quiet()
        }));
    }

    // ---- store edge semantics --------------------------------------------

    #[test]
    fn observe_edge_only_true_on_rising() {
        let store = ConditionStore::new();
        // never-seen -> false: no edge.
        assert!(!store.observe_edge("c", false));
        // false -> true: rising edge.
        assert!(store.observe_edge("c", true));
        // true -> true: no edge (still holding).
        assert!(!store.observe_edge("c", true));
        // true -> false: no fire (falling), latch re-armed.
        assert!(!store.observe_edge("c", false));
        // false -> true again: rising edge fires once more.
        assert!(store.observe_edge("c", true));
    }

    #[test]
    fn register_resets_latch_so_already_true_fires() {
        let store = ConditionStore::new();
        store.observe_edge("c", true); // latch = true
        let c = Condition {
            id: "c".into(),
            agent_group_id: AgentGroupId::new(),
            session_id: SessionId::new(),
            kind: ConditionKind::FlagSet { flag: "f".into() },
            prompt: "x".into(),
        };
        store.register(c);
        // Re-registration cleared the latch, so an immediately-true
        // observation is a rising edge again.
        assert_eq!(store.latch_of("c"), None);
        assert!(store.observe_edge("c", true));
    }

    #[test]
    fn remove_drops_condition_and_latch() {
        let store = ConditionStore::new();
        let sess_id = SessionId::new();
        store.register(Condition {
            id: "c".into(),
            agent_group_id: AgentGroupId::new(),
            session_id: sess_id,
            kind: ConditionKind::FlagSet { flag: "f".into() },
            prompt: "x".into(),
        });
        assert!(store.remove("c"));
        assert!(!store.remove("c"));
        assert!(store.for_session(&sess_id).is_empty());
    }

    // ---- fire path: fires ONLY when the condition holds ------------------

    #[test]
    fn empty_store_is_noop() {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let store = ConditionStore::new();
        let sampler = |_: &SessionId| ConditionContext::quiet();
        let fired = check(&store, &central, &root, &sampler, now()).unwrap();
        assert!(fired.is_empty());
        assert_eq!(audit_log::count(&central).unwrap(), 0);
    }

    #[test]
    fn condition_that_does_not_hold_never_fires() {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let sess = seed_running_session(&central);
        let store = ConditionStore::new();
        store.register(mk_condition(
            &sess,
            "c1",
            ConditionKind::PendingInboundAtLeast { min: 5 },
        ));
        // Context reports only 1 pending -> condition is FALSE -> no fire.
        let sampler = |_: &SessionId| ConditionContext {
            pending_inbound: 1,
            ..ConditionContext::quiet()
        };
        let fired = check(&store, &central, &root, &sampler, now()).unwrap();
        assert!(fired.is_empty(), "condition must not fire while false");
        assert_eq!(count_inbound(&root, &sess), 0);
        assert_eq!(audit_log::count(&central).unwrap(), 0);
        // The latch was armed to false.
        assert_eq!(store.latch_of("c1"), Some(false));
    }

    #[test]
    fn condition_that_holds_fires_wake_inbound_and_audits() {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let sess = seed_running_session(&central);
        let store = ConditionStore::new();
        store.register(mk_condition(
            &sess,
            "c1",
            ConditionKind::PendingInboundAtLeast { min: 2 },
        ));
        // Context reports 3 pending -> condition is TRUE -> fires.
        let sampler = |_: &SessionId| ConditionContext {
            pending_inbound: 3,
            ..ConditionContext::quiet()
        };
        let fired = check(&store, &central, &root, &sampler, now()).unwrap();
        assert_eq!(fired.len(), 1, "condition must fire while true");
        assert_eq!(fired[0].series_id, "c1");
        // A wake inbound landed in the session.
        assert_eq!(count_inbound(&root, &sess), 1);
        // The fire was audited under the condition-checkin command.
        let rows =
            audit_log::list_recent(&central, now() - chrono::Duration::hours(1), 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].command, CHECKIN_AUDIT_COMMAND);
        assert!(rows[0].args.contains("c1"));
        assert_eq!(rows[0].result, "ok");
    }

    #[test]
    fn fired_inbound_is_task_kind_on_wake_with_condition_marker() {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let sess = seed_running_session(&central);
        let store = ConditionStore::new();
        store.register(mk_condition(
            &sess,
            "c1",
            ConditionKind::FlagSet { flag: "go".into() },
        ));
        let sampler = |_: &SessionId| ConditionContext {
            flags_set: vec!["go".into()],
            ..ConditionContext::quiet()
        };
        check(&store, &central, &root, &sampler, now()).unwrap();
        let mut pool = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        let (kind, on_wake, content): (String, i64, String) = pool
            .conn_mut()
            .query_row(
                "SELECT kind, on_wake, content FROM messages_in LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(kind, "task");
        assert_eq!(on_wake, 1);
        assert!(content.contains("\"checkin\":true") || content.contains("\"checkin\": true"));
        assert!(content.contains("c1"));
    }

    #[test]
    fn holding_condition_fires_once_not_every_tick() {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let sess = seed_running_session(&central);
        let store = ConditionStore::new();
        store.register(mk_condition(
            &sess,
            "c1",
            ConditionKind::FlagSet { flag: "go".into() },
        ));
        let held = |_: &SessionId| ConditionContext {
            flags_set: vec!["go".into()],
            ..ConditionContext::quiet()
        };
        // Tick 1: rising edge -> fires.
        assert_eq!(
            check(&store, &central, &root, &held, now()).unwrap().len(),
            1
        );
        // Tick 2: still holds -> no re-fire.
        assert_eq!(
            check(&store, &central, &root, &held, now()).unwrap().len(),
            0
        );
        // Only one inbound + one audit row total.
        assert_eq!(count_inbound(&root, &sess), 1);
        assert_eq!(audit_log::count(&central).unwrap(), 1);
    }

    #[test]
    fn condition_refires_after_going_false_then_true() {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let sess = seed_running_session(&central);
        let store = ConditionStore::new();
        store.register(mk_condition(
            &sess,
            "c1",
            ConditionKind::FlagSet { flag: "go".into() },
        ));
        let held = |_: &SessionId| ConditionContext {
            flags_set: vec!["go".into()],
            ..ConditionContext::quiet()
        };
        let not_held = |_: &SessionId| ConditionContext::quiet();
        assert_eq!(
            check(&store, &central, &root, &held, now()).unwrap().len(),
            1
        );
        assert_eq!(
            check(&store, &central, &root, &not_held, now())
                .unwrap()
                .len(),
            0
        );
        // Rising edge again after the fall.
        assert_eq!(
            check(&store, &central, &root, &held, now()).unwrap().len(),
            1
        );
        assert_eq!(count_inbound(&root, &sess), 2);
        assert_eq!(audit_log::count(&central).unwrap(), 2);
    }

    #[test]
    fn only_matching_session_context_is_sampled() {
        // Two sessions; a condition on each. The sampler returns the holding
        // context only for session A, so only A fires.
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let sess_a = seed_running_session(&central);
        let sess_b = seed_running_session(&central);
        let store = ConditionStore::new();
        store.register(mk_condition(
            &sess_a,
            "a",
            ConditionKind::FlagSet { flag: "go".into() },
        ));
        store.register(mk_condition(
            &sess_b,
            "b",
            ConditionKind::FlagSet { flag: "go".into() },
        ));
        let a_id = sess_a.id;
        let sampler = move |sid: &SessionId| {
            if *sid == a_id {
                ConditionContext {
                    flags_set: vec!["go".into()],
                    ..ConditionContext::quiet()
                }
            } else {
                ConditionContext::quiet()
            }
        };
        let fired = check(&store, &central, &root, &sampler, now()).unwrap();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].series_id, "a");
        assert_eq!(count_inbound(&root, &sess_a), 1);
        assert_eq!(count_inbound(&root, &sess_b), 0);
    }
}
