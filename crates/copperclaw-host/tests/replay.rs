//! Replay-fixture integration tests.
//!
//! Each `#[tokio::test]` here loads one fixture under
//! `fixtures/<channel>/<scenario>/` and runs it through the in-process
//! `ReplayHarness`. The test fails the moment any of the four expected
//! JSONL streams diverges from the captured actual (after manifest
//! substitutions).
//!
//! See `docs/replay-fixtures.md` for the fixture format and capture
//! workflow. This file is the M11 acceptance gate.

#[path = "replay/diff.rs"]
mod diff;
#[path = "replay/fixture.rs"]
mod fixture;
#[path = "replay/harness.rs"]
mod harness;

use std::path::PathBuf;

use crate::fixture::Fixture;
use crate::harness::ReplayHarness;

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // CARGO_MANIFEST_DIR points at `crates/copperclaw-host/`; the workspace
    // root is two `parent()` calls up.
    manifest_dir
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root above copperclaw-host crate dir")
        .to_path_buf()
}

fn fixture_path(channel: &str, scenario: &str) -> PathBuf {
    workspace_root()
        .join("fixtures")
        .join(channel)
        .join(scenario)
}

async fn run_fixture(channel: &str, scenario: &str) {
    let path = fixture_path(channel, scenario);
    assert!(
        path.exists(),
        "fixture missing at {} — see docs/replay-fixtures.md",
        path.display()
    );
    let fixture = Fixture::load(&path).expect("load fixture");
    let mut harness = ReplayHarness::new(fixture).await.expect("boot harness");
    harness.run().await.expect("run harness");
    let report = harness.compare().expect("compare");
    assert!(report.is_clean(), "{report}");
}

/// Run a fixture and return the booted harness so the caller can make
/// channel-specific assertions on it (e.g. exact adapter delivery
/// count, `MockAdapter` state) on top of the JSONL diff.
async fn run_fixture_into_harness(channel: &str, scenario: &str) -> ReplayHarness {
    let path = fixture_path(channel, scenario);
    assert!(
        path.exists(),
        "fixture missing at {} — see docs/replay-fixtures.md",
        path.display()
    );
    let fixture = Fixture::load(&path).expect("load fixture");
    let mut harness = ReplayHarness::new(fixture).await.expect("boot harness");
    harness.run().await.expect("run harness");
    let report = harness.compare().expect("compare");
    assert!(report.is_clean(), "{report}");
    harness
}

/// Look up the captured `MockAdapter` for `channel_type` on a booted
/// `ReplayHarness`. Panics if the channel wasn't registered (every
/// channel-typed fixture should register an adapter via the harness's
/// `manifest.channel` + built-in set).
fn mock_for<'a>(
    harness: &'a ReplayHarness,
    channel_type: &str,
) -> &'a std::sync::Arc<copperclaw_channels_core::testing::MockAdapter> {
    let entry = harness
        .adapters
        .iter()
        .find(|(ct, _)| ct.as_str() == channel_type);
    match entry {
        Some((_, m)) => m,
        None => panic!("no MockAdapter registered for {channel_type}"),
    }
}

#[tokio::test]
async fn cli_text_reply_round_trip() {
    run_fixture("cli", "text-reply").await;
}

#[tokio::test]
async fn telegram_inbound_text_message_round_trip() {
    run_fixture("telegram", "inbound-text-message").await;
}

/// M18 C3 acceptance: telegram document attachment → staged →
/// router-materialized into the session inbox with a container-visible
/// `/data/inbox/...` path; a second, oversized document still yields
/// the `too_large` system fallback. See the fixture's README.md.
#[tokio::test]
async fn telegram_inbound_document_attachment_round_trip() {
    run_fixture("telegram", "inbound-document-attachment").await;
}

/// M18 C3 acceptance, file-readability half: beyond the JSONL diff
/// above, actually read the materialized bytes back off disk at the
/// exact host path `container_manager::spawn::build_spec` bind-mounts
/// as `/data` in production (`<session_root>/inbox/<msg_id>/<file>` ==
/// `/data/inbox/<msg_id>/<file>` once mounted) — the closest an
/// in-process harness (no real container) can get to "a runner reads
/// the file at /data/inbox/...".
#[tokio::test]
async fn telegram_inbound_document_attachment_file_readable_from_session_dir() {
    let harness = run_fixture_into_harness("telegram", "inbound-document-attachment").await;
    let (ag, sess) = harness.touched_sessions[0];
    let on_disk = harness
        .tempdir
        .path()
        .join("sessions")
        .join(ag.as_uuid().to_string())
        .join(sess.as_uuid().to_string())
        .join("inbox")
        .join("tg-doc-001")
        .join("spec.csv");
    let bytes = std::fs::read(&on_disk)
        .unwrap_or_else(|e| panic!("materialized attachment missing at {on_disk:?}: {e}"));
    assert_eq!(bytes, b"id,qty\n1,2\n");
}

