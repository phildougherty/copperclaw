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

/// Multi-workspace UX (`/switch` + `/projects`). Two host-answered steps
/// on the cli channel:
///
/// - `/switch fairway-focus` creates `/data/fairway-focus`, overwrites
///   `.shell_state` with `cd '/data/fairway-focus'`, enqueues a
///   context-reset `/clear` trigger row (left pending — the runner is not
///   driven for a host-answered command in the harness), and host-answers
///   the operator with the switch confirmation.
/// - `/projects` then host-answers the workspace listing, marking the
///   just-created workspace active (resolved from `.shell_state`).
///
/// Neither step writes a runner turn; both reply straight to
/// `messages_out`. Beyond the byte-stable JSONL diff this pins that
/// `/switch` really wrote the `cd` shell-state to the session root.
#[tokio::test]
async fn cli_slash_workspaces_switch_then_projects() {
    let harness = run_fixture_into_harness("cli", "slash-workspaces").await;
    let (ag, sess) = harness.touched_sessions[0];
    let state = harness
        .tempdir
        .path()
        .join("sessions")
        .join(ag.as_uuid().to_string())
        .join(sess.as_uuid().to_string())
        .join(".shell_state");
    let contents = std::fs::read_to_string(&state)
        .unwrap_or_else(|e| panic!(".shell_state missing at {state:?}: {e}"));
    assert_eq!(
        contents, "cd '/data/fairway-focus'\n",
        "/switch must point the container shell at the new workspace"
    );
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

// ─── M21 Wave-1 X-rider: recovery fixtures ───────────────────────────────────
//
// Lock the "nothing dies silently" wave into the replay test set. Two
// behaviours are genuinely reachable at the pipeline level and are pinned
// here (hung-tool -> StuckRestart -> single apology delivered; delivery
// retry counters surviving a service restart with exactly-once
// dead-lettering). The other two Wave-1 acceptance behaviours (loop-panic
// -> supervisor restart; OOM classification + backoff) are pinned by the
// implementing cards' own tests and mapped — with the reachability
// analysis — in `fixtures/README-m21-wave1.md`.

/// Fixture-authoring gate for the M21 Wave-1 X-rider fixtures. When
/// `COPPERCLAW_M21X1_GENERATE` is set the harness regenerates
/// `expected/*.jsonl` from a real run and the caller returns before
/// asserting. Never taken under a normal `cargo test`.
async fn m21x1_maybe_generate(channel: &str, scenario: &str) -> bool {
    if std::env::var_os("COPPERCLAW_M21X1_GENERATE").is_none() {
        return false;
    }
    let path = fixture_path(channel, scenario);
    let fixture = Fixture::load(&path).expect("load fixture");
    let mut harness = ReplayHarness::new(fixture).await.expect("boot harness");
    harness.run().await.expect("run harness");
    harness.dump_expected_jsonl();
    true
}

/// M21 S2 (X-rider W1): the hung-tool recovery sequence, end to end
/// through the replay pipeline — the leg S2's own mock-runtime
/// integration test (`stuck_actuator.rs`) stops short of: the apology
/// actually REACHING the channel adapter through the real
/// `DeliveryService`.
///
/// The fixture drives one normal turn (real router-created session +
/// routing). The test then reproduces exactly the mid-hang state
/// production would be in: session `Running`, heartbeat fresh (the
/// runner process IS alive — the shape the crash path can never catch,
/// decision (a)), one in-flight inbound with a `Processing` ack, and a
/// `container_state` tool row started 31 minutes ago — past the sweep's
/// unconditional 30-minute `ABSOLUTE_CEILING_MS`. One actuated sweep
/// pass (`SweepService` with the real `ContainerManager` wired as
/// `StuckActuator`, mock runtime) must issue the `StuckRestart`, clear
/// the tool state, stop the container, and write exactly one apology —
/// which the harness's delivery service then hands to the cli
/// `MockAdapter`. A second sweep + delivery pass stays byte-quiet
/// (dedup): no re-detection, no second apology on the wire.
#[tokio::test]
async fn cli_stuck_tool_restart_delivers_single_apology_end_to_end() {
    use copperclaw_db::session::{SessionPaths, open_inbound, open_outbound};
    use copperclaw_db::tables::{container_state, messages_in, messages_out, processing_ack};
    use copperclaw_host::container_manager::{
        ContainerManager, DEFAULT_HEARTBEAT_STALE_SECS, DEFAULT_IDLE_TIMEOUT_SECS,
        DEFAULT_STOP_GRACE_SECS, ManagerConfig,
    };
    use copperclaw_host_sweep::service::FilesystemSessionRoot;
    use copperclaw_host_sweep::{StuckActuator, SweepService};
    use copperclaw_types::{ChannelType, ContainerStatus, MessageId, MessageKind};
    use std::sync::Arc;

    if m21x1_maybe_generate("cli", "stuck-tool-restart").await {
        return;
    }
    let harness = run_fixture_into_harness("cli", "stuck-tool-restart").await;
    let (ag, sess) = harness.touched_sessions[0];
    let paths = SessionPaths::new(harness.tempdir.path(), ag, sess);

    // ── Reproduce the mid-hang state after the baseline turn ──
    // Session running (the harness already marked it so at route time;
    // be explicit) with a FRESH heartbeat: the runner is alive during a
    // hung tool, so heartbeat-based crash detection never fires.
    copperclaw_db::tables::sessions::mark_container_running(&harness.central, sess).unwrap();
    std::fs::write(&paths.heartbeat, b"").unwrap();

    // The in-flight inbound the hung tool was working on. Recent
    // timestamp so the sweep's own PendingTooLong apology stays out —
    // the crash-restart apology machinery is what must fire.
    let msg_id = MessageId::new();
    {
        let conn = open_inbound(&paths).unwrap();
        messages_in::insert(
            &conn,
            &messages_in::WriteInbound {
                id: msg_id,
                kind: MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "how is the build going?"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("stdin".into()),
                channel_type: Some(ChannelType::new(ChannelType::CLI)),
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
    }
    let outbound = open_outbound(&paths).unwrap();
    processing_ack::insert(
        &outbound,
        msg_id,
        processing_ack::ProcessingStatus::Processing,
    )
    .unwrap();
    // The hung tool: 31 minutes into a declared 1-hour timeout — past
    // the unconditional 30-minute ceiling but INSIDE its declared
    // budget, so only the ceiling path (not the claim threshold) fires.
    let now = chrono::Utc::now();
    container_state::set(
        &outbound,
        &container_state::ContainerState {
            current_tool: Some("bash".into()),
            tool_declared_timeout_ms: Some(3_600_000),
            tool_started_at: Some(now - chrono::Duration::minutes(31)),
            updated_at: Some(now),
        },
    )
    .unwrap();

    // ── One actuated sweep pass: detection + StuckRestart ──
    let cfg = ManagerConfig {
        install_slug: "replay".into(),
        data_dir: harness.tempdir.path().to_path_buf(),
        default_image_tag: "copperclaw/session:replay".into(),
        default_provider: "anthropic".into(),
        default_model: "claude-sonnet-4-6".into(),
        default_effort: None,
        anthropic_api_key: Some("harness".into()),
        anthropic_base_url: Some(harness.anthropic_server.uri()),
        idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
        heartbeat_stale_secs: DEFAULT_HEARTBEAT_STALE_SECS,
        stop_grace_secs: DEFAULT_STOP_GRACE_SECS,
        skills_dir: None,
        groups_dir: None,
        skills_mode: copperclaw_host::SkillsMode::default(),
        gpu_passthrough: false,
        forward_env: Vec::new(),
        egress_mode: copperclaw_container_rt::EgressMode::AllowAll,
    };
    let mgr = Arc::new(ContainerManager::new(
        harness.central.clone(),
        Arc::new(crate::harness::HarnessRuntime::default()),
        cfg,
    ));
    let sweep = SweepService::new(
        harness.central.clone(),
        Arc::new(FilesystemSessionRoot::new(harness.tempdir.path())),
    );
    sweep.set_stuck_actuator(Arc::clone(&mgr) as Arc<dyn StuckActuator>);
    let report = sweep.run_once_actuated().await.unwrap();
    assert_eq!(
        report.stuck_past_ceiling,
        vec![sess],
        "the hung tool must be detected past the absolute ceiling"
    );

    // The restart landed through the manager: container stopped, the
    // triggering tool state cleared so the next pass cannot re-fire.
    let updated = copperclaw_db::tables::sessions::get(&harness.central, sess).unwrap();
    assert!(
        matches!(updated.container_status, ContainerStatus::Stopped),
        "StuckRestart must stop the container: {:?}",
        updated.container_status
    );
    let state = container_state::get(&outbound).unwrap().unwrap();
    assert!(state.current_tool.is_none(), "tool state must be cleared");

    // ── The apology reaches the WIRE, exactly once ──
    harness
        .delivery
        .process_session_once(&updated)
        .await
        .unwrap();
    let mock = mock_for(&harness, "cli");
    let apologies: Vec<String> = mock
        .deliveries()
        .iter()
        .filter(|d| d.message.kind.as_str() == "chat")
        .filter_map(|d| d.message.content.get("text").and_then(|t| t.as_str()))
        .filter(|t| t.contains("snag") && t.contains("restart"))
        .map(str::to_owned)
        .collect();
    assert_eq!(
        apologies.len(),
        1,
        "exactly one recovery apology must be delivered: {:?}",
        mock.deliveries()
    );
    // Honest copy (decision (d), landed with S2): the apology must not
    // claim operator notification until O4 actually wires it.
    assert!(
        !apologies[0].contains("operator has been notified"),
        "pre-O4 apology copy must not claim operator notification: {}",
        apologies[0]
    );

    // ── Second sweep + delivery pass: byte-quiet ──
    let report2 = sweep.run_once_actuated().await.unwrap();
    assert!(
        report2.stuck_past_ceiling.is_empty(),
        "cleared tool state must not re-detect"
    );
    harness
        .delivery
        .process_session_once(&updated)
        .await
        .unwrap();
    let apology_count = mock
        .deliveries()
        .iter()
        .filter_map(|d| d.message.content.get("text").and_then(|t| t.as_str()))
        .filter(|t| t.contains("snag") && t.contains("restart"))
        .count();
    assert_eq!(apology_count, 1, "the apology stays deduped on the wire");
    // Corroborate against the outbound DB: exactly one apology row, in
    // reply to the wedged inbound.
    let apology_rows: Vec<_> = messages_out::list_due(&outbound)
        .unwrap()
        .into_iter()
        .filter(|r| {
            r.kind == MessageKind::Chat
                && r.content
                    .get("text")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t.contains("snag"))
        })
        .collect();
    assert_eq!(apology_rows.len(), 1);
    assert_eq!(apology_rows[0].in_reply_to, Some(msg_id));
}

/// M21 F2: expire `ask_user_question` out loud — ask -> expire -> user
/// replies later -> conversation resumes normally, end to end through
/// the replay pipeline over `fixtures/cli/question-expiry/`.
///
/// Step 1 (fixture): the scripted turn calls `ask_user_question`; the
/// runner's System row routes through the harness-installed
/// `InteractiveModule` (the production delivery-action path), so the
/// question card reaches the cli `MockAdapter` and the pending question
/// is recorded with its ask-time origin.
///
/// Expiry (test-side): host-side sweep timing runs on wall/tokio time —
/// NOT the runner `TestClock` — so a fixture cannot advance the 24h TTL
/// (see `fixtures/README-m21-wave1.md`, reachability item 2). The
/// module handle here is built with a ZERO TTL so the ask is already
/// lapsed when the real `SweepService` pass runs; TTL *selection*
/// precision is pinned by the paused-time crate tests in
/// `copperclaw-host-sweep` (`run_once_surfaces_expired_questions_when_
/// store_wired`) and `copperclaw-modules` (`sweep_expiry_selection_
/// honors_the_ttl_boundary`). The sweep pass must surface the lapse
/// exactly once: a terminal edit stamping the delivered card with the
/// expiry note (typed adapter edit — no live buttons left behind), plus
/// a `trigger = 0` synthetic no-answer result in the session inbox.
///
/// Step 2 (fixture): the user's late reply spawns a normal turn whose
/// provider request must carry the synthetic `ask_user_question_result`
/// — the agent's next turn sees the no-answer result — and the reply is
/// delivered normally. The expected JSONL streams pin every row,
/// including the sweep-written edit note and result row.
///
/// Regenerate expected streams with
/// `COPPERCLAW_M21F2_GENERATE=1 cargo test ... cli_question_expiry`.
#[tokio::test]
async fn cli_question_expiry_surfaces_lapse_and_resumes_on_reply() {
    use copperclaw_host_sweep::service::FilesystemSessionRoot;
    use copperclaw_host_sweep::{EXPIRED_QUESTION_TEXT, SweepService};
    use copperclaw_modules::InteractiveModule;
    use std::sync::Arc;

    let generate = std::env::var_os("COPPERCLAW_M21F2_GENERATE").is_some();
    let path = fixture_path("cli", "question-expiry");
    assert!(
        path.exists(),
        "fixture missing at {} — see docs/replay-fixtures.md",
        path.display()
    );
    let fixture = Fixture::load(&path).expect("load question-expiry fixture");
    let mut harness = ReplayHarness::new(fixture).await.expect("boot harness");

    // Production wiring in miniature: one InteractiveModule installed
    // into the delivery service, a state-sharing clone handed to the
    // sweep (`boot.rs` does exactly this via `set_question_store`).
    let interactive = InteractiveModule::with_timeout(chrono::Duration::zero());
    harness
        .install_interactive_module(&interactive)
        .await
        .expect("install InteractiveModule");

    // ── Step 1: ask ──
    harness.run_steps(0, 1).await.expect("drive ask step");
    assert_eq!(
        interactive.pending().len(),
        1,
        "the delivered ask must record one pending question"
    );
    let mock = Arc::clone(mock_for(&harness, "cli"));
    let cards = mock
        .deliveries()
        .iter()
        .filter(|d| d.message.content.get("card").is_some())
        .count();
    assert_eq!(cards, 1, "the question card must reach the adapter");

    // ── Expiry: one real sweep pass over the shared question set ──
    let sweep = SweepService::new(
        harness.central.clone(),
        Arc::new(FilesystemSessionRoot::new(harness.tempdir.path())),
    );
    sweep.set_question_store(interactive.clone());
    let report = sweep.run_once().expect("sweep pass");
    assert_eq!(report.questions_expired.len(), 1, "one lapsed question");
    assert!(report.questions_expired[0].note_emitted);
    assert!(!report.questions_expired[0].resolved_by_reply);
    assert!(interactive.pending().is_empty(), "terminal in module state");

    // Second pass byte-quiet: the lapse is surfaced exactly once.
    let report2 = sweep.run_once().expect("second sweep pass");
    assert!(report2.questions_expired.is_empty());

    // Deliver the expiry note: the cli MockAdapter supports the typed
    // edit API, so the card is stamped terminal IN PLACE.
    let (_ag, sess) = harness.touched_sessions[0];
    let session = copperclaw_db::tables::sessions::get(&harness.central, sess).unwrap();
    harness
        .delivery
        .process_session_once(&session)
        .await
        .expect("deliver expiry note");
    let edits = mock.edits();
    assert_eq!(edits.len(), 1, "exactly one terminal card edit");
    assert_eq!(
        edits[0].new_text, EXPIRED_QUESTION_TEXT,
        "the edit carries the expiry note copy"
    );

    // ── Step 2: the user replies later; conversation resumes ──
    harness
        .run_steps(1, 2)
        .await
        .expect("drive late-reply step");

    if generate {
        harness.dump_expected_jsonl();
        return;
    }

    // Byte-stable pipeline diff (includes the sweep-written edit note in
    // messages-out and the synthetic result row in messages-in).
    let diff = harness.compare().expect("compare");
    assert!(diff.is_clean(), "{diff}");

    // The agent's next turn saw the no-answer result: the last provider
    // request body contains the synthetic ask_user_question_result.
    let reqs = harness
        .anthropic_server
        .received_requests()
        .await
        .expect("wiremock request log");
    let last_body =
        String::from_utf8_lossy(&reqs.last().expect("provider calls").body).into_owned();
    assert!(
        last_body.contains("ask_user_question_result"),
        "final turn's prompt must carry the synthetic no-answer result: {last_body}"
    );
    assert!(
        last_body.contains("expired"),
        "the result must be marked expired: {last_body}"
    );
    assert!(
        last_body.contains("spaces please"),
        "the late reply must ride the same turn: {last_body}"
    );

    // And the reply itself was delivered normally after the expiry.
    let final_replies = mock
        .deliveries()
        .iter()
        .filter_map(|d| d.message.content.get("text").and_then(|t| t.as_str()))
        .filter(|t| t.contains("Spaces it is"))
        .count();
    assert_eq!(final_replies, 1, "conversation resumes normally");
}

/// M21 S3 (X-rider W1): delivery retry counters survive a service
/// restart, with exactly-once dead-lettering — at the pipeline level,
/// against a runner-produced outbound row, using the harness's
/// `restart_delivery` seam (fresh `DeliveryService` + adapters over the
/// same central DB and per-session files, exactly what a real host
/// restart preserves).
///
/// Attempt 1 happens inside the fixture run (a scripted transport
/// failure via `pre_delivery_failures`), persisting `tries = 1` and a
/// wall-clock `not_before` window ON THE ROW (migration 029). The test
/// then restarts the delivery service before each subsequent attempt —
/// so every attempt is served by a service that must re-prime its retry
/// cache from the row. Three failures across three service lifetimes
/// exhaust `MAX_DELIVERY_ATTEMPTS` (3): if a restart reset the budget,
/// the third attempt would defer instead of dead-lettering and every
/// assertion below fails. Exhaustion dead-letters exactly once (one
/// terminal `delivered{status=failed}` record + one ErrorCard — the
/// no-adapter expiry path, not retry exhaustion, is what feeds the
/// central dropped-messages table), the failure card reaches the user
/// exactly once, and a further restart + pass changes nothing.
///
/// (The window between attempts is elapsed by rewinding the PERSISTED
/// `not_before` — legitimate here because each restarted service reads
/// the window from the row, which is precisely the S3 contract under
/// test. Real sleeps would test the same thing slowly and jittery.)
#[tokio::test]
async fn cli_delivery_retry_restart_resumes_and_dead_letters_once() {
    use copperclaw_channels_core::AdapterError;
    use copperclaw_db::session::{SessionPaths, open_inbound_rw_no_mmap, open_outbound};
    use copperclaw_db::tables::{delivered, messages_out};
    use copperclaw_types::MessageKind;
    use std::sync::Arc;

    /// Read the row's persisted retry state and rewind its `not_before`
    /// window into the past — "the host was down longer than the backoff
    /// window". Returns the persisted attempt count.
    fn rewind_window(paths: &SessionPaths) -> u32 {
        let conn = open_outbound(paths).unwrap();
        let persisted = messages_out::list_retry_state(&conn).unwrap();
        assert_eq!(persisted.len(), 1, "one row mid-retry: {persisted:?}");
        messages_out::set_retry_state(
            &conn,
            persisted[0].id,
            persisted[0].tries,
            Some(chrono::Utc::now() - chrono::Duration::seconds(5)),
        )
        .unwrap();
        persisted[0].tries
    }

    if m21x1_maybe_generate("cli", "delivery-retry-restart").await {
        return;
    }
    let mut harness = run_fixture_into_harness("cli", "delivery-retry-restart").await;
    let (ag, sess) = harness.touched_sessions[0];
    let session = copperclaw_db::tables::sessions::get(&harness.central, sess).unwrap();
    let paths = SessionPaths::new(harness.tempdir.path(), ag, sess);

    // ── Attempt 1 (inside the fixture run) failed and PERSISTED ──
    assert!(
        mock_for(&harness, "cli").deliveries().is_empty(),
        "the scripted transport failure means nothing was delivered"
    );
    {
        let conn = open_outbound(&paths).unwrap();
        let persisted = messages_out::list_retry_state(&conn).unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].tries, 1, "attempt count persisted on the row");
        assert!(
            persisted[0].not_before.is_some(),
            "backoff window persisted as wall-clock not_before"
        );
    }

    // ── Restart 1: the resumed service picks up at tries=1, attempt 2 fails ──
    assert_eq!(rewind_window(&paths), 1);
    harness.restart_delivery();
    let mock2 = Arc::clone(mock_for(&harness, "cli"));
    mock2.fail_next_deliver(AdapterError::Transport("502 after restart".into()));
    let _ = harness
        .delivery
        .process_session_once(&session)
        .await
        .unwrap();
    assert!(
        mock2.deliveries().is_empty(),
        "attempt 2 failed on the wire"
    );
    assert_eq!(
        rewind_window(&paths),
        2,
        "the restarted service resumed at the persisted count (1 -> 2), not at 0"
    );

    // ── Restart 2: attempt 3 exhausts the budget — dead-letter exactly once ──
    harness.restart_delivery();
    let mock3 = Arc::clone(mock_for(&harness, "cli"));
    mock3.fail_next_deliver(AdapterError::Transport("502 final".into()));
    let rpt = harness
        .delivery
        .process_session_once(&session)
        .await
        .unwrap();
    assert_eq!(
        rpt.failed, 1,
        "third failure across three service lifetimes exhausts MAX_DELIVERY_ATTEMPTS"
    );
    assert!(
        mock3.deliveries().is_empty(),
        "the exhausting attempt failed; nothing reached the wire this pass"
    );
    {
        let in_conn = open_inbound_rw_no_mmap(&paths).unwrap();
        let failed: Vec<_> = delivered::list(&in_conn)
            .unwrap()
            .into_iter()
            .filter(|d| d.status == "failed")
            .collect();
        assert_eq!(
            failed.len(),
            1,
            "exactly one delivered{{status=failed}} row"
        );
        let out_conn = open_outbound(&paths).unwrap();
        let error_rows = messages_out::list_due(&out_conn)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == MessageKind::Error)
            .count();
        assert_eq!(error_rows, 1, "exactly one delivery-failure ErrorCard row");
    }

    // ── Restart 3: the user hears about the failure exactly once, and
    // the terminal state is stable across yet another restart ──
    harness.restart_delivery();
    let mock4 = Arc::clone(mock_for(&harness, "cli"));
    let rpt = harness
        .delivery
        .process_session_once(&session)
        .await
        .unwrap();
    assert_eq!(rpt.failed, 0, "no re-dead-letter after a further restart");
    assert_eq!(
        rpt.delivered, 1,
        "the delivery-failure ErrorCard reaches the wire (once)"
    );
    let cards: Vec<String> = mock4
        .deliveries()
        .iter()
        .filter_map(|d| d.message.content.get("text").and_then(|t| t.as_str()))
        .filter(|t| t.contains("Could not deliver message"))
        .map(str::to_owned)
        .collect();
    assert_eq!(
        cards.len(),
        1,
        "exactly one failure card on the wire: {:?}",
        mock4.deliveries()
    );
    // A second pass on the final service adds nothing: the failed row
    // and the delivered card are both terminal.
    let rpt2 = harness
        .delivery
        .process_session_once(&session)
        .await
        .unwrap();
    assert_eq!(rpt2.failed, 0);
    assert_eq!(rpt2.delivered, 0, "terminal state: nothing left to deliver");
    assert_eq!(mock4.deliveries().len(), 1, "no re-delivery of the card");
    // The poisoned chat text itself never reached any adapter incarnation.
    assert!(
        mock3
            .deliveries()
            .iter()
            .chain(mock4.deliveries().iter())
            .all(|d| d
                .message
                .content
                .get("text")
                .and_then(|t| t.as_str())
                .is_none_or(|t| !t.contains("Hi across the restart!"))),
        "the dead-lettered chat text must never reach the wire"
    );
    {
        let in_conn = open_inbound_rw_no_mmap(&paths).unwrap();
        let failed = delivered::list(&in_conn)
            .unwrap()
            .into_iter()
            .filter(|d| d.status == "failed")
            .count();
        assert_eq!(failed, 1, "dead-letter stays exactly-once across restarts");
        let out_conn = open_outbound(&paths).unwrap();
        let error_rows = messages_out::list_due(&out_conn)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == MessageKind::Error)
            .count();
        assert_eq!(
            error_rows, 1,
            "ErrorCard stays exactly-once across restarts"
        );
    }
}

