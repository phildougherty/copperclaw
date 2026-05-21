//! Runner poll loop. Module name is `run` because `loop` is a reserved
//! keyword in Rust 2024.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use ironclaw_db::tables::{
    container_state, messages_in, processing_ack,
};
use ironclaw_mcp::{ToolContext, ToolEntry};
use ironclaw_providers::{AgentProvider, HistoryMessage, QueryInput, ToolDef};
#[cfg_attr(not(test), allow(unused_imports))]
use ironclaw_types::MessageId;
use ironclaw_types::{Effort, MessageInRow, ProviderEvent};
use std::collections::HashMap;
use rusqlite::Connection;
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::compaction::{compact, estimate_tokens, CompactionCfg};
use crate::disallowed::is_disallowed;
use crate::formatter::format_messages;
use crate::state::{load_state, save_state};

/// Default poll interval (ms) while the loop is idle.
pub const POLL_INTERVAL_MS: u64 = 1000;
/// Active poll interval (ms) while messages are still flowing.
pub const ACTIVE_POLL_INTERVAL_MS: u64 = 500;

/// Dependencies injected into [`run_loop`]. Holding all of these in a struct
/// keeps the signature small and makes it easy to fan out variations from
/// tests.
pub struct RunnerDeps {
    /// Provider handle (Anthropic, Codex, …).
    pub provider: Arc<dyn AgentProvider>,
    /// Tool context the provider calls into.
    pub tool_ctx: Arc<dyn ToolContext>,
    /// `inbound.db` connection (host-written, read-only).
    pub inbound: Arc<Mutex<Connection>>,
    /// `outbound.db` connection (container-written).
    pub outbound: Arc<Mutex<Connection>>,
    /// Tools advertised to the model. Empty list means "no tools".
    pub tools: Vec<ToolDef>,
    /// System prompt to send on every turn.
    pub system: String,
    /// Model identifier.
    pub model: String,
    /// Effort hint.
    pub effort: Effort,
    /// Max tokens per turn.
    pub max_tokens: u32,
    /// Temperature, if any.
    pub temperature: Option<f32>,
    /// Display name of the assistant.
    pub assistant_name: Option<String>,
    /// Compaction configuration.
    pub compaction: CompactionCfg,
    /// How many turns to run before exiting cleanly. `None` means loop forever.
    pub max_turns: Option<usize>,
    /// Per-iteration sleep when the inbox is empty.
    pub idle_sleep: Duration,
    /// Heartbeat path the runner touches once per iteration (None to skip).
    pub heartbeat_path: Option<PathBuf>,
    /// Session id this runner is bound to. Stamped on every
    /// `usage_report` system row so the host can join to
    /// `agent_turns.session_id`.
    pub session_id: ironclaw_types::SessionId,
    /// Agent group id. Same use as `session_id`.
    pub agent_group_id: ironclaw_types::AgentGroupId,
    /// Runner-local turn counter. Bumped by the `usage_report` emitter
    /// after each turn so `agent_turns.seq` is monotonically
    /// increasing per session.
    pub turn_seq: Arc<std::sync::atomic::AtomicI64>,
    /// Per-tool dispatch map. Built once at runner startup from
    /// `ironclaw_mcp::build_tool_map()`. Keyed by the tool name the
    /// model emits in `tool_use` blocks; each entry knows how to
    /// validate the input and invoke the handler against the
    /// runner's `ToolContext`.
    pub tool_map: Arc<HashMap<String, Arc<ToolEntry>>>,
    /// Hard cap on consecutive tool-use turns per inbound. Stops a
    /// confused model from looping forever. Default 20.
    pub max_tool_turns: usize,
}