/// M18 G1 acceptance (telegram callback): an Owner taps the Approve button
/// (`approve:<id>` callback) — the router-side interceptor resolves the
/// approval via the same DB path the CLI uses, edits the card in place, and
/// audits it; a second tap from a registered-but-unprivileged sender is
/// refused with a "not authorized" reply, and that approval stays live. No
/// inbound row is written for either tap. See the fixture's README.md.
#[tokio::test]
async fn telegram_approval_callback_round_trip() {
    use copperclaw_db::tables::pending_approvals::{self, ApprovalStatus};
    use copperclaw_types::ApprovalId;

    let harness = run_fixture_into_harness("telegram", "approval-callback").await;

    let approved =
        ApprovalId(uuid::Uuid::parse_str("00000000-0000-0000-0000-0000000000b1").unwrap());
    let refused =
        ApprovalId(uuid::Uuid::parse_str("00000000-0000-0000-0000-0000000000b2").unwrap());

    // Owner tap: approval resolved through the shared DB path, decision names
    // the human approver (not "host").
    let row = pending_approvals::get(&harness.central, approved).unwrap();
    assert_eq!(row.status, ApprovalStatus::Approved);
    let decs = pending_approvals::list_decisions(&harness.central, Some(approved), 10).unwrap();
    assert_eq!(decs.len(), 1);
    assert_eq!(decs[0].decided_by, "Owner Olivia");

    // Stranger tap: approval stays live, no decision logged.
    let live = pending_approvals::get(&harness.central, refused).unwrap();
    assert_eq!(live.status, ApprovalStatus::Pending);
    assert!(
        pending_approvals::list_decisions(&harness.central, Some(refused), 10)
            .unwrap()
            .is_empty()
    );

    // The Owner's card was edited in place to "Approved by <name>", addressed
    // by the persisted platform_message_id.
    let tg = mock_for(&harness, "telegram");
    let edits = tg.edits();
    assert_eq!(edits.len(), 1, "exactly one card edit");
    assert_eq!(edits[0].external_id, "tg-card-b1");
    assert_eq!(edits[0].new_text, "Approved by Owner Olivia");

    // Both taps are audited (one ok, one unauthorized) and neither wrote an
    // inbound row (asserted by the empty messages-in stream in the diff).
    let audits = copperclaw_db::tables::audit_log::list_recent(
        &harness.central,
        chrono::Utc::now() - chrono::Duration::hours(1),
        50,
    )
    .unwrap();
    assert!(
        audits
            .iter()
            .any(|a| a.command == "approvals.approve" && a.result == "ok")
    );
    assert!(
        audits
            .iter()
            .any(|a| a.result == "error" && a.error_code.as_deref() == Some("unauthorized"))
    );
}

/// M18 G1 acceptance (slack block_action): the same Owner-approves /
/// stranger-refused flow as the telegram fixture, but the callback payload
/// arrives under `content.callback.value` (Slack's shape) instead of `data`.
/// See the fixture's README.md.
#[tokio::test]
async fn slack_approval_block_action_round_trip() {
    use copperclaw_db::tables::pending_approvals::{self, ApprovalStatus};
    use copperclaw_types::ApprovalId;

    let harness = run_fixture_into_harness("slack", "approval-block-action").await;

    let approved =
        ApprovalId(uuid::Uuid::parse_str("00000000-0000-0000-0000-0000000000c1").unwrap());
    let refused =
        ApprovalId(uuid::Uuid::parse_str("00000000-0000-0000-0000-0000000000c2").unwrap());

    let row = pending_approvals::get(&harness.central, approved).unwrap();
    assert_eq!(row.status, ApprovalStatus::Approved);
    let decs = pending_approvals::list_decisions(&harness.central, Some(approved), 10).unwrap();
    assert_eq!(decs.len(), 1);
    assert_eq!(decs[0].decided_by, "Owner Olivia");

    let live = pending_approvals::get(&harness.central, refused).unwrap();
    assert_eq!(live.status, ApprovalStatus::Pending);
    assert!(
        pending_approvals::list_decisions(&harness.central, Some(refused), 10)
            .unwrap()
            .is_empty()
    );

    let slack = mock_for(&harness, "slack");
    let edits = slack.edits();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].external_id, "slack-ts-c1");
    assert_eq!(edits[0].new_text, "Approved by Owner Olivia");
}