// ─── M21 Wave-2 X-rider: feedback fixtures ───────────────────────────────────
//
// Lock the "user is never in the dark" wave into the replay test set.
// Wave 2's third behavior (F2: question expiry -> terminal note ->
// unblocked agent) was already pinned end to end by F2's own fixture
// (`fixtures/cli/question-expiry/`,
// `cli_question_expiry_surfaces_lapse_and_resumes_on_reply` above) — it
// is folded into the coverage map, not duplicated here. The two tests
// below pin the remaining Wave-2 behaviors:
//
// - F1 (`fixtures/cli/slow-spawn-notice/`): spawn-phase typing + the one
//   slow-spawn notice, clock-advanced via a mid-test `tokio::time::pause()`
//   section (the F1 timers run on HOST tokio time, which a fixture
//   manifest cannot advance — see `fixtures/README-m21-wave2.md`).
// - F3 (`fixtures/cli/restart-recovery-notice/`): the host-restart
//   recovery notice, exactly once, through the REAL boot step
//   (`boot::reset_stale_running_sessions`) and the real delivery service.

/// Fixture-authoring gate for the M21 Wave-2 X-rider fixtures: when
/// `COPPERCLAW_M21X2_GENERATE` is set, the tests dump the captured
/// streams (via `dump_expected_jsonl`) instead of asserting the JSONL
/// diff, so `expected/*.jsonl` can be regenerated from a real run.
/// Never taken under a normal `cargo test`.
fn m21x2_generate() -> bool {
    std::env::var_os("COPPERCLAW_M21X2_GENERATE").is_some()
}