impl RunnerDeps {
    /// Convenience builder used by tests. Production callers populate fields
    /// directly.
    #[must_use]
    pub fn minimal(
        provider: Arc<dyn AgentProvider>,
        tool_ctx: Arc<dyn ToolContext>,
        inbound: Arc<Mutex<Connection>>,
        outbound: Arc<Mutex<Connection>>,
        archive_dir: PathBuf,
    ) -> Self {
        Self {
            provider,
            tool_ctx,
            inbound,
            outbound,
            tools: Vec::new(),
            system: "you are helpful".into(),
            model: "claude-sonnet-4-6".into(),
            effort: Effort::Medium,
            max_tokens: 4096,
            temperature: None,
            assistant_name: None,
            session_id: ironclaw_types::SessionId(uuid::Uuid::nil()),
            agent_group_id: ironclaw_types::AgentGroupId(uuid::Uuid::nil()),
            turn_seq: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            tool_map: Arc::new(HashMap::new()),
            max_tool_turns: 20,
            compaction: CompactionCfg {
                model_input_window: 200_000,
                safety_margin_tokens: 8_000,
                summary_model: "claude-sonnet-4-6".into(),
                summary_effort: Effort::Low,
                summary_max_tokens: 1024,
                archive_dir,
            },
            max_turns: None,
            idle_sleep: Duration::from_millis(POLL_INTERVAL_MS),
            heartbeat_path: None,
        }
    }
}

/// Drive the poll loop until `max_turns` turns have been executed (or
/// forever, if `max_turns` is `None`). The function is `async` and may be
/// awaited from any tokio runtime.
pub async fn run_loop(deps: RunnerDeps) -> Result<()> {
    // Bring the persisted message history into memory once at startup.
    let mut state = {
        let g = deps.outbound.lock().await;
        load_state(&g).context("load runner state")?
    };
    let mut turns_run: usize = 0;
    let mut first_poll = true;

    loop {
        if let Some(limit) = deps.max_turns {
            if turns_run >= limit {
                return Ok(());
            }
        }
        touch_heartbeat(deps.heartbeat_path.as_ref());

        let pending = {
            let g = deps.inbound.lock().await;
            messages_in::get_pending(&g, first_poll, 10)?
        };
        first_poll = false;

        if pending.is_empty() {
            sleep(deps.idle_sleep).await;
            continue;
        }

        ack_picked_up(&deps, &pending).await?;
        let formatted = format_messages(pending);

        state
            .history
            .push(HistoryMessage::User { content: formatted.prompt });

        if estimate_tokens(&state.history)
            > deps
                .compaction
                .model_input_window
                .saturating_sub(deps.compaction.safety_margin_tokens)
        {
            state.history = compact(state.history, deps.provider.as_ref(), &deps.compaction)
                .await
                .context("compaction failed")?;
        }

        let turn = drive_turn(&deps, &mut state.history, state.continuation.as_deref()).await?;
        state.continuation = turn.continuation.or(state.continuation);

        finalize_messages(&deps, &formatted.rows, turn.outcome).await?;

        {
            let g = deps.outbound.lock().await;
            save_state(&g, &state.history, state.continuation.as_deref())
                .context("save runner state")?;
        }
        turns_run += 1;
        // Active path: poll faster when traffic is flowing.
        sleep(Duration::from_millis(ACTIVE_POLL_INTERVAL_MS)).await;
    }
}

#[derive(Debug, Clone)]
struct TurnResult {
    continuation: Option<String>,
    outcome: TurnOutcome,
}

#[derive(Debug, Clone)]
enum TurnOutcome {
    /// Model produced a final response.
    Done,
    /// Provider returned an error event.
    Failed,
}

/// One pending tool call extracted from a streamed turn.
#[derive(Debug, Clone)]
struct PendingToolCall {
    id: String,
    name: String,
    input: serde_json::Value,
}

/// What one LLM round-trip produced.
#[derive(Debug, Clone, Default)]
struct LlmTurnOutput {
    continuation: Option<String>,
    /// Final assistant text accumulated during the stream. May be
    /// empty when the model produced only `tool_use` blocks.
    text: String,
    /// Tool calls the model requested. When non-empty the caller
    /// must execute them and run another LLM turn before treating
    /// the message as answered.
    tool_calls: Vec<PendingToolCall>,
    /// True if the provider emitted a terminal Error event.
    failed: bool,
}

