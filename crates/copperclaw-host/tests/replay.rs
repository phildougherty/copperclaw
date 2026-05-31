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

#[path = "replay/fixture.rs"]
mod fixture;
#[path = "replay/diff.rs"]
mod diff;
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

#[tokio::test]
async fn slack_event_message_round_trip() {
    run_fixture("slack", "event-message").await;
}

#[tokio::test]
async fn cli_multi_turn_round_trip() {
    run_fixture("cli", "multi-turn").await;
}

#[tokio::test]
async fn discord_inbound_message_round_trip() {
    run_fixture("discord", "inbound-message").await;
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
        assert!(text.chars().count() <= 40_000, "slack chunk {i} exceeds cap");
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
        assert!(text.chars().count() <= 2000, "discord chunk {i} exceeds cap");
    }
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
    let harness =
        run_fixture_into_harness("telegram", "rate-limited-retry").await;
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
