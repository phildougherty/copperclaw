//! `ToolContext` implementation that writes effects to `outbound.db`.
//!
//! The runner's [`RunnerToolCtx`] implements [`ironclaw_mcp::ToolContext`]
//! against the per-session `outbound.db` plus the session's `outbox/` dir
//! on disk. Each effect maps onto an insertion into `messages_out` (kind
//! `chat` for end-user messages, `system` for everything host-handled).
//!
//! # JSON shapes
//!
//! Host parsers can match on the following structure of system-kind outbound
//! message `content` blobs:
//!
//! ```json
//! { "edit":          { "seq": 7, "text": "..." } }
//! { "reaction":      { "seq": 7, "emoji": "..." } }
//! { "ask_question":  { "id": "q_<uuid>", "title": "...", "options": [...], "to": {...} } }
//! { "card":          { "to": {...}, "card": {...} } }
//! { "create_agent":  { "name": "...", "instructions": "...", "channel": "..." } }
//! { "install_packages": { "apt": [...], "npm": [...], "reason": "..." } }
//! { "add_mcp_server":   { "name": "...", "transport": {...}, "reason": "..." } }
//! { "schedule":      { "op": "create" | "cancel" | "pause" | "resume" | "update", "payload": {...} } }
//! ```
//!
//! `SendFile` writes the bytes to `outbox/<msg_id>/<filename>` and emits a
//! `chat`-kind row whose `content` includes a `files` array pointing at the
//! filename.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use ironclaw_db::attachments::safe_attachment_name;
use ironclaw_db::tables::messages_out::{self, WriteOutbound};
use ironclaw_db::DbError;
use ironclaw_mcp::{
    AddMcpServerSpec, AddReactionSpec, AskUserQuestionSpec, CreateAgentSpec, EditMessageSpec,
    InstallSpec, OutboundToolEffect, ScheduleSpec, SendCardSpec, SendFileSpec, SendMessageSpec,
    TaskSummary, ToolContext, ToolEffectAck, ToolError, UpdateTaskSpec,
};
use ironclaw_types::{MessageId, MessageKind};
use rusqlite::Connection;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Shared, async-safe handle to the runner's `outbound.db` connection.
pub type SharedOutbound = Arc<Mutex<Connection>>;

/// `ToolContext` implementation backed by the per-session `outbound.db` plus
/// the session's `outbox/` directory.
///
/// `list_tasks` always returns an empty `Vec` — task state lives on the host
/// side, so the runner can't enumerate it directly. Schedulers tooling is
/// surfaced to the host via `system`-kind outbound rows (see
/// [`OutboundToolEffect::ScheduleTask`] et al.) and the host writes any
/// resulting state into its own central DB.
pub struct RunnerToolCtx {
    outbound: SharedOutbound,
    outbox_root: PathBuf,
}

impl RunnerToolCtx {
    /// Build a fresh context around the given outbound DB handle and outbox
    /// directory.
    pub fn new(outbound: SharedOutbound, outbox_root: impl Into<PathBuf>) -> Self {
        Self {
            outbound,
            outbox_root: outbox_root.into(),
        }
    }

    /// Snapshot accessor for tests.
    pub fn outbox_root(&self) -> &Path {
        &self.outbox_root
    }
}

#[async_trait]
impl ToolContext for RunnerToolCtx {
    async fn emit_outbound(
        &self,
        effect: OutboundToolEffect,
    ) -> Result<ToolEffectAck, ToolError> {
        let outbox = self.outbox_root.clone();
        // Take the lock for the entire DB write.
        let mut guard = self.outbound.lock().await;
        let conn: &mut Connection = &mut guard;
        let ack = apply_effect(conn, &outbox, effect).map_err(to_tool_error)?;
        Ok(ack)
    }

    async fn list_tasks(&self) -> Result<Vec<TaskSummary>, ToolError> {
        // Task state lives on the host; the runner has no local view of it.
        // The expectation is that callers schedule a `list_tasks` system
        // message and await the host's response via `messages_in`. Until
        // that flow exists we surface an empty list.
        Ok(Vec::new())
    }
}

fn to_tool_error(err: ToolApplyError) -> ToolError {
    match err {
        ToolApplyError::Db(e) => ToolError::Context(e.to_string()),
        ToolApplyError::Io(e) => ToolError::Context(e.to_string()),
        ToolApplyError::Validation(s) => ToolError::Validation(s),
    }
}

