//! Format `messages_in` rows into the text body of one provider turn,
//! and shrink the replayed transcript before it goes back to the model.
//!
//! When multiple pending rows are picked up in a single poll we coalesce
//! them into a single user-side message. The format is human-readable and
//! deterministic so the model can rely on it.
//!
//! [`elide_stale_tool_results`] is the per-turn transcript shrinker: old,
//! already-acted-on tool outputs (file reads, command stdout, large diffs)
//! are otherwise re-sent verbatim every single turn, forever. Once a
//! result is outside the recent-results window it is replaced with a
//! one-line stub (`[elided: …]`) while its tool-use/tool-result pairing
//! stays byte-for-byte intact, so strict providers (Anthropic, minimax)
//! still accept the transcript.

use copperclaw_providers::HistoryMessage;
use copperclaw_types::MessageInRow;

/// Default number of most-recent tool results kept full (never elided).
/// The current turn's results are always inside this window, so the model
/// always sees fresh tool output verbatim. Six covers a typical
/// read → edit → run → re-read working set while still capping the tail.
pub const DEFAULT_RECENT_TOOL_RESULTS: usize = 6;
/// Default byte cap above which a *stale* tool-result body is elided to a
/// stub. ~2 KB keeps short results (exit codes, "ok", small JSON) intact —
/// stubbing those saves nothing — while collapsing the file reads, command
/// stdout, and large diffs that dominate a long transcript.
pub const DEFAULT_TOOL_RESULT_ELIDE_BYTES: usize = 2_048;

/// Knobs for [`elide_stale_tool_results`]. Build from `RunnerConfig`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElisionCfg {
    /// Keep this many of the most-recent tool results full. Everything
    /// older is *eligible* for elision. `0` makes every tool result
    /// eligible (the current turn included), which is rarely what you
    /// want; the runner defaults to [`DEFAULT_RECENT_TOOL_RESULTS`].
    pub recent_results_kept: usize,
    /// Elide an eligible (stale) tool-result body only when it exceeds
    /// this many bytes. `0` elides every eligible result regardless of
    /// size. Defaults to [`DEFAULT_TOOL_RESULT_ELIDE_BYTES`].
    pub max_result_bytes: usize,
}

impl Default for ElisionCfg {
    fn default() -> Self {
        Self {
            recent_results_kept: DEFAULT_RECENT_TOOL_RESULTS,
            max_result_bytes: DEFAULT_TOOL_RESULT_ELIDE_BYTES,
        }
    }
}

/// Output of [`format_messages`] — the user-side prompt plus the picked
/// rows in stable order. The caller persists `rows` (e.g. to ack each one).
#[derive(Debug, Clone)]
pub struct FormattedTurn {
    /// User-facing prompt text. Pass into [`copperclaw_providers::HistoryMessage::User`].
    pub prompt: String,
    /// Source rows in chronological (seq-ascending) order.
    pub rows: Vec<MessageInRow>,
}

/// Format `messages` into a single user-side prompt.
///
/// The output groups every message under a `[channel/platform/thread]`
/// header (or `[system]` for synthetic system messages) followed by the
/// message body. The function consumes the input and returns it back via
/// `FormattedTurn::rows` in seq-ascending order so the caller can drive
/// processing in the right sequence.
#[must_use]
pub fn format_messages(mut messages: Vec<MessageInRow>) -> FormattedTurn {
    messages.sort_by_key(|m| m.seq);
    let mut prompt = String::new();
    for (i, m) in messages.iter().enumerate() {
        if i > 0 {
            prompt.push_str("\n\n");
        }
        prompt.push('[');
        prompt.push_str(m.kind.as_str());
        if let Some(ct) = &m.channel_type {
            prompt.push_str(" channel=");
            prompt.push_str(ct.as_str());
        }
        if let Some(pid) = &m.platform_id {
            prompt.push_str(" platform=");
            prompt.push_str(pid);
        }
        if let Some(tid) = &m.thread_id {
            prompt.push_str(" thread=");
            prompt.push_str(tid);
        }
        prompt.push_str(" seq=");
        prompt.push_str(&m.seq.to_string());
        prompt.push(']');
        prompt.push('\n');
        prompt.push_str(&body_text(&m.content));
    }
    FormattedTurn {
        prompt,
        rows: messages,
    }
}