/// M19 F3 acceptance (a + c). Owner taps Approve on a card that recorded no
/// `platform_message_id` (fallback-id path): the interceptor resolves it and
/// posts the outcome as a follow-up reply (asserted via the fixture's
/// `delivered` stream) rather than leaving live buttons. The same tap's
/// opportunistic expiry sweep stamps a separately-lapsed card terminal.
#[tokio::test]
async fn telegram_approval_resolution_fallback_and_expiry() {
    use copperclaw_db::tables::pending_approvals::{self, ApprovalStatus};
    use copperclaw_types::ApprovalId;

    let harness = run_fixture_into_harness("telegram", "approval-resolution").await;

    let fallback =
        ApprovalId(uuid::Uuid::parse_str("00000000-0000-0000-0000-0000000000d1").unwrap());
    let expired =
        ApprovalId(uuid::Uuid::parse_str("00000000-0000-0000-0000-0000000000d3").unwrap());

    // (a) The fallback-id approval resolved via the shared DB path even though
    // its card had no editable anchor (the follow-up reply is checked by the
    // fixture's `delivered` diff).
    assert_eq!(
        pending_approvals::get(&harness.central, fallback)
            .unwrap()
            .status,
        ApprovalStatus::Approved
    );

    // (c) The lapsed approval was swept to `expired` and its card stamped
    // terminal by the opportunistic sweep the tap triggered.
    assert_eq!(
        pending_approvals::get(&harness.central, expired)
            .unwrap()
            .status,
        ApprovalStatus::Expired
    );
    let tg = mock_for(&harness, "telegram");
    let edits = tg.edits();
    let stamped = edits
        .iter()
        .find(|e| e.external_id == "tg-exp-card")
        .expect("expired card was stamped terminal");
    assert!(
        stamped.new_text.contains("expired"),
        "expired card must carry a terminal 'expired' note; got: {}",
        stamped.new_text
    );
    // The fallback-id approval had no card, so it is NOT edited — only the
    // lapsed card is.
    assert_eq!(edits.len(), 1, "exactly the one expired-card stamp");
}

/// M19 F3 acceptance (b). A tap on an already-resolved approval is not silent:
/// the loser is told who resolved it. Asserted via the fixture's `delivered`
/// stream ("This request was already resolved by host.").
#[tokio::test]
async fn slack_approval_conflict_already_resolved() {
    use copperclaw_db::tables::pending_approvals::{self, ApprovalStatus};
    use copperclaw_types::ApprovalId;

    let harness = run_fixture_into_harness("slack", "approval-conflict").await;

    let already =
        ApprovalId(uuid::Uuid::parse_str("00000000-0000-0000-0000-0000000000e1").unwrap());
    // Still exactly one decision (the original winner's); the loser tap did not
    // re-resolve or re-edit.
    assert_eq!(
        pending_approvals::get(&harness.central, already)
            .unwrap()
            .status,
        ApprovalStatus::Approved
    );
    assert_eq!(
        pending_approvals::list_decisions(&harness.central, Some(already), 10)
            .unwrap()
            .len(),
        1
    );
    let slack = mock_for(&harness, "slack");
    assert!(
        slack.edits().is_empty(),
        "loser tap must not re-edit the card"
    );
}

#[tokio::test]
async fn slack_event_message_round_trip() {
    run_fixture("slack", "event-message").await;
}

/// M18 C4a acceptance: a Slack `url_private` file is staged by the adapter
/// (bot-token download, size-capped) and materialized by the router into
/// the resolved session's inbox at the container-visible `/data/inbox/...`
/// path; a second, oversized file still yields the `too_large` system
/// fallback. See the fixture's README.md.
#[tokio::test]
async fn slack_inbound_file_attachment_round_trip() {
    run_fixture("slack", "inbound-file-attachment").await;
}

#[tokio::test]
async fn cli_multi_turn_round_trip() {
    run_fixture("cli", "multi-turn").await;
}

#[tokio::test]
async fn discord_inbound_message_round_trip() {
    run_fixture("discord", "inbound-message").await;
}

/// M18 C4b acceptance: a discord CDN attachment is staged by the adapter →
/// router-materialized into the resolved session's inbox with a
/// container-visible `/data/inbox/...` path; a second, oversized attachment
/// still yields the `too_large` system fallback. See the fixture's README.md.
#[tokio::test]
async fn discord_inbound_file_attachment_round_trip() {
    run_fixture("discord", "inbound-file-attachment").await;
}

/// M18 C4b acceptance, file-readability half: read the materialized bytes
/// back off disk at the exact host path `container_manager::spawn::build_spec`
/// bind-mounts as `/data` in production — the closest an in-process harness
/// can get to "a runner reads the file at /data/inbox/...".
#[tokio::test]
async fn discord_inbound_file_attachment_file_readable_from_session_dir() {
    let harness = run_fixture_into_harness("discord", "inbound-file-attachment").await;
    let (ag, sess) = harness.touched_sessions[0];
    let on_disk = harness
        .tempdir
        .path()
        .join("sessions")
        .join(ag.as_uuid().to_string())
        .join(sess.as_uuid().to_string())
        .join("inbox")
        .join("dc-doc-001")
        .join("spec.csv");
    let bytes = std::fs::read(&on_disk)
        .unwrap_or_else(|e| panic!("materialized attachment missing at {on_disk:?}: {e}"));
    assert_eq!(bytes, b"id,qty\n1,2\n");
}

#[tokio::test]
async fn matrix_room_message_round_trip() {
    run_fixture("matrix", "room-message").await;
}

