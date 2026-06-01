//! `agent_dispatch` delivery action — writes a `MessageKind::Agent` outbound
//! row into the target session's `inbound.db`.
//!
//! When a runner calls `send_message(to: Recipient::Agent { session_id })`,
//! or `send_message(to: None)` in a child session (the runner synthesises
//! the parent-as-recipient via `source_session_id`), the row lands in the
//! per-session `outbound.db` with `kind = Agent`. The host's delivery loop
//! dispatches that to the registered `agent_dispatch` handler — that's
//! this module.
//!
//! The handler:
//! 1. Pulls the target session id out of `payload.to.session_id`.
//! 2. Resolves the target session row in the central DB to get its
//!    `agent_group_id` (needed to compute its `inbound.db` path).
//! 3. Writes a `MessageKind::Chat` row into the target session's
//!    `messages_in` with `source_session_id` set to the originating
//!    session (so the parent agent can see which child reported what
//!    on its next turn).
//!
//! Failures log a warn and return `Ok(DeliveryActionOutput::default())`
//! — the delivery loop has already marked the outbound row "delivered"
//! by the time the handler returns, and there's no useful "retry" path
//! for an agent inbound write that failed.

use crate::context::{
    DeliveryActionHandler, DeliveryActionInput, DeliveryActionOutput, Module, ModuleContext,
};
use crate::error::ModuleError;
use async_trait::async_trait;
use copperclaw_db::central::CentralDb;
use copperclaw_db::session::{SessionPaths, open_inbound};
use copperclaw_db::tables::messages_in::{self, WriteInbound};
use copperclaw_db::tables::sessions;
use copperclaw_types::{MessageId, MessageKind, SessionId};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::warn;

/// Module wrapping the `agent_dispatch` handler. Installed by
/// `boot::install_modules` next to the existing `CreateAgentModule`.
pub struct AgentDispatchModule {
    deps: Deps,
}

#[derive(Clone)]
struct Deps {
    central: CentralDb,
    data_root: PathBuf,
}

impl AgentDispatchModule {
    pub fn new(central: CentralDb, data_root: impl Into<PathBuf>) -> Self {
        Self {
            deps: Deps {
                central,
                data_root: data_root.into(),
            },
        }
    }
}

#[async_trait]
impl Module for AgentDispatchModule {
    fn name(&self) -> &'static str {
        "agent_dispatch"
    }

    async fn install(&self, ctx: Arc<dyn ModuleContext>) -> Result<(), ModuleError> {
        ctx.register_delivery_action(
            "agent_dispatch",
            Arc::new(AgentDispatchHandler {
                deps: self.deps.clone(),
            }),
        );
        Ok(())
    }
}

struct AgentDispatchHandler {
    deps: Deps,
}

