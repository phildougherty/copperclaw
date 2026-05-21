//! In-process replay harness.
//!
//! The harness is intentionally lean for the v1 acceptance gate: it skips
//! the full `run_host` boot dance (channel mpsc consumers, socket server,
//! signal handling, container manager) and drives the underlying services
//! directly:
//!
//! - A `Router` against an in-memory `CentralDb` seeded from
//!   `central.sql`, writing to per-session inbound DBs under a `tempdir`.
//! - A `DeliveryService` reading the same per-session outbound DBs and
//!   handing rows to a `MockAdapter` that captures every `deliver` call.
//! - For each inbound step, an in-process runner with
//!   `max_turns = Some(1)` driven against a `wiremock`-served Anthropic
//!   endpoint that serves the fixture's pre-recorded SSE event stream.
//!
//! The harness exposes three entry points:
//!
//! - `ReplayHarness::new(fixture)` — boot.
//! - `ReplayHarness::run()` — drive each `inbound/NNN-*.json` event
//!   through the pipeline; record the resulting state.
//! - `ReplayHarness::compare()` — diff captured state against the
//!   fixture's `expected/*.jsonl` files using manifest substitutions.
#![allow(dead_code)]

use anyhow::{anyhow, Context, Result};
use ironclaw_channels_core::{testing::MockAdapter, ChannelAdapter};
use ironclaw_db::central::CentralDb;
use ironclaw_db::migrate::{run_migrations, MigrationSet};
use ironclaw_db::session::{open_inbound_rw_no_mmap, open_outbound, SessionPaths};
use ironclaw_db::tables::sessions;
use ironclaw_host_delivery::{DeliveryService, FsSessionRoot as DeliveryRoot, SessionRoot as DeliverySessionRoot};
use ironclaw_host_router::{FsSessionRoot as RouterRoot, RouteOutcome, Router, SessionRoot as RouterSessionRoot};
use ironclaw_providers::AnthropicProvider;
use ironclaw_runner::{compaction::CompactionCfg, run_loop, RunnerDeps, RunnerToolCtx};
use ironclaw_types::{
    AgentGroupId, ChannelType, Effort, InboundEvent, SessionId, SessionStatus,
};
use rusqlite::Connection;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::diff::{diff_stream, DiffReport, Substitutions};
use crate::fixture::{ClaudeTurn, Fixture};

/// Booted harness.
pub struct ReplayHarness {
    pub fixture: Fixture,
    pub tempdir: TempDir,
    pub central: CentralDb,
    pub router: Arc<Router>,
    pub delivery: Arc<DeliveryService>,
    /// Channel-type -> `MockAdapter` map. The harness pre-registers a
    /// `MockAdapter` for the channel named in `manifest.channel` plus a
    /// short builtin allow-list (`cli`, `telegram`, `slack`) so multi-
    /// channel fixtures (e.g. inter-agent fan-out) don't need extra
    /// wiring. `snapshot_delivered` aggregates deliveries across all of
    /// them in registration order.
    pub adapters: Vec<(ChannelType, Arc<MockAdapter>)>,
    pub anthropic_server: MockServer,
    /// Session IDs created or referenced by the router across the run.
    /// Used by `compare` to scan the right per-session DBs for actuals.
    pub touched_sessions: Vec<(AgentGroupId, SessionId)>,
    /// Replay of `inbound/*.json` events as they were driven, so the
    /// `inbound-events.jsonl` diff has a concrete actual stream.
    pub played_inbound: Vec<InboundEvent>,
}

impl ReplayHarness {
    pub async fn new(fixture: Fixture) -> Result<Self> {
        let tempdir = tempfile::tempdir().context("create harness tempdir")?;
        let central = CentralDb::open_in_memory().context("open in-memory central DB")?;
        {
            let mut conn = central.conn().context("borrow central conn")?;
            run_migrations(&mut conn, MigrationSet::Central)
                .context("run central migrations")?;
            conn.execute_batch(&fixture.central_sql)
                .context("apply fixture central.sql")?;
        }

        let router_root: Arc<dyn RouterSessionRoot + Send + Sync> =
            Arc::new(RouterRoot::new(tempdir.path().to_path_buf()));
        let router = Arc::new(Router::new(central.clone(), router_root));

        let delivery_root: Arc<dyn DeliverySessionRoot> =
            Arc::new(DeliveryRoot::new(tempdir.path().to_path_buf()));

        // Pre-register a deterministic `MockAdapter` for every channel
        // type we expect a fixture to drive. The order is fixed so
        // `snapshot_delivered` aggregates in a stable order across runs.
        let mut channel_types: Vec<ChannelType> = vec![
            ChannelType::new(ChannelType::CLI),
            ChannelType::new("telegram"),
            ChannelType::new("slack"),
        ];
        // Ensure the fixture's own channel is present even if it's some
        // future name (e.g. "discord") not in the built-in list.
        let fixture_ct = ChannelType::new(fixture.manifest.channel.as_str());
        if !channel_types.iter().any(|ct| ct == &fixture_ct) {
            channel_types.push(fixture_ct);
        }

        let mut adapters: Vec<(ChannelType, Arc<MockAdapter>)> = Vec::new();
        let mut initial: Vec<(ChannelType, Arc<dyn ChannelAdapter>)> = Vec::new();
        for ct in channel_types {
            let mock = Arc::new(MockAdapter::new(ct.as_str()));
            initial.push((ct.clone(), mock.clone() as Arc<dyn ChannelAdapter>));
            adapters.push((ct, mock));
        }
        let delivery =
            DeliveryService::with_default_dispatcher(central.clone(), delivery_root, initial);

        let anthropic_server = MockServer::start().await;
        mount_claude_turns(&anthropic_server, &fixture.claude_turns).await;

        Ok(Self {
            fixture,
            tempdir,
            central,
            router,
            delivery,
            adapters,
            anthropic_server,
            touched_sessions: Vec::new(),
            played_inbound: Vec::new(),
        })
    }