#[tokio::test]
async fn github_webhook_issue_comment_round_trip() {
    run_fixture("github", "webhook-issue-comment").await;
}

#[tokio::test]
async fn webhooks_generic_hmac_round_trip() {
    run_fixture("webhooks", "generic-hmac").await;
}

#[tokio::test]
async fn cli_tool_use_shell() {
    run_fixture("cli", "tool-use-shell").await;
}

/// Empty-content LLM response: runner completes the inbound without
/// emitting a chat outbound. Pins the no-content branch in `drive_turn`
/// so a regression that crashed on empty responses would surface.
#[tokio::test]
async fn cli_empty_llm_response() {
    run_fixture("cli", "empty-llm-response").await;
}

/// Provider 5xx + retry. The runner wraps `provider.query()` in an
/// exponential-backoff retry loop honouring
/// [`ProviderError::is_retryable`]: the first 503 reissues the call,
/// the second response succeeds, and the inbound completes normally.
#[tokio::test]
async fn cli_provider_5xx_retry() {
    run_fixture("cli", "provider-5xx-retry").await;
}

/// Provider timeout. The wiremock mock delays its response past the
/// runner's per-call deadline; the runner retries up to
/// `MAX_PROVIDER_ATTEMPTS` times, each time hitting the deadline, then
/// gives up and marks the inbound failed.
#[tokio::test]
async fn cli_provider_timeout() {
    run_fixture("cli", "provider-timeout").await;
}

#[tokio::test]
async fn cli_sender_not_approved() {
    run_fixture("cli", "sender-not-approved").await;
}

#[tokio::test]
async fn cli_budget_exhausted() {
    run_fixture("cli", "budget-exhausted").await;
}

#[tokio::test]
async fn cli_scheduled_wake() {
    run_fixture("cli", "scheduled-wake").await;
}

// ---- M18 R1: end-user slash commands (fixture per command, cli + telegram) ----

/// `/stop` persists a control{op:stop} row (kind=system, trigger=0,
/// status pending for R2); no runner turn, no outbound, no delivery.
#[tokio::test]
async fn cli_slash_stop_control_row() {
    run_fixture("cli", "slash-stop").await;
}

/// `/status` is answered by the host from central-DB state: no
/// messages_in row, no runner turn, one synthesized outbound + delivery.
#[tokio::test]
async fn cli_slash_status_host_answer() {
    run_fixture("cli", "slash-status").await;
}

/// `/clear` wires through to the runner's clear-history sentinel: no
/// LLM turn, one confirmation outbound, inbound marked completed.
#[tokio::test]
async fn cli_slash_clear_runner_sentinel() {
    run_fixture("cli", "slash-clear").await;
}

/// `/compact` wires through to the runner's compaction sentinel: short
/// history is a no-op, no LLM turn, one confirmation outbound.
#[tokio::test]
async fn cli_slash_compact_runner_sentinel() {
    run_fixture("cli", "slash-compact").await;
}

/// Telegram twin of `cli_slash_stop_control_row`, in a mention-gated
/// group with no mention — routes only via the command bypass.
#[tokio::test]
async fn telegram_slash_stop_control_row_bypasses_mention_gate() {
    run_fixture("telegram", "slash-stop").await;
}

/// Telegram twin of `cli_slash_status_host_answer` (gated group).
#[tokio::test]
async fn telegram_slash_status_host_answer_bypasses_mention_gate() {
    run_fixture("telegram", "slash-status").await;
}

/// M19 U7: a 👍 reaction in a mention-gated telegram group bypasses the gate
/// (interaction payload) and persists a non-trigger `content.reaction` row —
/// pending for the runner's R2 steering seam — with no runner turn, outbound,
/// or delivery. The runner-side steering is covered by drive_turn unit tests;
/// this fixture owns the inbound → router leg (twin of `slash-stop`).
#[tokio::test]
async fn telegram_reaction_steer_bypasses_mention_gate() {
    run_fixture("telegram", "reaction-steer").await;
}

/// Telegram twin of `cli_slash_clear_runner_sentinel`; also pins the
/// `@BotName` suffix + case normalisation (`/CLEAR@ReplayBot`).
#[tokio::test]
async fn telegram_slash_clear_normalises_bot_suffix() {
    run_fixture("telegram", "slash-clear").await;
}

/// Telegram twin of `cli_slash_compact_runner_sentinel` (gated group).
#[tokio::test]
async fn telegram_slash_compact_runner_sentinel() {
    run_fixture("telegram", "slash-compact").await;
}