/// Internal error type used while applying a tool effect to the outbound DB.
#[derive(Debug)]
enum ToolApplyError {
    Db(DbError),
    Io(std::io::Error),
    Validation(String),
}

impl From<DbError> for ToolApplyError {
    fn from(value: DbError) -> Self {
        Self::Db(value)
    }
}

impl From<std::io::Error> for ToolApplyError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[allow(clippy::needless_pass_by_value)]
fn apply_effect(
    conn: &mut Connection,
    outbox_root: &Path,
    effect: OutboundToolEffect,
) -> Result<ToolEffectAck, ToolApplyError> {
    match effect {
        OutboundToolEffect::SendMessage(spec) => apply_send_message(conn, spec),
        OutboundToolEffect::SendFile(spec) => apply_send_file(conn, outbox_root, spec),
        OutboundToolEffect::EditMessage(spec) => apply_edit_message(conn, spec),
        OutboundToolEffect::AddReaction(spec) => apply_add_reaction(conn, spec),
        OutboundToolEffect::AskUserQuestion(spec) => apply_ask_question(conn, spec),
        OutboundToolEffect::SendCard(spec) => apply_send_card(conn, spec),
        OutboundToolEffect::CreateAgent(spec) => apply_create_agent(conn, spec),
        OutboundToolEffect::InstallPackages(spec) => apply_install_packages(conn, spec),
        OutboundToolEffect::AddMcpServer(spec) => apply_add_mcp_server(conn, spec),
        OutboundToolEffect::ScheduleTask(spec) => apply_schedule_create(conn, spec),
        OutboundToolEffect::ListTasks => apply_schedule_list(conn),
        OutboundToolEffect::CancelTask { id } => apply_schedule_simple(conn, "cancel", &id),
        OutboundToolEffect::PauseTask { id } => apply_schedule_simple(conn, "pause", &id),
        OutboundToolEffect::ResumeTask { id } => apply_schedule_simple(conn, "resume", &id),
        OutboundToolEffect::UpdateTask(spec) => apply_schedule_update(conn, spec),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn apply_send_message(
    conn: &mut Connection,
    spec: SendMessageSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let SendMessageSpec { to, text } = spec;
    let mut body = serde_json::Map::new();
    body.insert("text".into(), serde_json::Value::String(text));
    if let Some(t) = to {
        body.insert("to".into(), serde_json::to_value(t).unwrap_or_default());
    }
    let seq = insert_row(conn, MessageKind::Chat, serde_json::Value::Object(body))?;
    Ok(ToolEffectAck::Message { seq })
}

#[allow(clippy::needless_pass_by_value)]
fn apply_send_file(
    conn: &mut Connection,
    outbox_root: &Path,
    spec: SendFileSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let SendFileSpec {
        to,
        filename,
        data,
        text,
    } = spec;
    safe_attachment_name(&filename)
        .map_err(|e| ToolApplyError::Validation(e.to_string()))?;
    let msg_id = MessageId::new();
    let msg_id_str = msg_id.as_uuid().to_string();
    let target_dir = outbox_root.join(&msg_id_str);
    std::fs::create_dir_all(&target_dir)?;
    let target = target_dir.join(&filename);
    std::fs::write(&target, &data)?;

    let mut body = serde_json::Map::new();
    if let Some(t) = text {
        body.insert("text".into(), serde_json::Value::String(t));
    }
    if let Some(t) = to {
        body.insert("to".into(), serde_json::to_value(t).unwrap_or_default());
    }
    body.insert(
        "files".into(),
        serde_json::json!([{ "filename": filename }]),
    );
    let seq = insert_row_with_id(
        conn,
        msg_id,
        MessageKind::Chat,
        serde_json::Value::Object(body),
    )?;
    Ok(ToolEffectAck::Message { seq })
}

#[allow(clippy::needless_pass_by_value)]
fn apply_edit_message(
    conn: &mut Connection,
    spec: EditMessageSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "edit": { "seq": spec.message_seq, "text": spec.text }
    });
    let seq = insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Message { seq })
}

#[allow(clippy::needless_pass_by_value)]
fn apply_add_reaction(
    conn: &mut Connection,
    spec: AddReactionSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "reaction": { "seq": spec.message_seq, "emoji": spec.emoji }
    });
    let seq = insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Message { seq })
}