/// Container runtime whose `spawn` blocks until released — the mock
/// stand-in for a slow first image build / pull (the exact shape F1's
/// 20s threshold targets). `entered` signals the moment the runtime
/// call begins; `release` lets it complete successfully.
#[derive(Default)]
struct HoldSpawnRuntime {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl copperclaw_container_rt::ContainerRuntime for HoldSpawnRuntime {
    async fn ensure_running(&self) -> Result<(), copperclaw_container_rt::RtError> {
        Ok(())
    }
    async fn cleanup_orphans(&self, _slug: &str) -> Result<(), copperclaw_container_rt::RtError> {
        Ok(())
    }
    async fn spawn(
        &self,
        spec: copperclaw_container_rt::ContainerSpec,
    ) -> Result<copperclaw_container_rt::ContainerHandle, copperclaw_container_rt::RtError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(copperclaw_container_rt::ContainerHandle::new(
            format!("hold-{}-id", spec.name),
            spec.name,
        ))
    }
    async fn stop(
        &self,
        _name: &str,
        _grace: std::time::Duration,
    ) -> Result<(), copperclaw_container_rt::RtError> {
        Ok(())
    }
    async fn build_image(
        &self,
        spec: copperclaw_container_rt::ImageBuildSpec,
    ) -> Result<String, copperclaw_container_rt::RtError> {
        Ok(spec.image_tag())
    }
}

