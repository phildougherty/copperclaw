//! Per-inbound conversation-context block spliced into the runner's
//! system prompt.
//!
//! [`RunnerDeps::system`] is a static string assembled once at runner
//! startup — it carries the persona / skills / safety preamble but
//! knows nothing about the channel a given turn is replying into. The
//! model therefore answered identically whether it was speaking to one
//! person in a DM or addressing a 50-person group thread. This module
//! formats a short "Conversation context" paragraph the run loop
//! appends to the system prompt for each inbound batch.
//!
//! Reads `kind`, `channel_type`, `platform_id`, `thread_id`,
//! `source_session_id`, `is_group`, and `reply_to` off [`MessageInRow`].
//! `is_group` and `reply_to` are populated by the channel adapter onto
//! [`InboundEvent`] and persisted through the router into `messages_in`;
//! when either is `None` (the channel doesn't distinguish, or the
//! inbound is a top-level send) the block degrades silently rather
//! than fabricating phrasing.

use copperclaw_types::{MessageInRow, MessageKind};

/// Render a "Conversation context" block for one inbound batch.
///
/// Returns `None` when there's nothing useful to say (the batch is
/// empty, or every row is a synthetic system event with no channel
/// routing). Callers should append the returned block to
/// [`RunnerDeps::system`] for the duration of one `run_llm_turn`.
///
/// `history_len` is the size of the conversation transcript the model
/// will see — the cumulative count of prior `HistoryMessage`s including
/// assistant turns. Pulled in so the block can hint at how much context
/// the agent is replying into.
#[must_use]
pub(super) fn render_conversation_context(
    rows: &[MessageInRow],
    history_len: usize,
) -> Option<String> {
    // Pick the origin row the same way `run_loop` does for outbound
    // routing: first Chat-kind row if present, otherwise first row.
    // Keeps the block consistent with the channel the assistant's reply
    // will actually land on.
    let origin = rows
        .iter()
        .find(|r| r.kind == MessageKind::Chat)
        .or_else(|| rows.first())?;

    let mut block = String::from("Conversation context: ");

    // Lead with the venue shape. Three signals, in order of specificity:
    //   1. `is_group` (channel-adapter-populated, persisted onto
    //      `messages_in.is_group`): the most accurate read of "is this
    //      a group chat or a 1-on-1 DM" because it comes from the wire
    //      data. Used when present.
    //   2. `thread_id` fallback: when the channel doesn't surface
    //      `is_group` (CLI, file-watcher, webhooks), a non-null
    //      thread_id is the best proxy for "this is a sub-conversation"
    //      and we say "a thread"; otherwise "a direct conversation".
    // Either way, the phrasing is chosen so the model can act on it
    // without us having to combine the two signals into an awkward
    // four-arm matrix.
    let venue_kind = match origin.is_group {
        Some(true) => "a group chat",
        Some(false) => "a 1-on-1 DM",
        None => {
            if origin.thread_id.is_some() {
                "a thread"
            } else {
                "a direct conversation"
            }
        }
    };
    block.push_str("this turn is in ");
    block.push_str(venue_kind);

    if let Some(ct) = origin.channel_type.as_ref() {
        block.push_str(" on ");
        block.push_str(ct.as_str());
    }

    if let Some(pid) = origin.platform_id.as_deref() {
        block.push_str(" (");
        block.push_str(pid);
        block.push(')');
    }

    if let Some(tid) = origin.thread_id.as_deref() {
        block.push_str(", thread ");
        block.push_str(tid);
    }

    // Reply context: when the wire event carried a parent-message link
    // (Telegram `reply_to_message`, Slack `thread_ts` reply, Signal
    // quote, ...), the router persisted the parent's platform-side
    // message id onto `messages_in.reply_to`. Surface it so the model
    // knows the user is replying to a specific earlier exchange rather
    // than starting a fresh thought. We don't render the raw id (the
    // model can't usefully act on a Telegram message_id); the phrasing
    // is the actionable bit.
    if origin.reply_to.is_some() {
        block.push_str(", in reply to an earlier message");
    }

    // Inbound-batch shape: most turns are a single message; coalesced
    // batches (multiple inbounds in one poll) get surfaced so the model
    // knows it's replying to N things at once.
    let batch_len = rows.len();
    if batch_len > 1 {
        block.push_str(&format!(
            "; this turn coalesces {batch_len} new messages"
        ));
    }

    // Sub-agent context: when the runner is a child session spawned
    // by another agent, source_session_id is populated. Surface that so
    // the model knows it's not talking to a human directly.
    if let Some(src) = origin.source_session_id.as_deref() {
        block.push_str(&format!(
            "; relayed from parent agent session {src}"
        ));
    }

    // History depth — capped phrasing so we don't lie when the count
    // is 0 (fresh turn). The number is the count of prior history
    // entries (user/assistant/tool), not message-pairs.
    if history_len == 0 {
        block.push_str("; no prior history in this session");
    } else if history_len == 1 {
        block.push_str("; 1 prior entry in session history");
    } else {
        block.push_str(&format!(
            "; {history_len} prior entries in session history"
        ));
    }

    block.push('.');
    Some(block)
}