#[allow(clippy::needless_pass_by_value)]
fn apply_ask_question(
    conn: &mut Connection,
    spec: AskUserQuestionSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let qid = format!("q_{}", Uuid::new_v4());
    let mut q = serde_json::Map::new();
    q.insert("id".into(), serde_json::Value::String(qid.clone()));
    q.insert("title".into(), serde_json::Value::String(spec.title));
    q.insert(
        "options".into(),
        serde_json::Value::Array(
            spec.options
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
    );
    if let Some(t) = spec.to {
        q.insert("to".into(), serde_json::to_value(t).unwrap_or_default());
    }
    let payload = serde_json::json!({ "ask_question": q });
    insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Question { id: qid })
}

#[allow(clippy::needless_pass_by_value)]
fn apply_send_card(
    conn: &mut Connection,
    spec: SendCardSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let mut body = serde_json::Map::new();
    if let Some(t) = spec.to {
        body.insert("to".into(), serde_json::to_value(t).unwrap_or_default());
    }
    body.insert("card".into(), spec.card);
    let payload = serde_json::json!({ "card": body });
    let seq = insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Message { seq })
}

#[allow(clippy::needless_pass_by_value)]
fn apply_create_agent(
    conn: &mut Connection,
    spec: CreateAgentSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "create_agent": {
            "name": spec.name,
            "instructions": spec.instructions,
            "channel": spec.channel,
        }
    });
    insert_row(conn, MessageKind::System, payload)?;
    // The host assigns the session id; we surface a synthetic placeholder so
    // the calling agent knows the request was queued.
    Ok(ToolEffectAck::Accepted)
}

#[allow(clippy::needless_pass_by_value)]
fn apply_install_packages(
    conn: &mut Connection,
    spec: InstallSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "install_packages": {
            "apt": spec.apt,
            "npm": spec.npm,
            "reason": spec.reason,
        }
    });
    insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Accepted)
}

#[allow(clippy::needless_pass_by_value)]
fn apply_add_mcp_server(
    conn: &mut Connection,
    spec: AddMcpServerSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "add_mcp_server": {
            "name": spec.name,
            "transport": spec.transport,
            "reason": spec.reason,
        }
    });
    insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Accepted)
}

#[allow(clippy::needless_pass_by_value)]
fn apply_schedule_create(
    conn: &mut Connection,
    spec: ScheduleSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "schedule": {
            "op": "create",
            "payload": {
                "name": spec.name,
                "when": spec.when,
                "prompt": spec.prompt,
                "recurrence": spec.recurrence,
            }
        }
    });
    insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Accepted)
}

fn apply_schedule_list(conn: &mut Connection) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "schedule": { "op": "list", "payload": {} }
    });
    insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Accepted)
}

fn apply_schedule_simple(
    conn: &mut Connection,
    op: &str,
    id: &str,
) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "schedule": { "op": op, "payload": { "id": id } }
    });
    insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Task { id: id.to_string() })
}

#[allow(clippy::needless_pass_by_value)]
fn apply_schedule_update(
    conn: &mut Connection,
    spec: UpdateTaskSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "schedule": {
            "op": "update",
            "payload": {
                "id": spec.id,
                "prompt": spec.prompt,
                "when": spec.when,
                "recurrence": spec.recurrence,
            }
        }
    });
    let id_for_ack = spec.id.clone();
    insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Task { id: id_for_ack })
}

fn insert_row(
    conn: &mut Connection,
    kind: MessageKind,
    content: serde_json::Value,
) -> Result<i64, ToolApplyError> {
    let id = MessageId::new();
    insert_row_with_id(conn, id, kind, content)
}

