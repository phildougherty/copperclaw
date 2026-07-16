//! Runner-side JOIN SEAM for the `delegate_batch` tool (M19 A1).
//!
//! `delegate_batch` spawns N `delegate` workers and BLOCKS the parent turn
//! until they all report (or a budget elapses), returning one aggregated
//! result. The spawn half reuses the exact single-`delegate` machinery (a
//! `{"delegate": {...}}` System row the host's delegate handler picks up —
//! own container, writable `sib/<id>` worktree, NULL messaging group so the
//! worker reports ONLY back to its parent). This module is the JOIN: it
//! block-polls the parent's own `inbound.db` for
//!
//!   1. each worker's SPAWN RESULT — the host writes a
//!      `{"delegate_result": {status, session_id, detail}}` System row into
//!      the parent's inbound for every spawn (created / rejected / denied /
//!      invalid), and
//!   2. each spawned worker's REPORT — the worker's single `send_message`
//!      back to the parent lands as a `MessageKind::Chat` row whose
//!      `source_session_id` is the worker's child session (see
//!      `agent_to_agent::dispatch`).
//!
//! This mirrors how an external-MCP call block-polls the host's response
//! (`run::external_mcp`): a bounded poll loop over a per-session DB, marking
//! every consumed row `completed` so it never re-surfaces as a spurious
//! parent turn on the next `run_loop` / mid-turn-steering poll.
//!
//! Correlation: a `created` result's `detail` is the worker's `instructions`
//! verbatim (the host echoes it back), so we attribute a spawned child to
//! its worker by matching instructions; identical instructions (a degenerate
//! batch) fall back to the first unassigned slot, which is harmless. A
//! spawn *failure* carries a reason instead of instructions and is
//! batch-uniform (depth / permission gate the whole batch the same way), so
//! we pool failure reasons and assign them to whatever slots never got a
//! child.

use std::sync::Arc;
use std::time::{Duration, Instant};

use copperclaw_db::tables::messages_in;
use copperclaw_mcp::{DelegateBatchOutcome, DelegateBatchWorker, WorkerOutcome, WorkerStatus};
use copperclaw_types::MessageId;
use rusqlite::Connection;
use tokio::sync::Mutex;
use tokio::time::sleep;

/// Poll cadence (ms) while waiting on the host's spawn results + the
/// workers' reports. The host's active delivery loop answers a spawn
/// within ~1s; a worker report lands whenever the child finishes.
const DELEGATE_BATCH_POLL_MS: u64 = 250;

/// Per-worker mutable join state accumulated across poll iterations.
struct WorkerJoin {
    name: String,
    instructions: String,
    /// Child session id once the host reports the worker `created`.
    session_id: Option<String>,
    /// The worker's report text once it sends back to the parent.
    report: Option<String>,
    /// A terminal error (spawn failure) recorded before the deadline.
    error: Option<String>,
}

/// Spawn results are all in once we've consumed one `delegate_result` row
/// per requested worker.
struct SpawnPhase {
    results_seen: usize,
    expected: usize,
    /// Pooled failure reasons (rejected / denied / invalid spawns).
    failure_reasons: Vec<String>,
}