    /// Drive every `inbound/*.json` event through the pipeline. After
    /// each event the harness:
    ///
    /// 1. Calls `Router::route` and records the touched sessions.
    /// 2. Spawns an in-process runner against the (newly-created)
    ///    per-session DBs with `max_turns = Some(1)` so it processes
    ///    exactly one turn and exits.
    /// 3. Calls `DeliveryService::process_session_once` to drain
    ///    `messages_out` through the `MockAdapter`.
    pub async fn run(&mut self) -> Result<()> {
        let events: Vec<InboundEvent> = self.fixture.inbound.clone();
        for event in events {
            self.played_inbound.push(event.clone());
            let outcome = self
                .router
                .route(event.clone())
                .await
                .context("router.route")?;
            match outcome {
                RouteOutcome::Delivered { sessions } => {
                    for d in sessions {
                        if !self
                            .touched_sessions
                            .iter()
                            .any(|(_, s)| *s == d.session_id)
                        {
                            self.touched_sessions
                                .push((d.agent_group_id, d.session_id));
                        }
                        // Ensure session_routing is populated so delivery
                        // can resolve a destination for runner-emitted
                        // chat rows. The host's container_manager normally
                        // does this; we mirror the minimum here.
                        self.seed_session_routing(d.agent_group_id, d.session_id, &event)?;
                        // Mark running so DeliveryService's active path
                        // would pick up the session if it were running.
                        let _ = sessions::mark_container_running(&self.central, d.session_id);
                    }
                }
                RouteOutcome::Dropped { reason } => {
                    anyhow::bail!("router dropped event: {reason:?}");
                }
                RouteOutcome::Pending { reason } => {
                    anyhow::bail!("router deferred event: {reason:?}");
                }
            }

            // One turn per inbound step. The runner exits when
            // `max_turns` is reached.
            let (ag, sess) = self
                .touched_sessions
                .last()
                .copied()
                .ok_or_else(|| anyhow!("no session touched by route"))?;
            self.run_one_turn(ag, sess).await?;

            // Drain outbound for this session through the mock adapter.
            self.deliver_session(ag, sess).await?;
        }
        Ok(())
    }

    async fn run_one_turn(&self, ag: AgentGroupId, sess: SessionId) -> Result<()> {
        let paths = SessionPaths::new(self.tempdir.path(), ag, sess);
        paths.ensure_dirs().context("ensure session dirs")?;
        let inbound = open_inbound_rw_no_mmap(&paths).context("open inbound (rw)")?;
        let outbound = open_outbound(&paths).context("open outbound (rw)")?;
        let inbound = Arc::new(Mutex::new(inbound));
        let outbound = Arc::new(Mutex::new(outbound));

        let provider = Arc::new(AnthropicProvider::with_base_url(
            "harness-key",
            self.anthropic_server.uri(),
        ));
        let tool_ctx: Arc<dyn ironclaw_mcp::ToolContext> = Arc::new(RunnerToolCtx::new(
            outbound.clone(),
            paths.outbox.clone(),
        ));

        let tool_set = ironclaw_mcp::build_tool_set();
        let tool_defs: Vec<ironclaw_providers::ToolDef> = tool_set
            .iter()
            .map(|e| ironclaw_providers::ToolDef {
                name: e.tool.name.to_string(),
                description: e
                    .tool
                    .description
                    .as_deref()
                    .unwrap_or("")
                    .to_string(),
                input_schema: serde_json::Value::Object((*e.tool.input_schema).clone()),
            })
            .collect();
        let tool_map: Arc<
            std::collections::HashMap<String, Arc<ironclaw_mcp::ToolEntry>>,
        > = Arc::new(
            tool_set
                .into_iter()
                .map(|e| (e.tool.name.to_string(), Arc::new(e)))
                .collect(),
        );

        let deps = RunnerDeps {
            provider,
            tool_ctx,
            inbound,
            outbound,
            tools: tool_defs,
            system: "you are a replay test agent".into(),
            model: "claude-sonnet-4-6".into(),
            effort: Effort::Medium,
            max_tokens: 1024,
            temperature: None,
            assistant_name: Some("Replay".into()),
            compaction: CompactionCfg {
                model_input_window: 200_000,
                safety_margin_tokens: 8_000,
                summary_model: "claude-sonnet-4-6".into(),
                summary_effort: Effort::Low,
                summary_max_tokens: 1024,
                archive_dir: paths.outbox.join("_compactions"),
            },
            max_turns: Some(1),
            idle_sleep: Duration::from_millis(10),
            heartbeat_path: Some(paths.heartbeat.clone()),
            session_id: sess,
            agent_group_id: ag,
            turn_seq: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            tool_map,
            max_tool_turns: 5,
        };
        run_loop(deps).await.context("runner one-turn")?;
        Ok(())
    }