impl DeliveryActionHandler for AgentDispatchHandler {
    fn handle(&self, input: DeliveryActionInput) -> Result<DeliveryActionOutput, ModuleError> {
        let DeliveryActionInput {
            payload,
            session_id: source_session_id,
            row_id,
            ..
        } = input;

        // 1. Parse the target. Permanent failure (return Ok) — no retry
        //    will rescue a malformed payload.
        let Some(target_session_id) = parse_target_session(&payload) else {
            warn!(
                payload = %payload,
                "agent_dispatch: payload missing tagged `to: {{kind: 'agent', session_id: ...}}`; dropping",
            );
            return Ok(DeliveryActionOutput::default());
        };

        // 2. Resolve the target session. NotFound is permanent (target was
        //    deleted before this row was dispatched) — log and Ok.
        let target = match sessions::get(&self.deps.central, target_session_id) {
            Ok(s) => s,
            Err(err) => {
                warn!(
                    ?err,
                    target_session_id = %target_session_id.as_uuid(),
                    "agent_dispatch: target session not found; dropping",
                );
                return Ok(DeliveryActionOutput::default());
            }
        };

        // 3. Refuse to dead-letter into a non-Active parent. The row
        //    would land in an inbound.db nobody is reading. Permanent
        //    (the parent's status won't flip back to Active for us).
        if !matches!(target.status, copperclaw_types::SessionStatus::Active) {
            warn!(
                target_session_id = %target.id.as_uuid(),
                status = ?target.status,
                "agent_dispatch: target session is not Active; dropping (dead letter)",
            );
            return Ok(DeliveryActionOutput::default());
        }

        // 4. Open the target inbound. Filesystem-level errors are
        //    transient (FS hiccup, permissions race during a chmod) so
        //    propagate as ModuleError to trigger the delivery loop's
        //    retry/backoff.
        let paths = SessionPaths::new(&self.deps.data_root, target.agent_group_id, target.id);
        let conn = open_inbound(&paths).map_err(|err| {
            warn!(
                ?err,
                target_session_id = %target.id.as_uuid(),
                "agent_dispatch: open_inbound failed; will retry",
            );
            ModuleError::other(
                "agent_dispatch",
                format!("open_inbound for {}: {err}", target.id.as_uuid()),
            )
        })?;

        // 5. Build the body. Propagate text and thread_id from the
        //    source payload so thread-aware features on the parent side
        //    can correlate child reports back to the user's thread.
        let text = payload
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_owned();
        let thread_id = payload
            .get("thread_id")
            .and_then(|t| t.as_str())
            .map(str::to_owned);
        let content = serde_json::json!({ "text": text });

        // 6. Use the source outbound row's MessageId as the parent
        //    inbound row's id. Combined with `INSERT OR IGNORE`, this
        //    makes the dispatch idempotent under retry: if the handler
        //    succeeded but `delivered::insert` failed on the caller side,
        //    the next retry re-runs us — and the duplicate insert is a
        //    no-op rather than a second row. Falls back to a fresh
        //    MessageId for the test-construction case where row_id is
        //    absent (the test_support handler-only paths).
        let inbound_id = row_id.unwrap_or_else(MessageId::new);
        let msg = WriteInbound {
            id: inbound_id,
            kind: MessageKind::Chat,
            timestamp: chrono::Utc::now(),
            content,
            trigger: true,
            on_wake: false,
            process_after: None,
            recurrence: None,
            series_id: None,
            platform_id: None,
            channel_type: None,
            thread_id,
            source_session_id: source_session_id.map(|s| s.as_uuid().to_string()),
            // Agent-to-agent dispatch has no channel-level reply/group
            // signal — the parent agent's send is the "wire event" and
            // it carries no platform-side thread or group context to
            // propagate.
            reply_to: None,
            is_group: None,
        };
        // 7. INSERT OR IGNORE on a constraint conflict. Transient SQLite
        //    errors (busy_timeout exceeded, disk full) are propagated as
        //    ModuleError so the delivery loop retries with backoff.
        messages_in::insert_idempotent(&conn, &msg).map_err(|err| {
            warn!(
                ?err,
                target_session_id = %target.id.as_uuid(),
                "agent_dispatch: messages_in::insert_idempotent failed; will retry",
            );
            ModuleError::other(
                "agent_dispatch",
                format!("messages_in::insert for {}: {err}", target.id.as_uuid()),
            )
        })?;
        Ok(DeliveryActionOutput::default())
    }
}