/// Pull inlined inbound images out of the picked rows, in seq order, as
/// `(media_type, base64_data)`. The channel ingress sets
/// `content.attachment.data_base64` for image attachments; each becomes a
/// `HistoryMessage::Image` so vision-capable models see photos the user
/// sent. Rows are already seq-sorted by [`format_messages`]'s caller.
#[must_use]
pub fn extract_inbound_images(rows: &[MessageInRow]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for m in rows {
        let Some(att) = m.content.get("attachment") else {
            continue;
        };
        let Some(data) = att.get("data_base64").and_then(|v| v.as_str()) else {
            continue;
        };
        let media_type = att
            .get("mime_type")
            .and_then(|v| v.as_str())
            .unwrap_or("image/jpeg")
            .to_string();
        out.push((media_type, data.to_string()));
    }
    out
}

/// Shrink the replayed transcript by stubbing out stale, oversized
/// tool-result bodies.
///
/// The model has already read and acted on old tool output; re-sending a
/// 4 KB file read or a screenful of command stdout on every subsequent
/// turn is pure waste. This walks `history`, finds the [`HistoryMessage::Tool`]
/// results, keeps the most-recent `cfg.recent_results_kept` of them full
/// (so the current turn and recent context are always intact), and
/// replaces the body of any older result that exceeds
/// `cfg.max_result_bytes` with a compact stub such as:
///
/// ```text
/// [elided: stale tool result, 4.2 KB, 142 lines — re-run the tool if you need it again]
/// ```
///
/// Crucially, only the `content` *string* of a `Tool` entry is rewritten.
/// No entry is added, removed, or reordered, and `tool_use_id` is never
/// touched — so the `tool_use` ↔ `tool_result` pairing that strict
/// providers require is preserved exactly. Already-errored results
/// (`is_error == true`) are left full: they're usually short and the
/// model may still be reasoning about the failure.
///
/// Returns the input untouched when `recent_results_kept` is large enough
/// to cover every result, when no eligible result exceeds the cap, or
/// when the history has no tool results at all — so the common short
/// transcript pays only one cheap pass.
#[must_use]
pub fn elide_stale_tool_results(
    history: Vec<HistoryMessage>,
    cfg: &ElisionCfg,
) -> Vec<HistoryMessage> {
    // Index every tool result so we can tell "recent" from "stale" by
    // position among results (a stable, provider-agnostic notion of
    // "turns old" — each result corresponds to one executed tool call).
    let result_indices: Vec<usize> = history
        .iter()
        .enumerate()
        .filter_map(|(i, m)| matches!(m, HistoryMessage::Tool { .. }).then_some(i))
        .collect();

    // Nothing to do if everything is within the protected recent window.
    if result_indices.len() <= cfg.recent_results_kept {
        return history;
    }
    // History positions `< stale_cutoff_idx` are "stale" and eligible;
    // positions `>=` it are recent and kept full. `recent_results_kept == 0`
    // protects nothing, so the cutoff is the end of the history (every
    // result eligible). Otherwise it's the position of the
    // `recent_results_kept`-th-from-last result.
    let keep_from = result_indices.len() - cfg.recent_results_kept;
    let stale_cutoff_idx = result_indices
        .get(keep_from)
        .copied()
        .unwrap_or(history.len());

    let mut out = history;
    for (i, m) in out.iter_mut().enumerate() {
        if i >= stale_cutoff_idx {
            // Recent result (or anything after the cutoff) — leave full.
            break;
        }
        if let HistoryMessage::Tool {
            content, is_error, ..
        } = m
        {
            if *is_error {
                continue; // keep failures legible
            }
            if already_elided(content) {
                continue; // idempotent across turns
            }
            if content.len() > cfg.max_result_bytes {
                *content = elision_stub(content);
            }
        }
    }
    out
}

/// Marker prefix the stub carries so a second pass over an
/// already-elided transcript is a no-op (the runner re-elides every turn).
const ELISION_MARKER: &str = "[elided: stale tool result";

fn already_elided(content: &str) -> bool {
    content.starts_with(ELISION_MARKER)
}

/// Build the one-line replacement body for a stale tool result, recording
/// its original size so the model knows what it's missing and can re-run
/// the tool if it genuinely needs the output again.
fn elision_stub(original: &str) -> String {
    let bytes = original.len();
    let lines = original.lines().count().max(1);
    format!(
        "{ELISION_MARKER}, {bytes} bytes, {lines} lines — re-run the tool if you need this output again]"
    )
}