    async fn deliver_session(&self, ag: AgentGroupId, sess: SessionId) -> Result<()> {
        let session = sessions::get(&self.central, sess)
            .context("load session row for delivery")?;
        debug_assert_eq!(session.agent_group_id, ag);
        let _report = self
            .delivery
            .process_session_once(&session)
            .await
            .context("delivery.process_session_once")?;
        Ok(())
    }

    fn seed_session_routing(
        &self,
        ag: AgentGroupId,
        sess: SessionId,
        event: &InboundEvent,
    ) -> Result<()> {
        use ironclaw_db::session::open_inbound;
        use ironclaw_db::tables::session_routing;
        use ironclaw_types::routing::SessionRouting;
        let paths = SessionPaths::new(self.tempdir.path(), ag, sess);
        paths.ensure_dirs()?;
        let conn = open_inbound(&paths)?;
        session_routing::write(
            &conn,
            &SessionRouting {
                channel_type: Some(event.channel_type.clone()),
                platform_id: Some(event.platform_id.clone()),
                thread_id: event.thread_id.clone(),
            },
        )?;
        Ok(())
    }

    /// Diff captured actuals against the fixture's expected JSONL streams.
    pub fn compare(&self) -> Result<DiffReport> {
        let subs = Substitutions::compile(&self.fixture.manifest.substitutions)?;
        let mut report = DiffReport::default();

        let actual_inbound: Vec<serde_json::Value> = self
            .played_inbound
            .iter()
            .map(|e| serde_json::to_value(e).expect("inbound to json"))
            .collect();
        report.extend(diff_stream(
            "inbound-events",
            &self.fixture.expected.inbound_events,
            &actual_inbound,
            &subs,
        ));

        let actual_in = self.snapshot_messages_in()?;
        report.extend(diff_stream(
            "messages-in",
            &self.fixture.expected.messages_in,
            &actual_in,
            &subs,
        ));

        let actual_out = self.snapshot_messages_out()?;
        report.extend(diff_stream(
            "messages-out",
            &self.fixture.expected.messages_out,
            &actual_out,
            &subs,
        ));

        let actual_delivered = self.snapshot_delivered();
        report.extend(diff_stream(
            "delivered",
            &self.fixture.expected.delivered,
            &actual_delivered,
            &subs,
        ));
        Ok(report)
    }

    fn snapshot_messages_in(&self) -> Result<Vec<serde_json::Value>> {
        let mut rows: Vec<serde_json::Value> = Vec::new();
        let mut seen: HashSet<SessionId> = HashSet::new();
        for (ag, sess) in &self.touched_sessions {
            if !seen.insert(*sess) {
                continue;
            }
            let paths = SessionPaths::new(self.tempdir.path(), *ag, *sess);
            let conn = open_inbound_rw_no_mmap(&paths)?;
            rows.extend(read_messages_in(&conn)?);
        }
        Ok(rows)
    }

    fn snapshot_messages_out(&self) -> Result<Vec<serde_json::Value>> {
        let mut rows: Vec<serde_json::Value> = Vec::new();
        let mut seen: HashSet<SessionId> = HashSet::new();
        for (ag, sess) in &self.touched_sessions {
            if !seen.insert(*sess) {
                continue;
            }
            let paths = SessionPaths::new(self.tempdir.path(), *ag, *sess);
            let conn = open_outbound(&paths)?;
            rows.extend(read_messages_out(&conn)?);
        }
        Ok(rows)
    }