/// Append a rendered conversation-context block to the static system
/// prompt. Returns the combined string the provider call should use.
/// When `block` is `None` or empty, returns `system` clone-equivalent
/// so the caller never has to special-case the empty path.
#[must_use]
pub(super) fn system_with_context(system: &str, block: Option<&str>) -> String {
    match block {
        Some(b) if !b.is_empty() => {
            if system.is_empty() {
                b.to_string()
            } else {
                // Two newlines so the block reads as its own paragraph
                // regardless of whether the base system prompt ends in
                // one trailing newline already.
                let trimmed = system.trim_end_matches('\n');
                format!("{trimmed}\n\n{b}")
            }
        }
        _ => system.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use copperclaw_types::{ChannelType, MessageId, MessageInRow, MessageKind};
    use serde_json::json;

    fn row(
        kind: MessageKind,
        channel: Option<&str>,
        platform: Option<&str>,
        thread: Option<&str>,
    ) -> MessageInRow {
        MessageInRow {
            id: MessageId::new(),
            seq: 1,
            kind,
            timestamp: Utc::now(),
            status: "pending".into(),
            process_after: None,
            recurrence: None,
            series_id: None,
            tries: 0,
            trigger: true,
            platform_id: platform.map(str::to_string),
            channel_type: channel.map(ChannelType::new),
            thread_id: thread.map(str::to_string),
            content: json!({"text": "hi"}),
            source_session_id: None,
            on_wake: false,
            reply_to: None,
            is_group: None,
        }
    }

    #[test]
    fn dm_on_telegram_renders_direct_conversation() {
        let rows = vec![row(
            MessageKind::Chat,
            Some("telegram"),
            Some("8929393356"),
            None,
        )];
        let block = render_conversation_context(&rows, 0).expect("expected a block");
        assert!(
            block.contains("direct conversation"),
            "expected DM phrasing, got: {block}"
        );
        assert!(block.contains("telegram"), "got: {block}");
        assert!(block.contains("8929393356"), "got: {block}");
        // No "thread" phrasing for a DM.
        assert!(!block.contains(", thread "), "DM must not say 'thread': {block}");
        assert!(block.contains("no prior history"), "got: {block}");
    }

    #[test]
    fn telegram_group_thread_renders_thread_and_channel() {
        let rows = vec![row(
            MessageKind::Chat,
            Some("telegram"),
            Some("-100123456789"),
            Some("topic-42"),
        )];
        let block = render_conversation_context(&rows, 12).expect("expected a block");
        assert!(block.contains("a thread"), "got: {block}");
        assert!(block.contains("on telegram"), "got: {block}");
        assert!(block.contains("-100123456789"), "got: {block}");
        assert!(block.contains("thread topic-42"), "got: {block}");
        assert!(
            block.contains("12 prior entries"),
            "history depth must surface, got: {block}"
        );
    }

    #[test]
    fn empty_batch_returns_none() {
        assert!(render_conversation_context(&[], 0).is_none());
    }

    #[test]
    fn batch_of_three_messages_says_coalesces() {
        let rows = vec![
            row(MessageKind::Chat, Some("slack"), Some("C123"), None),
            row(MessageKind::Chat, Some("slack"), Some("C123"), None),
            row(MessageKind::Chat, Some("slack"), Some("C123"), None),
        ];
        let block = render_conversation_context(&rows, 4).expect("expected a block");
        assert!(
            block.contains("coalesces 3 new messages"),
            "got: {block}"
        );
    }

    #[test]
    fn source_session_id_surfaces_parent_relay() {
        let mut r = row(MessageKind::Chat, Some("cli"), Some("p1"), None);
        r.source_session_id = Some("11111111-1111-1111-1111-111111111111".into());
        let block = render_conversation_context(&[r], 0).expect("expected a block");
        assert!(
            block.contains("relayed from parent agent session"),
            "got: {block}"
        );
        assert!(block.contains("11111111-1111-1111-1111-111111111111"), "got: {block}");
    }

    #[test]
    fn agent_kind_only_batch_still_renders_when_no_chat_present() {
        // No Chat row → falls back to first row (the Agent-kind one).
        let mut r = row(MessageKind::Agent, Some("cli"), None, None);
        r.kind = MessageKind::Agent;
        let block = render_conversation_context(&[r], 1).expect("expected a block");
        assert!(block.contains("direct conversation"), "got: {block}");
        assert!(block.contains("1 prior entry"), "got: {block}");
    }

    #[test]
    fn singular_history_entry_uses_singular_phrasing() {
        let rows = vec![row(MessageKind::Chat, Some("cli"), Some("p1"), None)];
        let block = render_conversation_context(&rows, 1).expect("expected a block");
        assert!(block.contains("1 prior entry in session history"), "got: {block}");
    }

    #[test]
    fn system_with_context_appends_when_block_present() {
        let combined = system_with_context(
            "you are helpful",
            Some("Conversation context: this turn is in a direct conversation on cli."),
        );
        assert!(combined.starts_with("you are helpful"));
        assert!(combined.contains("Conversation context:"));
        // Paragraph break between persona and context.
        assert!(combined.contains("\n\n"));
    }

    #[test]
    fn system_with_context_empty_block_returns_original() {
        let combined = system_with_context("you are helpful", None);
        assert_eq!(combined, "you are helpful");
        let combined = system_with_context("you are helpful", Some(""));
        assert_eq!(combined, "you are helpful");
    }

    #[test]
    fn dm_with_reply_to_surfaces_both_signals() {
        // Channel populated `is_group=Some(false)` (a DM) and
        // `reply_to=Some(...)` (the user replied to an earlier message).
        // Both signals must land in the rendered block: DM phrasing
        // wins over the thread_id-derived fallback, and the reply clause
        // appears immediately after the venue/channel run.
        let mut r = row(MessageKind::Chat, Some("telegram"), Some("8929393356"), None);
        r.is_group = Some(false);
        r.reply_to = Some("parent-msg-42".into());
        let block = render_conversation_context(&[r], 3).expect("expected a block");
        assert!(
            block.contains("a 1-on-1 DM"),
            "is_group=Some(false) must render DM phrasing, got: {block}"
        );
        assert!(
            block.contains("in reply to an earlier message"),
            "reply_to must surface a clause, got: {block}"
        );
        assert!(block.contains("on telegram"), "got: {block}");
        // We never render the raw parent message id — the actionable
        // signal is the phrasing, not the wire id.
        assert!(
            !block.contains("parent-msg-42"),
            "raw reply_to id must not leak into the block: {block}"
        );
    }

    #[test]
    fn group_with_reply_to_surfaces_both_signals() {
        // Telegram group thread with a reply: is_group=Some(true) wins
        // over the thread-fallback phrasing, AND we still render the
        // thread id (so the operator can correlate the model's reply
        // back to the right Telegram topic).
        let mut r = row(
            MessageKind::Chat,
            Some("telegram"),
            Some("-100123456789"),
            Some("topic-42"),
        );
        r.is_group = Some(true);
        r.reply_to = Some("parent-msg-99".into());
        let block = render_conversation_context(&[r], 12).expect("expected a block");
        assert!(
            block.contains("a group chat"),
            "is_group=Some(true) must render group phrasing, got: {block}"
        );
        assert!(
            block.contains("in reply to an earlier message"),
            "reply_to must surface a clause, got: {block}"
        );
        // The thread id is still load-bearing for a group with topics —
        // even though `is_group` now drives the venue noun, we don't
        // drop the thread identifier from the block.
        assert!(block.contains("thread topic-42"), "got: {block}");
        // History depth must still surface unchanged by the new clauses.
        assert!(block.contains("12 prior entries"), "got: {block}");
    }

    #[test]
    fn is_group_some_true_overrides_thread_only_fallback() {
        // No thread_id, is_group=Some(true): the channel says "this
        // is a group chat" even though there's no sub-thread, and
        // the block must trust that signal over the
        // thread_id-derived fallback (which would otherwise say
        // "a direct conversation").
        let mut r = row(MessageKind::Chat, Some("signal"), Some("chat-1"), None);
        r.is_group = Some(true);
        let block = render_conversation_context(&[r], 0).expect("expected a block");
        assert!(block.contains("a group chat"), "got: {block}");
        assert!(
            !block.contains("a direct conversation"),
            "is_group must override the thread-fallback phrasing: {block}"
        );
    }

    #[test]
    fn none_signals_preserve_legacy_phrasing() {
        // Both new signals absent: the rendered block must read exactly
        // as it did before the slice-2 follow-up. Locks in
        // backward-compatibility for adapters that don't populate
        // is_group / reply_to (cli, file-watcher, webhook-only).
        let r = row(MessageKind::Chat, Some("cli"), Some("p1"), None);
        let block = render_conversation_context(&[r], 0).expect("expected a block");
        assert!(block.contains("a direct conversation"), "got: {block}");
        assert!(
            !block.contains("in reply to"),
            "reply_to=None must not render a reply clause: {block}"
        );
        assert!(
            !block.contains("group chat") && !block.contains("1-on-1"),
            "is_group=None must not fabricate group/DM phrasing: {block}"
        );
    }

    #[test]
    fn system_with_context_empty_system_returns_just_block() {
        let combined = system_with_context("", Some("Conversation context: x."));
        assert_eq!(combined, "Conversation context: x.");
    }
}