/// D2 e2e: after a real replayed turn (per-session DBs on disk, WAL
/// outbound), `sessions.get` attaches the recent message rows its help
/// text promises and `sessions.tail` returns the merged, time-ordered
/// rows plus seq cursors that make `--follow` incremental.
#[tokio::test]
async fn cli_text_reply_sessions_get_and_tail() {
    use copperclaw_cclaw::Caller;
    use copperclaw_host::handlers::sessions as sessions_handlers;
    use copperclaw_host::socket::HandlerCtx;

    let harness = run_fixture_into_harness("cli", "text-reply").await;
    let (_ag, sess) = harness.touched_sessions[0];
    let ctx = HandlerCtx::with_data_dir(harness.central.clone(), harness.tempdir.path());
    let args = serde_json::json!({"id": sess.as_uuid().to_string()});

    // sessions.get: session row + last inbound/outbound rows.
    let v = sessions_handlers::get(&args, &Caller::Host, &ctx).unwrap();
    assert_eq!(v["id"].as_str().unwrap(), sess.as_uuid().to_string());
    let inbound = v["recent_inbound"].as_array().unwrap();
    assert_eq!(inbound.len(), 1, "one inbound chat row: {inbound:?}");
    assert_eq!(inbound[0]["preview"], "hello");
    assert_eq!(inbound[0]["kind"], "chat");
    assert_eq!(inbound[0]["status"], "completed");
    let outbound = v["recent_outbound"].as_array().unwrap();
    assert!(
        outbound.iter().any(|r| r["preview"] == "Hello back!"),
        "outbound rows must include the chat reply: {outbound:?}"
    );

    // sessions.tail: merged, time-ordered, inbound first.
    let t = sessions_handlers::tail(&args, &Caller::Host, &ctx).unwrap();
    let rows = t["rows"].as_array().unwrap();
    assert!(
        rows.len() >= 2,
        "expected inbound + outbound rows: {rows:?}"
    );
    assert_eq!(rows[0]["direction"], "in");
    assert_eq!(rows[0]["preview"], "hello");
    let reply = rows
        .iter()
        .find(|r| r["preview"] == "Hello back!")
        .expect("tail must show the outbound reply");
    assert_eq!(reply["direction"], "out");
    // Timestamps are non-decreasing (merged order).
    let ts: Vec<&str> = rows.iter().map(|r| r["ts"].as_str().unwrap()).collect();
    let mut sorted = ts.clone();
    sorted.sort_unstable();
    assert_eq!(ts, sorted, "tail rows must be time-ordered");

    // Cursoring: replaying with the returned seqs yields nothing new.
    let again = sessions_handlers::tail(
        &serde_json::json!({
            "id": sess.as_uuid().to_string(),
            "since_in_seq": t["last_in_seq"],
            "since_out_seq": t["last_out_seq"],
        }),
        &Caller::Host,
        &ctx,
    )
    .unwrap();
    assert!(again["rows"].as_array().unwrap().is_empty());
}

/// Telegram outbound text exceeding the adapter's 4096-char cap is
/// split by the delivery loop into two paragraph-bounded chunks before
/// reaching the adapter (slice-1 chat-text splitter). Beyond the JSONL
/// diff this also pins the EXACT chunk count and the per-chunk char
/// count so a regression that double-splits, drops a chunk, or stops
/// honouring the paragraph boundary surfaces directly here.
#[tokio::test]
async fn telegram_long_message_split_paragraph_boundary() {
    let harness = run_fixture_into_harness("telegram", "long-message-split").await;
    let mock = mock_for(&harness, "telegram");
    let deliveries = mock.deliveries();
    assert_eq!(
        deliveries.len(),
        2,
        "expected splitter to produce 2 telegram deliveries, got {}",
        deliveries.len()
    );
    for (i, d) in deliveries.iter().enumerate() {
        let text = d
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("chunk {i} missing text"));
        assert_eq!(
            text.chars().count(),
            2500,
            "telegram chunk {i} should be exactly 2500 chars (paragraph length)"
        );
        assert!(
            text.chars().count() <= 4096,
            "telegram chunk {i} exceeds the 4096-char cap"
        );
    }
}

/// Slack outbound text exceeding the adapter's 40 000-char cap splits
/// into two equal paragraph chunks of 25 000 chars each. Mirrors the
/// telegram variant — different cap, same splitter contract.
#[tokio::test]
async fn slack_long_message_split_paragraph_boundary() {
    let harness = run_fixture_into_harness("slack", "long-message-split").await;
    let mock = mock_for(&harness, "slack");
    let deliveries = mock.deliveries();
    assert_eq!(
        deliveries.len(),
        2,
        "expected splitter to produce 2 slack deliveries, got {}",
        deliveries.len()
    );
    for (i, d) in deliveries.iter().enumerate() {
        let text = d
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("chunk {i} missing text"));
        assert_eq!(text.chars().count(), 25_000, "slack chunk {i} length");
        assert!(
            text.chars().count() <= 40_000,
            "slack chunk {i} exceeds cap"
        );
    }
}