/// Typing-observation seam: wraps the harness delivery service's REAL
/// dispatcher (the same `DeliveryDispatcher` handle boot hands the
/// production ticker), records every `set_typing` target, and forwards
/// everything to the inner dispatcher so the real adapter path still
/// runs. Needed because `MockAdapter` does not record `set_typing`
/// calls — the dispatcher seam is the closest observable point that
/// still exercises the production wiring.
struct RecordingTypingDispatcher {
    inner: std::sync::Arc<dyn copperclaw_modules::DeliveryDispatcher>,
    typing: std::sync::Mutex<Vec<copperclaw_modules::DispatchTarget>>,
}

impl RecordingTypingDispatcher {
    fn new(inner: std::sync::Arc<dyn copperclaw_modules::DeliveryDispatcher>) -> Self {
        Self {
            inner,
            typing: std::sync::Mutex::new(Vec::new()),
        }
    }
    fn typing_targets(&self) -> Vec<copperclaw_modules::DispatchTarget> {
        self.typing.lock().unwrap().clone()
    }
}

impl copperclaw_modules::DeliveryDispatcher for RecordingTypingDispatcher {
    fn set_typing(
        &self,
        target: &copperclaw_modules::DispatchTarget,
    ) -> Option<tokio::sync::oneshot::Receiver<copperclaw_modules::TypingOutcome>> {
        self.typing.lock().unwrap().push(target.clone());
        self.inner.set_typing(target)
    }
    fn dispatch(
        &self,
        target: &copperclaw_modules::DispatchTarget,
        message: &copperclaw_types::OutboundMessage,
    ) {
        self.inner.dispatch(target, message);
    }
    fn edit_message(
        &self,
        target: &copperclaw_modules::DispatchTarget,
        platform_message_id: &str,
        text: &str,
    ) {
        self.inner.edit_message(target, platform_message_id, text);
    }
}