/// Drive one inbound through to a final assistant response. Loops
/// LLM-turn → execute-tools → LLM-turn until the model produces a
/// turn with no `tool_use` blocks (or we hit `max_tool_turns`).
async fn drive_turn(
    deps: &RunnerDeps,
    history: &mut Vec<HistoryMessage>,
    previous_continuation: Option<&str>,
) -> Result<TurnResult> {
    let mut continuation: Option<String> = previous_continuation.map(str::to_string);

    for tool_turn in 0..deps.max_tool_turns.max(1) {
        let output = run_llm_turn(deps, history, continuation.as_deref()).await?;
        continuation = output.continuation.or(continuation);

        if output.failed {
            return Ok(TurnResult {
                continuation,
                outcome: TurnOutcome::Failed,
            });
        }

        // Append the model's assistant turn (text + tool_use blocks)
        // to history before deciding what to do next. Anthropic's
        // serializer coalesces consecutive same-role entries, so
        // Assistant{text} + ToolUse{...} round-trip as one
        // multi-block assistant message.
        if !output.text.is_empty() {
            history.push(HistoryMessage::Assistant {
                content: output.text.clone(),
            });
        }
        for call in &output.tool_calls {
            history.push(HistoryMessage::ToolUse {
                id: call.id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
            });
        }

        // No tools requested → this is the final answer for the
        // inbound. Surface the text to the channel and return.
        if output.tool_calls.is_empty() {
            if !output.text.is_empty() {
                let spec = ironclaw_mcp::SendMessageSpec {
                    to: None,
                    text: output.text,
                };
                let _ack = deps
                    .tool_ctx
                    .emit_outbound(ironclaw_mcp::OutboundToolEffect::SendMessage(spec))
                    .await
                    .map_err(|e| anyhow::anyhow!("send_message failed: {e}"))?;
            }
            return Ok(TurnResult {
                continuation,
                outcome: TurnOutcome::Done,
            });
        }

        // Tools requested → execute each, push the result as a
        // user-role tool_result history entry, and loop into
        // another LLM turn.
        tracing::info!(
            tool_turn,
            n = output.tool_calls.len(),
            "executing tool calls"
        );
        for call in &output.tool_calls {
            let (content, is_error) = invoke_tool(deps, call).await;
            history.push(HistoryMessage::Tool {
                tool_use_id: call.id.clone(),
                content,
                is_error,
            });
        }
    }

    // Exhausted the cap. Push a synthetic system message so the
    // model can see what happened on the next inbound, return
    // Failed so finalize_messages marks the inbound that way too.
    tracing::warn!(
        max = deps.max_tool_turns,
        "tool-use cycle exceeded max turns; bailing"
    );
    Ok(TurnResult {
        continuation,
        outcome: TurnOutcome::Failed,
    })
}