/// Discord's 2000-char cap is the tightest of the mainstream channels;
/// the splitter still cuts at the paragraph boundary, producing two
/// 1200-char chunks for the same shape of fixture.
#[tokio::test]
async fn discord_long_message_split_paragraph_boundary() {
    let harness = run_fixture_into_harness("discord", "long-message-split").await;
    let mock = mock_for(&harness, "discord");
    let deliveries = mock.deliveries();
    assert_eq!(
        deliveries.len(),
        2,
        "expected splitter to produce 2 discord deliveries, got {}",
        deliveries.len()
    );
    for (i, d) in deliveries.iter().enumerate() {
        let text = d
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("chunk {i} missing text"));
        assert_eq!(text.chars().count(), 1200, "discord chunk {i} length");
        assert!(
            text.chars().count() <= 2000,
            "discord chunk {i} exceeds cap"
        );
    }
}

/// Telegram outbound whose text is an intro line plus a fenced python
/// code block, the fence alone exceeding the 4096-char cap. Pins the
/// fence-aware splitter (M18/C2): the intro is cut off before the
/// fence, the fence is closed at the mid-block cut and reopened with
/// the same info string on the next chunk. Beyond the JSONL diff this
/// asserts the exact chunk count, the per-chunk cap, and — the point
/// of the card — that every delivered chunk parses with balanced
/// fences (an even number of fence-marker lines, so no chunk leaves a
/// code block dangling open in the Telegram rendering).
#[tokio::test]
async fn telegram_long_code_reply_fence_balanced() {
    let harness = run_fixture_into_harness("telegram", "long-code-reply").await;
    let mock = mock_for(&harness, "telegram");
    let deliveries = mock.deliveries();
    assert_eq!(
        deliveries.len(),
        3,
        "expected intro + close/reopen fence chunks (3 deliveries), got {}",
        deliveries.len()
    );
    for (i, d) in deliveries.iter().enumerate() {
        let text = d
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("chunk {i} missing text"));
        assert!(
            text.chars().count() <= 4096,
            "telegram chunk {i} exceeds the 4096-char cap"
        );
        let fence_lines = text
            .lines()
            .filter(|l| l.trim_start().starts_with("```"))
            .count();
        assert_eq!(
            fence_lines % 2,
            0,
            "chunk {i} has unbalanced code fences ({fence_lines} fence lines):\n{text}"
        );
    }
    // The reopened chunk must carry the original info string.
    let last = deliveries[2].message.content["text"].as_str().unwrap();
    assert!(
        last.starts_with("```python\n"),
        "reopened chunk must restore the ```python info string: {last}"
    );
    assert!(last.ends_with("```"), "final chunk must close the fence");
}

/// Telegram adapter's first `deliver` returns `Rate { retry_after: 1 }`.
/// The delivery loop defers the row, the harness sleeps 1200 ms (past
/// the 1 s retry_after window), and the second `process_session_once`
/// pass delivers successfully. Beyond the JSONL diff this pins exactly
/// ONE adapter delivery in the captured `MockAdapter` log (the failed
/// first attempt does not register, only the successful second one
/// does) — and the elapsed wall time is at least `redrive_after_ms`,
/// implicitly pinning the slice-1 contract that `retry_after` is
/// honoured (a regression that ignored it and used the default 5 s
/// exponential schedule would leave the row deferred and produce zero
/// deliveries).
#[tokio::test]
async fn telegram_rate_limited_retry_honours_retry_after() {
    let started = std::time::Instant::now();
    let harness = run_fixture_into_harness("telegram", "rate-limited-retry").await;
    let elapsed = started.elapsed();
    let mock = mock_for(&harness, "telegram");
    let deliveries = mock.deliveries();
    assert_eq!(
        deliveries.len(),
        1,
        "expected exactly 1 successful deliver after rate-limit retry, got {}",
        deliveries.len()
    );
    assert_eq!(
        deliveries[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str()),
        Some("Hi after rate-limit retry"),
    );
    // Sanity: the fixture's redrive sleeps 1200 ms; the harness must
    // have observed that much wall time at minimum. (Tolerance for CI
    // jitter: we only assert >= 1000 ms — anything shorter than the
    // 1 s retry_after window would mean the second tick fired too
    // early, which would also fail the deliveries-length assertion
    // above.)
    assert!(
        elapsed >= std::time::Duration::from_millis(1000),
        "rate-limit fixture finished in {elapsed:?}, expected >= 1000 ms"
    );
}

/// M18 X1: the program-acceptance golden-path fixture. "build me a tiny
/// HTTP todo app" drives an 8-round scripted tool loop (git init,
/// scaffold + verify a stdlib Python HTTP todo server, commit, expose a
/// mock-brokered preview, send the P3 ritual card, close with a summary).
/// See `fixtures/cli/prototype-golden/README.md` for the two pieces of
/// the card's acceptance line this fixture can NOT honestly exercise
/// (the R3 verify-gate and the H1 live HUD) and the precise, diagnosed
/// reason for each.
#[tokio::test]
async fn cli_prototype_golden_path() {
    if std::env::var_os("COPPERCLAW_X2_GENERATE").is_some() {
        // Fixture-authoring path (X2): regenerate expected/*.jsonl from a
        // real run. Never taken under a normal `cargo test`.
        let path = fixture_path("cli", "prototype-golden");
        let fixture = Fixture::load(&path).expect("load fixture");
        let mut harness = ReplayHarness::new(fixture).await.expect("boot harness");
        harness.run().await.expect("run harness");
        harness.dump_expected_jsonl();
        return;
    }
    run_fixture("cli", "prototype-golden").await;
}