/// The stuck-tool / cold-start `ManagerConfig`, parameterized on the
/// harness (same shape as the Wave-1 stuck-restart test's inline copy).
fn m21x2_manager_cfg(harness: &ReplayHarness) -> copperclaw_host::container_manager::ManagerConfig {
    use copperclaw_host::container_manager::{
        DEFAULT_HEARTBEAT_STALE_SECS, DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_STOP_GRACE_SECS,
        ManagerConfig,
    };
    ManagerConfig {
        install_slug: "replay".into(),
        data_dir: harness.tempdir.path().to_path_buf(),
        default_image_tag: "copperclaw/session:replay".into(),
        default_provider: "anthropic".into(),
        default_model: "claude-sonnet-4-6".into(),
        default_effort: None,
        anthropic_api_key: Some("harness".into()),
        anthropic_base_url: Some(harness.anthropic_server.uri()),
        idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
        heartbeat_stale_secs: DEFAULT_HEARTBEAT_STALE_SECS,
        stop_grace_secs: DEFAULT_STOP_GRACE_SECS,
        skills_dir: None,
        groups_dir: None,
        skills_mode: copperclaw_host::SkillsMode::default(),
        gpu_passthrough: false,
        forward_env: Vec::new(),
        egress_mode: copperclaw_container_rt::EgressMode::AllowAll,
    }
}

/// M21 F1 (X-rider W2): cold-start feedback, end to end through the
/// replay pipeline over `fixtures/cli/slow-spawn-notice/` — the leg
/// F1's own paused-clock crate tests (`cold_start.rs`,
/// `typing_ticker.rs`) stop short of: a ROUTER-created session, the
/// REAL `TypingTicker::run_loop` reading the manager's shared
/// `SpawnActivity` registry through the harness delivery service's real
/// dispatcher, and the slow-spawn notice reaching the WIRE through the
/// real `DeliveryService`, with the same pending inbound then processed
/// by the fixture's scripted turn.
///
/// Timing: F1's watchdog (20s `SLOW_SPAWN_NOTICE_AFTER`) and the
/// ticker's 4s cadence run on HOST tokio time — no fixture manifest can
/// advance them (the S6 `TestClock` reaches only the runner). The test
/// therefore brackets the spawn-phase leg in a mid-test
/// `tokio::time::pause()` section: everything inside it (the held
/// runtime spawn, the ticker loop, the watchdog) is pure timer/DB work,
/// so the paused clock auto-advances deterministically with zero real
/// waits; the clock is resumed before the delivery + runner legs, which
/// do real (wiremock) I/O.
///
/// Sequence pinned:
/// 1. First message routed cold: session `Stopped`, due inbound,
///    routing seeded (`ReplayHarness::route_step_cold`).
/// 2. `maybe_spawn` held mid-runtime-call: typing pulses through the
///    real dispatcher within one tick, BEFORE the runner is up; zero
///    notices below the threshold.
/// 3. Crossing 20s posts exactly ONE notice row; three more minutes
///    held adds none (episode dedup).
/// 4. Released spawn completes; the notice reaches the cli MockAdapter
///    exactly once; the deferred turn answers the same inbound; a
///    further delivery pass adds nothing; all four JSONL streams
///    byte-stable.
///
/// Regenerate expected streams with
/// `COPPERCLAW_M21X2_GENERATE=1 cargo test -p copperclaw-host --test replay cli_slow_spawn -- --nocapture`.
#[tokio::test]
async fn cli_slow_spawn_typing_and_single_notice_end_to_end() {
    use copperclaw_db::session::{SessionPaths, open_outbound};
    use copperclaw_db::tables::{messages_out, sessions};
    use copperclaw_host::container_manager::ContainerManager;
    use copperclaw_host::container_manager::cold_start::{
        SLOW_SPAWN_NOTICE_AFTER, SLOW_SPAWN_NOTICE_TEXT, SpawnActivity,
    };
    use copperclaw_host::typing_ticker::TypingTicker;
    use copperclaw_types::ContainerStatus;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    /// Outbound rows carrying the slow-spawn notice text.
    fn notice_rows(paths: &SessionPaths) -> usize {
        let conn = open_outbound(paths).unwrap();
        messages_out::list_due(&conn)
            .unwrap()
            .into_iter()
            .filter(|r| {
                r.content.get("text").and_then(|v| v.as_str()) == Some(SLOW_SPAWN_NOTICE_TEXT)
            })
            .count()
    }

    let path = fixture_path("cli", "slow-spawn-notice");
    assert!(
        path.exists(),
        "fixture missing at {} — see docs/replay-fixtures.md",
        path.display()
    );
    let fixture = Fixture::load(&path).expect("load slow-spawn-notice fixture");
    let mut harness = ReplayHarness::new(fixture).await.expect("boot harness");

    // ── 1. Route the first message COLD: no runner, no delivery, no
    // mark-running — the exact state the spawn classifier sees. ──
    let (ag, sess) = harness.route_step_cold(0).await.expect("route cold");
    let paths = SessionPaths::new(harness.tempdir.path(), ag, sess);
    assert_eq!(
        sessions::get(&harness.central, sess)
            .unwrap()
            .container_status,
        ContainerStatus::Stopped,
        "cold route must leave the session un-spawned"
    );

    // Production wiring in miniature (boot.rs hands the SAME registry to
    // the manager and the ticker, and the SAME delivery dispatcher to
    // the ticker): one shared SpawnActivity; a manager over a runtime
    // that holds its spawn; the real run_loop ticker observing typing
    // through a recorder that forwards to the harness delivery
    // service's real dispatcher.
    let activity = Arc::new(SpawnActivity::new());
    let runtime = Arc::new(HoldSpawnRuntime::default());
    let mgr = Arc::new(
        ContainerManager::new(
            harness.central.clone(),
            Arc::clone(&runtime) as Arc<dyn copperclaw_container_rt::ContainerRuntime>,
            m21x2_manager_cfg(&harness),
        )
        .with_spawn_activity(Arc::clone(&activity)),
    );
    let recorder = Arc::new(RecordingTypingDispatcher::new(
        harness.delivery.dispatcher(),
    ));
    let ticker = Arc::new(
        TypingTicker::new(
            harness.central.clone(),
            Arc::clone(&recorder) as Arc<dyn copperclaw_modules::DeliveryDispatcher>,
            harness.tempdir.path(),
        )
        .with_spawn_activity(Arc::clone(&activity)),
    );

    // ── Paused-clock section: the spawn-phase leg. ──
    tokio::time::pause();
    let cancel = CancellationToken::new();
    let ticker_task = tokio::spawn(Arc::clone(&ticker).run_loop(cancel.clone()));
    let spawn_task = {
        let mgr = Arc::clone(&mgr);
        let session = sessions::get(&harness.central, sess).unwrap();
        tokio::spawn(async move { mgr.maybe_spawn(&session).await })
    };
    runtime.entered.notified().await;

    // 2. The runner is NOT up (the runtime call is blocked; the session
    // is still Stopped) — one ticker interval in, typing has pulsed
    // through the real dispatcher at the session's routed target, and
    // no notice exists below the threshold.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        sessions::get(&harness.central, sess)
            .unwrap()
            .container_status,
        ContainerStatus::Stopped,
        "the spawn attempt must still be in flight"
    );
    let typed = recorder.typing_targets();
    assert!(
        !typed.is_empty(),
        "mid-spawn session with pending inbound must pulse typing within one tick"
    );
    assert_eq!(
        typed[0]
            .channel_type
            .as_ref()
            .map(copperclaw_types::ChannelType::as_str),
        Some("cli"),
        "typing must land on the session's routed channel"
    );
    assert_eq!(typed[0].platform_id.as_deref(), Some("stdin"));
    assert_eq!(notice_rows(&paths), 0, "no notice below the 20s threshold");

    // 3. Cross the threshold: exactly one notice; minutes more of the
    // same held spawn add none.
    tokio::time::sleep(SLOW_SPAWN_NOTICE_AFTER).await;
    assert_eq!(
        notice_rows(&paths),
        1,
        "crossing the threshold posts the one slow-spawn notice"
    );
    tokio::time::sleep(Duration::from_secs(180)).await;
    assert_eq!(
        notice_rows(&paths),
        1,
        "a held spawn never posts a second notice"
    );

    // 4. Release: the spawn completes, the attempt unregisters.
    runtime.release.notify_one();
    assert!(
        spawn_task.await.unwrap().unwrap(),
        "released spawn completes"
    );
    assert!(activity.active_sessions().is_empty());
    cancel.cancel();
    ticker_task.await.unwrap();
    tokio::time::resume();

    // ── Real-time tail: the notice reaches the wire, then the deferred
    // turn processes the SAME pending inbound. ──
    let session = sessions::get(&harness.central, sess).unwrap();
    harness
        .delivery
        .process_session_once(&session)
        .await
        .expect("deliver slow-spawn notice");
    let mock = Arc::clone(mock_for(&harness, "cli"));
    let on_wire = |mock: &copperclaw_channels_core::testing::MockAdapter| {
        mock.deliveries()
            .iter()
            .filter_map(|d| d.message.content.get("text").and_then(|t| t.as_str()))
            .filter(|t| *t == SLOW_SPAWN_NOTICE_TEXT)
            .count()
    };
    assert_eq!(on_wire(&mock), 1, "exactly one notice on the wire");

    harness
        .run_turn_and_deliver(ag, sess)
        .await
        .expect("deferred turn");
    let replies = mock
        .deliveries()
        .iter()
        .filter_map(|d| d.message.content.get("text").and_then(|t| t.as_str()))
        .filter(|t| t.contains("All set up and ready"))
        .count();
    assert_eq!(replies, 1, "the held-spawn inbound still gets its reply");
    assert_eq!(on_wire(&mock), 1, "the notice stays exactly-once");

    // A further delivery pass adds nothing.
    let rpt = harness
        .delivery
        .process_session_once(&sessions::get(&harness.central, sess).unwrap())
        .await
        .unwrap();
    assert_eq!(rpt.delivered, 0, "nothing left to deliver");

    if m21x2_generate() {
        harness.dump_expected_jsonl();
        return;
    }
    let diff = harness.compare().expect("compare");
    assert!(diff.is_clean(), "{diff}");
}

