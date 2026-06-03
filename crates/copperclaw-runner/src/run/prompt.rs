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
        block.push_str(&format!("; this turn coalesces {batch_len} new messages"));
    }

    // Sub-agent context: when the runner is a child session spawned
    // by another agent, source_session_id is populated. Surface that so
    // the model knows it's not talking to a human directly.
    if let Some(src) = origin.source_session_id.as_deref() {
        block.push_str(&format!("; relayed from parent agent session {src}"));
    }

    // History depth — capped phrasing so we don't lie when the count
    // is 0 (fresh turn). The number is the count of prior history
    // entries (user/assistant/tool), not message-pairs.
    if history_len == 0 {
        block.push_str("; no prior history in this session");
    } else if history_len == 1 {
        block.push_str("; 1 prior entry in session history");
    } else {
        block.push_str(&format!("; {history_len} prior entries in session history"));
    }

    block.push('.');
    Some(block)
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
        assert!(
            !block.contains(", thread "),
            "DM must not say 'thread': {block}"
        );
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
        assert!(block.contains("coalesces 3 new messages"), "got: {block}");
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
        assert!(
            block.contains("11111111-1111-1111-1111-111111111111"),
            "got: {block}"
        );
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
        assert!(
            block.contains("1 prior entry in session history"),
            "got: {block}"
        );
    }

    #[test]
    fn dm_with_reply_to_surfaces_both_signals() {
        // Channel populated `is_group=Some(false)` (a DM) and
        // `reply_to=Some(...)` (the user replied to an earlier message).
        // Both signals must land in the rendered block: DM phrasing
        // wins over the thread_id-derived fallback, and the reply clause
        // appears immediately after the venue/channel run.
        let mut r = row(
            MessageKind::Chat,
            Some("telegram"),
            Some("8929393356"),
            None,
        );
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

    /// End-to-end guard for the prompt-caching fix: drive TWO successive
    /// inbounds through the REAL `render_conversation_context` and the REAL
    /// Anthropic request-build path, then assert the cached prefix the
    /// provider emits (static system block + tools + the prior-transcript
    /// blocks up to the transcript-tail breakpoint) is BYTE-IDENTICAL
    /// across the two turns — even though the volatile conversation-context
    /// paragraph differs between them (turn 2 has a deeper history, so
    /// `render_conversation_context` produces a different "N prior entries"
    /// clause).
    ///
    /// This replaces the earlier provider-only stability test that fed a
    /// static system string straight in: that gave FALSE confidence because
    /// the volatile per-turn context never flowed through the path under
    /// test. Here the context is produced by the genuine renderer and placed
    /// into `QueryInput::system_context` exactly as `run::run_llm_turn` does,
    /// so a regression that folds it back into the cached system block (the
    /// original v1 bug) would flip this test red.
    #[test]
    fn cached_prefix_stable_across_two_real_inbounds_despite_changing_context() {
        use copperclaw_providers::anthropic::build_request_body_for_model;
        use copperclaw_providers::{HistoryMessage, QueryInput, ToolDef};

        // The static system prompt + tool catalogue the runner holds across
        // the whole session (RunnerDeps::system / ::tools).
        const STATIC_SYSTEM: &str = "you are a large stable persona + skills preamble";
        let tools = vec![
            ToolDef {
                name: "read_file".into(),
                description: "read a file".into(),
                input_schema: serde_json::json!({ "type": "object" }),
            },
            ToolDef {
                name: "shell".into(),
                description: "run a shell command".into(),
                input_schema: serde_json::json!({ "type": "object" }),
            },
        ];

        // Build one QueryInput the way `run::run_llm_turn` does: static
        // system in `system`, the rendered volatile block in
        // `system_context`, on a Claude model so the caching gate fires.
        let make_input = |history: Vec<HistoryMessage>, context: Option<String>| QueryInput {
            system: STATIC_SYSTEM.to_string(),
            system_context: context.filter(|c| !c.is_empty()),
            model: "claude-sonnet-4-6".into(),
            history,
            tools: tools.clone(),
            ..QueryInput::default()
        };

        // Turn 1: one inbound, two prior history entries.
        let rows1 = vec![row(MessageKind::Chat, Some("cli"), Some("p1"), None)];
        let history1 = vec![
            HistoryMessage::User {
                content: "first question".into(),
            },
            HistoryMessage::Assistant {
                content: "first answer".into(),
            },
            HistoryMessage::User {
                content: "second question".into(),
            },
        ];
        let ctx1 = render_conversation_context(&rows1, history1.len());
        let body1 = build_request_body_for_model(&make_input(history1.clone(), ctx1.clone()));

        // Turn 2: the same transcript grown by one exchange, so the rendered
        // context reports a DIFFERENT history depth — the realistic live
        // case where the per-turn context paragraph changes every inbound.
        let mut history2 = history1.clone();
        history2.push(HistoryMessage::Assistant {
            content: "second answer".into(),
        });
        history2.push(HistoryMessage::User {
            content: "third question".into(),
        });
        let rows2 = vec![row(MessageKind::Chat, Some("cli"), Some("p1"), None)];
        let ctx2 = render_conversation_context(&rows2, history2.len());
        let body2 = build_request_body_for_model(&make_input(history2, ctx2.clone()));

        // The renderer really did produce different volatile context across
        // the two turns (different "N prior entries" clause).
        assert_ne!(
            ctx1, ctx2,
            "the rendered conversation-context must differ between the two turns"
        );

        // System block + tools are byte-identical across turns.
        assert_eq!(
            serde_json::to_vec(&body1["system"]).unwrap(),
            serde_json::to_vec(&body2["system"]).unwrap(),
            "static system block (incl. its cache breakpoint) must be byte-stable"
        );
        assert_eq!(
            serde_json::to_vec(&body1["tools"]).unwrap(),
            serde_json::to_vec(&body2["tools"]).unwrap(),
            "tools array (incl. its tail breakpoint) must be byte-stable"
        );
        // The volatile context must NOT have leaked into the cached system
        // block (the v1 bug). The system is a single static text block.
        let sys1 = body1["system"].as_array().expect("system is a block array");
        assert_eq!(sys1.len(), 1);
        assert!(
            !sys1[0]["text"]
                .as_str()
                .unwrap()
                .contains("Conversation context"),
            "volatile context must stay OUT of the cached system block"
        );

        // Turn 1's cached message prefix (everything up to and including its
        // transcript-tail breakpoint, with the volatile context block that
        // follows the breakpoint dropped) must be a byte-stable PREFIX of
        // turn 2's flattened messages — that is exactly the condition under
        // which Anthropic's longest-prefix cache match HITS on turn 2.
        let cached1 = cached_message_prefix(&body1);
        let turn2_blocks = flatten_message_blocks(&body2);
        assert!(!cached1.is_empty(), "the cached prefix must span something");
        assert!(
            turn2_blocks.starts_with(&cached1),
            "turn 1's cached message prefix must be a byte-stable prefix of \
             turn 2's messages (cache HIT); cached1.len()={}, turn2.len()={}",
            cached1.len(),
            turn2_blocks.len(),
        );

        // And the volatile context block sits AFTER the breakpoint (outside
        // the cached span) and differs across the two turns.
        let last1 = body1["messages"].as_array().unwrap().last().unwrap();
        let vol1 = last1["content"].as_array().unwrap().last().unwrap();
        assert!(
            vol1.get("cache_control").is_none(),
            "the volatile context block must not carry a breakpoint"
        );
        assert_eq!(vol1["text"], ctx1.unwrap());
        let last2 = body2["messages"].as_array().unwrap().last().unwrap();
        let vol2 = last2["content"].as_array().unwrap().last().unwrap();
        assert_eq!(vol2["text"], ctx2.unwrap());
        assert_ne!(vol1["text"], vol2["text"]);
    }

    /// Flatten a body's `messages` into the `[role, block]` sequence
    /// Anthropic concatenates for cache-prefix matching, with every
    /// `cache_control` marker stripped so two turns compare on stable bytes.
    fn flatten_message_blocks(body: &serde_json::Value) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for msg in body["messages"].as_array().unwrap() {
            let role = msg["role"].clone();
            for block in msg["content"].as_array().unwrap() {
                let mut b = block.clone();
                if let Some(o) = b.as_object_mut() {
                    o.remove("cache_control");
                }
                out.push(json!([role.clone(), b]));
            }
        }
        out
    }

    /// Turn-1 cached message prefix: the flattened `[role, block]` sequence
    /// up to AND INCLUDING the block carrying the transcript-tail
    /// `cache_control`. Everything after it (the volatile context block) is
    /// dropped — it is outside the cached span.
    fn cached_message_prefix(body: &serde_json::Value) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        'outer: for msg in body["messages"].as_array().unwrap() {
            let role = msg["role"].clone();
            for block in msg["content"].as_array().unwrap() {
                let has_bp = block.get("cache_control").is_some();
                let mut b = block.clone();
                if let Some(o) = b.as_object_mut() {
                    o.remove("cache_control");
                }
                out.push(json!([role.clone(), b]));
                if has_bp {
                    break 'outer;
                }
            }
        }
        out
    }
}
