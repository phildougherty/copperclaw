//! End-to-end chat round-trip integration test.
//!
//! Boots `copperclaw_host::run_host` against a temp install root, mounts a
//! `wiremock` Anthropic stub that streams back `"hi from the mock"`,
//! writes `"hello"` into the cli channel's FIFO, and asserts the reply
//! appears in `<install_root>/chat.log`.
//!
//! This is the test that would have caught the FIFO-vs-stdin wiring bug
//! from M11: both halves had unit tests, but nothing exercised them
//! against each other. Here the cli channel's FIFO mode is driven by a
//! real file the host wires up at boot, and the reply file is a real log
//! file the host's `DeliveryService` writes to.
//!
//! The host's container manager is intentionally left disabled (no
//! `default_image_tag` configured) — instead, the test runs an
//! in-process "runner driver" that polls the central sessions table for
//! new sessions and processes each session's inbound via the runner
//! library directly. This is the same library-level seam the existing
//! `replay/harness.rs` uses to avoid spawning Docker; here we glue it to
//! `run_host` rather than to a hand-rolled router. The result is a real
//! end-to-end test of FIFO -> cli adapter -> router -> `messages_in` ->
//! runner -> `messages_out` -> `DeliveryService` -> cli adapter -> log.
//!
//! Both tests run in-process. Neither spawns a subprocess. Neither
//! requires Docker or network access.

#![forbid(unsafe_code)]

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use copperclaw_cclaw::{CallTransport, Caller, ClientError, RunOutput, run_cli};
use copperclaw_container_rt::{
    ContainerHandle, ContainerRuntime, ContainerSpec, ImageBuildSpec, RtError,
};
use copperclaw_db::central::CentralDb;
use copperclaw_db::session::{SessionPaths, open_inbound, open_inbound_rw_no_mmap, open_outbound};
use copperclaw_db::tables::{messages_in, session_routing, sessions};
use copperclaw_host::config::ChannelInit;
use copperclaw_host::{HostConfig, run_host};
use copperclaw_providers::AnthropicProvider;
use copperclaw_runner::{RunnerDeps, RunnerToolCtx, compaction::CompactionCfg, run_loop};
use copperclaw_types::{AgentGroupId, ChannelType, Effort, SessionId, routing::SessionRouting};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep, timeout};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Wiremock helper: mount a deterministic Anthropic-flavoured SSE response
// that ends with `"hi from the mock"` as the assistant's text. Matches
// what `AnthropicProvider` expects on POST /v1/messages.
// ---------------------------------------------------------------------------

/// SSE body the wiremock returns for every `POST /v1/messages`. Encodes
/// a minimal Anthropic streaming response: `message_start`,
/// `content_block_start` (text), one `content_block_delta` carrying the
/// reply, `content_block_stop`, `message_stop`. The runner's SSE pump
/// buffers the deltas and emits `Result { text }` on `message_stop`.
fn anthropic_sse_body(reply_text: &str) -> String {
    let events = [
        json!({"type": "message_start", "message": {"id": "msg_e2e_001"}}),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": reply_text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_stop"}),
    ];
    let mut out = String::new();
    for ev in events {
        let name = ev.get("type").and_then(|v| v.as_str()).unwrap_or("message");
        out.push_str("event: ");
        out.push_str(name);
        out.push('\n');
        out.push_str("data: ");
        out.push_str(&serde_json::to_string(&ev).expect("serialize SSE event"));
        out.push_str("\n\n");
    }
    out
}

