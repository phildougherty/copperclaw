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
use ironclaw_db::central::CentralDb;
use ironclaw_db::session::{open_inbound, SessionPaths};
use ironclaw_db::tables::messages_in::{self, WriteInbound};
use ironclaw_db::tables::sessions;
use ironclaw_types::{MessageId, MessageKind, SessionId};
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
            ..
        } = input;

        let Some(target_session_id) = parse_target_session(&payload) else {
            warn!(
                payload = %payload,
                "agent_dispatch: payload missing `to.session_id`; dropping",
            );
            return Ok(DeliveryActionOutput::default());
        };

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

        let paths = SessionPaths::new(
            &self.deps.data_root,
            target.agent_group_id,
            target.id,
        );
        let conn = match open_inbound(&paths) {
            Ok(c) => c,
            Err(err) => {
                warn!(
                    ?err,
                    target_session_id = %target.id.as_uuid(),
                    "agent_dispatch: open_inbound failed; dropping",
                );
                return Ok(DeliveryActionOutput::default());
            }
        };

        let text = payload
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_owned();
        let content = serde_json::json!({ "text": text });

        let msg = WriteInbound {
            id: MessageId::new(),
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
            thread_id: None,
            source_session_id: source_session_id.map(|s| s.as_uuid().to_string()),
        };
        if let Err(err) = messages_in::insert(&conn, &msg) {
            warn!(
                ?err,
                target_session_id = %target.id.as_uuid(),
                "agent_dispatch: messages_in::insert failed",
            );
        }
        Ok(DeliveryActionOutput::default())
    }
}

fn parse_target_session(payload: &serde_json::Value) -> Option<SessionId> {
    let to = payload.get("to")?;
    // Accept both the tagged Recipient::Agent form `{ "kind": "agent",
    // "session_id": "..." }` and a bare-string `{ "to": "<uuid>" }`
    // shape that some pre-Phase-2 callers might still emit. The latter
    // is best-effort — newer callers always use the tagged form.
    if let Some(kind) = to.get("kind").and_then(|k| k.as_str()) {
        if kind != "agent" {
            return None;
        }
    }
    let id_str = to
        .get("session_id")
        .and_then(|s| s.as_str())
        .or_else(|| to.as_str())?;
    let parsed = uuid::Uuid::parse_str(id_str).ok()?;
    Some(SessionId(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_db::tables::{
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

    fn fresh_session(central: &CentralDb, name: &str) -> ironclaw_types::Session {
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
            })
            .unwrap();
        assert!(out.message.is_none());
    }
}