/// Block-poll `inbound` until every worker in `workers` has a report or an
/// error (or `timeout` elapses), then aggregate into a
/// [`DelegateBatchOutcome`]. `start_seq` is the inbound high-water mark
/// captured BEFORE the spawn rows were emitted, so the poll only observes
/// rows the host wrote in response to THIS batch.
pub(crate) async fn join_workers(
    inbound: &Arc<Mutex<Connection>>,
    workers: &[DelegateBatchWorker],
    start_seq: i64,
    timeout: Duration,
) -> DelegateBatchOutcome {
    // M19 A1: record the fan-out width of this batch.
    copperclaw_metrics::observe_delegate_batch_width(workers.len() as u64);
    let mut joins: Vec<WorkerJoin> = workers
        .iter()
        .map(|w| WorkerJoin {
            name: w.name.clone(),
            instructions: w.instructions.clone(),
            session_id: None,
            report: None,
            error: None,
        })
        .collect();
    let mut spawn = SpawnPhase {
        results_seen: 0,
        expected: joins.len(),
        failure_reasons: Vec::new(),
    };

    let start = Instant::now();
    loop {
        let rows = {
            let conn = inbound.lock().await;
            messages_in::get_new_since(&conn, start_seq).unwrap_or_default()
        };
        for row in &rows {
            if let Some(dr) = row.content.get("delegate_result") {
                consume_spawn_result(&mut joins, &mut spawn, dr);
                mark_completed(inbound, row.id).await;
            } else if let Some(sid) = row.source_session_id.as_deref() {
                if let Some(j) = joins
                    .iter_mut()
                    .find(|j| j.session_id.as_deref() == Some(sid) && j.report.is_none())
                {
                    let text = row
                        .content
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    j.report = Some(text);
                    mark_completed(inbound, row.id).await;
                }
                // A report from a session outside this batch is left
                // pending — it belongs to a different flow and the runner's
                // normal poll will pick it up.
            }
            // Any other row (a user interjection, a `/stop`, an unrelated
            // scheduled fire) is left pending for the runner's own loop.
        }

        // Once every spawn attempt has resolved, any slot that never got a
        // child was a spawn failure — attribute a pooled reason to it.
        if spawn.results_seen >= spawn.expected {
            for j in &mut joins {
                if j.session_id.is_none() && j.report.is_none() && j.error.is_none() {
                    let reason = spawn
                        .failure_reasons
                        .pop()
                        .unwrap_or_else(|| "worker was not spawned".into());
                    j.error = Some(format!("spawn failed: {reason}"));
                }
            }
        }

        if joins
            .iter()
            .all(|j| j.report.is_some() || j.error.is_some())
        {
            break;
        }
        if start.elapsed() >= timeout {
            fill_timeouts(&mut joins, timeout);
            break;
        }
        sleep(Duration::from_millis(DELEGATE_BATCH_POLL_MS)).await;
    }

    DelegateBatchOutcome {
        workers: joins.into_iter().map(finalize_worker).collect(),
    }
}

/// Fold one `delegate_result` payload into the join state.
fn consume_spawn_result(joins: &mut [WorkerJoin], spawn: &mut SpawnPhase, dr: &serde_json::Value) {
    spawn.results_seen += 1;
    let status = dr
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let detail = dr.get("detail").and_then(serde_json::Value::as_str);
    let sid = dr.get("session_id").and_then(serde_json::Value::as_str);
    if status == "created" {
        if let Some(sid) = sid {
            assign_created(joins, sid, detail);
        }
    } else {
        spawn
            .failure_reasons
            .push(detail.map_or_else(|| format!("spawn {status}"), str::to_owned));
    }
}

/// Attribute a newly-created child session to a worker. Prefer matching by
/// instructions (a created result's `detail` is the instructions verbatim);
/// fall back to the first unassigned slot when instructions are ambiguous.
fn assign_created(joins: &mut [WorkerJoin], sid: &str, detail: Option<&str>) {
    if let Some(detail) = detail {
        if let Some(j) = joins
            .iter_mut()
            .find(|j| j.session_id.is_none() && j.instructions == detail)
        {
            j.session_id = Some(sid.to_owned());
            return;
        }
    }
    if let Some(j) = joins.iter_mut().find(|j| j.session_id.is_none()) {
        j.session_id = Some(sid.to_owned());
    }
}

/// On deadline, record a per-worker timeout for any slot without a report
/// or error — distinguishing "spawned but silent" from "never spawned".
fn fill_timeouts(joins: &mut [WorkerJoin], timeout: Duration) {
    for j in joins.iter_mut() {
        if j.report.is_none() && j.error.is_none() {
            j.error = Some(if j.session_id.is_some() {
                format!("worker did not report within {}s", timeout.as_secs())
            } else {
                format!("worker was not spawned within {}s", timeout.as_secs())
            });
        }
    }
}

/// Collapse a `WorkerJoin` into its public [`WorkerOutcome`].
fn finalize_worker(j: WorkerJoin) -> WorkerOutcome {
    let status = if j.report.is_some() {
        WorkerStatus::Ok
    } else if j.session_id.is_some() {
        WorkerStatus::Timeout
    } else {
        WorkerStatus::SpawnFailed
    };
    // M19 A1: per-worker terminal outcome.
    copperclaw_metrics::inc_delegate_batch_worker(match status {
        WorkerStatus::Ok => "ok",
        WorkerStatus::Timeout => "timeout",
        WorkerStatus::SpawnFailed => "spawn_failed",
    });
    WorkerOutcome {
        name: j.name,
        status,
        report: j.report,
        error: j.error,
        session_id: j.session_id,
    }
}

