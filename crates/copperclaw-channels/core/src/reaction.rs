//! Inbound-reaction contract (M19 U7).
//!
//! A user reacting 👍 / ✅ / 👀 / ❌ / 👎 on a message the agent sent is the
//! most natural lightweight steering input — "yes, ship it" without typing.
//! Historically no adapter parsed reaction events at all, so a reaction
//! produced zero agent-visible signal. This module defines the single
//! normalized shape every reaction-capable adapter emits and the runner
//! consumes.
//!
//! ## Wire shape
//!
//! A reaction is surfaced as an ordinary [`copperclaw_types::MessageKind::Chat`]
//! inbound event whose `content` carries a [`REACTION_KEY`] (`"reaction"`)
//! object:
//!
//! ```json
//! {
//!   "text": "[reaction] 👍",
//!   "reaction": {
//!     "emoji": "👍",
//!     "target_seq": "1234",
//!     "actor": "alice"
//!   }
//! }
//! ```
//!
//! - `emoji` — the platform's reaction token verbatim. Some platforms send a
//!   unicode emoji (Telegram, Discord unicode, `WhatsApp` Cloud); Slack sends a
//!   shortcode name (`thumbsup`, `white_check_mark`, …). [`classify`] accepts
//!   both, so adapters never have to normalize.
//! - `target_seq` — the platform-side id (message id / ts / snowflake / wamid)
//!   of the message that was reacted to. Despite the name it is stored as a
//!   string: platform message ids are heterogeneous (Telegram's is a monotonic
//!   integer that reads as a sequence, Slack's is a float-ish `ts`), but they
//!   all compare byte-for-byte against the per-session `delivered` table's
//!   `platform_message_id`, which is how the runner decides a reaction landed
//!   on *its own* last message vs. some unrelated one.
//! - `actor` — a best-effort display name / id of who reacted (informational).
//!
//! ## Trust
//!
//! A reaction is EXTERNAL content authored by a channel user. Like any inbound
//! it must NOT be able to launder trust: the runner marks the turn untrusted
//! (`ToolContext::mark_untrusted_context`) when it folds a reaction in, exactly
//! as a `web_fetch` body or an untrusted memory hit would.
//!
//! ## Gate
//!
//! The presence of [`REACTION_KEY`] whitelists the event past the router's
//! mention gate the same way `content.callback` / `content.button` /
//! `content.command` do (`copperclaw_host_router::mention::is_interaction_payload`).

use serde_json::{Map, Value, json};

/// Content key marking an inbound event as a reaction. Presence of this key
/// whitelists the event past the mention gate and routes it to the runner's
/// reaction-steering seam.
pub const REACTION_KEY: &str = "reaction";

/// Sub-key: the platform reaction token (unicode emoji or Slack shortcode).
pub const REACTION_EMOJI_KEY: &str = "emoji";

/// Sub-key: the platform-side id of the reacted-to message (stored as a
/// string; compared against the `delivered` table's `platform_message_id`).
pub const REACTION_TARGET_KEY: &str = "target_seq";

/// Sub-key: best-effort display name / id of who reacted (informational).
pub(crate) const REACTION_ACTOR_KEY: &str = "actor";

/// A parsed inbound reaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundReaction {
    /// The platform reaction token verbatim.
    pub emoji: String,
    /// Platform-side id of the reacted-to message, if the platform supplied
    /// one (all four reaction-capable adapters do).
    pub target_seq: Option<String>,
    /// Who reacted (informational).
    pub actor: Option<String>,
}

/// The curated steering meaning of a reaction. Reactions outside this set are
/// parsed but carry no steering signal (the runner consumes them silently so
/// they never spawn a spurious turn).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionSignal {
    /// ✅ / 👍 — "yes, go ahead".
    Affirmative,
    /// 👀 — "I'm looking / reviewing".
    Looking,
    /// ❌ / 👎 — "no / hold off".
    Negative,
}

impl ReactionSignal {
    /// A stable, lowercase machine label (metrics, logs).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Affirmative => "affirmative",
            Self::Looking => "looking",
            Self::Negative => "negative",
        }
    }

    /// The one-line interjection the runner folds into the transcript when a
    /// curated reaction lands on the agent's own last message. `emoji` is the
    /// verbatim token the user reacted with, echoed so the model sees exactly
    /// what happened.
    #[must_use]
    pub fn interjection_line(self, emoji: &str) -> String {
        let gloss = match self {
            Self::Affirmative => {
                "read this as affirmative — the user approves; proceed with what you proposed"
            }
            Self::Looking => "the user is watching / reviewing; keep going, no reply needed",
            Self::Negative => "read this as negative — hold off / reconsider; do NOT proceed",
        };
        format!("[the user reacted {emoji} to your last message — {gloss}]")
    }
}

/// Build the `content` value for a reaction inbound event. Adapters call this
/// so every reaction-capable channel emits the identical wire shape.
#[must_use]
pub fn reaction_content(emoji: &str, target_seq: Option<&str>, actor: Option<&str>) -> Value {
    let mut inner = Map::new();
    inner.insert(
        REACTION_EMOJI_KEY.to_owned(),
        Value::String(emoji.to_owned()),
    );
    if let Some(t) = target_seq {
        inner.insert(REACTION_TARGET_KEY.to_owned(), Value::String(t.to_owned()));
    }
    if let Some(a) = actor {
        inner.insert(REACTION_ACTOR_KEY.to_owned(), Value::String(a.to_owned()));
    }
    json!({
        // A human-readable fallback so a generic text renderer never shows raw
        // JSON if a reaction ever escapes the runner's dedicated handling.
        "text": format!("[reaction] {emoji}"),
        REACTION_KEY: Value::Object(inner),
    })
}