/// Best-effort extraction of a text body from a stored JSON content blob.
///
/// Recognised shapes (in order):
/// 1. `{ "text": "..." }` — chat/system messages.
/// 2. `{ "prompt": "..." }` — scheduled task fires.
/// 3. Anything else — the JSON is rendered as a compact string.
fn body_text(content: &serde_json::Value) -> String {
    if let Some(s) = content.get("text").and_then(|v| v.as_str()) {
        if !s.is_empty() {
            return s.to_string();
        }
        // Empty caption but an inlined image rides as a separate content
        // block — note it so the user turn isn't a blank line.
        if content
            .get("attachment")
            .and_then(|a| a.get("data_base64"))
            .is_some()
        {
            return "[sent an image]".to_string();
        }
        return s.to_string();
    }
    if let Some(s) = content.get("prompt").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    content.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use copperclaw_types::{ChannelType, MessageId, MessageInRow, MessageKind};
    use serde_json::json;

    fn row(seq: i64, content: serde_json::Value) -> MessageInRow {
        MessageInRow {
            id: MessageId::new(),
            seq,
            kind: MessageKind::Chat,
            timestamp: Utc::now(),
            status: "pending".into(),
            process_after: None,
            recurrence: None,
            series_id: None,
            tries: 0,
            trigger: true,
            platform_id: Some("chat-1".into()),
            channel_type: Some(ChannelType::new("cli")),
            thread_id: None,
            content,
            source_session_id: None,
            on_wake: false,
            reply_to: None,
            is_group: None,
        }
    }

    #[test]
    fn extract_inbound_images_pulls_inlined_attachments() {
        let rows = vec![
            row(1, json!({"text": "no attachment"})),
            row(
                2,
                json!({
                    "text": "look at this",
                    "attachment": {
                        "kind": "telegram.photo",
                        "mime_type": "image/jpeg",
                        "data_base64": "QUJD"
                    }
                }),
            ),
            // Attachment present but no inlined bytes (e.g. a document) → skipped.
            row(
                3,
                json!({"attachment": {"kind": "telegram.document", "path": "/x"}}),
            ),
        ];
        let imgs = extract_inbound_images(&rows);
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].0, "image/jpeg");
        assert_eq!(imgs[0].1, "QUJD");
    }

    #[test]
    fn extract_inbound_images_defaults_mime_when_absent() {
        let rows = vec![row(1, json!({"attachment": {"data_base64": "QQ=="}}))];
        let imgs = extract_inbound_images(&rows);
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].0, "image/jpeg");
    }

    #[test]
    fn single_message_renders_header_and_text() {
        let ft = format_messages(vec![row(2, json!({"text": "hi there"}))]);
        assert!(ft.prompt.contains("[chat"));
        assert!(ft.prompt.contains("channel=cli"));
        assert!(ft.prompt.contains("platform=chat-1"));
        assert!(ft.prompt.contains("seq=2"));
        assert!(ft.prompt.contains("hi there"));
        assert_eq!(ft.rows.len(), 1);
    }

    #[test]
    fn multiple_messages_separated_by_blank_line_and_seq_ordered() {
        let ft = format_messages(vec![
            row(4, json!({"text": "later"})),
            row(2, json!({"text": "earlier"})),
        ]);
        let pos_earlier = ft.prompt.find("earlier").unwrap();
        let pos_later = ft.prompt.find("later").unwrap();
        assert!(pos_earlier < pos_later);
        assert!(ft.prompt.contains("\n\n["));
        assert_eq!(ft.rows[0].seq, 2);
        assert_eq!(ft.rows[1].seq, 4);
    }

    #[test]
    fn falls_back_to_prompt_field_for_tasks() {
        let ft = format_messages(vec![row(2, json!({"prompt": "wake task"}))]);
        assert!(ft.prompt.contains("wake task"));
    }

    #[test]
    fn falls_back_to_compact_json_when_unknown_shape() {
        let ft = format_messages(vec![row(2, json!({"event": "x"}))]);
        // JSON-rendered to_string is what we expect.
        assert!(ft.prompt.contains("{\"event\":\"x\"}"));
    }

    #[test]
    fn empty_input_renders_empty_prompt() {
        let ft = format_messages(vec![]);
        assert!(ft.prompt.is_empty());
        assert!(ft.rows.is_empty());
    }

    #[test]
    fn missing_channel_or_thread_omitted_from_header() {
        let mut m = row(2, json!({"text": "x"}));
        m.channel_type = None;
        m.platform_id = None;
        m.thread_id = None;
        let ft = format_messages(vec![m]);
        assert!(!ft.prompt.contains("channel="));
        assert!(!ft.prompt.contains("platform="));
        assert!(!ft.prompt.contains("thread="));
        assert!(ft.prompt.contains("seq=2"));
    }

    #[test]
    fn thread_id_appears_when_set() {
        let mut m = row(2, json!({"text": "x"}));
        m.thread_id = Some("t-99".into());
        let ft = format_messages(vec![m]);
        assert!(ft.prompt.contains("thread=t-99"));
    }

    #[test]
    fn system_kind_header_renders_as_system() {
        let mut m = row(2, json!({"text": "ack"}));
        m.kind = MessageKind::System;
        let ft = format_messages(vec![m]);
        assert!(ft.prompt.starts_with("[system"));
    }

    // ---- tool-result elision ----

    fn tu(id: &str) -> HistoryMessage {
        HistoryMessage::ToolUse {
            id: id.into(),
            name: "read_file".into(),
            input: json!({"path": "src/x.rs"}),
        }
    }
    fn tr(id: &str, body: &str) -> HistoryMessage {
        HistoryMessage::Tool {
            tool_use_id: id.into(),
            content: body.into(),
            is_error: false,
        }
    }
    fn big(n: usize) -> String {
        "x".repeat(n)
    }
    fn is_stub(m: &HistoryMessage) -> bool {
        matches!(m, HistoryMessage::Tool { content, .. } if content.starts_with("[elided: stale tool result"))
    }

    #[test]
    fn stale_large_result_is_elided_recent_stays_full() {
        // Two tool turns. Keep the most-recent 1 result full; the older
        // (large) one must be stubbed.
        let cfg = ElisionCfg {
            recent_results_kept: 1,
            max_result_bytes: 1_024,
        };
        let history = vec![
            HistoryMessage::User {
                content: "do it".into(),
            },
            tu("a"),
            tr("a", &big(4_096)), // stale + large → elide
            HistoryMessage::Assistant {
                content: "read it".into(),
            },
            tu("b"),
            tr("b", &big(4_096)), // most-recent result → keep full
        ];
        let out = elide_stale_tool_results(history, &cfg);
        // Structure preserved: same length, same kinds, ids untouched.
        assert_eq!(out.len(), 6);
        assert!(is_stub(&out[2]), "stale large result should be a stub");
        // Stub mentions the original size so the model knows what's gone.
        if let HistoryMessage::Tool { content, .. } = &out[2] {
            assert!(content.contains("4096 bytes"));
            assert!(content.contains("re-run the tool"));
        } else {
            panic!("expected Tool at 2");
        }
        // Recent (current-turn) result is untouched.
        assert!(!is_stub(&out[5]));
        if let HistoryMessage::Tool { content, .. } = &out[5] {
            assert_eq!(content.len(), 4_096);
        }
        // Pairing intact: every tool_use_id still has a matching result.
        assert_pairing_valid(&out);
    }

    #[test]
    fn small_stale_result_is_not_elided() {
        // Stale, but under the byte cap — stubbing it would save nothing.
        let cfg = ElisionCfg {
            recent_results_kept: 1,
            max_result_bytes: 1_024,
        };
        let history = vec![
            tu("a"),
            tr("a", "exit code 0"), // stale + small → keep
            tu("b"),
            tr("b", &big(4_096)),
        ];
        let out = elide_stale_tool_results(history, &cfg);
        assert!(!is_stub(&out[1]), "small stale result must stay full");
        if let HistoryMessage::Tool { content, .. } = &out[1] {
            assert_eq!(content, "exit code 0");
        }
    }

    #[test]
    fn errored_results_are_kept_full() {
        let cfg = ElisionCfg {
            recent_results_kept: 0,
            max_result_bytes: 16,
        };
        let history = vec![
            tu("a"),
            HistoryMessage::Tool {
                tool_use_id: "a".into(),
                content: big(4_096),
                is_error: true, // failures stay legible
            },
            tu("b"),
            tr("b", &big(4_096)),
        ];
        let out = elide_stale_tool_results(history, &cfg);
        assert!(!is_stub(&out[1]), "errored result must stay full");
    }

    #[test]
    fn nothing_elided_when_all_within_recent_window() {
        // recent_results_kept covers every result → input returned as-is.
        let cfg = ElisionCfg {
            recent_results_kept: 8,
            max_result_bytes: 0,
        };
        let history = vec![tu("a"), tr("a", &big(4_096)), tu("b"), tr("b", &big(4_096))];
        let out = elide_stale_tool_results(history.clone(), &cfg);
        assert_eq!(out, history);
    }

    #[test]
    fn no_tool_results_is_a_noop() {
        let cfg = ElisionCfg::default();
        let history = vec![
            HistoryMessage::User {
                content: big(10_000),
            },
            HistoryMessage::Assistant {
                content: big(10_000),
            },
        ];
        let out = elide_stale_tool_results(history.clone(), &cfg);
        assert_eq!(out, history, "non-Tool entries are never touched");
    }

    #[test]
    fn elision_is_idempotent_across_turns() {
        // The runner re-runs elision every turn; a second pass over an
        // already-stubbed transcript must not double-stub or change sizes.
        let cfg = ElisionCfg {
            recent_results_kept: 1,
            max_result_bytes: 1_024,
        };
        let history = vec![tu("a"), tr("a", &big(4_096)), tu("b"), tr("b", &big(4_096))];
        let once = elide_stale_tool_results(history, &cfg);
        let twice = elide_stale_tool_results(once.clone(), &cfg);
        assert_eq!(once, twice);
    }

    #[test]
    fn parallel_batch_stale_results_elide_without_breaking_pairs() {
        // A parallel tool batch: tu a, tu b, tr a, tr b, then a newer
        // batch. The older batch's large results elide; pairing holds.
        let cfg = ElisionCfg {
            recent_results_kept: 2,
            max_result_bytes: 1_024,
        };
        let history = vec![
            tu("a"),
            tu("b"),
            tr("a", &big(4_096)),
            tr("b", &big(4_096)),
            tu("c"),
            tu("d"),
            tr("c", &big(4_096)),
            tr("d", &big(4_096)),
        ];
        let out = elide_stale_tool_results(history, &cfg);
        // Oldest two results (a, b) elide; newest two (c, d) stay full.
        assert!(is_stub(&out[2]));
        assert!(is_stub(&out[3]));
        assert!(!is_stub(&out[6]));
        assert!(!is_stub(&out[7]));
        assert_pairing_valid(&out);
    }

    #[test]
    fn zero_byte_cap_elides_every_stale_result() {
        // max_result_bytes == 0 means "elide all eligible regardless of
        // size", protecting only the recent window.
        let cfg = ElisionCfg {
            recent_results_kept: 1,
            max_result_bytes: 0,
        };
        let history = vec![tu("a"), tr("a", "tiny"), tu("b"), tr("b", "also tiny")];
        let out = elide_stale_tool_results(history, &cfg);
        assert!(is_stub(&out[1]), "even a tiny stale result elides at cap 0");
        assert!(!is_stub(&out[3]), "the recent result is protected");
    }

    /// Every `tool_use_id` on a `Tool` result must correspond to a
    /// `ToolUse` id that appears earlier — the invariant strict providers
    /// enforce.
    /// Elision must never disturb this.
    fn assert_pairing_valid(history: &[HistoryMessage]) {
        use std::collections::HashSet;
        let mut seen_uses: HashSet<&str> = HashSet::new();
        for m in history {
            match m {
                HistoryMessage::ToolUse { id, .. } => {
                    seen_uses.insert(id.as_str());
                }
                HistoryMessage::Tool { tool_use_id, .. } => {
                    assert!(
                        seen_uses.contains(tool_use_id.as_str()),
                        "tool_result {tool_use_id} has no preceding tool_use"
                    );
                }
                _ => {}
            }
        }
    }

    #[test]
    fn elision_cfg_default_matches_documented_constants() {
        let d = ElisionCfg::default();
        assert_eq!(d.recent_results_kept, DEFAULT_RECENT_TOOL_RESULTS);
        assert_eq!(d.max_result_bytes, DEFAULT_TOOL_RESULT_ELIDE_BYTES);
    }
}