fn parse_target_session(payload: &serde_json::Value) -> Option<SessionId> {
    let to = payload.get("to")?;
    // Require the tagged `Recipient::Agent` form. The looser bare-string
    // fallback that previously existed allowed any `to: "<uuid>"` shape to
    // route into the matching session — a cross-routing risk if a
    // different recipient variant ever happened to serialise with a
    // session-id-shaped string.
    let kind = to.get("kind").and_then(|k| k.as_str())?;
    if kind != "agent" {
        return None;
    }
    let id_str = to.get("session_id").and_then(|s| s.as_str())?;
    let parsed = uuid::Uuid::parse_str(id_str).ok()?;
    Some(SessionId(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::{
        agent_groups::{self, CreateAgentGroup},
        sessions::CreateSession,
    };
    use tempfile::TempDir;

    fn fresh_central() -> (TempDir, CentralDb) {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("central.db");
        let db = CentralDb::open(&db_path).unwrap();
        (tmp, db)
    }

    fn fresh_session(central: &CentralDb, name: &str) -> copperclaw_types::Session {
        let g = agent_groups::create(
            central,
            CreateAgentGroup {
                name: name.into(),
                folder: name.into(),
                agent_provider: None,
            },
        )
        .unwrap();
        sessions::create(
            central,
            CreateSession {
                agent_group_id: g.id,
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn parse_target_handles_tagged_form() {
        let payload = serde_json::json!({
            "text": "hi",
            "to": { "kind": "agent", "session_id": "00000000-0000-0000-0000-000000000001" }
        });
        let id = parse_target_session(&payload).unwrap();
        assert_eq!(
            id.as_uuid().to_string(),
            "00000000-0000-0000-0000-000000000001"
        );
    }

    #[test]
    fn parse_target_rejects_non_agent_kind() {
        let payload = serde_json::json!({
            "to": { "kind": "channel", "id": "telegram:123" }
        });
        assert!(parse_target_session(&payload).is_none());
    }

    #[test]
    fn parse_target_rejects_garbage() {
        assert!(parse_target_session(&serde_json::json!({})).is_none());
        assert!(parse_target_session(&serde_json::json!({"to": "not-a-uuid"})).is_none());
    }

    #[test]
    fn parse_target_rejects_untagged_uuid_string() {
        // A bare-string `to: "<uuid>"` (no `{kind: 'agent'}` wrapper)
        // must NOT route as an agent target. The old permissive parser
        // accepted this and could cross-route into any session.
        let payload = serde_json::json!({
            "to": "00000000-0000-0000-0000-000000000001"
        });
        assert!(parse_target_session(&payload).is_none());
    }

    #[test]
    fn parse_target_rejects_untagged_object_with_session_id() {
        // Object missing the `kind: "agent"` discriminator also must
        // not match — without the tag we cannot tell whether the row
        // was actually intended as an Agent recipient.
        let payload = serde_json::json!({
            "to": { "session_id": "00000000-0000-0000-0000-000000000001" }
        });
        assert!(parse_target_session(&payload).is_none());
    }

    #[test]
    fn dispatch_writes_into_target_inbound() {
        let (tmp, central) = fresh_central();
        let parent = fresh_session(&central, "parent");
        let child = fresh_session(&central, "child");

        let module = AgentDispatchModule::new(central.clone(), tmp.path());
        let handler = AgentDispatchHandler {
            deps: module.deps.clone(),
        };

        let payload = serde_json::json!({
            "text": "summary from child",
            "to": { "kind": "agent", "session_id": parent.id.as_uuid().to_string() }
        });
        let out = handler
            .handle(DeliveryActionInput {
                action: "agent_dispatch".into(),
                payload,
                target: crate::context::DispatchTarget::default(),
                session_id: Some(child.id),
                row_id: None,
            })
            .unwrap();
        assert!(out.message.is_none());

        // Confirm the parent's inbound now has a chat row whose source
        // is the child.
        let parent_paths = SessionPaths::new(tmp.path(), parent.agent_group_id, parent.id);
        let in_conn = open_inbound(&parent_paths).unwrap();
        let pending = messages_in::get_pending(&in_conn, true, 10).unwrap();
        assert_eq!(pending.len(), 1, "expected exactly one new inbound");
        let row = &pending[0];
        assert_eq!(row.kind, MessageKind::Chat);
        assert_eq!(
            row.source_session_id.as_deref(),
            Some(child.id.as_uuid().to_string().as_str()),
            "source_session_id should be the child"
        );
        let text = row.content.get("text").and_then(|t| t.as_str()).unwrap();
        assert_eq!(text, "summary from child");
    }

    #[test]
    fn dispatch_missing_target_is_a_warn_not_a_crash() {
        let (tmp, central) = fresh_central();
        let child = fresh_session(&central, "child");

        let module = AgentDispatchModule::new(central, tmp.path());
        let handler = AgentDispatchHandler {
            deps: module.deps.clone(),
        };
        let payload = serde_json::json!({
            "text": "lost summary",
            "to": { "kind": "agent", "session_id": "00000000-0000-0000-0000-000000000099" }
        });
        let out = handler
            .handle(DeliveryActionInput {
                action: "agent_dispatch".into(),
                payload,
                target: crate::context::DispatchTarget::default(),
                session_id: Some(child.id),
                row_id: None,
            })
            .unwrap();
        assert!(out.message.is_none());
    }

    #[test]
    fn dispatch_is_idempotent_under_retry() {
        // Two dispatch calls with the same row_id must produce ONE
        // parent inbound row, not two. Without `INSERT OR IGNORE` the
        // delivery loop's retry path (transient `delivered::insert`
        // failure after handler success) would create duplicates.
        let (tmp, central) = fresh_central();
        let parent = fresh_session(&central, "parent");
        let child = fresh_session(&central, "child");

        let module = AgentDispatchModule::new(central.clone(), tmp.path());
        let handler = AgentDispatchHandler {
            deps: module.deps.clone(),
        };
        let row_id = MessageId::new();
        let payload = serde_json::json!({
            "text": "report",
            "to": { "kind": "agent", "session_id": parent.id.as_uuid().to_string() }
        });
        for _ in 0..2 {
            handler
                .handle(DeliveryActionInput {
                    action: "agent_dispatch".into(),
                    payload: payload.clone(),
                    target: crate::context::DispatchTarget::default(),
                    session_id: Some(child.id),
                    row_id: Some(row_id),
                })
                .unwrap();
        }
        let parent_paths = SessionPaths::new(tmp.path(), parent.agent_group_id, parent.id);
        let in_conn = open_inbound(&parent_paths).unwrap();
        let pending = messages_in::get_pending(&in_conn, true, 10).unwrap();
        assert_eq!(pending.len(), 1, "retry must dedup via INSERT OR IGNORE");
    }

    #[test]
    fn dispatch_propagates_thread_id_from_payload() {
        // Thread context must survive the cross-session write so
        // thread-aware features on the parent side (per-thread mute,
        // summarise-thread, scope-tools-to-thread) can correlate the
        // child's reply with the originating user thread.
        let (tmp, central) = fresh_central();
        let parent = fresh_session(&central, "parent");
        let child = fresh_session(&central, "child");

        let module = AgentDispatchModule::new(central.clone(), tmp.path());
        let handler = AgentDispatchHandler {
            deps: module.deps.clone(),
        };
        let payload = serde_json::json!({
            "text": "in-thread reply",
            "thread_id": "thread-abc",
            "to": { "kind": "agent", "session_id": parent.id.as_uuid().to_string() }
        });
        handler
            .handle(DeliveryActionInput {
                action: "agent_dispatch".into(),
                payload,
                target: crate::context::DispatchTarget::default(),
                session_id: Some(child.id),
                row_id: Some(MessageId::new()),
            })
            .unwrap();
        let parent_paths = SessionPaths::new(tmp.path(), parent.agent_group_id, parent.id);
        let in_conn = open_inbound(&parent_paths).unwrap();
        let pending = messages_in::get_pending(&in_conn, true, 10).unwrap();
        assert_eq!(pending[0].thread_id.as_deref(), Some("thread-abc"));
    }

    #[test]
    fn dispatch_refuses_archived_target() {
        // A target session that is not Active must not be written into
        // — no runner is reading that inbound.db. The handler must log
        // and return Ok (permanent — retry won't help) rather than
        // dead-lettering.
        let (tmp, central) = fresh_central();
        let parent = fresh_session(&central, "parent");
        // Archive the parent.
        archive_session(&central, parent.id);
        let child = fresh_session(&central, "child");

        let module = AgentDispatchModule::new(central, tmp.path());
        let handler = AgentDispatchHandler {
            deps: module.deps.clone(),
        };
        let payload = serde_json::json!({
            "text": "into the void",
            "to": { "kind": "agent", "session_id": parent.id.as_uuid().to_string() }
        });
        handler
            .handle(DeliveryActionInput {
                action: "agent_dispatch".into(),
                payload,
                target: crate::context::DispatchTarget::default(),
                session_id: Some(child.id),
                row_id: Some(MessageId::new()),
            })
            .unwrap();
        let parent_paths = SessionPaths::new(tmp.path(), parent.agent_group_id, parent.id);
        // open_inbound might still create the file; we just verify no
        // row was inserted into messages_in.
        if let Ok(in_conn) = open_inbound(&parent_paths) {
            let pending = messages_in::get_pending(&in_conn, true, 10).unwrap();
            assert!(
                pending.is_empty(),
                "dispatch must not write into an archived session"
            );
        }
    }

    fn archive_session(central: &copperclaw_db::central::CentralDb, id: SessionId) {
        sessions::set_status(central, id, copperclaw_types::SessionStatus::Archived).unwrap();
    }
}