/// M18 X2 (P3 ritual card shape): beyond the byte-stable JSONL diff, pin
/// the exact shape of the closing "prototype ready" ritual — the P3
/// `send_card` (title + one-liner body + a "What to try" field + the
/// `artifact_path` host-path footer field + an Open-preview URL button)
/// AND the screenshot delivered alongside it via `send_file`. P3 teaches
/// this ritual in the prompt/skills; X2 owns asserting the delivered
/// shape here (the coordination is one-way — P3 never touches
/// fixtures/replay.rs).
#[tokio::test]
async fn cli_prototype_golden_ritual_card_and_screenshot_shape() {
    let harness = run_fixture_into_harness("cli", "prototype-golden").await;
    let cli = mock_for(&harness, "cli");
    let deliveries = cli.deliveries();

    // The screenshot rides its own message ahead of the card (cards can't
    // attach a local PNG — P3's "send_file alongside" rule).
    let screenshot = deliveries
        .iter()
        .find(|d| {
            d.message
                .content
                .get("files")
                .and_then(|f| f.as_array())
                .is_some_and(|arr| {
                    arr.iter()
                        .any(|f| f.get("filename").and_then(|n| n.as_str()) == Some("preview.png"))
                })
        })
        .expect("a preview.png screenshot must be delivered alongside the ritual card");
    assert_eq!(
        screenshot
            .message
            .content
            .get("text")
            .and_then(|t| t.as_str()),
        Some("Preview screenshot of the running todo app."),
        "screenshot caption",
    );

    // The ritual card renders through the cli text-fallback with every
    // required element: title, one-liner, "What to try", the artifact_path
    // host-path footer, and the Open-preview button.
    let card_text = deliveries
        .iter()
        .filter_map(|d| d.message.content.get("text").and_then(|t| t.as_str()))
        .find(|t| t.contains("Todo app is ready"))
        .expect("the ritual card must be delivered");
    assert!(
        card_text.contains("**Todo app is ready**"),
        "card must carry the title as a headline: {card_text}",
    );
    assert!(
        card_text.contains("stdlib-only HTTP todo server"),
        "card must carry the one-liner body: {card_text}",
    );
    assert!(
        card_text.contains("What to try:"),
        "card must carry the What-to-try field: {card_text}",
    );
    assert!(
        card_text.contains("Project path: /tmp/copperclaw-x1-golden/todo-app"),
        "card must carry the artifact_path host-path footer: {card_text}",
    );
    assert!(
        card_text.contains("[Open preview] -> http://192.0.2.10:8100/__preview/fixture-tok-8000"),
        "card must carry the Open-preview URL button: {card_text}",
    );
}

// ---- M18 X2: the R3 verify-gate refuse -> fix -> pass loop ----
//
// X1 could not exercise this leg because `verify_gate::data_root()` /
// `todo.rs`'s path resolution were hardcoded to `/data` (an unwritable
// root-owned path on any host running the suite). T2 added the
// unconditional `COPPERCLAW_DATA_ROOT` override; this test points the
// in-process runner's gate at a writable per-run dir through it.
//
// The catch: the workspace `forbid(unsafe_code)` (applied to this
// integration-test target via `[lints] workspace = true`) makes
// `std::env::set_var` unavailable, and the gate reads its root from
// process env at call time with no per-instance seam. So instead of
// mutating our own env we re-exec THIS test binary as a child with the
// var set via the safe `Command::env`, and the child runs the real
// `ReplayHarness` against the `prototype-verify-gate` fixture. The gate
// then genuinely engages against real, writable project state.

/// Fixed data root for the verify-gate fixture. The fixture's claude
/// turns hard-code project paths under this dir, so it must be a stable,
/// known path (same `/tmp` convention `prototype-golden` already uses for
/// its project dir).
const X2_VERIFY_DATA_ROOT: &str = "/tmp/copperclaw-x2-verify-gate";
/// Set on the re-exec'd child so it runs the scenario instead of
/// re-spawning itself.
const X2_CHILD_ENV: &str = "COPPERCLAW_X2_VERIFY_GATE_CHILD";
/// `assert` (default) or `dump` — the latter prints the captured actual
/// streams as JSONL so `expected/*.jsonl` can be regenerated from a real
/// run (`COPPERCLAW_X2_GENERATE=1 cargo test ... cli_prototype_verify_gate`).
const X2_MODE_ENV: &str = "COPPERCLAW_X2_VERIFY_GATE_MODE";