/// Make one LLM call. Pumps the streamed provider events into
/// per-turn buffers and returns when the stream ends.
async fn run_llm_turn(
    deps: &RunnerDeps,
    history: &[HistoryMessage],
    previous_continuation: Option<&str>,
) -> Result<LlmTurnOutput> {
    let input = QueryInput {
        system: deps.system.clone(),
        model: deps.model.clone(),
        effort: deps.effort,
        previous_continuation: previous_continuation.map(str::to_string),
        history: history.to_vec(),
        tools: deps.tools.clone(),
        max_tokens: deps.max_tokens,
        temperature: deps.temperature,
        assistant_name: deps.assistant_name.clone(),
        display_name: None,
    };
    let turn_started_at = chrono::Utc::now();
    let mut query = deps
        .provider
        .query(input)
        .await
        .context("provider query failed")?;

    let mut out = LlmTurnOutput::default();
    let mut input_tokens: u32 = 0;
    let mut output_tokens: u32 = 0;

    while let Some(event) = query.next_event().await {
        match event {
            ProviderEvent::Init { continuation: c } => {
                out.continuation = Some(c);
            }
            ProviderEvent::Usage {
                input_tokens: it,
                output_tokens: ot,
            } => {
                if it > 0 {
                    input_tokens = it;
                }
                if ot > 0 {
                    output_tokens = ot;
                }
            }
            ProviderEvent::Result { text } => {
                if let Some(t) = text {
                    out.text = t;
                }
                break;
            }
            ProviderEvent::Error { message, .. } => {
                tracing::warn!(error = %message, "provider returned an error event");
                out.failed = true;
                break;
            }
            ProviderEvent::ToolStart {
                name,
                declared_timeout_ms,
            } => {
                set_current_tool(deps, &name, declared_timeout_ms).await?;
            }
            ProviderEvent::ToolCall { id, name, input } => {
                if is_disallowed(&name) {
                    // Synthesise a refusal result inline; the model
                    // sees it on the next turn via the tool_result.
                    out.tool_calls.push(PendingToolCall {
                        id,
                        name: name.clone(),
                        input,
                    });
                    out.tool_calls.last_mut().unwrap();
                    // Mark the tool as disallowed so invoke_tool can
                    // produce the refusal text without re-checking
                    // here. The actual refusal happens at
                    // invoke_tool below.
                } else {
                    out.tool_calls.push(PendingToolCall { id, name, input });
                }
            }
            ProviderEvent::ToolEnd => {
                clear_current_tool(deps).await?;
            }
            ProviderEvent::Progress { message } => {
                tracing::debug!(message = %message, "provider progress");
                touch_heartbeat(deps.heartbeat_path.as_ref());
            }
            ProviderEvent::Activity => {
                touch_heartbeat(deps.heartbeat_path.as_ref());
            }
        }
    }
    query.abort().await;
    let outcome = if out.failed {
        TurnOutcome::Failed
    } else {
        TurnOutcome::Done
    };

    // Emit Prometheus metrics for this LLM call.
    let elapsed_ms = (chrono::Utc::now() - turn_started_at)
        .num_milliseconds()
        .max(0);
    // i64 -> f64 loses precision for large values (> 2^53 ms = ~285 years);
    // acceptable here since we're measuring LLM call durations in seconds.
    #[allow(clippy::cast_precision_loss)]
    let elapsed_secs = elapsed_ms as f64 / 1000.0;
    ironclaw_metrics::observe_llm_call_seconds(elapsed_secs.max(0.0));
    if input_tokens > 0 {
        ironclaw_metrics::observe_llm_tokens_input(input_tokens);
    }
    if output_tokens > 0 {
        ironclaw_metrics::observe_llm_tokens_output(output_tokens);
    }

    emit_usage_report(deps, input_tokens, output_tokens, turn_started_at, &outcome).await;
    Ok(out)
}

/// Execute one tool call against the runner's tool map. Returns the
/// `(content, is_error)` pair for the `HistoryMessage::Tool` row the
/// model sees on the next turn.
async fn invoke_tool(
    deps: &RunnerDeps,
    call: &PendingToolCall,
) -> (String, bool) {
    if is_disallowed(&call.name) {
        return (
            format!(
                "Tool `{}` is disallowed inside the ironclaw container.",
                call.name
            ),
            true,
        );
    }
    let Some(entry) = deps.tool_map.get(&call.name) else {
        return (
            format!("Unknown tool `{}` — no handler registered.", call.name),
            true,
        );
    };
    // ToolHandler::call wants `Option<JsonObject>`; convert from the
    // Value we got off the wire.
    let arguments = match &call.input {
        serde_json::Value::Object(map) => Some(map.clone()),
        serde_json::Value::Null => None,
        _ => {
            return (
                format!(
                    "Tool `{}` input must be a JSON object, got {}",
                    call.name,
                    short_type(&call.input)
                ),
                true,
            );
        }
    };
    match entry.handler.call(arguments, deps.tool_ctx.as_ref()).await {
        Ok(result) => (render_tool_result(&result), false),
        Err(err) => (format!("Tool `{}` failed: {err}", call.name), true),
    }
}

/// Pluck the textual content out of a `CallToolResult`. Multiple
/// blocks get joined with double newlines; non-text blocks
/// (resources, images) are rendered as their type tag so the model
/// at least sees they happened.
fn render_tool_result(result: &rmcp::model::CallToolResult) -> String {
    let mut out = String::new();
    for block in &result.content {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let raw = &block.raw;
        match raw {
            rmcp::model::RawContent::Text(t) => out.push_str(&t.text),
            rmcp::model::RawContent::Image(_) => out.push_str("<image>"),
            rmcp::model::RawContent::Audio(_) => out.push_str("<audio>"),
            rmcp::model::RawContent::Resource(_) => out.push_str("<resource>"),
        }
    }
    if out.is_empty() {
        "(tool produced no output)".to_string()
    } else {
        out
    }
}