async fn mount_hi_from_mock(server: &MockServer, reply: &str) {
    let body = anthropic_sse_body(reply);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// No-op container runtime. Lets the host boot without Docker on hand. The
// container manager itself is also disabled (no default_image_tag), so
// `spawn`/`stop`/etc. are never actually called — the runtime is here
// purely so `run_host` doesn't fall through to `copperclaw_container_rt::detect()`.
// ---------------------------------------------------------------------------

#[derive(Default, Debug)]
struct NoopRuntime;

#[async_trait]
impl ContainerRuntime for NoopRuntime {
    async fn ensure_running(&self) -> Result<(), RtError> {
        Ok(())
    }
    async fn cleanup_orphans(&self, _slug: &str) -> Result<(), RtError> {
        Ok(())
    }
    async fn spawn(&self, spec: ContainerSpec) -> Result<ContainerHandle, RtError> {
        Ok(ContainerHandle::new(
            format!("noop-{}-id", spec.name),
            spec.name,
        ))
    }
    async fn stop(&self, _name: &str, _grace: Duration) -> Result<(), RtError> {
        Ok(())
    }
    async fn build_image(&self, spec: ImageBuildSpec) -> Result<String, RtError> {
        Ok(spec.image_tag())
    }
}

// ---------------------------------------------------------------------------
// Central-DB seeding. Mirrors `fixtures/cli/text-reply/central.sql` but
// applied programmatically against the already-migrated central DB the
// host created at boot. The wiring is: one agent group ("E2E"), one
// messaging group (cli/stdin), engage mode `pattern` with pattern `.*`
// so every line matches, sender scope `all`, session mode `shared`.
// ---------------------------------------------------------------------------

fn seed_central_e2e(central_db_path: &std::path::Path) -> Result<()> {
    let db = CentralDb::open(central_db_path).context("open central DB for seeding")?;
    let conn = db.conn().context("borrow central conn")?;
    conn.execute_batch(
        "INSERT INTO agent_groups (id, name, folder, agent_provider, created_at) VALUES
           ('11111111-1111-1111-1111-111111111111', 'E2E', 'e2e', 'anthropic', '2026-01-01T00:00:00Z');
         INSERT INTO messaging_groups (id, channel_type, platform_id, name, is_group, unknown_sender_policy, created_at) VALUES
           ('22222222-2222-2222-2222-222222222222', 'cli', 'stdin', 'cli/stdin', 0, 'lenient', '2026-01-01T00:00:00Z');
         INSERT INTO messaging_group_agents (
             id, messaging_group_id, agent_group_id,
             engage_mode, engage_pattern, sender_scope,
             ignored_message_policy, session_mode, priority, created_at
         ) VALUES (
             '33333333-3333-3333-3333-333333333333',
             '22222222-2222-2222-2222-222222222222',
             '11111111-1111-1111-1111-111111111111',
             'pattern', '.*', 'all',
             'drop', 'shared', 0,
             '2026-01-01T00:00:00Z'
         );",
    )
    .context("apply e2e seed sql")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// In-process runner driver. Replaces the container manager for the
// test: polls the central sessions table for fresh sessions and runs
// one turn of `copperclaw_runner::run_loop` against the configured
// wiremock-backed `AnthropicProvider`.
//
// This is the same library-level seam the replay harness uses
// (see crates/copperclaw-host/tests/replay/harness.rs `run_one_turn`).
// ---------------------------------------------------------------------------

struct RunnerDriverCfg {
    central: CentralDb,
    data_dir: PathBuf,
    provider_base_url: String,
    shutdown: CancellationToken,
}

async fn runner_driver(cfg: RunnerDriverCfg) {
    let mut processed: std::collections::HashSet<SessionId> = std::collections::HashSet::new();
    loop {
        if cfg.shutdown.is_cancelled() {
            return;
        }
        // Snapshot active sessions. Process each new one exactly once.
        let active = sessions::list_active(&cfg.central).unwrap_or_default();
        for session in active {
            if processed.contains(&session.id) {
                continue;
            }
            // Make sure session_routing is populated (the container
            // manager normally writes this on first spawn). The cli
            // delivery path needs it to know where to deliver to.
            if let Err(err) = ensure_session_routing(&cfg.data_dir, &session) {
                tracing::warn!(?err, "ensure session_routing failed");
                continue;
            }
            // Mark the session's container as running before the
            // turn — the host's `DeliveryService::run_active_loop`
            // only polls sessions in `container_status = Running`,
            // and the sweep loop's 60s cadence is too slow for the
            // 30s budget. The container manager normally writes
            // this row when it spawns; we mirror that here.
            if let Err(err) = sessions::mark_container_running(&cfg.central, session.id) {
                tracing::warn!(?err, "mark_container_running failed");
            }
            // Drive one turn against the wiremock provider.
            if let Err(err) = run_one_turn(
                &cfg.data_dir,
                session.agent_group_id,
                session.id,
                &cfg.provider_base_url,
            )
            .await
            {
                tracing::warn!(?err, "runner turn failed");
                continue;
            }
            processed.insert(session.id);
            // Bump last_active so the delivery active-loop notices it.
            let _ = sessions::touch_last_active(&cfg.central, session.id);
        }
        // Bounded poll. Tight enough that the test finishes inside 30s.
        tokio::select! {
            () = cfg.shutdown.cancelled() => return,
            () = sleep(Duration::from_millis(100)) => {}
        }
    }
}

fn ensure_session_routing(
    data_dir: &std::path::Path,
    session: &copperclaw_types::Session,
) -> Result<()> {
    let paths = SessionPaths::new(data_dir, session.agent_group_id, session.id);
    paths.ensure_dirs()?;
    let conn = open_inbound(&paths)?;
    // Only write if absent — the router may already have it.
    let existing = session_routing::read(&conn).ok().flatten();
    if existing.is_some() {
        return Ok(());
    }
    session_routing::write(
        &conn,
        &SessionRouting {
            channel_type: Some(ChannelType::new(ChannelType::CLI)),
            platform_id: Some("stdin".to_string()),
            thread_id: session.thread_id.clone(),
        },
    )?;
    Ok(())
}

async fn run_one_turn(
    data_dir: &std::path::Path,
    ag: AgentGroupId,
    sess: SessionId,
    provider_base_url: &str,
) -> Result<()> {
    let paths = SessionPaths::new(data_dir, ag, sess);
    paths.ensure_dirs().context("ensure session dirs")?;

    // Wait briefly until inbound has at least one pending row. The
    // router writes the message after the route() call returns, but
    // our driver may see the session row before the inbound write
    // has committed in the worst case.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let n = {
            let conn = open_inbound(&paths)?;
            messages_in::count_due(&conn).unwrap_or(0)
        };
        if n > 0 {
            break;
        }
        if Instant::now() > deadline {
            return Err(anyhow!(
                "no inbound rows landed within 5s for session {sess:?}"
            ));
        }
        sleep(Duration::from_millis(25)).await;
    }

    let inbound = open_inbound_rw_no_mmap(&paths).context("open inbound (rw)")?;
    let outbound = open_outbound(&paths).context("open outbound (rw)")?;
    let inbound = Arc::new(Mutex::new(inbound));
    let outbound = Arc::new(Mutex::new(outbound));

    let provider = Arc::new(AnthropicProvider::with_base_url(
        "e2e-test-key",
        provider_base_url.to_string(),
    ));
    let tool_ctx: Arc<dyn copperclaw_mcp::ToolContext> =
        Arc::new(RunnerToolCtx::new(outbound.clone(), paths.outbox.clone()));
    let tool_set = copperclaw_mcp::build_tool_set();
    let tool_defs: Vec<copperclaw_providers::ToolDef> = tool_set
        .iter()
        .map(|e| copperclaw_providers::ToolDef {
            name: e.tool.name.to_string(),
            description: e.tool.description.as_deref().unwrap_or("").to_string(),
            input_schema: serde_json::Value::Object((*e.tool.input_schema).clone()),
        })
        .collect();
    let tool_map: Arc<std::collections::HashMap<String, Arc<copperclaw_mcp::ToolEntry>>> = Arc::new(
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
        system: "you are an e2e test agent".into(),
        model: "claude-sonnet-4-6".into(),
        effort: Effort::Medium,
        max_tokens: 1024,
        temperature: None,
        assistant_name: Some("E2E".into()),
        compaction: CompactionCfg {
            model_input_window: 200_000,
            safety_margin_tokens: 8_000,
            output_reserve_tokens: 4_096,
            soft_target_tokens: 0,
            summary_model: "claude-sonnet-4-6".into(),
            summary_effort: Effort::Low,
            summary_max_tokens: 1024,
            archive_dir: paths.outbox.join("_compactions"),
        },
        elision: copperclaw_runner::ElisionCfg {
            recent_results_kept: 0,
            max_result_bytes: usize::MAX,
        },
        max_turns: Some(1),
        idle_sleep: Duration::from_millis(10),
        heartbeat_path: Some(paths.heartbeat.clone()),
        session_id: sess,
        agent_group_id: ag,
        turn_seq: Arc::new(std::sync::atomic::AtomicI64::new(0)),
        tool_map,
        max_tool_turns: 5,
        // Tests run against wiremock; keep the deadline tight so
        // accidental hangs surface as a test timeout instead of a
        // 60-second stall.
        provider_deadline: Duration::from_millis(5_000),
        tool_deadline_secs: 30,
        // Tests don't observe the typing-keepalive signal; the noop
        // pinger keeps `RunnerDeps` construction trivial without
        // touching the heartbeat file behind the e2e test's back.
        activity_pinger: Arc::new(copperclaw_runner::NoopPinger),
        // Slice-3.5 surface defaults to off — tests don't rely on it.
        surface_thinking: false,
    };
    run_loop(deps).await.context("runner one-turn")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 1: full FIFO -> log round-trip against `run_host` + wiremock.
// ---------------------------------------------------------------------------

/// Bounded poll: re-read `log_path` every 100ms until `needle` appears
/// or the deadline elapses.
async fn wait_for_log_contains(
    log_path: &std::path::Path,
    needle: &str,
    deadline: Duration,
) -> Result<String> {
    let stop = Instant::now() + deadline;
    loop {
        let body = tokio::fs::read_to_string(log_path)
            .await
            .unwrap_or_default();
        if body.contains(needle) {
            return Ok(body);
        }
        if Instant::now() > stop {
            return Err(anyhow!(
                "log did not contain {needle:?} within {deadline:?}; current body: {body:?}"
            ));
        }
        sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // E2E setup is intentionally sequential.
async fn e2e_chat_round_trip_via_fifo_and_log() {
    // Each test owns its own tempdir + wiremock so they're safe to run
    // in parallel and don't share state. The install_root is the
    // tempdir; the host's data_dir is `install_root/data` (so the
    // cli channel's default-resolution gives us
    // `install_root/chat.fifo` and `install_root/chat.log`).
    let install_root = tempfile::tempdir().expect("install root tempdir");
    let data_dir = install_root.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    let fifo_path = install_root.path().join("chat.fifo");
    let log_path = install_root.path().join("chat.log");
    let socket_path = install_root.path().join("cclaw.sock");

    // Print key paths so debugging from `cargo test --nocapture`
    // is straightforward when the assertion eventually fails.
    eprintln!(
        "e2e_chat install_root={} data_dir={} fifo={} log={}",
        install_root.path().display(),
        data_dir.display(),
        fifo_path.display(),
        log_path.display(),
    );

    // 1. Wiremock with a deterministic Anthropic stub.
    let server = MockServer::start().await;
    mount_hi_from_mock(&server, "hi from the mock").await;
    let provider_base_url = server.uri();
    eprintln!("e2e_chat wiremock uri={provider_base_url}");

    // 2. HostConfig with explicit FIFO/log on the cli channel and no
    //    default_image_tag (container manager stays disabled — we'll
    //    process inbound in-process below).
    let cfg = HostConfig {
        data_dir: data_dir.clone(),
        ncl_socket_path: socket_path.clone(),
        channels: vec![ChannelInit {
            channel_type: "cli".into(),
            config: json!({
                "fifo": fifo_path.to_string_lossy(),
                "log": log_path.to_string_lossy(),
            }),
        }],
        ..HostConfig::default()
    };

    // 3. Boot the host. Spawned on the same runtime so we can cancel
    //    via the shutdown token.
    let shutdown = CancellationToken::new();
    let host_shutdown = shutdown.clone();
    let host_cfg = cfg.clone();
    let host_task = tokio::spawn(async move {
        let rt: Box<dyn ContainerRuntime> = Box::new(NoopRuntime);
        run_host(host_cfg, Some(rt), host_shutdown, None)
            .await
            .expect("run_host clean exit");
    });

    // 4. Wait for the host to be ready: socket + FIFO must exist.
    //    Bounded — never sleep-loop indefinitely.
    let ready = timeout(Duration::from_secs(10), async {
        loop {
            if socket_path.exists() && fifo_path.exists() && log_path.exists() {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        ready.is_ok(),
        "host did not become ready in 10s; socket exists={}, fifo exists={}, log exists={}",
        socket_path.exists(),
        fifo_path.exists(),
        log_path.exists(),
    );

    // 5. Seed central DB AFTER `run_host` ran migrations. Doing it
    //    before would race the host's `run_migrations_only` call which
    //    expects an empty DB it can migrate up cleanly. (Migrations
    //    are idempotent in practice, but only against schemas they
    //    own — extra rows in user tables must come after.)
    seed_central_e2e(&cfg.central_db_path()).expect("seed central");

    // 6. Spin up the in-process runner driver. It mirrors what the
    //    container manager would do, sans Docker.
    let central_for_driver =
        CentralDb::open(cfg.central_db_path()).expect("reopen central for driver");
    let driver_shutdown = shutdown.clone();
    let data_dir_for_driver = data_dir.clone();
    let provider_base_url_clone = provider_base_url.clone();
    let driver_task = tokio::spawn(async move {
        runner_driver(RunnerDriverCfg {
            central: central_for_driver,
            data_dir: data_dir_for_driver,
            provider_base_url: provider_base_url_clone,
            shutdown: driver_shutdown,
        })
        .await;
    });

    // 7. Write `hello\n` into the FIFO. The cli channel adapter is
    //    reading the other end via `tokio::net::unix::pipe::Receiver`.
    {
        let mut fifo = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&fifo_path)
            .await
            .expect("open FIFO for write");
        fifo.write_all(b"hello\n")
            .await
            .expect("write hello into FIFO");
        fifo.flush().await.expect("flush FIFO write");
    }

    // 8. Tail the log until the mocked reply appears.
    let body =
        match wait_for_log_contains(&log_path, "hi from the mock", Duration::from_secs(25)).await {
            Ok(b) => b,
            Err(e) => {
                // Snapshot what was actually persisted so a CI failure is
                // diagnosable from the test output alone.
                let central = CentralDb::open(cfg.central_db_path()).ok();
                if let Some(db) = central {
                    let active = sessions::list_active(&db).unwrap_or_default();
                    eprintln!("debug: {} active sessions", active.len());
                    for s in active {
                        eprintln!(
                            "  session {} agent_group {} status={:?} cstatus={:?}",
                            s.id.as_uuid(),
                            s.agent_group_id.as_uuid(),
                            s.status,
                            s.container_status,
                        );
                        let paths = SessionPaths::new(&data_dir, s.agent_group_id, s.id);
                        if let Ok(conn) = open_inbound(&paths) {
                            if let Ok(n) = messages_in::count_due(&conn) {
                                eprintln!("    inbound pending={n}");
                            }
                        }
                    }
                }
                // Dump wiremock received requests so we know whether the
                // runner even attempted a provider call.
                let received = server.received_requests().await.unwrap_or_default();
                eprintln!("debug: {} wiremock requests received", received.len());
                panic!("expected reply in log: {e}");
            }
        };
    // Sanity: the cli adapter prefixes outbound with its label
    // (`"agent> "` by default). Anchor the assert on that to catch
    // regressions in the cli renderer too.
    assert!(
        body.contains("agent> hi from the mock") || body.contains("hi from the mock"),
        "log body did not contain expected reply: {body:?}",
    );

    // 9. Clean shutdown. Cancel the shared token; both tasks observe
    //    it on their own select! / shutdown.cancelled() arms.
    shutdown.cancel();
    let _ = timeout(Duration::from_secs(10), host_task).await;
    let _ = timeout(Duration::from_secs(2), driver_task).await;
    drop(server);
}

// ---------------------------------------------------------------------------
// Test 2: auto-start friendly error when host isn't running.
//
// Drives `cclaw chat` (via `copperclaw_cclaw::run_cli`) with
// `--no-autostart` against a definitely-missing FIFO. Asserts the
// stderr names the FIFO path and hints at `copperclaw start`. No
// wiremock or host needed.
// ---------------------------------------------------------------------------

/// Stub transport that errors on every call. `cclaw chat` is a
/// composite client-side op (`composite.chat`) that bypasses the
/// transport entirely, so this exists only to satisfy `run_cli`'s
/// signature.
struct UnreachableTransport;

#[async_trait]
impl CallTransport for UnreachableTransport {
    async fn call(
        &self,
        command: &str,
        _args: serde_json::Value,
        _caller: Caller,
    ) -> Result<serde_json::Value, ClientError> {
        Err(ClientError::Remote(copperclaw_cclaw::ErrorPayload::new(
            "unreachable",
            format!("test transport should never be called; got {command}"),
        )))
    }
}

#[tokio::test]
async fn chat_no_autostart_friendly_error_when_host_not_running() {
    // Pure file-I/O assertion. No host, no wiremock, no runner.
    let tmp = tempfile::tempdir().expect("tempdir");
    let fifo_path = tmp.path().join("chat.fifo");
    let log_path = tmp.path().join("chat.log");
    assert!(!fifo_path.exists(), "precondition: FIFO must not exist");

    let transport = UnreachableTransport;
    let out: RunOutput = run_cli(
        [
            "cclaw",
            "chat",
            "--fifo",
            fifo_path.to_str().unwrap(),
            "--log",
            log_path.to_str().unwrap(),
            "--no-autostart",
        ],
        &transport,
    )
    .await;

    assert!(
        out.stdout.is_empty(),
        "stdout unexpectedly populated: {:?}",
        out.stdout
    );
    assert!(
        out.stderr.contains("no FIFO"),
        "stderr should mention missing FIFO, got: {:?}",
        out.stderr,
    );
    assert!(
        out.stderr.contains("copperclaw start") || out.stderr.contains("copperclaw run"),
        "stderr should hint at how to start the host, got: {:?}",
        out.stderr,
    );
    assert!(
        out.stderr.contains(fifo_path.to_str().unwrap()),
        "stderr should name the FIFO path, got: {:?}",
        out.stderr,
    );
}
