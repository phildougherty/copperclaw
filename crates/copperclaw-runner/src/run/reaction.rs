//! Inbound-reaction steering (M19 U7).
//!
//! A user reacting 👍 / ✅ / 👀 / ❌ / 👎 on a message the agent sent is a
//! lightweight steering signal, NOT a full turn. The channel adapters parse
//! native reaction events into the shared
//! [`copperclaw_channels_core::reaction`] contract (`content.reaction { emoji,
//! target_seq, actor }`) and the router persists them as non-trigger rows (so
//! an idle reaction never spawns a container). This module turns such a row
//! into the one-line interjection the runner folds into the transcript, reusing
//! the M18 R2 mid-turn steering seam.
//!
//! Two rules keep this honest and minimal:
//!
//! 1. **Own message only.** A reaction steers only when it landed on one of the
//!    agent's OWN delivered messages — resolved by matching the reaction's
//!    `target_seq` against the per-session `delivered` table's
//!    `platform_message_id`. A reaction on some unrelated user's message is
//!    consumed and ignored (never a spurious turn).
//! 2. **Curated set only.** Only ✅/👍 (affirmative), 👀 (looking), ❌/👎
//!    (negative) carry a signal; anything else is consumed silently.
//!
//! Trust: a reaction is EXTERNAL content. When a reaction is folded in, the
//! caller marks the turn untrusted (`ToolContext::mark_untrusted_context`) so a
//! reaction can never launder trust into a credentialed external action.

use std::collections::HashSet;

use copperclaw_channels_core::{ReactionSignal, classify_reaction, parse_reaction};
use copperclaw_types::MessageInRow;

use super::RunnerDeps;

/// Whether an inbound row carries a reaction payload.
pub(super) fn is_reaction_row(row: &MessageInRow) -> bool {
    copperclaw_channels_core::is_reaction_content(&row.content)
}

/// The set of platform-side message ids the agent has delivered on this
/// session, read from the per-session `delivered` table. A reaction whose
/// `target_seq` is in this set landed on the agent's own message.
///
/// Best-effort: a read error yields an empty set (no reaction is treated as
/// "own"), which fails safe — an unresolvable reaction is ignored rather than
/// mistakenly steering the turn.
pub(super) async fn own_delivered_ids(deps: &RunnerDeps) -> HashSet<String> {
    let guard = deps.inbound.lock().await;
    match copperclaw_db::tables::delivered::list(&guard) {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|d| d.platform_message_id)
            .collect(),
        Err(err) => {
            tracing::warn!(
                ?err,
                "U7: reading delivered table for reaction steering failed"
            );
            HashSet::new()
        }
    }
}

/// Classify a reaction row against the agent's own delivered messages.
///
/// Returns `Some((signal, emoji))` when the row is a curated reaction on the
/// agent's own last message (steer it); `None` when the row is not a reaction,
/// targets a message the agent didn't send, or uses an uncurated emoji (in all
/// of which cases the caller still consumes the row so it never re-surfaces).
pub(super) fn steer_for_row(
    row: &MessageInRow,
    own_ids: &HashSet<String>,
) -> Option<(ReactionSignal, String)> {
    let reaction = parse_reaction(&row.content)?;
    // Own-message gate: the reaction must target a message the agent delivered.
    let target = reaction.target_seq.as_deref()?;
    if !own_ids.contains(target) {
        return None;
    }
    let signal = classify_reaction(&reaction.emoji)?;
    Some((signal, reaction.emoji))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use copperclaw_channels_core::reaction_content;
    use copperclaw_types::{MessageId, MessageKind};

    fn reaction_row(emoji: &str, target: Option<&str>) -> MessageInRow {
        MessageInRow {
            id: MessageId::new(),
            seq: 1,
            kind: MessageKind::Chat,
            timestamp: Utc::now(),
            status: "pending".into(),
            process_after: None,
            recurrence: None,
            series_id: None,
            tries: 0,
            trigger: false,
            platform_id: Some("chat-1".into()),
            channel_type: None,
            thread_id: None,
            content: reaction_content(emoji, target, Some("alice")),
            source_session_id: None,
            on_wake: false,
            reply_to: None,
            is_group: None,
        }
    }

    fn own(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn curated_on_own_message_steers() {
        let row = reaction_row("\u{1F44D}", Some("out-7"));
        let (sig, emoji) = steer_for_row(&row, &own(&["out-7"])).unwrap();
        assert_eq!(sig, ReactionSignal::Affirmative);
        assert_eq!(emoji, "\u{1F44D}");
    }

    #[test]
    fn reaction_on_unrelated_message_does_not_steer() {
        let row = reaction_row("\u{1F44D}", Some("some-other-msg"));
        assert!(steer_for_row(&row, &own(&["out-7"])).is_none());
    }

    #[test]
    fn uncurated_emoji_on_own_message_does_not_steer() {
        let row = reaction_row("\u{1F389}", Some("out-7")); // 🎉
        assert!(steer_for_row(&row, &own(&["out-7"])).is_none());
    }

    #[test]
    fn non_reaction_row_does_not_steer() {
        let mut row = reaction_row("\u{1F44D}", Some("out-7"));
        row.content = serde_json::json!({"text": "just chat"});
        assert!(steer_for_row(&row, &own(&["out-7"])).is_none());
    }
}