/// M21 F3 (X-rider W2): the host-restart recovery notice, end to end
/// through the replay pipeline over
/// `fixtures/cli/restart-recovery-notice/` — the leg F3's own boot
/// crate tests (`boot.rs::tests::boot_recovery`) stop short of: a
/// ROUTER-created session whose routing and baseline turn came off the
/// real pipeline, the REAL boot step
/// (`boot::reset_stale_running_sessions`) run over it, and the notice
/// actually REACHING the channel adapter through the real
/// `DeliveryService`, exactly once — with the interrupted inbound then
/// processed normally by the respawned (deferred) turn.
///
/// The fixture drives one normal turn. The test then reproduces exactly
/// the state a host death mid-turn leaves behind — this is state, not
/// behavior (the harness cannot kill and re-run a host process):
/// session `running`, one pending inbound old enough that the sweep's
/// `pending_too_long` apology WOULD fire were the dedupe stamp absent,
/// and a `Processing` claim (a runner had picked the turn up). Then:
///
/// 1. The real boot step emits exactly one notice row (crash-restart
///    copy), flips the claim, stamps the row — and leaves it due.
/// 2. A second boot pass (host restarted twice) emits nothing.
/// 3. A real `SweepService` pass emits nothing either (the stamps keep
///    both sweep apology paths out).
/// 4. Delivery hands the notice to the cli `MockAdapter` exactly once;
///    a second pass is quiet.
/// 5. The re-queued inbound processes: the deferred turn answers it and
///    the reply is delivered. All four JSONL streams byte-stable.
///
/// Regenerate expected streams with
/// `COPPERCLAW_M21X2_GENERATE=1 cargo test -p copperclaw-host --test replay cli_restart_recovery -- --nocapture`.
#[tokio::test]
async fn cli_restart_recovery_notice_delivered_exactly_once() {
    use copperclaw_db::session::{SessionPaths, open_inbound, open_outbound};
    use copperclaw_db::tables::{messages_in, messages_out, processing_ack, sessions};
    use copperclaw_host::boot::reset_stale_running_sessions;
    use copperclaw_host_sweep::SweepService;
    use copperclaw_host_sweep::service::FilesystemSessionRoot;
    use copperclaw_types::{ChannelType, ContainerStatus, MessageId, MessageKind};
    use std::sync::Arc;

    /// Chat rows carrying the recovery-notice copy (the crash-restart
    /// apology text — `CRASH_RESTART_APOLOGY_TEXT` is crate-private, so
    /// match on its stable "snag"/"restart" phrasing like the Wave-1
    /// stuck-restart test does).
    fn notice_rows(paths: &SessionPaths) -> Vec<copperclaw_types::MessageOutRow> {
        let conn = open_outbound(paths).unwrap();
        messages_out::list_due(&conn)
            .unwrap()
            .into_iter()
            .filter(|r| {
                r.kind == MessageKind::Chat
                    && r.content
                        .get("text")
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| t.contains("snag") && t.contains("restart"))
            })
            .collect()
    }

    let path = fixture_path("cli", "restart-recovery-notice");
    assert!(
        path.exists(),
        "fixture missing at {} — see docs/replay-fixtures.md",
        path.display()
    );
    let fixture = Fixture::load(&path).expect("load restart-recovery-notice fixture");
    let mut harness = ReplayHarness::new(fixture).await.expect("boot harness");

    // ── Baseline turn off the real pipeline. ──
    harness.run_steps(0, 1).await.expect("baseline step");
    let (ag, sess) = harness.touched_sessions[0];
    let paths = SessionPaths::new(harness.tempdir.path(), ag, sess);
    let mock = Arc::clone(mock_for(&harness, "cli"));
    assert_eq!(mock.deliveries().len(), 1, "baseline reply delivered");

    // ── Reproduce the host-death-mid-turn state. ──
    sessions::mark_container_running(&harness.central, sess).unwrap();
    let msg_id = MessageId::new();
    {
        let conn = open_inbound(&paths).unwrap();
        messages_in::insert(
            &conn,
            &messages_in::WriteInbound {
                id: msg_id,
                kind: MessageKind::Chat,
                // Old enough that the sweep's pending_too_long apology
                // WOULD fire were the boot path's tries stamp absent —
                // making assertion 3 below meaningful.
                timestamp: chrono::Utc::now() - chrono::Duration::minutes(10),
                content: serde_json::json!({"text": "are you done with the report?"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("stdin".into()),
                channel_type: Some(ChannelType::new(ChannelType::CLI)),
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
    }
    {
        let outbound = open_outbound(&paths).unwrap();
        processing_ack::insert(
            &outbound,
            msg_id,
            processing_ack::ProcessingStatus::Processing,
        )
        .unwrap();
    }

    // ── 1. The REAL boot step: reset + one recovery notice. ──
    reset_stale_running_sessions(&harness.central, harness.tempdir.path());
    assert_eq!(
        sessions::get(&harness.central, sess)
            .unwrap()
            .container_status,
        ContainerStatus::Stopped,
        "boot reset must make the session spawnable again"
    );
    let notices = notice_rows(&paths);
    assert_eq!(notices.len(), 1, "exactly one recovery notice row");
    assert_eq!(
        notices[0].in_reply_to,
        Some(msg_id),
        "the notice is routed at the interrupted inbound"
    );
    {
        let outbound = open_outbound(&paths).unwrap();
        let claim = processing_ack::get(&outbound, msg_id).unwrap().unwrap();
        assert_eq!(
            claim.status,
            processing_ack::ProcessingStatus::Failed,
            "the boot path owns the interrupted turn now"
        );
        let inbound = open_inbound(&paths).unwrap();
        assert_eq!(
            messages_in::count_due(&inbound).unwrap(),
            1,
            "the interrupted inbound stays due for the respawned runner"
        );
    }

    // ── 2. Second boot pass (host restarted twice): byte-quiet. ──
    sessions::mark_container_running(&harness.central, sess).unwrap();
    reset_stale_running_sessions(&harness.central, harness.tempdir.path());
    assert_eq!(
        notice_rows(&paths).len(),
        1,
        "a repeat boot pass must not re-notice"
    );

    // ── 3. A real sweep pass: the dedupe stamps keep both sweep
    // apology paths out (the row is past the pending_too_long
    // threshold, so absent the stamp this WOULD add an apology). ──
    let sweep = SweepService::new(
        harness.central.clone(),
        Arc::new(FilesystemSessionRoot::new(harness.tempdir.path())),
    );
    sweep.run_once().expect("sweep pass");
    assert_eq!(
        notice_rows(&paths).len(),
        1,
        "the sweep's apology paths must stay out"
    );

    // ── 4. The notice reaches the WIRE exactly once. ──
    let session = sessions::get(&harness.central, sess).unwrap();
    harness
        .delivery
        .process_session_once(&session)
        .await
        .expect("deliver recovery notice");
    let on_wire = |mock: &copperclaw_channels_core::testing::MockAdapter| {
        mock.deliveries()
            .iter()
            .filter_map(|d| d.message.content.get("text").and_then(|t| t.as_str()))
            .filter(|t| t.contains("snag") && t.contains("restart"))
            .count()
    };
    assert_eq!(on_wire(&mock), 1, "exactly one recovery notice on the wire");
    harness
        .delivery
        .process_session_once(&session)
        .await
        .expect("second delivery pass");
    assert_eq!(on_wire(&mock), 1, "the notice stays exactly-once");

    // ── 5. The re-queued inbound processes normally. ──
    harness
        .run_turn_and_deliver(ag, sess)
        .await
        .expect("requeued turn");
    let resumed = mock
        .deliveries()
        .iter()
        .filter_map(|d| d.message.content.get("text").and_then(|t| t.as_str()))
        .filter(|t| t.contains("picking your report right back up"))
        .count();
    assert_eq!(resumed, 1, "the interrupted turn resumes after the notice");
    assert_eq!(on_wire(&mock), 1, "still exactly one notice after resume");

    if m21x2_generate() {
        harness.dump_expected_jsonl();
        return;
    }
    let diff = harness.compare().expect("compare");
    assert!(diff.is_clean(), "{diff}");
}

// ── M21 Wave-3 X-rider (operator-surface fixtures) ──────────────────
//
// One replay-registered end-to-end test lives here: the O4 opt-in
// operator-alert destination, enqueue -> real `DeliveryService` -> wire,
// for a loop-death event, plus the secure-by-default silence when no
// destination is configured. The other three Wave-3 acceptance behaviours
// (O1 doctor rows, O2 quarantine sidecar/exclusion, O3 mid-session
// failover) are not reachable through the inbound->...->delivery replay
// pipeline; they are pinned by focused tests at the right layer and mapped
// — with the reachability analysis — in `fixtures/README-m21-wave3.md`.
// The O2->O1 cross-card seam (sweep quarantine artifact <-> doctor reader
// contract) is pinned by `tests/wave3_quarantine_doctor.rs`.

/// Fixture-authoring gate for the M21 Wave-3 X-rider fixture: when
/// `COPPERCLAW_M21X3_GENERATE` is set the caller regenerates
/// `expected/*.jsonl` from a real baseline run and returns before
/// asserting. Never taken under a normal `cargo test`.
fn m21x3_generate() -> bool {
    std::env::var_os("COPPERCLAW_M21X3_GENERATE").is_some()
}

/// M21 O4 (X-rider W3): the opt-in operator-alert destination, end to end
/// through the replay pipeline over `fixtures/cli/operator-alert-delivery/`
/// — the leg O4's own unit tests (`operator_alerts.rs`) stop short of: the
/// enqueued alert row actually REACHING the channel adapter through the
/// real `DeliveryService`, routed to the operator's OWN configured target,
/// exactly once — and the secure-by-default silence when unconfigured.
///
/// The fixture drives one normal turn, leaving exactly one `Active`
/// carrier session (the one `OperatorAlerts::pick_carrier` selects). The
/// test then, on top of the byte-stable baseline:
///
/// 1. **Silence when unconfigured.** A DISABLED `OperatorAlerts` fires a
///    loop-death alert; a delivery pass follows. Zero alert rows, zero
///    operator-target deliveries — the pre-O4 log+metric-only world.
/// 2. **Enqueue -> delivery for a loop-death event.** A CONFIGURED
///    `OperatorAlerts` (cli channel, distinct `operator-cli` target) is
///    driven through the REAL S1 permanent-failure seam
///    (`run_degraded_watch`): flipping the supervisor `degraded` watch to
///    `true` enqueues exactly one alert row into the carrier's
///    `outbound.db`, carrying its OWN routing.
/// 3. A real `DeliveryService::process_session_once` pass hands it to the
///    cli `MockAdapter` at `operator-cli` (not the chat's `stdin`, proving
///    `resolve_target` routes on the row's own fields); a second pass is
///    quiet — delivered exactly once.
///
/// The enqueue-side semantics (dedup, rate-limit, disabled-default, the
/// degraded-watch fire-once) are pinned exhaustively by O4's own unit
/// tests; this test adds the one leg they omit — the wire.
///
/// Regenerate expected streams with
/// `COPPERCLAW_M21X3_GENERATE=1 cargo test -p copperclaw-host --test replay cli_operator_alert -- --nocapture`.
#[tokio::test]
async fn cli_operator_alert_delivered_and_silent_when_unconfigured() {
    use copperclaw_db::session::{SessionPaths, open_outbound};
    use copperclaw_db::tables::{messages_out, sessions};
    use copperclaw_host::operator_alerts::{AlertDestination, AlertSeverity, OperatorAlerts};
    use copperclaw_types::ChannelType;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    const OPERATOR_TARGET: &str = "operator-cli";
    const LOOP_DEATH_KEY: &str = "supervisor.degraded";

    /// Alert rows (cli channel, the operator target) in the carrier's
    /// `outbound.db`.
    fn alert_rows(paths: &SessionPaths) -> usize {
        let conn = open_outbound(paths).unwrap();
        messages_out::list_due(&conn)
            .unwrap()
            .into_iter()
            .filter(|r| {
                r.channel_type.as_ref().map(ChannelType::as_str) == Some("cli")
                    && r.platform_id.as_deref() == Some(OPERATOR_TARGET)
            })
            .count()
    }

    let path = fixture_path("cli", "operator-alert-delivery");
    assert!(
        path.exists(),
        "fixture missing at {} — see docs/replay-fixtures.md",
        path.display()
    );
    let fixture = Fixture::load(&path).expect("load operator-alert-delivery fixture");
    let mut harness = ReplayHarness::new(fixture).await.expect("boot harness");
    harness.run().await.expect("run baseline turn");

    if m21x3_generate() {
        harness.dump_expected_jsonl();
        return;
    }
    let diff = harness.compare().expect("compare");
    assert!(diff.is_clean(), "{diff}");

    let (ag, sess) = harness.touched_sessions[0];
    let carrier_paths = SessionPaths::new(harness.tempdir.path(), ag, sess);
    let carrier = sessions::get(&harness.central, sess).unwrap();
    let mock = Arc::clone(mock_for(&harness, "cli"));

    // Deliveries that reached the OPERATOR target (distinct from the
    // chat's `stdin`, so the baseline reply is never miscounted here).
    let to_operator = |mock: &copperclaw_channels_core::testing::MockAdapter| {
        mock.deliveries()
            .into_iter()
            .filter(|d| d.platform_id == OPERATOR_TARGET)
            .count()
    };
    assert_eq!(
        alert_rows(&carrier_paths),
        0,
        "no alert rows before O4 fires"
    );
    assert_eq!(to_operator(&mock), 0, "nothing at the operator target yet");

    // ── 1. Silence when unconfigured (secure-by-default). ──
    let disabled = OperatorAlerts::with_destination(
        harness.central.clone(),
        harness.tempdir.path().to_path_buf(),
        None,
    );
    assert!(!disabled.is_enabled());
    disabled.fire(
        AlertSeverity::Critical,
        LOOP_DEATH_KEY,
        "the sweep loop exceeded its restart budget",
    );
    assert_eq!(
        alert_rows(&carrier_paths),
        0,
        "a disabled destination must enqueue zero new outbound"
    );
    harness
        .delivery
        .process_session_once(&carrier)
        .await
        .expect("delivery pass after disabled fire");
    assert_eq!(
        to_operator(&mock),
        0,
        "nothing reaches the operator target when unconfigured"
    );

    // ── 2. Configured: a loop-death event enqueues exactly one alert. ──
    let dest = AlertDestination {
        channel_type: ChannelType::new("cli"),
        platform_id: OPERATOR_TARGET.into(),
        thread_id: None,
    };
    let alerts = Arc::new(OperatorAlerts::with_destination(
        harness.central.clone(),
        harness.tempdir.path().to_path_buf(),
        Some(dest),
    ));
    assert!(alerts.is_enabled());

    // Drive the REAL S1 permanent-failure seam: a supervised loop
    // exceeding its restart budget flips the `degraded` watch to true.
    let (tx, rx) = tokio::sync::watch::channel(false);
    let shutdown = CancellationToken::new();
    let watch = tokio::spawn(Arc::clone(&alerts).run_degraded_watch(rx, shutdown.clone()));
    tx.send(true).expect("flip degraded (loop death)");
    for _ in 0..200 {
        if alert_rows(&carrier_paths) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    shutdown.cancel();
    let _ = watch.await;
    assert_eq!(
        alert_rows(&carrier_paths),
        1,
        "a loop-death event enqueues exactly one alert row"
    );

    // ── 3. The alert reaches the operator target through real delivery. ──
    harness
        .delivery
        .process_session_once(&carrier)
        .await
        .expect("deliver the alert");
    assert_eq!(
        to_operator(&mock),
        1,
        "the alert reaches the operator target exactly once"
    );
    // It landed on the cli `MockAdapter` (channel routing from the row's own
    // `channel_type=cli`) at the operator target (platform routing from the
    // row's own `platform_id`) — not the session's `stdin` — so
    // `resolve_target` chose the alert row's OWN routing fields.
    let alert = mock
        .deliveries()
        .into_iter()
        .find(|d| d.platform_id == OPERATOR_TARGET)
        .expect("operator delivery present");
    let text = alert
        .message
        .content
        .get("text")
        .and_then(|t| t.as_str())
        .expect("alert body");
    assert!(
        text.contains("[copperclaw critical]"),
        "severity-prefixed body: {text}"
    );
    assert!(
        text.contains("degraded"),
        "the loop-death copy names the degraded host: {text}"
    );

    // A second delivery pass is quiet: delivered exactly once.
    harness
        .delivery
        .process_session_once(&carrier)
        .await
        .expect("second delivery pass");
    assert_eq!(
        to_operator(&mock),
        1,
        "the alert stays exactly-once on the wire"
    );

    // The baseline chat reply stayed on `stdin` — the alert's OWN routing,
    // not the carrier session's, chose the operator target.
    let stdin_replies = mock
        .deliveries()
        .into_iter()
        .filter(|d| {
            d.platform_id == "stdin"
                && d.message.content.get("text").and_then(|t| t.as_str())
                    == Some("All systems nominal.")
        })
        .count();
    assert_eq!(
        stdin_replies, 1,
        "the chat reply and the alert stayed on separate targets"
    );
}
