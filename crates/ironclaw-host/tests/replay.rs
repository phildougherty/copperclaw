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
    // CARGO_MANIFEST_DIR points at `crates/ironclaw-host/`; the workspace
    // root is two `parent()` calls up.
    manifest_dir
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root above ironclaw-host crate dir")
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