#[tokio::test]
async fn cli_prototype_verify_gate_refuse_fix_pass() {
    // Child leg: env already set by the parent's re-exec. Run the real
    // scenario against the gate rooted at X2_VERIFY_DATA_ROOT.
    if std::env::var_os(X2_CHILD_ENV).is_some() {
        let dump = std::env::var(X2_MODE_ENV).ok().as_deref() == Some("dump");
        run_verify_gate_child(dump).await;
        return;
    }

    // Parent leg: re-exec ourselves with COPPERCLAW_DATA_ROOT set (via the
    // safe Command::env — set_var is unavailable under forbid(unsafe_code)).
    let _ = std::fs::remove_dir_all(X2_VERIFY_DATA_ROOT);
    std::fs::create_dir_all(X2_VERIFY_DATA_ROOT).expect("create x2 verify-gate data root");

    let mode = if std::env::var_os("COPPERCLAW_X2_GENERATE").is_some() {
        "dump"
    } else {
        "assert"
    };
    let exe = std::env::current_exe().expect("current_exe");
    let output = std::process::Command::new(exe)
        .args([
            "--exact",
            "cli_prototype_verify_gate_refuse_fix_pass",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(X2_CHILD_ENV, "1")
        .env(X2_MODE_ENV, mode)
        .env("COPPERCLAW_DATA_ROOT", X2_VERIFY_DATA_ROOT)
        .output()
        .expect("spawn verify-gate re-exec child");

    let _ = std::fs::remove_dir_all(X2_VERIFY_DATA_ROOT);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if mode == "dump" {
        // Generation run: surface the child's dumped JSONL to the operator.
        println!("{stdout}");
    }
    assert!(
        output.status.success(),
        "verify-gate child failed (status {:?})\n\
         --- child stdout ---\n{stdout}\n--- child stderr ---\n{stderr}",
        output.status.code(),
    );
}

/// The child scenario: drive the `prototype-verify-gate` fixture through
/// the real harness. In `dump` mode, print the captured actuals (fixture
/// authoring). Otherwise diff against the committed `expected/*.jsonl` and
/// assert the refuse -> fix -> pass shape genuinely occurred.
async fn run_verify_gate_child(dump: bool) {
    let path = fixture_path("cli", "prototype-verify-gate");
    assert!(
        path.exists(),
        "fixture missing at {} — see docs/replay-fixtures.md",
        path.display()
    );
    let fixture = Fixture::load(&path).expect("load verify-gate fixture");
    let mut harness = ReplayHarness::new(fixture).await.expect("boot harness");
    harness.run().await.expect("run harness");

    if dump {
        harness.dump_expected_jsonl();
        return;
    }

    // Byte-stable pipeline diff first (expected/*.jsonl).
    let report = harness.compare().expect("compare");
    assert!(report.is_clean(), "{report}");

    // The refusal messages are handed back to the model as tool_result
    // blocks, so they ride along in the provider request bodies captured
    // by the wiremock server. Concatenate every received request body and
    // assert the refuse -> fix -> pass shape.
    let reqs = harness
        .anthropic_server
        .received_requests()
        .await
        .expect("wiremock recorded received requests");
    let bodies: String = reqs
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect::<Vec<_>>()
        .join("\n");

    // Completion refused while the project was dirty, naming the project
    // and the recorded verify command.
    assert!(
        bodies.contains("unverified changes"),
        "expected the verify-gate refusal in a tool_result body",
    );
    assert!(
        bodies.contains("python3 -m py_compile app.py"),
        "refusal must name the recorded verify command",
    );
    // The failing verify run's stderr (a Python SyntaxError) reached the
    // model — "refused WITH stderr", per the card.
    assert!(
        bodies.contains("SyntaxError"),
        "failing verify run's stderr must reach the model",
    );
    // Both fix-cycle counts appear across the two refusals (2 remaining,
    // then 1) — proof the loop advanced through a failed verify rather
    // than refusing statically.
    assert!(
        bodies.contains("2 fix cycle(s) remaining"),
        "first refusal reports the full fix-cycle budget",
    );
    assert!(
        bodies.contains("1 fix cycle(s) remaining"),
        "second refusal reports the burned-down fix-cycle budget",
    );

    // The pass: the todo store (also resolved under COPPERCLAW_DATA_ROOT)
    // shows the item genuinely completed once the verify passed.
    let todo_store = std::path::Path::new(X2_VERIFY_DATA_ROOT).join("agent_todos.json");
    let raw = std::fs::read_to_string(&todo_store)
        .unwrap_or_else(|e| panic!("todo store missing at {}: {e}", todo_store.display()));
    let todos: serde_json::Value = serde_json::from_str(&raw).expect("todo store is JSON");
    let first = &todos.as_array().expect("todo store is an array")[0];
    assert_eq!(
        first["status"], "completed",
        "the todo must end completed once the verify passed: {todos}",
    );

    // The passing verify run cleared the project's dirty marker.
    let dirty = std::path::Path::new(X2_VERIFY_DATA_ROOT).join("proj/.copperclaw/dirty");
    assert!(
        !dirty.exists(),
        "passing verify must clear the dirty marker at {}",
        dirty.display(),
    );
}
