//! Inbound-db write helpers for the `create_agent` handler.
//!
//! These methods are split into their own `impl CreateAgentHandler`
//! block because they don't touch the central DB / depth cache that the
//! main handler logic in [`super::create_agent`] manipulates — they
//! exclusively read/write per-session `inbound.db` files via
//! [`ironclaw_db::session::open_inbound`].
//!
//! Rust splits multiple `impl` blocks for the same type across files
//! cleanly, so callers see one logical `CreateAgentHandler`.

use super::create_agent::{CreateAgentHandler, ParentSession, ResultStatus};
use chrono::Utc;
use ironclaw_db::session::SessionPaths;
use ironclaw_db::tables::messages_in::{self, WriteInbound};
use ironclaw_types::{AgentGroupId, MessageId, MessageKind, SessionId};
use tracing::{info, warn};

impl CreateAgentHandler {
    /// Append a `create_agent_result` system row to the parent session's
    /// inbound.db. The runner's `format_messages` will render this into the
    /// next turn's prompt as a `system:` line so the calling agent learns
    /// the real session / agent-group ids.
    pub(super) fn write_parent_result(
        &self,
        parent: Option<&ParentSession>,
        status: ResultStatus,
        session_id: Option<SessionId>,
        agent_group_id: Option<AgentGroupId>,
        detail: Option<&str>,
    ) {
        let Some(parent) = parent else {
            info!(
                ?status,
                "create_agent_result: no parent session resolvable; skipping inbound notice",
            );
            return;
        };
        self.write_inbound_payload(
            parent.agent_group_id,
            parent.session_id,
            MessageKind::System,
            Self::build_result_content(status, session_id, agent_group_id, detail),
            false,
        );
    }

    /// Compose the JSON payload for a `create_agent_result` inbound row.
    pub(super) fn build_result_content(
        status: ResultStatus,
        session_id: Option<SessionId>,
        agent_group_id: Option<AgentGroupId>,
        detail: Option<&str>,
    ) -> serde_json::Value {
        let mut body = serde_json::Map::new();
        body.insert("status".into(), serde_json::json!(status.as_str()));
        if let Some(sid) = session_id {
            body.insert(
                "session_id".into(),
                serde_json::json!(sid.as_uuid().to_string()),
            );
        }
        if let Some(agid) = agent_group_id {
            body.insert(
                "agent_group_id".into(),
                serde_json::json!(agid.as_uuid().to_string()),
            );
        }
        if let Some(d) = detail {
            body.insert("detail".into(), serde_json::json!(d));
        }
        serde_json::json!({ "create_agent_result": body })
    }

    /// Mirror the parent session's `session_routing` record into the
    /// child's `inbound.db`. The delivery service uses this row — not
    /// `sessions.messaging_group_id` — to resolve where outbound chat
    /// messages should be sent; without it the child's first
    /// `send_message` call fails with `NoRoute` and the operator never
    /// sees the reply. Logs on failure; non-fatal (the operator can
    /// hand-write the row later if it really matters).
    pub(super) fn copy_parent_session_routing(
        &self,
        parent_agent_group: AgentGroupId,
        parent_session: SessionId,
        child_agent_group: AgentGroupId,
        child_session: SessionId,
    ) {
        let parent_paths =
            SessionPaths::new(&self.deps.data_root, parent_agent_group, parent_session);
        let parent_conn = match ironclaw_db::session::open_inbound(&parent_paths) {
            Ok(c) => c,
            Err(err) => {
                warn!(?err, "create_agent: open parent inbound for routing copy failed");
                return;
            }
        };
        let routing = match ironclaw_db::tables::session_routing::read(&parent_conn) {
            Ok(Some(r)) => r,
            Ok(None) => {
                warn!(
                    parent_session = %parent_session.as_uuid(),
                    "create_agent: parent has no session_routing — child outbound will fail until one is written",
                );
                return;
            }
            Err(err) => {
                warn!(?err, "create_agent: read parent session_routing failed");
                return;
            }
        };
        let child_paths =
            SessionPaths::new(&self.deps.data_root, child_agent_group, child_session);
        let child_conn = match ironclaw_db::session::open_inbound(&child_paths) {
            Ok(c) => c,
            Err(err) => {
                warn!(?err, "create_agent: open child inbound for routing write failed");
                return;
            }
        };
        if let Err(err) = ironclaw_db::tables::session_routing::write(&child_conn, &routing) {
            warn!(
                ?err,
                child_session = %child_session.as_uuid(),
                "create_agent: write child session_routing failed; outbound will NoRoute",
            );
        }
    }