fn short_type(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Append a `usage_report` system row to `outbound.db`. The host's
/// delivery service intercepts this kind of system action (instead of
/// dispatching it to a channel adapter) and writes the corresponding
/// `agent_turns` row.
async fn emit_usage_report(
    deps: &RunnerDeps,
    input_tokens: u32,
    output_tokens: u32,
    started_at: chrono::DateTime<chrono::Utc>,
    outcome: &TurnOutcome,
) {
    use ironclaw_db::tables::messages_out::{insert as insert_out, WriteOutbound};
    let payload = serde_json::json!({
        "usage_report": {
            "session_id": deps.session_id.to_string(),
            "agent_group_id": deps.agent_group_id.to_string(),
            "seq": deps.turn_seq.load(std::sync::atomic::Ordering::Relaxed),
            "model": deps.model,
            "provider": deps.provider.name(),
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "started_at": started_at.to_rfc3339(),
            "ended_at": chrono::Utc::now().to_rfc3339(),
            "status": match outcome {
                TurnOutcome::Done => "ok",
                TurnOutcome::Failed => "error",
            },
        }
    });
    let row = WriteOutbound {
        id: ironclaw_types::MessageId::new(),
        in_reply_to: None,
        timestamp: chrono::Utc::now(),
        deliver_after: None,
        recurrence: None,
        kind: ironclaw_types::MessageKind::System,
        platform_id: None,
        channel_type: None,
        thread_id: None,
        content: payload,
    };
    let outbound = deps.outbound.lock().await;
    let conn: &rusqlite::Connection = &outbound;
    if let Err(err) = insert_out(conn, &row) {
        tracing::warn!(?err, "usage_report insert failed");
    }
    deps.turn_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

async fn ack_picked_up(deps: &RunnerDeps, rows: &[MessageInRow]) -> Result<()> {
    let mut g = deps.outbound.lock().await;
    let conn: &mut Connection = &mut g;
    for row in rows {
        // `insert` errors on duplicate; tolerate retries by switching to update.
        match processing_ack::insert(conn, row.id, processing_ack::ProcessingStatus::Processing) {
            Ok(()) => {}
            Err(ironclaw_db::DbError::Sqlite(_)) => {
                processing_ack::update_status(
                    conn,
                    row.id,
                    processing_ack::ProcessingStatus::Processing,
                )?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

async fn finalize_messages(
    deps: &RunnerDeps,
    rows: &[MessageInRow],
    outcome: TurnOutcome,
) -> Result<()> {
    let (in_status, ack_status) = match outcome {
        TurnOutcome::Done => ("completed", processing_ack::ProcessingStatus::Done),
        TurnOutcome::Failed => ("failed", processing_ack::ProcessingStatus::Failed),
    };
    let _ = in_status;
    {
        let inbound = deps.inbound.lock().await;
        for row in rows {
            match outcome {
                TurnOutcome::Done => {
                    let _ = messages_in::mark_completed(&inbound, row.id);
                }
                TurnOutcome::Failed => {
                    let _ = messages_in::mark_failed(&inbound, row.id);
                }
            }
        }
    }
    {
        let mut outbound = deps.outbound.lock().await;
        let conn: &mut Connection = &mut outbound;
        for row in rows {
            processing_ack::update_status(conn, row.id, ack_status)?;
        }
    }
    Ok(())
}

async fn set_current_tool(
    deps: &RunnerDeps,
    name: &str,
    declared_timeout_ms: Option<u64>,
) -> Result<()> {
    let g = deps.outbound.lock().await;
    let timeout_i64 = declared_timeout_ms.and_then(|v| i64::try_from(v).ok());
    let state = container_state::ContainerState {
        current_tool: Some(name.to_string()),
        tool_declared_timeout_ms: timeout_i64,
        tool_started_at: Some(Utc::now()),
        updated_at: Some(Utc::now()),
    };
    container_state::set(&g, &state)?;
    Ok(())
}

async fn clear_current_tool(deps: &RunnerDeps) -> Result<()> {
    let g = deps.outbound.lock().await;
    container_state::clear_tool(&g)?;
    Ok(())
}

/// Refresh the heartbeat file's mtime so the host's container
/// manager knows the runner is alive. Just opening the file is *not*
/// enough — Linux only updates mtime on actual writes, so the file
/// would look frozen at first-create. Truncate to 0 then write one
/// byte; that's the minimum change that bumps mtime portably.
fn touch_heartbeat(path: Option<&PathBuf>) {
    use std::io::Write;
    if let Some(p) = path {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(p)
        {
            let _ = f.write_all(b".");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::RunnerToolCtx;
    use async_trait::async_trait;
    use ironclaw_db::session::{open_inbound, open_outbound, SessionPaths};
    use ironclaw_db::tables::messages_in::{insert as insert_in, WriteInbound};
    use ironclaw_db::tables::messages_out;
    use ironclaw_providers::{AgentProvider, AgentQuery, ProviderError};
    use ironclaw_types::{AgentGroupId, ChannelType, MessageKind, SessionId};
    use std::sync::Mutex as StdMutex;

    /// Provider that yields a pre-baked sequence of events for each turn.
    struct ScriptedProvider {
        scripts: StdMutex<Vec<Vec<ProviderEvent>>>,
    }

    impl ScriptedProvider {
        fn new(scripts: Vec<Vec<ProviderEvent>>) -> Arc<Self> {
            Arc::new(Self {
                scripts: StdMutex::new(scripts),
            })
        }
    }

    #[async_trait]
    impl AgentProvider for ScriptedProvider {
        fn name(&self) -> &'static str {
            "scripted"
        }
        async fn query(
            &self,
            _input: QueryInput,
        ) -> Result<Box<dyn AgentQuery>, ProviderError> {
            let mut g = self.scripts.lock().unwrap();
            let events = if g.is_empty() {
                vec![ProviderEvent::Result { text: None }]
            } else {
                g.remove(0)
            };
            Ok(Box::new(ScriptedQuery {
                events: StdMutex::new(events),
            }))
        }
        fn is_session_invalid(&self, _err: &ProviderError) -> bool {
            false
        }
    }

    struct ScriptedQuery {
        events: StdMutex<Vec<ProviderEvent>>,
    }

    #[async_trait]
    impl AgentQuery for ScriptedQuery {
        async fn push(&mut self, _: String) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<ProviderEvent> {
            let mut g = self.events.lock().unwrap();
            if g.is_empty() {
                None
            } else {
                Some(g.remove(0))
            }
        }
        async fn abort(&mut self) {}
    }

    struct Setup {
        _tmp: tempfile::TempDir,
        paths: SessionPaths,
        deps: RunnerDeps,
    }

    fn build_setup(scripts: Vec<Vec<ProviderEvent>>) -> Setup {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let inbound = open_inbound(&paths).unwrap();
        let outbound = open_outbound(&paths).unwrap();
        let inbound = Arc::new(Mutex::new(inbound));
        let outbound = Arc::new(Mutex::new(outbound));
        let provider = ScriptedProvider::new(scripts);
        let tool_ctx: Arc<dyn ToolContext> =
            Arc::new(RunnerToolCtx::new(outbound.clone(), paths.outbox.clone()));
        let archive_dir = paths.outbox.join("_compactions");
        let mut deps = RunnerDeps::minimal(provider, tool_ctx, inbound, outbound, archive_dir);
        deps.max_turns = Some(1);
        deps.idle_sleep = Duration::from_millis(1);
        Setup {
            _tmp: tmp,
            paths,
            deps,
        }
    }

    fn insert_pending(inbound: &Connection, text: &str) -> MessageId {
        let id = MessageId::new();
        let msg = WriteInbound {
            id,
            kind: MessageKind::Chat,
            timestamp: Utc::now(),
            content: serde_json::json!({"text": text}),
            trigger: true,
            on_wake: false,
            process_after: None,
            recurrence: None,
            series_id: None,
            platform_id: Some("chat-1".into()),
            channel_type: Some(ChannelType::new("cli")),
            thread_id: None,
            source_session_id: None,
        };
        insert_in(inbound, &msg).unwrap();
        id
    }

    #[tokio::test]
    async fn empty_inbox_exits_when_max_turns_zero() {
        let mut setup = build_setup(vec![vec![ProviderEvent::Result {
            text: Some("ignored".into()),
        }]]);
        setup.deps.max_turns = Some(0);
        run_loop(setup.deps).await.unwrap();
    }

    #[tokio::test]
    async fn one_message_writes_response_and_completes() {
        let mut setup = build_setup(vec![vec![
            ProviderEvent::Init {
                continuation: "c1".into(),
            },
            ProviderEvent::Result {
                text: Some("hello back".into()),
            },
        ]]);
        let id = {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "hi")
        };
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        // Outbound row landed. After M13 the runner also writes a
        // `MessageKind::System` `usage_report` row per turn, so we
        // pick the chat row explicitly rather than asserting on
        // `.last()`.
        let outbound = open_outbound(&setup.paths).unwrap();
        let rows = messages_out::list_due(&outbound).unwrap();
        let chat = rows
            .iter()
            .find(|r| r.kind == ironclaw_types::MessageKind::Chat)
            .expect("expected one Chat outbound row");
        assert_eq!(chat.content["text"], "hello back");
        // Inbound message marked completed.
        let inbound = open_inbound(&setup.paths).unwrap();
        let status: String = inbound
            .query_row(
                "SELECT status FROM messages_in WHERE id = ?1",
                rusqlite::params![id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "completed");
        // processing_ack status went to Done.
        let claim = processing_ack::get(&outbound, id).unwrap().unwrap();
        assert_eq!(claim.status, processing_ack::ProcessingStatus::Done);
        // Continuation persisted.
        let st = load_state(&outbound).unwrap();
        assert_eq!(st.continuation.as_deref(), Some("c1"));
        assert!(!st.history.is_empty());
    }

    #[tokio::test]
    async fn error_event_marks_inbound_failed() {
        let mut setup = build_setup(vec![vec![ProviderEvent::Error {
            message: "boom".into(),
            retryable: false,
        }]]);
        let id = {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "hi")
        };
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        let inbound = open_inbound(&setup.paths).unwrap();
        let status: String = inbound
            .query_row(
                "SELECT status FROM messages_in WHERE id = ?1",
                rusqlite::params![id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "failed");

        let outbound = open_outbound(&setup.paths).unwrap();
        let claim = processing_ack::get(&outbound, id).unwrap().unwrap();
        assert_eq!(claim.status, processing_ack::ProcessingStatus::Failed);
    }

    #[tokio::test]
    async fn tool_start_writes_container_state_and_tool_end_clears() {
        let mut setup = build_setup(vec![vec![
            ProviderEvent::ToolStart {
                name: "bash".into(),
                declared_timeout_ms: Some(30_000),
            },
            ProviderEvent::ToolEnd,
            ProviderEvent::Result {
                text: Some("ok".into()),
            },
        ]]);
        {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "do something");
        }
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        let outbound = open_outbound(&setup.paths).unwrap();
        let st = container_state::get(&outbound).unwrap().unwrap();
        assert!(st.current_tool.is_none(), "tool should be cleared by ToolEnd");
        assert!(st.updated_at.is_some());
    }

    #[tokio::test]
    async fn disallowed_tool_produces_refusal_in_history() {
        // First turn: model emits a ToolCall to a disallowed tool;
        // the runner pushes a `Tool { is_error: true }` refusal,
        // then runs a second turn where the model concedes.
        let mut setup = build_setup(vec![
            vec![
                ProviderEvent::ToolCall {
                    id: "tu_1".into(),
                    name: "CronCreate".into(),
                    input: serde_json::json!({}),
                },
            ],
            vec![ProviderEvent::Result {
                text: Some("ok".into()),
            }],
        ]);
        {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "please cron");
        }
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        let outbound = open_outbound(&setup.paths).unwrap();
        let st = load_state(&outbound).unwrap();
        assert!(
            st.history.iter().any(|m| matches!(
                m,
                HistoryMessage::Tool { content, is_error: true, .. }
                    if content.contains("disallowed")
            )),
            "expected a disallowed-tool refusal in history, got: {:?}",
            st.history
        );
    }

    #[tokio::test]
    async fn progress_and_activity_events_are_tolerated() {
        let mut setup = build_setup(vec![vec![
            ProviderEvent::Progress {
                message: "thinking".into(),
            },
            ProviderEvent::Activity,
            ProviderEvent::Result {
                text: Some("done".into()),
            },
        ]]);
        {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "hi");
        }
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();
    }

    #[tokio::test]
    async fn empty_result_text_does_not_emit_outbound_row() {
        let mut setup = build_setup(vec![vec![ProviderEvent::Result { text: None }]]);
        {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "hi");
        }
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();
        let outbound = open_outbound(&setup.paths).unwrap();
        let rows = messages_out::list_due(&outbound).unwrap();
        // M13 emits a `usage_report` System row per turn; the chat
        // path still shouldn't emit anything for an empty result.
        let chat_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == ironclaw_types::MessageKind::Chat)
            .collect();
        assert!(
            chat_rows.is_empty(),
            "no Chat outbound row expected for empty result, got {chat_rows:?}"
        );
    }

    #[tokio::test]
    async fn heartbeat_file_touched_when_path_set() {
        let mut setup = build_setup(vec![vec![ProviderEvent::Result {
            text: Some("hi".into()),
        }]]);
        {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "x");
        }
        let hb_path = setup.paths.heartbeat.clone();
        setup.deps.heartbeat_path = Some(hb_path.clone());
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();
        assert!(hb_path.exists(), "heartbeat path should exist after a turn");
    }

    #[tokio::test]
    async fn minimal_builds_valid_deps() {
        // Smoke: just check `minimal` produces a runnable Deps that exits
        // immediately with max_turns=0.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let inbound = Arc::new(Mutex::new(open_inbound(&paths).unwrap()));
        let outbound = Arc::new(Mutex::new(open_outbound(&paths).unwrap()));
        let provider = ScriptedProvider::new(vec![]);
        let tool_ctx: Arc<dyn ToolContext> =
            Arc::new(RunnerToolCtx::new(outbound.clone(), paths.outbox.clone()));
        let mut d = RunnerDeps::minimal(
            provider,
            tool_ctx,
            inbound,
            outbound,
            paths.outbox.join("_compactions"),
        );
        d.max_turns = Some(0);
        d.idle_sleep = Duration::from_millis(1);
        run_loop(d).await.unwrap();
    }

    #[tokio::test]
    async fn processing_ack_re_ack_succeeds_on_existing_row() {
        // First insert one ack manually to exercise the duplicate-handling path.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let outbound = open_outbound(&paths).unwrap();
        let id = MessageId::new();
        processing_ack::insert(&outbound, id, processing_ack::ProcessingStatus::Processing)
            .unwrap();
        // Building deps just to call ack_picked_up.
        let provider = ScriptedProvider::new(vec![]);
        let outbound = Arc::new(Mutex::new(outbound));
        let inbound = Arc::new(Mutex::new(open_inbound(&paths).unwrap()));
        let tool_ctx: Arc<dyn ToolContext> =
            Arc::new(RunnerToolCtx::new(outbound.clone(), paths.outbox.clone()));
        let mut d = RunnerDeps::minimal(
            provider,
            tool_ctx,
            inbound,
            outbound.clone(),
            paths.outbox.join("_compactions"),
        );
        d.max_turns = Some(0);
        let row = MessageInRow {
            id,
            seq: 2,
            kind: MessageKind::Chat,
            timestamp: Utc::now(),
            status: "pending".into(),
            process_after: None,
            recurrence: None,
            series_id: None,
            tries: 0,
            trigger: true,
            platform_id: None,
            channel_type: None,
            thread_id: None,
            content: serde_json::json!({}),
            source_session_id: None,
            on_wake: false,
        };
        ack_picked_up(&d, &[row]).await.unwrap();
        let g = outbound.lock().await;
        let claim = processing_ack::get(&g, id).unwrap().unwrap();
        assert_eq!(claim.status, processing_ack::ProcessingStatus::Processing);
    }
}