fn insert_row_with_id(
    conn: &mut Connection,
    id: MessageId,
    kind: MessageKind,
    content: serde_json::Value,
) -> Result<i64, ToolApplyError> {
    let row = WriteOutbound {
        id,
        in_reply_to: None,
        timestamp: Utc::now(),
        deliver_after: None,
        recurrence: None,
        kind,
        platform_id: None,
        channel_type: None,
        thread_id: None,
        content,
    };
    let seq = messages_out::insert(conn, &row)?;
    Ok(seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_db::session::{open_outbound, SessionPaths};
    use ironclaw_mcp::Recipient;
    use ironclaw_types::{AgentGroupId, SessionId};

    fn fresh_ctx() -> (tempfile::TempDir, RunnerToolCtx) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone());
        (tmp, ctx)
    }

    async fn last_row(ctx: &RunnerToolCtx) -> ironclaw_types::MessageOutRow {
        let guard = ctx.outbound.lock().await;
        let rows = ironclaw_db::tables::messages_out::list_due(&guard).unwrap();
        rows.into_iter().next_back().unwrap()
    }

    #[tokio::test]
    async fn send_message_writes_chat_row() {
        let (_tmp, ctx) = fresh_ctx();
        let ack = ctx
            .emit_outbound(OutboundToolEffect::SendMessage(SendMessageSpec {
                to: Some(Recipient::Channel {
                    id: "telegram:chat-1".into(),
                }),
                text: "hello".into(),
            }))
            .await
            .unwrap();
        let ToolEffectAck::Message { seq } = ack else {
            panic!("unexpected ack")
        };
        assert!(seq > 0 && seq % 2 == 1);
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::Chat);
        assert_eq!(row.content["text"], "hello");
        assert!(row.content.get("to").is_some());
    }

    #[tokio::test]
    async fn send_file_writes_to_disk_and_row() {
        let (_tmp, ctx) = fresh_ctx();
        ctx.emit_outbound(OutboundToolEffect::SendFile(SendFileSpec {
            to: None,
            filename: "doc.txt".into(),
            data: b"data!".to_vec(),
            text: Some("see attached".into()),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::Chat);
        let files = row.content.get("files").and_then(|v| v.as_array()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["filename"], "doc.txt");
        // Bytes are on disk under the outbox at the row's id.
        let id_str = row.id.as_uuid().to_string();
        let file_path = ctx.outbox_root().join(&id_str).join("doc.txt");
        let bytes = std::fs::read(&file_path).unwrap();
        assert_eq!(bytes, b"data!");
    }

    #[tokio::test]
    async fn send_file_rejects_dangerous_filename() {
        let (_tmp, ctx) = fresh_ctx();
        let err = ctx
            .emit_outbound(OutboundToolEffect::SendFile(SendFileSpec {
                to: None,
                filename: "../escape.txt".into(),
                data: b"x".to_vec(),
                text: None,
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn edit_message_writes_system_row() {
        let (_tmp, ctx) = fresh_ctx();
        ctx.emit_outbound(OutboundToolEffect::EditMessage(EditMessageSpec {
            message_seq: 7,
            text: "edited".into(),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::System);
        assert_eq!(row.content["edit"]["seq"], 7);
        assert_eq!(row.content["edit"]["text"], "edited");
    }

    #[tokio::test]
    async fn add_reaction_writes_system_row() {
        let (_tmp, ctx) = fresh_ctx();
        ctx.emit_outbound(OutboundToolEffect::AddReaction(AddReactionSpec {
            message_seq: 7,
            emoji: "thumbsup".into(),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::System);
        assert_eq!(row.content["reaction"]["emoji"], "thumbsup");
    }

    #[tokio::test]
    async fn ask_question_returns_question_ack() {
        let (_tmp, ctx) = fresh_ctx();
        let ack = ctx
            .emit_outbound(OutboundToolEffect::AskUserQuestion(AskUserQuestionSpec {
                title: "ok?".into(),
                options: vec!["yes".into(), "no".into()],
                to: None,
            }))
            .await
            .unwrap();
        let qid = match ack {
            ToolEffectAck::Question { id } => id,
            other => panic!("unexpected: {other:?}"),
        };
        assert!(qid.starts_with("q_"));
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::System);
        assert_eq!(row.content["ask_question"]["id"], qid);
        assert_eq!(row.content["ask_question"]["title"], "ok?");
    }

    #[tokio::test]
    async fn send_card_writes_system_row() {
        let (_tmp, ctx) = fresh_ctx();
        ctx.emit_outbound(OutboundToolEffect::SendCard(SendCardSpec {
            to: None,
            card: serde_json::json!({"hi": 1}),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::System);
        assert_eq!(row.content["card"]["card"]["hi"], 1);
    }

    #[tokio::test]
    async fn create_agent_writes_system_row_and_accepts() {
        let (_tmp, ctx) = fresh_ctx();
        let ack = ctx
            .emit_outbound(OutboundToolEffect::CreateAgent(CreateAgentSpec {
                name: "n".into(),
                instructions: "i".into(),
                channel: Some("cli".into()),
            }))
            .await
            .unwrap();
        assert_eq!(ack, ToolEffectAck::Accepted);
        let row = last_row(&ctx).await;
        assert_eq!(row.content["create_agent"]["name"], "n");
        assert_eq!(row.content["create_agent"]["channel"], "cli");
    }

    #[tokio::test]
    async fn install_packages_writes_system_row() {
        let (_tmp, ctx) = fresh_ctx();
        let ack = ctx
            .emit_outbound(OutboundToolEffect::InstallPackages(InstallSpec {
                apt: vec!["jq".into()],
                npm: vec!["zod".into()],
                reason: "needed for x".into(),
            }))
            .await
            .unwrap();
        assert_eq!(ack, ToolEffectAck::Accepted);
        let row = last_row(&ctx).await;
        assert_eq!(row.content["install_packages"]["apt"], serde_json::json!(["jq"]));
        assert_eq!(
            row.content["install_packages"]["npm"],
            serde_json::json!(["zod"])
        );
    }

    #[tokio::test]
    async fn add_mcp_server_writes_system_row() {
        let (_tmp, ctx) = fresh_ctx();
        ctx.emit_outbound(OutboundToolEffect::AddMcpServer(AddMcpServerSpec {
            name: "n".into(),
            transport: serde_json::json!({"kind": "stdio"}),
            reason: "r".into(),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(row.content["add_mcp_server"]["name"], "n");
        assert_eq!(row.content["add_mcp_server"]["transport"]["kind"], "stdio");
    }

    #[tokio::test]
    async fn schedule_create_writes_system_row() {
        let (_tmp, ctx) = fresh_ctx();
        let ack = ctx
            .emit_outbound(OutboundToolEffect::ScheduleTask(ScheduleSpec {
                name: "t".into(),
                when: None,
                prompt: "p".into(),
                recurrence: Some("0 * * * *".into()),
            }))
            .await
            .unwrap();
        assert_eq!(ack, ToolEffectAck::Accepted);
        let row = last_row(&ctx).await;
        assert_eq!(row.content["schedule"]["op"], "create");
        assert_eq!(row.content["schedule"]["payload"]["name"], "t");
    }

    #[tokio::test]
    async fn schedule_list_writes_system_row() {
        let (_tmp, ctx) = fresh_ctx();
        let ack = ctx
            .emit_outbound(OutboundToolEffect::ListTasks)
            .await
            .unwrap();
        assert_eq!(ack, ToolEffectAck::Accepted);
        let row = last_row(&ctx).await;
        assert_eq!(row.content["schedule"]["op"], "list");
    }

    #[tokio::test]
    async fn schedule_cancel_pause_resume_each_write_rows() {
        let (_tmp, ctx) = fresh_ctx();
        for (effect, op) in [
            (
                OutboundToolEffect::CancelTask {
                    id: "task_1".into(),
                },
                "cancel",
            ),
            (
                OutboundToolEffect::PauseTask {
                    id: "task_2".into(),
                },
                "pause",
            ),
            (
                OutboundToolEffect::ResumeTask {
                    id: "task_3".into(),
                },
                "resume",
            ),
        ] {
            let ack = ctx.emit_outbound(effect).await.unwrap();
            assert!(matches!(ack, ToolEffectAck::Task { .. }));
            let row = last_row(&ctx).await;
            assert_eq!(row.content["schedule"]["op"], op);
        }
    }

    #[tokio::test]
    async fn schedule_update_writes_system_row() {
        let (_tmp, ctx) = fresh_ctx();
        let ack = ctx
            .emit_outbound(OutboundToolEffect::UpdateTask(UpdateTaskSpec {
                id: "task_9".into(),
                prompt: Some("new prompt".into()),
                when: Some(None),
                recurrence: Some(Some("@daily".into())),
            }))
            .await
            .unwrap();
        match ack {
            ToolEffectAck::Task { id } => assert_eq!(id, "task_9"),
            other => panic!("unexpected: {other:?}"),
        }
        let row = last_row(&ctx).await;
        assert_eq!(row.content["schedule"]["op"], "update");
        assert_eq!(row.content["schedule"]["payload"]["prompt"], "new prompt");
    }

    #[tokio::test]
    async fn list_tasks_returns_empty_vec() {
        let (_tmp, ctx) = fresh_ctx();
        let tasks = ctx.list_tasks().await.unwrap();
        assert!(tasks.is_empty());
    }
}