    /// Seed a newly-spawned child agent's `inbound.db` with its initial
    /// instructions, written as a `kind=Chat` message with `trigger=true`
    /// so the container manager spawns the child on its next reconcile
    /// tick. Without this the child has zero pending inbound, the manager
    /// considers it idle, and it never starts — the `payload.instructions`
    /// is otherwise only stashed in the parent's `create_agent_result`
    /// row and lost from there. Failures are logged but non-fatal: the
    /// `agent_groups` + `sessions` rows are already committed, so the
    /// operator can still drive the child manually via `iclaw chat`.
    pub(super) fn seed_child_inbound(
        &self,
        agent_group_id: AgentGroupId,
        session_id: SessionId,
        name: &str,
        parent_name: Option<&str>,
        instructions: &str,
    ) {
        // Routing is now architectural (see Phase 2 of
        // docs/plans/agent-to-agent-routing.md): the runner's
        // `apply_send_message` checks `source_session_id` (threaded
        // through from `sessions.source_session_id`) and routes the
        // child's default `send_message(to: None)` calls back to the
        // parent's inbound automatically. The kicker no longer has to
        // tell the child about the parent's name — the runtime does
        // the work. We still mention who spawned us so the child has
        // context, but the wire-level routing is no longer the model's
        // problem.
        let prelude = match parent_name {
            Some(p) => format!(
                "You are agent `{name}`, spawned by parent agent `{p}` for \
                 the task below. Work through it autonomously, then call \
                 `send_message` to deliver a single consolidated report. \
                 Your replies route back to the parent by default; the \
                 parent will summarise across siblings (if any) before \
                 surfacing anything to the end user.\n\nTask:\n\n"
            ),
            None => format!(
                "You are agent `{name}`, spawned for the task below. Work \
                 through it autonomously, then call `send_message` to \
                 deliver your findings.\n\nTask:\n\n"
            ),
        };
        let text = format!("{prelude}{instructions}");
        self.write_inbound_payload(
            agent_group_id,
            session_id,
            MessageKind::Chat,
            serde_json::json!({ "text": text }),
            true,
        );
    }

    /// Shared insert helper for the two `messages_in::insert` call sites
    /// in this module (parent-result notify + child-kicker seed). Logs
    /// on failure; never panics.
    pub(super) fn write_inbound_payload(
        &self,
        agent_group_id: AgentGroupId,
        session_id: SessionId,
        kind: MessageKind,
        content: serde_json::Value,
        trigger: bool,
    ) {
        let paths = SessionPaths::new(&self.deps.data_root, agent_group_id, session_id);
        let conn = match ironclaw_db::session::open_inbound(&paths) {
            Ok(c) => c,
            Err(err) => {
                warn!(
                    ?err,
                    session = %session_id.as_uuid(),
                    "create_agent: open_inbound failed; skipping inbound write",
                );
                return;
            }
        };
        let msg = WriteInbound {
            id: MessageId::new(),
            kind,
            timestamp: Utc::now(),
            content,
            trigger,
            on_wake: false,
            process_after: None,
            recurrence: None,
            series_id: None,
            platform_id: None,
            channel_type: None,
            thread_id: None,
            source_session_id: None,
        };
        if let Err(err) = messages_in::insert(&conn, &msg) {
            warn!(
                session = %session_id.as_uuid(),
                ?err,
                "create_agent: messages_in::insert failed",
            );
        }
    }
}
