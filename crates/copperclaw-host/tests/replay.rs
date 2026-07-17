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

/// M19 F1 acceptance (matrix live HUD edit): F1 reconciled matrix's trait
/// `edit_message` with its long-standing `EDIT_CAPABLE_CHANNELS` listing,
/// so the runner's Task HUD now runs *live* on matrix — it posts one
/// breadcrumb chip at the first tool call and edits that chip in place on
/// every later frame instead of degrading to new-message spam.
///
/// The replay harness substitutes a `MockAdapter` for the real matrix
/// adapter, so `model_rich_breadcrumbs` in the manifest makes the wrapper
/// model matrix's real `deliver_breadcrumb` contract (post once, edit in
/// place). Beyond the byte-stable JSONL diff this pins the point of the
/// card: exactly ONE breadcrumb chip is posted, and every subsequent HUD
/// frame is an in-place `edit_message` against that single anchor — the
/// host delivery pipeline (`dispatch_breadcrumb` +
/// `handle_update_breadcrumb` + `lookup_prior_breadcrumb_external_id`)
/// threads the anchor id through so the chip is edited, never re-posted.
/// Combined with F1's drift guard (matrix really overrides trait
/// `edit_message`) and the matrix adapter's own unit tests, this closes
/// the "advertised edit-capable but silently re-posts" gap. See the
/// fixture's README.md.
#[tokio::test]
async fn matrix_hud_live_edit_edits_one_chip_in_place() {
    // Fixture-authoring path: regenerate expected/*.jsonl from a real run.
    // Never taken under a normal `cargo test`.
    if std::env::var_os("COPPERCLAW_XR1_GENERATE").is_some() {
        let path = fixture_path("matrix", "hud-live-edit");
        let fixture = Fixture::load(&path).expect("load fixture");
        let mut harness = ReplayHarness::new(fixture).await.expect("boot harness");
        harness.run().await.expect("run harness");
        harness.dump_expected_jsonl();
        return;
    }

    let harness = run_fixture_into_harness("matrix", "hud-live-edit").await;
    let mx = mock_for(&harness, "matrix");

    // The HUD posts exactly ONE breadcrumb chip (a `Breadcrumb`-kind
    // delivery). Anything more would be the re-post spam F1 kills.
    let deliveries = mx.deliveries();
    let posts: Vec<_> = deliveries
        .iter()
        .filter(|d| d.message.kind.as_str() == "breadcrumb")
        .collect();
    assert_eq!(
        posts.len(),
        1,
        "the live HUD must post exactly one breadcrumb chip on matrix, got {}: {deliveries:?}",
        posts.len(),
    );
    assert_eq!(
        posts[0].platform_id, "!a:m.org",
        "the chip must land in the originating matrix room",
    );

    // Every later frame is an in-place edit of that ONE chip: at least one
    // edit, all addressed to a single anchor id (never a fresh post).
    let edits = mx.edits();
    assert!(
        !edits.is_empty(),
        "the live HUD must edit the chip in place at least once, got zero edits",
    );
    let anchors: std::collections::BTreeSet<&str> =
        edits.iter().map(|e| e.external_id.as_str()).collect();
    assert_eq!(
        anchors.len(),
        1,
        "every HUD edit must target the SINGLE posted chip (one anchor), got {anchors:?}",
    );
    for e in &edits {
        assert_eq!(
            e.platform_id, "!a:m.org",
            "each edit must target the originating matrix room",
        );
    }

    // Sanity: the model's final answer still reaches the user as its own
    // chat message (the HUD chip is progress, not the reply).
    assert!(
        deliveries.iter().any(|d| d.message.kind.as_str() == "chat"
            && d.message
                .content
                .get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.contains("build is fine"))),
        "the final chat answer must be delivered alongside the HUD chip: {deliveries:?}",
    );
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