/// Best-effort `mark_completed` for a batch row we consumed. Errors are
/// logged and swallowed — a transient `SQLite` hiccup leaves the row pending
/// (picked up on the next poll), which is safe: we only ever act on a row
/// once (a consumed report flips `report`/`error`, a consumed spawn result
/// increments `results_seen`) and a completed row never re-appears.
async fn mark_completed(inbound: &Arc<Mutex<Connection>>, id: MessageId) {
    let conn = inbound.lock().await;
    if let Err(err) = messages_in::mark_completed(&conn, id) {
        tracing::warn!(
            ?err,
            row_id = %id.as_uuid(),
            "delegate_batch: mark_completed failed; row left pending",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use copperclaw_db::session::{SessionPaths, open_inbound};
    use copperclaw_db::tables::messages_in::WriteInbound;
    use copperclaw_types::{AgentGroupId, MessageKind, SessionId};

    fn fresh_inbound() -> (tempfile::TempDir, Arc<Mutex<Connection>>) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_inbound(&paths).unwrap();
        (tmp, Arc::new(Mutex::new(conn)))
    }

    async fn write_row(
        inbound: &Arc<Mutex<Connection>>,
        kind: MessageKind,
        content: serde_json::Value,
        source_session_id: Option<String>,
    ) {
        let conn = inbound.lock().await;
        let msg = WriteInbound {
            id: MessageId::new(),
            kind,
            timestamp: Utc::now(),
            content,
            trigger: matches!(kind, MessageKind::Chat),
            on_wake: false,
            process_after: None,
            recurrence: None,
            series_id: None,
            platform_id: None,
            channel_type: None,
            thread_id: None,
            source_session_id,
            reply_to: None,
            is_group: None,
        };
        messages_in::insert(&conn, &msg).unwrap();
    }

    fn worker(name: &str, instructions: &str) -> DelegateBatchWorker {
        DelegateBatchWorker {
            name: name.into(),
            instructions: instructions.into(),
        }
    }

    fn delegate_result(status: &str, session_id: Option<&str>, detail: &str) -> serde_json::Value {
        let mut body = serde_json::Map::new();
        body.insert("status".into(), serde_json::json!(status));
        if let Some(s) = session_id {
            body.insert("session_id".into(), serde_json::json!(s));
        }
        body.insert("detail".into(), serde_json::json!(detail));
        serde_json::json!({ "delegate_result": body })
    }

    #[tokio::test]
    async fn joins_all_reports_after_they_arrive() {
        let (_tmp, inbound) = fresh_inbound();
        let workers = vec![worker("api", "build api"), worker("cli", "build cli")];
        let sid_api = SessionId::new().as_uuid().to_string();
        let sid_cli = SessionId::new().as_uuid().to_string();

        // A background "host": write both spawn results, then both reports.
        let writer = inbound.clone();
        let (a, c) = (sid_api.clone(), sid_cli.clone());
        let host = tokio::spawn(async move {
            write_row(
                &writer,
                MessageKind::System,
                delegate_result("created", Some(&a), "build api"),
                None,
            )
            .await;
            write_row(
                &writer,
                MessageKind::System,
                delegate_result("created", Some(&c), "build cli"),
                None,
            )
            .await;
            tokio::time::sleep(Duration::from_millis(60)).await;
            write_row(
                &writer,
                MessageKind::Chat,
                serde_json::json!({"text": "api done"}),
                Some(a),
            )
            .await;
            write_row(
                &writer,
                MessageKind::Chat,
                serde_json::json!({"text": "cli done"}),
                Some(c),
            )
            .await;
        });

        let outcome = join_workers(&inbound, &workers, 0, Duration::from_secs(5)).await;
        host.await.unwrap();

        assert_eq!(outcome.completed(), 2);
        // Attributed by instructions, in request order.
        assert_eq!(outcome.workers[0].name, "api");
        assert_eq!(outcome.workers[0].status, WorkerStatus::Ok);
        assert_eq!(outcome.workers[0].report.as_deref(), Some("api done"));
        assert_eq!(
            outcome.workers[0].session_id.as_deref(),
            Some(sid_api.as_str())
        );
        assert_eq!(outcome.workers[1].name, "cli");
        assert_eq!(outcome.workers[1].report.as_deref(), Some("cli done"));

        // Every consumed row was marked completed (none left pending).
        let conn = inbound.lock().await;
        let pending = messages_in::get_pending(&conn, true, 50).unwrap();
        assert!(
            pending.is_empty(),
            "batch rows must be consumed, got {pending:?}"
        );
    }

    #[tokio::test]
    async fn a_rejected_spawn_becomes_a_per_worker_error() {
        // Models the depth-cap refusal: the host writes a `rejected`
        // delegate_result for the single worker → per-worker spawn error,
        // and the whole outcome is "all spawn-failed" (a refusal).
        let (_tmp, inbound) = fresh_inbound();
        let workers = vec![worker("w", "do it")];
        write_row(
            &inbound,
            MessageKind::System,
            delegate_result("rejected", None, "nested create_agent (max depth = 3)"),
            None,
        )
        .await;

        let outcome = join_workers(&inbound, &workers, 0, Duration::from_secs(5)).await;
        assert!(outcome.all_spawn_failed());
        assert_eq!(outcome.workers[0].status, WorkerStatus::SpawnFailed);
        assert!(
            outcome.workers[0]
                .error
                .as_deref()
                .unwrap()
                .contains("max depth"),
            "spawn error should carry the depth-cap reason"
        );
    }

    #[tokio::test]
    async fn a_silent_worker_times_out_without_losing_the_batch() {
        // One worker spawns + reports; the other spawns but never reports.
        // The join must return after the (short) budget with a per-worker
        // timeout for the silent one — not hang, not lose the batch.
        let (_tmp, inbound) = fresh_inbound();
        let workers = vec![worker("fast", "a"), worker("slow", "b")];
        let sid_fast = SessionId::new().as_uuid().to_string();
        let sid_slow = SessionId::new().as_uuid().to_string();
        write_row(
            &inbound,
            MessageKind::System,
            delegate_result("created", Some(&sid_fast), "a"),
            None,
        )
        .await;
        write_row(
            &inbound,
            MessageKind::System,
            delegate_result("created", Some(&sid_slow), "b"),
            None,
        )
        .await;
        write_row(
            &inbound,
            MessageKind::Chat,
            serde_json::json!({"text": "fast done"}),
            Some(sid_fast.clone()),
        )
        .await;

        let outcome = join_workers(&inbound, &workers, 0, Duration::from_millis(400)).await;
        assert_eq!(outcome.completed(), 1);
        let slow = outcome.workers.iter().find(|w| w.name == "slow").unwrap();
        assert_eq!(slow.status, WorkerStatus::Timeout);
        assert!(slow.error.as_deref().unwrap().contains("did not report"));
        assert!(!outcome.all_spawn_failed());
    }

    #[tokio::test]
    async fn a_report_from_an_unrelated_session_is_left_pending() {
        // A Chat row whose source is NOT one of our workers must be left
        // untouched (pending) — it belongs to the runner's normal flow.
        let (_tmp, inbound) = fresh_inbound();
        let workers = vec![worker("w", "task")];
        let sid = SessionId::new().as_uuid().to_string();
        write_row(
            &inbound,
            MessageKind::System,
            delegate_result("created", Some(&sid), "task"),
            None,
        )
        .await;
        write_row(
            &inbound,
            MessageKind::Chat,
            serde_json::json!({"text": "from a stranger"}),
            Some(SessionId::new().as_uuid().to_string()),
        )
        .await;
        write_row(
            &inbound,
            MessageKind::Chat,
            serde_json::json!({"text": "worker report"}),
            Some(sid),
        )
        .await;

        let outcome = join_workers(&inbound, &workers, 0, Duration::from_secs(5)).await;
        assert_eq!(outcome.workers[0].report.as_deref(), Some("worker report"));
        // The stranger's row is still pending.
        let conn = inbound.lock().await;
        let pending = messages_in::get_pending(&conn, true, 50).unwrap();
        assert_eq!(pending.len(), 1, "unrelated report must be left pending");
        assert_eq!(
            pending[0].content.get("text").and_then(|t| t.as_str()),
            Some("from a stranger")
        );
    }
}