/// Parse a reaction out of an inbound event's `content`. Returns `None` when
/// the content carries no [`REACTION_KEY`] object with an `emoji`.
#[must_use]
pub fn parse_reaction(content: &Value) -> Option<InboundReaction> {
    let obj = content.get(REACTION_KEY)?.as_object()?;
    let emoji = obj.get(REACTION_EMOJI_KEY)?.as_str()?.to_owned();
    let target_seq = obj
        .get(REACTION_TARGET_KEY)
        .and_then(Value::as_str)
        .map(str::to_owned);
    let actor = obj
        .get(REACTION_ACTOR_KEY)
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(InboundReaction {
        emoji,
        target_seq,
        actor,
    })
}

/// Whether an inbound event's `content` carries a reaction payload.
#[must_use]
pub fn is_reaction_content(content: &Value) -> bool {
    content
        .get(REACTION_KEY)
        .and_then(Value::as_object)
        .is_some_and(|o| o.contains_key(REACTION_EMOJI_KEY))
}

/// Classify a platform reaction token into its curated steering meaning, or
/// `None` for reactions outside the curated set. Accepts both unicode emoji
/// (Telegram, Discord, `WhatsApp`) and Slack shortcode names.
#[must_use]
pub fn classify(emoji: &str) -> Option<ReactionSignal> {
    // Strip a trailing variation selector (VS16, U+FE0F) so "✅\u{FE0F}" and
    // "✅" both match, and lowercase so Slack shortcodes compare case-blind.
    let norm = emoji
        .trim()
        .trim_end_matches('\u{FE0F}')
        .to_ascii_lowercase();
    match norm.as_str() {
        // Affirmative.
        "\u{1F44D}" | "\u{2705}" | "thumbsup" | "+1" | "white_check_mark" | "heavy_check_mark"
        | "ok_hand" => Some(ReactionSignal::Affirmative),
        // Looking / reviewing.
        "\u{1F440}" | "eyes" => Some(ReactionSignal::Looking),
        // Negative.
        "\u{1F44E}" | "\u{274C}" | "thumbsdown" | "-1" | "x" | "cross_mark" | "no_entry_sign" => {
            Some(ReactionSignal::Negative)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_and_parses_round_trip() {
        let c = reaction_content("\u{1F44D}", Some("1234"), Some("alice"));
        assert!(is_reaction_content(&c));
        let r = parse_reaction(&c).unwrap();
        assert_eq!(r.emoji, "\u{1F44D}");
        assert_eq!(r.target_seq.as_deref(), Some("1234"));
        assert_eq!(r.actor.as_deref(), Some("alice"));
        // Human-readable fallback text is present.
        assert_eq!(c["text"], "[reaction] \u{1F44D}");
    }

    #[test]
    fn parse_none_without_emoji() {
        assert!(parse_reaction(&json!({"reaction": {}})).is_none());
        assert!(parse_reaction(&json!({"text": "hi"})).is_none());
    }

    #[test]
    fn omits_absent_optional_fields() {
        let c = reaction_content("\u{2705}", None, None);
        let inner = c[REACTION_KEY].as_object().unwrap();
        assert!(!inner.contains_key(REACTION_TARGET_KEY));
        assert!(!inner.contains_key(REACTION_ACTOR_KEY));
        let r = parse_reaction(&c).unwrap();
        assert!(r.target_seq.is_none());
        assert!(r.actor.is_none());
    }

    #[test]
    fn classifies_unicode_and_shortcodes() {
        assert_eq!(classify("\u{1F44D}"), Some(ReactionSignal::Affirmative));
        assert_eq!(classify("\u{2705}"), Some(ReactionSignal::Affirmative));
        assert_eq!(classify("thumbsup"), Some(ReactionSignal::Affirmative));
        assert_eq!(
            classify("white_check_mark"),
            Some(ReactionSignal::Affirmative)
        );
        assert_eq!(classify("\u{1F440}"), Some(ReactionSignal::Looking));
        assert_eq!(classify("eyes"), Some(ReactionSignal::Looking));
        assert_eq!(classify("\u{1F44E}"), Some(ReactionSignal::Negative));
        assert_eq!(classify("\u{274C}"), Some(ReactionSignal::Negative));
        assert_eq!(classify("-1"), Some(ReactionSignal::Negative));
    }

    #[test]
    fn variation_selector_and_case_normalized() {
        assert_eq!(
            classify("\u{2705}\u{FE0F}"),
            Some(ReactionSignal::Affirmative)
        );
        assert_eq!(classify("ThumbsUp"), Some(ReactionSignal::Affirmative));
    }

    #[test]
    fn uncurated_reactions_are_none() {
        assert_eq!(classify("\u{1F389}"), None); // 🎉
        assert_eq!(classify("heart"), None);
        assert_eq!(classify(""), None);
    }

    #[test]
    fn interjection_lines_are_one_line_and_echo_emoji() {
        for (sig, emoji) in [
            (ReactionSignal::Affirmative, "\u{1F44D}"),
            (ReactionSignal::Looking, "\u{1F440}"),
            (ReactionSignal::Negative, "\u{1F44E}"),
        ] {
            let line = sig.interjection_line(emoji);
            assert!(!line.contains('\n'), "must be one line: {line}");
            assert!(line.contains(emoji), "must echo the emoji: {line}");
        }
    }
}