    fn snapshot_delivered(&self) -> Vec<serde_json::Value> {
        let mut out: Vec<serde_json::Value> = Vec::new();
        for (ct, adapter) in &self.adapters {
            for d in adapter.deliveries() {
                out.push(serde_json::json!({
                    "channel_type": ct.as_str(),
                    "platform_id": d.platform_id,
                    "thread_id": d.thread_id,
                    "kind": d.message.kind.as_str(),
                    "content": d.message.content,
                    "files": d.message.files.len(),
                }));
            }
        }
        out
    }
}

/// Mount one mock per claude turn at `/v1/messages`. Each mock has
/// `up_to_n_times(1)` so the i-th request matches the i-th mock.
async fn mount_claude_turns(server: &MockServer, turns: &[ClaudeTurn]) {
    // wiremock priorities are u8 with 1 == highest, 255 == lowest. We
    // want the i-th request to consume the i-th mock; once a mock hits
    // its `up_to_n_times` cap it stops matching, so the next-lowest
    // priority takes over. Clamp the index into [1, 255].
    for (i, turn) in turns.iter().enumerate() {
        let body = encode_sse(turn);
        let pri = u8::try_from(i + 1).unwrap_or(u8::MAX);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .up_to_n_times(1)
            .with_priority(pri)
            .mount(server)
            .await;
    }
}

fn encode_sse(turn: &ClaudeTurn) -> String {
    let mut out = String::new();
    for ev in &turn.events {
        let name = ev
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("message");
        out.push_str("event: ");
        out.push_str(name);
        out.push('\n');
        out.push_str("data: ");
        out.push_str(&serde_json::to_string(ev).expect("serialize SSE event"));
        out.push_str("\n\n");
    }
    out
}

/// Hand-rolled SELECT against `messages_in`. Snapshots every row in
/// ascending `seq` order and renders deterministic JSON the diff can
/// compare against.
fn read_messages_in(conn: &Connection) -> Result<Vec<serde_json::Value>> {
    let mut stmt = conn.prepare(
        "SELECT id, seq, kind, timestamp, status, process_after, recurrence,
                series_id, tries, trigger, platform_id, channel_type, thread_id,
                content, source_session_id, on_wake
         FROM messages_in
         ORDER BY seq ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        let content_str: String = row.get("content")?;
        let content: serde_json::Value = serde_json::from_str(&content_str)
            .unwrap_or(serde_json::Value::Null);
        let trigger: i64 = row.get("trigger")?;
        let on_wake: i64 = row.get("on_wake")?;
        Ok(serde_json::json!({
            "id": row.get::<_, String>("id")?,
            "seq": row.get::<_, i64>("seq")?,
            "kind": row.get::<_, String>("kind")?,
            "timestamp": row.get::<_, String>("timestamp")?,
            "status": row.get::<_, String>("status")?,
            "process_after": row.get::<_, Option<String>>("process_after")?,
            "recurrence": row.get::<_, Option<String>>("recurrence")?,
            "series_id": row.get::<_, Option<String>>("series_id")?,
            "tries": row.get::<_, i64>("tries")?,
            "trigger": trigger != 0,
            "platform_id": row.get::<_, Option<String>>("platform_id")?,
            "channel_type": row.get::<_, Option<String>>("channel_type")?,
            "thread_id": row.get::<_, Option<String>>("thread_id")?,
            "content": content,
            "source_session_id": row.get::<_, Option<String>>("source_session_id")?,
            "on_wake": on_wake != 0,
        }))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn read_messages_out(conn: &Connection) -> Result<Vec<serde_json::Value>> {
    let mut stmt = conn.prepare(
        "SELECT id, seq, in_reply_to, timestamp, deliver_after, recurrence,
                kind, platform_id, channel_type, thread_id, content
         FROM messages_out
         ORDER BY seq ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        let content_str: String = row.get("content")?;
        let content: serde_json::Value = serde_json::from_str(&content_str)
            .unwrap_or(serde_json::Value::Null);
        Ok(serde_json::json!({
            "id": row.get::<_, String>("id")?,
            "seq": row.get::<_, i64>("seq")?,
            "in_reply_to": row.get::<_, Option<String>>("in_reply_to")?,
            "timestamp": row.get::<_, String>("timestamp")?,
            "deliver_after": row.get::<_, Option<String>>("deliver_after")?,
            "recurrence": row.get::<_, Option<String>>("recurrence")?,
            "kind": row.get::<_, String>("kind")?,
            "platform_id": row.get::<_, Option<String>>("platform_id")?,
            "channel_type": row.get::<_, Option<String>>("channel_type")?,
            "thread_id": row.get::<_, Option<String>>("thread_id")?,
            "content": content,
        }))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

// Suppress unused-symbol clippy warnings when only a subset of features
// of `sessions` are referenced.
#[allow(dead_code)]
fn _ensure_session_status_in_scope(_s: SessionStatus) {}