/// M21 S6: the previously-unfixturable bare-channel Task HUD timed legs
/// (the M18 X2 known gap), pinned deterministically via the harness's
/// test-clock seam. The fixture's `provider_responses` advance the
/// shared runner `TestClock` mid-turn (61s while serving the first
/// scripted `tool_use` call, another 90s on the second), so the two
/// tool-batch boundaries land at exactly 61s and 151s of "wall" time
/// with zero real waiting. The expected streams pin the 60s first-fire
/// status row ("61s in ... I'll keep going.") and the 150s softening
/// ("151s in ... taking longer than usual") byte-for-byte.
#[tokio::test]
async fn cli_status_row_heartbeat_pins_60s_and_150s_legs() {
    let harness = run_fixture_into_harness("cli", "status-row-heartbeat").await;
    // Belt-and-braces over the JSONL diff: exactly two heartbeat rows
    // were delivered, in cadence order, and only the second is softened.
    let mock = mock_for(&harness, "cli");
    let heartbeats: Vec<String> = mock
        .deliveries()
        .iter()
        .filter_map(|d| d.message.content["text"].as_str().map(str::to_owned))
        .filter(|t| t.starts_with("Still working on this"))
        .collect();
    assert_eq!(heartbeats.len(), 2, "one row per elapsed 60s window");
    assert!(heartbeats[0].contains("61s in") && heartbeats[0].ends_with("I'll keep going."));
    assert!(
        heartbeats[1].contains("151s in")
            && heartbeats[1].contains("This is taking longer than usual"),
        "the 150s leg softens: {}",
        heartbeats[1]
    );
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

// ---- M20 X-rider (Wave 1): the Q2 multi-stage verify gate's refuse ->
// narrow -> pass loop ----
//
// Sibling to the M18 X2 fixture above, which pins the pre-Q2 single-
// command gate byte-for-byte. This one proves the Q2 extension: a
// `.copperclaw/verify` with THREE named stages (lint/typecheck/test),
// where the todo-completion refusal narrows to name only the stages that
// haven't yet recorded green — not a single project-wide pass/fail bit.
// Uses the identical re-exec seam (`COPPERCLAW_DATA_ROOT`) X2 built,
// since `forbid(unsafe_code)` still blocks `std::env::set_var` here.

/// Fixed data root for the multi-stage verify fixture — a distinct `/tmp`
/// path from [`X2_VERIFY_DATA_ROOT`] so the two fixtures' re-exec'd child
/// processes never collide if a future change runs them concurrently.
const M20X1_STAGES_DATA_ROOT: &str = "/tmp/copperclaw-m20x1-verify-stages";
/// Set on the re-exec'd child so it runs the scenario instead of
/// re-spawning itself.
const M20X1_CHILD_ENV: &str = "COPPERCLAW_M20X1_VERIFY_STAGES_CHILD";
/// `assert` (default) or `dump` — the latter prints the captured actual
/// streams as JSONL so `expected/*.jsonl` can be regenerated from a real
/// run (`COPPERCLAW_X2_GENERATE=1 cargo test ... cli_prototype_verify_gate_multistage`).
const M20X1_MODE_ENV: &str = "COPPERCLAW_M20X1_VERIFY_STAGES_MODE";

#[tokio::test]
async fn cli_prototype_verify_gate_multistage_refuse_narrow_pass() {
    // Child leg: env already set by the parent's re-exec. Run the real
    // scenario against the gate rooted at M20X1_STAGES_DATA_ROOT.
    if std::env::var_os(M20X1_CHILD_ENV).is_some() {
        let dump = std::env::var(M20X1_MODE_ENV).ok().as_deref() == Some("dump");
        run_verify_gate_multistage_child(dump).await;
        return;
    }

    // Parent leg: re-exec ourselves with COPPERCLAW_DATA_ROOT set (via the
    // safe Command::env — set_var is unavailable under forbid(unsafe_code)).
    let _ = std::fs::remove_dir_all(M20X1_STAGES_DATA_ROOT);
    std::fs::create_dir_all(M20X1_STAGES_DATA_ROOT).expect("create m20x1 verify-stages data root");

    let mode = if std::env::var_os("COPPERCLAW_X2_GENERATE").is_some() {
        "dump"
    } else {
        "assert"
    };
    let exe = std::env::current_exe().expect("current_exe");
    let output = std::process::Command::new(exe)
        .args([
            "--exact",
            "cli_prototype_verify_gate_multistage_refuse_narrow_pass",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(M20X1_CHILD_ENV, "1")
        .env(M20X1_MODE_ENV, mode)
        .env("COPPERCLAW_DATA_ROOT", M20X1_STAGES_DATA_ROOT)
        .output()
        .expect("spawn verify-stages re-exec child");

    let _ = std::fs::remove_dir_all(M20X1_STAGES_DATA_ROOT);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if mode == "dump" {
        // Generation run: surface the child's dumped JSONL to the operator.
        println!("{stdout}");
    }
    assert!(
        output.status.success(),
        "verify-stages child failed (status {:?})\n\
         --- child stdout ---\n{stdout}\n--- child stderr ---\n{stderr}",
        output.status.code(),
    );
}

/// The child scenario: drive the `prototype-verify-gate-multistage`
/// fixture through the real harness. In `dump` mode, print the captured
/// actuals (fixture authoring). Otherwise diff against the committed
/// `expected/*.jsonl` and assert the refuse -> narrow -> pass shape
/// genuinely occurred.
async fn run_verify_gate_multistage_child(dump: bool) {
    let path = fixture_path("cli", "prototype-verify-gate-multistage");
    assert!(
        path.exists(),
        "fixture missing at {} — see docs/replay-fixtures.md",
        path.display()
    );
    let fixture = Fixture::load(&path).expect("load verify-gate-multistage fixture");
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
    // assert the refuse -> narrow -> pass shape.
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

    assert!(
        bodies.contains("unverified changes"),
        "expected the verify-gate refusal in a tool_result body",
    );
    // First refusal (only `lint` has recorded green): names the two
    // stages still missing/failing, and both their exact commands, but
    // never re-names the stage that already passed.
    assert!(
        bodies.contains("missing/failing stage(s): typecheck, test"),
        "first refusal must name exactly the two unmet stages: {bodies}",
    );
    // (Command text is matched by a quote-agnostic prefix rather than the
    // full string: the refusal rides inside a `tool_result` JSON string, so
    // the embedded `"` in each command is JSON-escaped as `\"` in the raw
    // request body — matching only up to the first quote sidesteps that
    // escaping entirely.)
    assert!(
        bodies.contains("typecheck: `python3 -c"),
        "first refusal must cite the typecheck stage's exact command: {bodies}",
    );
    assert!(
        bodies.contains("test: `python3 -c"),
        "first refusal must cite the test stage's exact command: {bodies}",
    );
    // Second refusal (lint + typecheck both green): narrows to naming
    // ONLY `test` — proof the gate re-evaluates per-stage state on each
    // attempt rather than caching the first refusal's stage list.
    assert!(
        bodies.contains("missing/failing stage(s): test ("),
        "second refusal must narrow to naming only the remaining stage: {bodies}",
    );
    assert!(
        !bodies.contains("missing/failing stage(s): lint"),
        "a stage that already recorded green must never be (re-)named as pending: {bodies}",
    );

    // The pass: the todo store (also resolved under COPPERCLAW_DATA_ROOT)
    // shows the item genuinely completed once every stage read green.
    let todo_store = std::path::Path::new(M20X1_STAGES_DATA_ROOT).join("agent_todos.json");
    let raw = std::fs::read_to_string(&todo_store)
        .unwrap_or_else(|e| panic!("todo store missing at {}: {e}", todo_store.display()));
    let todos: serde_json::Value = serde_json::from_str(&raw).expect("todo store is JSON");
    let first = &todos.as_array().expect("todo store is an array")[0];
    assert_eq!(
        first["status"], "completed",
        "the todo must end completed once every stage passed: {todos}",
    );

    // The passing final stage cleared the project's dirty marker (Q2:
    // only once ALL stages read green, not on any single stage's pass).
    let dirty = std::path::Path::new(M20X1_STAGES_DATA_ROOT).join("proj/.copperclaw/dirty");
    assert!(
        !dirty.exists(),
        "the project must clear dirty once every stage has passed at {}",
        dirty.display(),
    );

    // Every stage genuinely ran and recorded green — proof of the
    // per-stage state file (`.copperclaw/stages`), not just the absence
    // of a dirty marker.
    let stages_file = std::path::Path::new(M20X1_STAGES_DATA_ROOT).join("proj/.copperclaw/stages");
    let stages_raw = std::fs::read_to_string(&stages_file)
        .unwrap_or_else(|e| panic!("stages file missing at {}: {e}", stages_file.display()));
    let stages: serde_json::Value = serde_json::from_str(&stages_raw).expect("stages is JSON");
    for name in ["lint", "typecheck", "test"] {
        assert_eq!(
            stages[name]["passed"], true,
            "stage `{name}` must be recorded passed: {stages}",
        );
    }
}

// ---- M20 Q6: the enforced self-review gate before final delivery ----
//
// Sibling to the two verify-gate fixtures above, exercising the SEPARATE
// M20 Q6 gate: completing the final/delivery todo of a never-reviewed
// project refuses, naming `self_review` and `load_skill("code-review")`;
// calling `self_review` (read, then submit `no_findings: true`) records
// the review; completion then succeeds. Uses the identical re-exec seam
// (`COPPERCLAW_DATA_ROOT`) X2/M20X1 built, since `forbid(unsafe_code)`
// still blocks `std::env::set_var` here.

/// Fixed data root for the self-review-gate fixture — a distinct `/tmp`
/// path from [`X2_VERIFY_DATA_ROOT`]/[`M20X1_STAGES_DATA_ROOT`] so the
/// three fixtures' re-exec'd child processes never collide.
const M20Q6_SELF_REVIEW_DATA_ROOT: &str = "/tmp/copperclaw-q6-self-review-gate";
/// Set on the re-exec'd child so it runs the scenario instead of
/// re-spawning itself.
const M20Q6_CHILD_ENV: &str = "COPPERCLAW_M20Q6_SELF_REVIEW_CHILD";
/// `assert` (default) or `dump` — the latter prints the captured actual
/// streams as JSONL so `expected/*.jsonl` can be regenerated from a real
/// run (`COPPERCLAW_X2_GENERATE=1 cargo test ... cli_prototype_self_review_gate`).
const M20Q6_MODE_ENV: &str = "COPPERCLAW_M20Q6_SELF_REVIEW_MODE";

#[tokio::test]
async fn cli_prototype_self_review_gate_refuse_review_pass() {
    // Child leg: env already set by the parent's re-exec. Run the real
    // scenario against the gate rooted at M20Q6_SELF_REVIEW_DATA_ROOT.
    if std::env::var_os(M20Q6_CHILD_ENV).is_some() {
        let dump = std::env::var(M20Q6_MODE_ENV).ok().as_deref() == Some("dump");
        run_self_review_gate_child(dump).await;
        return;
    }

    // Parent leg: re-exec ourselves with COPPERCLAW_DATA_ROOT set (via the
    // safe Command::env — set_var is unavailable under forbid(unsafe_code)).
    let _ = std::fs::remove_dir_all(M20Q6_SELF_REVIEW_DATA_ROOT);
    std::fs::create_dir_all(M20Q6_SELF_REVIEW_DATA_ROOT)
        .expect("create m20q6 self-review-gate data root");

    let mode = if std::env::var_os("COPPERCLAW_X2_GENERATE").is_some() {
        "dump"
    } else {
        "assert"
    };
    let exe = std::env::current_exe().expect("current_exe");
    let output = std::process::Command::new(exe)
        .args([
            "--exact",
            "cli_prototype_self_review_gate_refuse_review_pass",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(M20Q6_CHILD_ENV, "1")
        .env(M20Q6_MODE_ENV, mode)
        .env("COPPERCLAW_DATA_ROOT", M20Q6_SELF_REVIEW_DATA_ROOT)
        .output()
        .expect("spawn self-review-gate re-exec child");

    let _ = std::fs::remove_dir_all(M20Q6_SELF_REVIEW_DATA_ROOT);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if mode == "dump" {
        // Generation run: surface the child's dumped JSONL to the operator.
        println!("{stdout}");
    }
    assert!(
        output.status.success(),
        "self-review-gate child failed (status {:?})\n\
         --- child stdout ---\n{stdout}\n--- child stderr ---\n{stderr}",
        output.status.code(),
    );
}

/// The child scenario: drive the `prototype-self-review-gate` fixture
/// through the real harness. In `dump` mode, print the captured actuals
/// (fixture authoring). Otherwise diff against the committed
/// `expected/*.jsonl` and assert the refuse -> `self_review` (read) ->
/// `self_review` (submit) -> completion shape genuinely occurred.
async fn run_self_review_gate_child(dump: bool) {
    let path = fixture_path("cli", "prototype-self-review-gate");
    assert!(
        path.exists(),
        "fixture missing at {} — see docs/replay-fixtures.md",
        path.display()
    );
    let fixture = Fixture::load(&path).expect("load self-review-gate fixture");
    let mut harness = ReplayHarness::new(fixture).await.expect("boot harness");
    harness.run().await.expect("run harness");

    if dump {
        harness.dump_expected_jsonl();
        return;
    }

    // Byte-stable pipeline diff first (expected/*.jsonl).
    let report = harness.compare().expect("compare");
    assert!(report.is_clean(), "{report}");

    // The refusal message is handed back to the model as a tool_result
    // block, so it rides along in the provider request bodies captured by
    // the wiremock server. Concatenate every received request body and
    // assert the refuse -> review -> pass shape.
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

    // The refusal names the gate, the teaching hint, and the review-cycle
    // budget — never-reviewed is refusal cycle 1 of REVIEW_CYCLE_CAP (2).
    assert!(
        bodies.contains("has not been self-reviewed"),
        "expected the self-review-gate refusal in a tool_result body: {bodies}",
    );
    assert!(
        bodies.contains("final/delivery todo"),
        "refusal must name this as the final/delivery todo: {bodies}",
    );
    assert!(
        bodies.contains("self_review"),
        "refusal must teach the self_review tool: {bodies}",
    );
    assert!(
        bodies.contains("load_skill(\\\"code-review\\\")"),
        "refusal must teach load_skill(\"code-review\"): {bodies}",
    );
    assert!(
        bodies.contains("1 review cycle(s) remaining"),
        "first refusal must report the review-cycle budget: {bodies}",
    );

    // The self_review READ phase's diff-since-first-commit reached the
    // model, proving the tool actually ran the real git diff rather than
    // being a stub.
    assert!(
        bodies.contains("def greet(name)"),
        "self_review's read-phase diff must reach the model: {bodies}",
    );
    assert!(
        bodies.contains("\\\"mode\\\": \\\"submit\\\""),
        "self_review's submit-phase acknowledgement must reach the model: {bodies}",
    );

    // The pass: the todo store (also resolved under
    // COPPERCLAW_DATA_ROOT) shows the item genuinely completed once the
    // review was recorded.
    let todo_store = std::path::Path::new(M20Q6_SELF_REVIEW_DATA_ROOT).join("agent_todos.json");
    let raw = std::fs::read_to_string(&todo_store)
        .unwrap_or_else(|e| panic!("todo store missing at {}: {e}", todo_store.display()));
    let todos: serde_json::Value = serde_json::from_str(&raw).expect("todo store is JSON");
    let first = &todos.as_array().expect("todo store is an array")[0];
    assert_eq!(
        first["status"], "completed",
        "the todo must end completed once the review was recorded: {todos}",
    );

    // The submitted review actually wrote the marker `self_review` owns.
    let reviewed_marker = std::path::Path::new(M20Q6_SELF_REVIEW_DATA_ROOT)
        .join("proj")
        .join(".copperclaw")
        .join("reviewed");
    assert!(
        reviewed_marker.is_file(),
        "self_review's submit phase must write .copperclaw/reviewed at {}",
        reviewed_marker.display(),
    );
}

// ---- M19 F4 (X-rider W1): a blocked todo renders as blocked, not in-progress ----
//
// F4 gave `copperclaw_channels_core::TodoItemStatus` a real `Blocked`
// variant (glyph `[!]` + a one-line reason) and mapped the runner's
// storage-side `TodoStatus::Blocked` onto it in `status_to_wire`, so a
// step that auto-blocked after burning its verify fix-cycles renders as
// blocked on every adapter — instead of the misleading "in progress
// forever" the pre-F4 wire enum forced (`Blocked -> InProgress`).
//
// This fixture drives the GENUINE auto-block path end to end: a project
// that is dirty AND already at the fix-cycle cap, with a recorded verify
// failure, plus one `in_progress` todo. When the model attempts
// `todo_update(status="completed")` the gate auto-transitions the todo to
// `blocked` (never silently completed, never permanently refusing),
// attaches the recorded failure as the reason, and emits the post-mutation
// `TodoList`. The delivery loop degrades `deliver_todo_list` to its text
// fallback on the harness mock — the exact surface F4 taught the `[!]`
// glyph — so the delivered checklist must show the step blocked with its
// reason, and NOT with the in-progress glyph.
//
// Like the X2 verify-gate fixture, the todo store + verify gate resolve
// against `COPPERCLAW_DATA_ROOT`, which `forbid(unsafe_code)` forbids us
// from setting via `std::env::set_var`. So we re-exec THIS test binary as
// a child with the var set through the safe `Command::env`; the child runs
// the real `ReplayHarness` against pre-seeded, writable project state.

/// Fixed data root for the blocked-todo fixture. Distinct from the X2
/// verify-gate root so the two re-exec fixtures never collide.
const XR1_BLOCKED_DATA_ROOT: &str = "/tmp/copperclaw-xr1-blocked-todo";
/// Set on the re-exec'd child so it runs the scenario instead of
/// re-spawning itself.
const XR1_BLOCKED_CHILD_ENV: &str = "COPPERCLAW_XR1_BLOCKED_TODO_CHILD";
/// `assert` (default) or `dump` — the latter prints the captured actual
/// streams so `expected/*.jsonl` can be regenerated from a real run.
const XR1_BLOCKED_MODE_ENV: &str = "COPPERCLAW_XR1_BLOCKED_TODO_MODE";
/// The recorded verify failure that becomes the blocked todo's reason.
const XR1_BLOCKED_REASON: &str = "app.py: SyntaxError: invalid syntax (line 3)";

#[tokio::test]
async fn telegram_blocked_todo_renders_as_blocked() {
    // Child leg: env already set by the parent's re-exec. Run the real
    // scenario against the todo store / gate rooted at XR1_BLOCKED_DATA_ROOT.
    if std::env::var_os(XR1_BLOCKED_CHILD_ENV).is_some() {
        let dump = std::env::var(XR1_BLOCKED_MODE_ENV).ok().as_deref() == Some("dump");
        run_blocked_todo_child(dump).await;
        return;
    }

    // Parent leg: seed a dirty project at the fix-cycle cap + a recorded
    // failure + one in_progress todo, then re-exec ourselves with
    // COPPERCLAW_DATA_ROOT set (via the safe Command::env).
    let _ = std::fs::remove_dir_all(XR1_BLOCKED_DATA_ROOT);
    let root = std::path::Path::new(XR1_BLOCKED_DATA_ROOT);
    let state = root.join("proj/.copperclaw");
    std::fs::create_dir_all(&state).expect("create blocked-todo project state dir");
    // Dirty marker so the completion gate finds a dirty project.
    std::fs::write(state.join("dirty"), b"").expect("write dirty marker");
    // Fix-cycle count already at the cap (2) so the gate auto-BLOCKS the
    // todo rather than refusing-with-cycles-remaining.
    std::fs::write(state.join("fix_cycles"), b"2").expect("write fix_cycles");
    // The recorded verify failure that becomes the blocked reason.
    std::fs::write(state.join("last_failure"), XR1_BLOCKED_REASON.as_bytes())
        .expect("write last_failure");
    // One in_progress todo the model will attempt to complete.
    std::fs::write(
        root.join("agent_todos.json"),
        br#"[{"id":1,"text":"Verify the build passes","status":"in_progress","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}]"#,
    )
    .expect("seed agent_todos.json");

    let mode = if std::env::var_os("COPPERCLAW_XR1_GENERATE").is_some() {
        "dump"
    } else {
        "assert"
    };
    let exe = std::env::current_exe().expect("current_exe");
    let output = std::process::Command::new(exe)
        .args([
            "--exact",
            "telegram_blocked_todo_renders_as_blocked",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(XR1_BLOCKED_CHILD_ENV, "1")
        .env(XR1_BLOCKED_MODE_ENV, mode)
        .env("COPPERCLAW_DATA_ROOT", XR1_BLOCKED_DATA_ROOT)
        .output()
        .expect("spawn blocked-todo re-exec child");

    let _ = std::fs::remove_dir_all(XR1_BLOCKED_DATA_ROOT);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if mode == "dump" {
        println!("{stdout}");
    }
    assert!(
        output.status.success(),
        "blocked-todo child failed (status {:?})\n\
         --- child stdout ---\n{stdout}\n--- child stderr ---\n{stderr}",
        output.status.code(),
    );
}

/// The child scenario: drive the `blocked-todo` fixture through the real
/// harness. In `dump` mode, print the captured actuals (fixture
/// authoring). Otherwise diff against the committed `expected/*.jsonl` and
/// assert the blocked step genuinely rendered as blocked (not in-progress)
/// on the delivered checklist.
async fn run_blocked_todo_child(dump: bool) {
    let path = fixture_path("telegram", "blocked-todo");
    assert!(
        path.exists(),
        "fixture missing at {} — see docs/replay-fixtures.md",
        path.display()
    );
    let fixture = Fixture::load(&path).expect("load blocked-todo fixture");
    let mut harness = ReplayHarness::new(fixture).await.expect("boot harness");
    harness.run().await.expect("run harness");

    if dump {
        harness.dump_expected_jsonl();
        return;
    }

    // Byte-stable pipeline diff first (expected/*.jsonl).
    let report = harness.compare().expect("compare");
    assert!(report.is_clean(), "{report}");

    // The delivered todo checklist must show the auto-blocked step with the
    // `[!]` glyph AND its reason — never the in-progress glyph `[~]` for
    // that step. The mock degrades `deliver_todo_list` to its text
    // fallback, so the checklist rides a plain chat delivery.
    let tg = mock_for(&harness, "telegram");
    let deliveries = tg.deliveries();
    let checklist = deliveries
        .iter()
        .filter_map(|d| d.message.content.get("text").and_then(|t| t.as_str()))
        .find(|t| t.contains("Verify the build passes"))
        .expect("the post-mutation todo checklist must be delivered");
    assert!(
        checklist.contains("[!] Verify the build passes"),
        "the auto-blocked step must render with the [!] blocked glyph: {checklist}",
    );
    assert!(
        checklist.contains(XR1_BLOCKED_REASON),
        "the blocked step must carry its one-line failure reason: {checklist}",
    );
    assert!(
        !checklist.contains("[~] Verify the build passes"),
        "the blocked step must NOT render as in-progress (the pre-F4 bug): {checklist}",
    );
    assert!(
        checklist.contains("1 blocked"),
        "the checklist footer must count the blocked step: {checklist}",
    );

    // Corroborate against the on-disk todo store the tool wrote under the
    // data root: the item genuinely auto-transitioned to `blocked` with the
    // recorded reason attached (proof the wire render reflects real state,
    // not a hand-built list).
    let store = std::path::Path::new(XR1_BLOCKED_DATA_ROOT).join("agent_todos.json");
    let raw = std::fs::read_to_string(&store)
        .unwrap_or_else(|e| panic!("todo store missing at {}: {e}", store.display()));
    let todos: serde_json::Value = serde_json::from_str(&raw).expect("todo store is JSON");
    let first = &todos.as_array().expect("todo store is an array")[0];
    assert_eq!(
        first["status"], "blocked",
        "the todo must have auto-transitioned to blocked: {todos}",
    );
    assert_eq!(
        first["blocked_reason"], XR1_BLOCKED_REASON,
        "the recorded verify failure must be attached as the block reason: {todos}",
    );
}

// ---- M19 F5 (X-rider W1): pre-first-tool "thinking…" HUD frame ----
//
// F5 made the live HUD post an initial "thinking…" frame after a short
// threshold (`hud::THINKING_THRESHOLD` = 6s) so a multi-minute
// pure-reasoning answer on an edit-capable channel stops looking like a
// hang, while a fast turn stays byte-stable (posts nothing).
//
// Why a targeted paused-clock test, not a replay fixture: the F5 frame is
// driven purely by WALL-CLOCK timing *before the first tool call*, and the
// replay harness (real time, a wiremock provider, a 200ms provider
// deadline) has no seam to inject a deterministic 6s+ pure-reasoning wait
// — a real 6s sleep would be slow and jittery around the threshold. So
// this drives the real `run_loop` end to end under tokio's paused clock
// with a controllable provider. It goes *beyond* the runner-crate unit
// tests (which construct `TaskHud` directly): here the frame flows through
// the real `run_loop` → `drive_turn` arm/finalize → the session's outbound
// DB, and carries the originating-channel routing the delivery loop needs
// to reach the wire. The delivery leg itself (a breadcrumb → adapter edit)
// is locked by the F1 `matrix/hud-live-edit` fixture.

/// Controllable provider for the F5 tests: on the first (and only)
/// streamed event it sleeps `delay` — the pure-reasoning "thinking" wait —
/// then emits the final answer text. Under a paused clock the test decides
/// exactly when that wait elapses relative to the HUD's threshold.
struct F5PausedProvider {
    delay: std::time::Duration,
    text: String,
}

#[async_trait::async_trait]
impl copperclaw_providers::AgentProvider for F5PausedProvider {
    fn name(&self) -> &'static str {
        "f5-paused"
    }
    async fn query(
        &self,
        _input: copperclaw_providers::QueryInput,
    ) -> Result<Box<dyn copperclaw_providers::AgentQuery>, copperclaw_providers::ProviderError>
    {
        Ok(Box::new(F5PausedQuery {
            delay: self.delay,
            text: Some(self.text.clone()),
        }))
    }
    fn is_session_invalid(&self, _err: &copperclaw_providers::ProviderError) -> bool {
        false
    }
}

struct F5PausedQuery {
    delay: std::time::Duration,
    text: Option<String>,
}

#[async_trait::async_trait]
impl copperclaw_providers::AgentQuery for F5PausedQuery {
    async fn push(&mut self, _message: String) -> Result<(), copperclaw_providers::ProviderError> {
        Ok(())
    }
    async fn end(&mut self) -> Result<(), copperclaw_providers::ProviderError> {
        Ok(())
    }
    async fn next_event(&mut self) -> Option<copperclaw_types::ProviderEvent> {
        // The pure-reasoning wait: block for `delay`, then emit the final
        // answer text exactly once (a zero-tool turn).
        let text = self.text.take()?;
        tokio::time::sleep(self.delay).await;
        Some(copperclaw_types::ProviderEvent::Result { text: Some(text) })
    }
    async fn abort(&mut self) {}
}

/// Build a one-turn `run_loop` deps + shared outbound handle for the F5
/// tests, seeded with a single pending TELEGRAM chat inbound (an
/// edit-capable channel → the HUD resolves to `Behavior::Live`). The
/// provider sleeps `delay` before answering; `provider_deadline` is set
/// well past `delay` so the deadline never trips.
async fn f5_build(
    paths: &copperclaw_db::session::SessionPaths,
    delay: std::time::Duration,
) -> (
    copperclaw_runner::RunnerDeps,
    std::sync::Arc<tokio::sync::Mutex<rusqlite::Connection>>,
) {
    use copperclaw_db::session::{open_inbound, open_outbound};
    use copperclaw_db::tables::messages_in::{self, WriteInbound};

    let inbound = std::sync::Arc::new(tokio::sync::Mutex::new(open_inbound(paths).unwrap()));
    let outbound = std::sync::Arc::new(tokio::sync::Mutex::new(open_outbound(paths).unwrap()));

    {
        let g = inbound.lock().await;
        messages_in::insert(
            &g,
            &WriteInbound {
                id: copperclaw_types::MessageId::new(),
                kind: copperclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "think hard about this"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("100".into()),
                channel_type: Some(copperclaw_types::ChannelType::new("telegram")),
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
    }

    let tool_ctx: std::sync::Arc<dyn copperclaw_mcp::ToolContext> = std::sync::Arc::new(
        copperclaw_runner::RunnerToolCtx::new(outbound.clone(), paths.outbox.clone()),
    );
    let provider: std::sync::Arc<dyn copperclaw_providers::AgentProvider> =
        std::sync::Arc::new(F5PausedProvider {
            delay,
            text: "Here is the carefully-reasoned answer.".into(),
        });
    let mut deps = copperclaw_runner::RunnerDeps::minimal(
        provider,
        tool_ctx,
        inbound,
        outbound.clone(),
        paths.outbox.join("_compactions"),
    );
    deps.max_turns = Some(1);
    deps.idle_sleep = std::time::Duration::from_millis(1);
    deps.provider_deadline = std::time::Duration::from_secs(600);
    (deps, outbound)
}

/// Yield repeatedly so spawned tasks (the HUD ticker, the `run_loop` task)
/// can make progress under the paused clock between `advance` calls.
async fn f5_settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

/// Snapshot the session's outbound `Breadcrumb`-kind rows via a fresh read
/// connection (the runner holds its own write handle).
fn f5_breadcrumb_summaries(paths: &copperclaw_db::session::SessionPaths) -> Vec<String> {
    let conn = copperclaw_db::session::open_outbound(paths).unwrap();
    copperclaw_db::tables::messages_out::list_due(&conn)
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == copperclaw_types::MessageKind::Breadcrumb)
        .filter_map(|r| {
            r.content
                .get("breadcrumb")
                .and_then(|b| b.get("summary"))
                .and_then(|s| s.as_str())
                .map(str::to_owned)
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn f5_thinking_frame_posts_after_threshold_via_run_loop() {
    use copperclaw_runner::run_loop;

    let tmp = tempfile::tempdir().unwrap();
    let paths = copperclaw_db::session::SessionPaths::new(
        tmp.path(),
        copperclaw_types::AgentGroupId::new(),
        copperclaw_types::SessionId::new(),
    );
    // A 5-minute pure-reasoning wait before the answer lands.
    let (deps, _outbound) = f5_build(&paths, std::time::Duration::from_secs(300)).await;

    let handle = tokio::spawn(run_loop(deps));
    // Let run_loop reach `drive_turn` (arm the HUD ticker) and park on the
    // provider's pure-reasoning sleep before we touch the clock.
    f5_settle().await;

    // Just before the threshold: nothing has posted — a fast turn would
    // have finalized by now and stayed byte-stable.
    tokio::time::advance(std::time::Duration::from_secs(5)).await;
    f5_settle().await;
    assert!(
        f5_breadcrumb_summaries(&paths).is_empty(),
        "no HUD frame may post before the {}s thinking threshold",
        6,
    );

    // Past the threshold: exactly one "thinking…" breadcrumb chip posts,
    // even though no tool has run yet.
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    f5_settle().await;
    let summaries = f5_breadcrumb_summaries(&paths);
    assert_eq!(
        summaries.len(),
        1,
        "exactly one pre-first-tool thinking frame; got {summaries:?}",
    );
    assert!(
        summaries[0].starts_with("thinking…"),
        "the pre-first-tool frame must read as thinking…; got {:?}",
        summaries[0],
    );

    // Let the reasoning wait elapse; the turn answers and run_loop returns.
    tokio::time::advance(std::time::Duration::from_secs(300)).await;
    f5_settle().await;
    handle.await.expect("run_loop task").expect("run_loop ok");

    // The thinking frame did not dangle: finalize collapsed it to a
    // "done in M:SS" update (a zero-tool turn omits the tool count), and
    // the model's answer was delivered as its own chat row.
    let conn = copperclaw_db::session::open_outbound(&paths).unwrap();
    let rows = copperclaw_db::tables::messages_out::list_due(&conn).unwrap();
    let collapsed = rows.iter().find_map(|r| {
        r.content
            .get("update_breadcrumb")
            .and_then(|u| u.get("breadcrumb"))
            .and_then(|b| {
                let done = b.get("status").and_then(|s| s.as_str()) == Some("done");
                let summary = b.get("summary").and_then(|s| s.as_str()).unwrap_or("");
                (done && summary.starts_with("done in")).then(|| summary.to_owned())
            })
    });
    assert!(
        collapsed.is_some(),
        "finalize must collapse the thinking frame to a done summary: {rows:?}",
    );
    assert!(
        rows.iter()
            .any(|r| r.kind == copperclaw_types::MessageKind::Chat
                && r.content.get("text").and_then(|t| t.as_str())
                    == Some("Here is the carefully-reasoned answer.")),
        "the model's final answer must be emitted: {rows:?}",
    );
    // The thinking frame carried the originating-channel routing the
    // delivery loop needs to reach the wire (telegram, chat 100).
    let thinking = rows
        .iter()
        .find(|r| r.kind == copperclaw_types::MessageKind::Breadcrumb)
        .expect("a thinking breadcrumb row exists");
    assert_eq!(
        thinking
            .channel_type
            .as_ref()
            .map(copperclaw_types::ChannelType::as_str),
        Some("telegram"),
    );
    assert_eq!(thinking.platform_id.as_deref(), Some("100"));
}

#[tokio::test(start_paused = true)]
async fn f5_fast_turn_posts_no_thinking_frame_via_run_loop() {
    use copperclaw_runner::run_loop;

    let tmp = tempfile::tempdir().unwrap();
    let paths = copperclaw_db::session::SessionPaths::new(
        tmp.path(),
        copperclaw_types::AgentGroupId::new(),
        copperclaw_types::SessionId::new(),
    );
    // A fast answer: the provider returns immediately, well under the
    // thinking threshold, so the armed ticker is aborted by finalize
    // before it ever posts.
    let (deps, _outbound) = f5_build(&paths, std::time::Duration::ZERO).await;

    let handle = tokio::spawn(run_loop(deps));
    f5_settle().await;
    handle.await.expect("run_loop task").expect("run_loop ok");

    // Advance well past the threshold to prove the aborted ticker is truly
    // dead and never fires a late frame.
    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    f5_settle().await;

    assert!(
        f5_breadcrumb_summaries(&paths).is_empty(),
        "a sub-threshold pure-reasoning turn must post no HUD frame (byte-stable)",
    );
    // Sanity: the fast turn still answered.
    let conn = copperclaw_db::session::open_outbound(&paths).unwrap();
    let rows = copperclaw_db::tables::messages_out::list_due(&conn).unwrap();
    assert!(
        rows.iter()
            .any(|r| r.kind == copperclaw_types::MessageKind::Chat
                && r.content.get("text").and_then(|t| t.as_str())
                    == Some("Here is the carefully-reasoned answer.")),
        "the fast turn must still deliver its answer: {rows:?}",
    );
}

// ---- M19 X-rider Wave 2: parity fixtures (U1/U2/U4/U5/U7) ----
//
// One fixture per newly-rich surface, each locking the *host delivery
// pipeline* half of a Wave-2 adapter-floor card. The per-adapter *wire*
// rendering (signal-cli, teams Bot Framework, gchat Cards-v2, matrix HTML,
// deltachat JSON-RPC) lives in each adapter crate's own unit tests; these
// fixtures prove the runner's rich surface reaches the adapter's rich hook
// (breadcrumb edit-in-place / deliver_card / deliver_todo_list) instead of
// degrading host-side. See each fixture's README.md.

/// Fixture-authoring gate shared by the scripted-turn W2 parity fixtures
/// (signal / teams / gchat). When `COPPERCLAW_XR2_GENERATE` is set the
/// harness regenerates `expected/*.jsonl` from a real run and the caller
/// returns before asserting. Never taken under a normal `cargo test`.
async fn xr2_maybe_generate(channel: &str, scenario: &str) -> bool {
    if std::env::var_os("COPPERCLAW_XR2_GENERATE").is_none() {
        return false;
    }
    let path = fixture_path(channel, scenario);
    let fixture = Fixture::load(&path).expect("load fixture");
    let mut harness = ReplayHarness::new(fixture).await.expect("boot harness");
    harness.run().await.expect("run harness");
    harness.dump_expected_jsonl();
    true
}

/// Assert an edit-capable channel ran the live HUD: exactly ONE breadcrumb
/// chip posted, >=1 in-place edit all targeting that single anchor in the
/// originating conversation, and the model's final answer still delivered
/// as its own chat message. Shared by the signal (U1) and teams (U2)
/// breadcrumb parity fixtures — the same live-HUD contract the F1
/// `matrix/hud-live-edit` fixture locks, one edit-capable channel over.
fn assert_live_hud_edits_one_chip(
    harness: &ReplayHarness,
    channel: &str,
    platform_id: &str,
    final_answer_fragment: &str,
) {
    let mock = mock_for(harness, channel);

    // Exactly ONE breadcrumb chip is posted (anything more is the
    // re-post spam the live HUD exists to kill).
    let deliveries = mock.deliveries();
    let posts: Vec<_> = deliveries
        .iter()
        .filter(|d| d.message.kind.as_str() == "breadcrumb")
        .collect();
    assert_eq!(
        posts.len(),
        1,
        "the live HUD must post exactly one breadcrumb chip on {channel}, got {}: {deliveries:?}",
        posts.len(),
    );
    assert_eq!(
        posts[0].platform_id, platform_id,
        "the chip must land in the originating {channel} conversation",
    );

    // Every later frame is an in-place edit of that ONE chip.
    let edits = mock.edits();
    assert!(
        !edits.is_empty(),
        "the live HUD must edit the chip in place at least once on {channel}, got zero edits",
    );
    let anchors: std::collections::BTreeSet<&str> =
        edits.iter().map(|e| e.external_id.as_str()).collect();
    assert_eq!(
        anchors.len(),
        1,
        "every HUD edit must target the SINGLE posted chip (one anchor) on {channel}, got {anchors:?}",
    );
    for e in &edits {
        assert_eq!(
            e.platform_id, platform_id,
            "each edit must target the originating {channel} conversation",
        );
    }

    // The model's final answer still reaches the user as its own chat row.
    assert!(
        deliveries.iter().any(|d| d.message.kind.as_str() == "chat"
            && d.message
                .content
                .get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.contains(final_answer_fragment))),
        "the final chat answer must be delivered alongside the HUD chip on {channel}: {deliveries:?}",
    );
}

/// M19 U1 (X-rider W2): signal, raised to the rich-surface floor, now runs
/// the live HUD — one breadcrumb chip posted, edited in place, never the
/// stacked prose the pre-U1 fall-through produced. Twin of the F1 matrix
/// fixture on a different edit-capable channel. See the fixture's README.
#[tokio::test]
async fn signal_hud_breadcrumb_edits_one_chip_in_place() {
    if xr2_maybe_generate("signal", "hud-breadcrumb").await {
        return;
    }
    let harness = run_fixture_into_harness("signal", "hud-breadcrumb").await;
    assert_live_hud_edits_one_chip(&harness, "signal", "+15550001234", "build is fine");
}

/// M19 U2 (X-rider W2): teams gained a trait `edit_message` override and a
/// slot in `EDIT_CAPABLE_CHANNELS`, so the HUD now edits ONE message in
/// place instead of posting N (the new-message spam U2 kills). See the
/// fixture's README.
#[tokio::test]
async fn teams_hud_live_edit_edits_one_chip_in_place() {
    if xr2_maybe_generate("teams", "hud-live-edit").await {
        return;
    }
    let harness = run_fixture_into_harness("teams", "hud-live-edit").await;
    assert_live_hud_edits_one_chip(&harness, "teams", "19:teamschat@thread.v2", "build is fine");
}

/// M19 U4 (X-rider W2): a `send_card` reaches gchat's `deliver_card` as a
/// STRUCTURED card (both buttons intact), not a host-flattened prose blob.
/// `model_rich_cards` models gchat's card-capable contract; the gchat
/// Cards-v2 wire JSON itself is proven in the gchat adapter's unit tests.
/// Represents the U4 card surface (gchat + matrix) at the pipeline level.
/// See the fixture's README.
#[tokio::test]
async fn gchat_approval_card_delivers_structured_card() {
    if xr2_maybe_generate("gchat", "approval-card").await {
        return;
    }
    let harness = run_fixture_into_harness("gchat", "approval-card").await;
    let gchat = mock_for(&harness, "gchat");
    let deliveries = gchat.deliveries();

    // Exactly one native (Card-kind) card delivery in the originating space.
    let cards: Vec<_> = deliveries
        .iter()
        .filter(|d| d.message.kind.as_str() == "card")
        .collect();
    assert_eq!(
        cards.len(),
        1,
        "exactly one structured card must be delivered to gchat, got {}: {deliveries:?}",
        cards.len(),
    );
    assert_eq!(cards[0].platform_id, "spaces/AAAAq.replay");

    // The card kept its structure: title, body, field, and BOTH buttons as
    // real structured elements (callback value + url preserved) — not the
    // `- [Label] -> …` prose the text fallback would have produced.
    let card = &cards[0].message.content["card"];
    assert_eq!(
        card["title"], "Ready to ship",
        "card title preserved: {card}"
    );
    assert!(
        card["body"]
            .as_str()
            .is_some_and(|b| b.contains("built and verified")),
        "card body preserved: {card}",
    );
    let buttons = card["buttons"]
        .as_array()
        .expect("card carries a structured buttons array");
    assert_eq!(
        buttons.len(),
        2,
        "both buttons survive structurally: {card}"
    );
    assert!(
        buttons
            .iter()
            .any(|b| b["value"] == "approve:deploy-42" && b["label"] == "Approve"),
        "the Approve callback button survives as a structured element: {card}",
    );
    assert!(
        buttons
            .iter()
            .any(|b| b["url"] == "http://192.0.2.10:8100/diff" && b["label"] == "View diff"),
        "the url button survives as a structured element: {card}",
    );

    // Sanity: the card was NOT flattened to prose (no `Buttons:` text blob
    // in any chat delivery).
    assert!(
        !deliveries.iter().any(|d| d.message.kind.as_str() == "chat"
            && d.message
                .content
                .get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.contains("Buttons:"))),
        "a native card must not degrade to a `Buttons:` prose block: {deliveries:?}",
    );

    // The closing text answer is still delivered as its own chat message.
    assert!(
        deliveries.iter().any(|d| d.message.kind.as_str() == "chat"
            && d.message
                .content
                .get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.contains("tap Approve to roll out v1.4.2"))),
        "the closing text answer must be delivered: {deliveries:?}",
    );
}

// ---- M19 U5 (X-rider W2): deltachat native card + todo chip ----
//
// deltachat's `todo_*` tools persist under `COPPERCLAW_DATA_ROOT`, which
// `forbid(unsafe_code)` forbids us from setting via `std::env::set_var`.
// So — exactly like the X2 verify-gate and F4 blocked-todo fixtures — we
// re-exec THIS test binary as a child with the var set via the safe
// `Command::env`; the child runs the real `ReplayHarness` against a
// writable, per-run todo store.

/// Fixed data root for the deltachat card+todo fixture. Distinct from the
/// X2 / blocked-todo roots so the re-exec fixtures never collide.
const XR2_DELTACHAT_DATA_ROOT: &str = "/tmp/copperclaw-xr2-deltachat-card-todo";
/// Set on the re-exec'd child so it runs the scenario instead of
/// re-spawning itself.
const XR2_DELTACHAT_CHILD_ENV: &str = "COPPERCLAW_XR2_DELTACHAT_CHILD";
/// `assert` (default) or `dump` — the latter regenerates `expected/*.jsonl`.
const XR2_DELTACHAT_MODE_ENV: &str = "COPPERCLAW_XR2_DELTACHAT_MODE";

/// M19 U5 (X-rider W2): a `send_card` reaches deltachat's `deliver_card`
/// as a STRUCTURED card, and a todo list is routed to `deliver_todo_list`
/// carrying its glyphs + footer counts — on a formerly-bare interactive
/// chat surface U5 raised to the rich floor. See the fixture's README.
#[tokio::test]
async fn deltachat_card_and_todo_native_surfaces() {
    // Child leg: env already set by the parent's re-exec.
    if std::env::var_os(XR2_DELTACHAT_CHILD_ENV).is_some() {
        let dump = std::env::var(XR2_DELTACHAT_MODE_ENV).ok().as_deref() == Some("dump");
        run_deltachat_card_todo_child(dump).await;
        return;
    }

    // Parent leg: create a writable data root (the todo store lands here)
    // and re-exec ourselves with COPPERCLAW_DATA_ROOT set.
    let _ = std::fs::remove_dir_all(XR2_DELTACHAT_DATA_ROOT);
    std::fs::create_dir_all(XR2_DELTACHAT_DATA_ROOT).expect("create deltachat data root");

    let mode = if std::env::var_os("COPPERCLAW_XR2_GENERATE").is_some() {
        "dump"
    } else {
        "assert"
    };
    let exe = std::env::current_exe().expect("current_exe");
    let output = std::process::Command::new(exe)
        .args([
            "--exact",
            "deltachat_card_and_todo_native_surfaces",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(XR2_DELTACHAT_CHILD_ENV, "1")
        .env(XR2_DELTACHAT_MODE_ENV, mode)
        .env("COPPERCLAW_DATA_ROOT", XR2_DELTACHAT_DATA_ROOT)
        .output()
        .expect("spawn deltachat card+todo re-exec child");

    let _ = std::fs::remove_dir_all(XR2_DELTACHAT_DATA_ROOT);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if mode == "dump" {
        println!("{stdout}");
    }
    assert!(
        output.status.success(),
        "deltachat card+todo child failed (status {:?})\n\
         --- child stdout ---\n{stdout}\n--- child stderr ---\n{stderr}",
        output.status.code(),
    );
}

/// The child scenario: drive the `deltachat/card-and-todo` fixture through
/// the real harness. In `dump` mode print the captured actuals (fixture
/// authoring); otherwise diff against `expected/*.jsonl` and assert the
/// native card + todo chip genuinely reached the deltachat delivery hooks.
async fn run_deltachat_card_todo_child(dump: bool) {
    let path = fixture_path("deltachat", "card-and-todo");
    assert!(
        path.exists(),
        "fixture missing at {} — see docs/replay-fixtures.md",
        path.display()
    );
    let fixture = Fixture::load(&path).expect("load deltachat card+todo fixture");
    let mut harness = ReplayHarness::new(fixture).await.expect("boot harness");
    harness.run().await.expect("run harness");

    if dump {
        harness.dump_expected_jsonl();
        return;
    }

    // Byte-stable pipeline diff first.
    let report = harness.compare().expect("compare");
    assert!(report.is_clean(), "{report}");

    let dc = mock_for(&harness, "deltachat");
    let deliveries = dc.deliveries();

    // Exactly one native (Card-kind) card delivery to the deltachat chat,
    // with its structure — title, body, field, and both buttons — intact.
    let cards: Vec<_> = deliveries
        .iter()
        .filter(|d| d.message.kind.as_str() == "card")
        .collect();
    assert_eq!(
        cards.len(),
        1,
        "exactly one structured card must be delivered to deltachat, got {}: {deliveries:?}",
        cards.len(),
    );
    assert_eq!(cards[0].platform_id, "account/1/chat/10");
    let card = &cards[0].message.content["card"];
    assert_eq!(card["title"], "Demo staged", "card title preserved: {card}");
    let buttons = card["buttons"]
        .as_array()
        .expect("card carries a structured buttons array");
    assert_eq!(
        buttons.len(),
        2,
        "both buttons survive structurally: {card}"
    );
    assert!(
        buttons.iter().any(|b| b["value"] == "approve:demo"),
        "the Approve callback button survives structurally: {card}",
    );

    // The todo chip reaches `deliver_todo_list` (text fallback on the bare
    // mock) carrying BOTH items with their pending glyphs and the footer
    // counts — the surface U5's native deltachat renderer replaces.
    let todo_chip = deliveries
        .iter()
        .filter(|d| d.message.kind.as_str() == "chat")
        .filter_map(|d| d.message.content.get("text").and_then(|t| t.as_str()))
        .find(|t| t.contains("Wire the approval card"))
        .expect("a todo chip carrying both items must be delivered to deltachat");
    assert!(
        todo_chip.contains("[ ] Scaffold the demo"),
        "the first todo renders with its pending glyph: {todo_chip}",
    );
    assert!(
        todo_chip.contains("[ ] Wire the approval card"),
        "the second todo renders with its pending glyph: {todo_chip}",
    );
    assert!(
        todo_chip.contains("(0/2 done, 0 in progress, 2 pending)"),
        "the todo chip carries the footer counts: {todo_chip}",
    );

    // The closing text answer is delivered as its own chat message.
    assert!(
        deliveries.iter().any(|d| d.message.kind.as_str() == "chat"
            && d.message
                .content
                .get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.contains("tap Approve when ready"))),
        "the closing text answer must be delivered: {deliveries:?}",
    );

    // Corroborate against the on-disk todo store the tool wrote under the
    // data root: both items genuinely exist (proof the chip reflects real
    // state, not a hand-built list).
    let store = std::path::Path::new(XR2_DELTACHAT_DATA_ROOT).join("agent_todos.json");
    let raw = std::fs::read_to_string(&store)
        .unwrap_or_else(|e| panic!("todo store missing at {}: {e}", store.display()));
    let todos: serde_json::Value = serde_json::from_str(&raw).expect("todo store is JSON");
    let arr = todos.as_array().expect("todo store is an array");
    assert_eq!(arr.len(), 2, "both todo items were persisted: {todos}");
}

/// M19 U7 (X-rider W2): the slack parity twin of `telegram/reaction-steer`.
/// A ✅ slack reaction (normalized `content.reaction`) in a mention-gated
/// slack channel bypasses the gate (interaction payload) and persists a
/// non-trigger reaction row — pending for the runner's R2 steering seam —
/// with no runner turn, outbound, or delivery. Proves the inbound → router
/// reaction leg is channel-agnostic (slack's identity/target shape, not
/// just telegram's). The runner-side steering is covered by drive_turn
/// unit tests; this fixture owns the inbound → router leg. See README.
#[tokio::test]
async fn slack_reaction_inbound_bypasses_mention_gate() {
    run_fixture("slack", "reaction-inbound").await;
}

// ─── X-rider W3: capability fixtures (A1 / A3 / A7) ──────────────────────────

/// Fixture-authoring gate for the W3 capability fixtures. When
/// `COPPERCLAW_XR3_GENERATE` is set the harness regenerates `expected/*.jsonl`
/// from a real run and the caller returns before asserting. Never taken under
/// a normal `cargo test`.
async fn xr3_maybe_generate(channel: &str, scenario: &str) -> bool {
    if std::env::var_os("COPPERCLAW_XR3_GENERATE").is_none() {
        return false;
    }
    let path = fixture_path(channel, scenario);
    let fixture = Fixture::load(&path).expect("load fixture");
    let mut harness = ReplayHarness::new(fixture).await.expect("boot harness");
    harness.run().await.expect("run harness");
    harness.dump_expected_jsonl();
    true
}

/// M19 A3 (X-rider W3): the public-verb ritual card. After `make_preview_public`
/// returns a PUBLIC https URL (relayed through the reserved `__preview` server
/// to the harness's `FixtureTunnelBroker`), the agent's prototype-ready ritual
/// card gains an "Open the public link" button pointing at that public URL —
/// alongside the LAN "Open preview" button. This locks the card-with-public-URL
/// button shape at the pipeline level (inbound → router → runner → outbound →
/// delivery), on top of the byte-stable JSONL diff. A3's approval round-trip +
/// tunnel-broker internals are covered by its host-handler `preview.rs` tests
/// and the `tunnel.rs` module tests; here the FixtureTunnelBroker models the
/// post-approval `Exposed` reply so the fixture can prove the surfaced card.
/// See the fixture's README.
#[tokio::test]
async fn cli_prototype_public_share_ritual_card_has_public_button() {
    if xr3_maybe_generate("cli", "prototype-public-share").await {
        return;
    }
    let harness = run_fixture_into_harness("cli", "prototype-public-share").await;
    let cli = mock_for(&harness, "cli");
    let deliveries = cli.deliveries();

    // The ritual card renders through the cli text-fallback carrying BOTH the
    // LAN "Open preview" button (from the M17 preview broker) and the new A3
    // "Open the public link" button (from the tunnel broker) — the public verb
    // added a button to the same prototype-ready card, it did not replace it.
    let card_text = deliveries
        .iter()
        .filter_map(|d| d.message.content.get("text").and_then(|t| t.as_str()))
        .find(|t| t.contains("Todo app is live and public"))
        .expect("the public-share ritual card must be delivered");
    assert!(
        card_text.contains("**Todo app is live and public**"),
        "card must carry the title as a headline: {card_text}",
    );
    // The LAN button is still present (the public verb augments, not replaces).
    assert!(
        card_text.contains("[Open preview] -> http://192.0.2.10:8100/__preview/fixture-tok-8000"),
        "card must still carry the LAN Open-preview button: {card_text}",
    );
    // The headline A3 assertion: the public-URL button, pointing at the PUBLIC
    // https tunnel URL the FixtureTunnelBroker returned for make_preview_public.
    assert!(
        card_text.contains(
            "[Open the public link] -> https://fixture-tunnel.example/__preview/fixture-tok-8000"
        ),
        "card must carry the A3 public-URL button: {card_text}",
    );
    // The public URL is genuinely public (https, off-network host) — the
    // contrast with the LAN http URL that shares the card.
    assert!(
        card_text.contains("https://fixture-tunnel.example/"),
        "the public link must be an https tunnel URL, not the LAN address: {card_text}",
    );
}
