//! Mention gating for group-chat inbound events.
//!
//! In a busy group chat the agent should not respond to every passing
//! message — only to messages that actually engage it. This module decides,
//! per wiring, whether a given inbound event is allowed past the gate.
//!
//! The policy has two layers:
//!
//! 1. A construction-time [`MentionGate`] default carried on the
//!    [`crate::Router`]. It encodes "require a mention in group chats; never
//!    require one in DMs". This is the host-wide baseline, applied where a
//!    per-group override leaves the decision open.
//! 2. A per-group override read off the wiring's
//!    [`copperclaw_types::EngageMode`] — the existing per-(messaging-group,
//!    agent-group) knob operators already set via `cclaw wirings`. `Mention`
//!    / `MentionSticky` require an explicit mention; `Pattern` engages on a
//!    regex match instead and does not require a mention.
//!
//! Crucially the gate only ever applies to plain conversational text. Three
//! classes of event are NEVER gated, matching the security requirement that
//! non-text interactions reach the agent even in a strict group:
//!
//! - Any [`copperclaw_types::MessageKind`] other than `Chat` (tasks,
//!   webhooks, system, agent-to-agent, cards, …).
//! - Direct messages (`is_group == Some(false)` or `None`): a DM is always a
//!   direct address.
//! - Interaction / command payloads — a button tap (`content.callback`), a
//!   structured button reply (`content.button`), or a slash command
//!   (`content.command`). A user tapping a button the agent rendered is
//!   unambiguously engaging it regardless of mention state.
//!
//! A native mention is the channel-supplied `message.is_mention == Some(true)`.
//! A reply counts only when the adapter resolved that the parent message was
//! authored by the agent itself
//! ([`copperclaw_types::ReplyTo::replying_to_self`] `== Some(true)`); an
//! arbitrary reply to another user never counts.

use copperclaw_types::{EngageMode, InboundEvent, MessageKind};

/// Host-wide default mention policy, carried on the [`crate::Router`] and
/// applied where a per-group override does not settle the decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MentionGate {
    /// Whether group chats require a mention by default. Default `true`.
    pub require_in_groups: bool,
    /// Whether DMs require a mention by default. Default `false` — a DM is
    /// inherently a direct address, so DMs are always processed.
    pub require_in_dms: bool,
}

impl Default for MentionGate {
    fn default() -> Self {
        Self {
            // Secure default: a fresh group chat ignores ambient chatter and
            // only engages on an explicit mention / reply-to-self / pattern.
            require_in_groups: true,
            // A DM is always a direct address — never gate it.
            require_in_dms: false,
        }
    }
}

/// Outcome of evaluating the gate for one wiring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MentionDecision {
    /// The event engages the agent (or the venue does not gate) — route it.
    Process,
    /// The event is ambient group chatter the agent was not addressed in —
    /// drop it for this wiring. The string is a short machine label for the
    /// `dropped_messages` diagnostic row.
    Drop(&'static str),
}

impl MentionGate {
    /// Decide whether `event` engages the agent for a wiring with the given
    /// `engage_mode` / `engage_pattern`, in a venue whose group-ness is
    /// `is_group` (the `messaging_groups.is_group` flag, falling back to the
    /// event's own `message.is_group` when the row does not distinguish).
    #[must_use]
    pub fn decide(
        &self,
        event: &InboundEvent,
        is_group: bool,
        engage_mode: EngageMode,
        engage_pattern: Option<&str>,
    ) -> MentionDecision {
        // 1. Only plain conversational chat is ever gated. Tasks, webhooks,
        //    system messages, agent-to-agent traffic, cards, breadcrumbs, …
        //    all pass straight through.
        if event.message.kind != MessageKind::Chat {
            return MentionDecision::Process;
        }

        // 2. Interaction / command payloads always engage the agent — a user
        //    tapping a button the agent rendered, or issuing a slash command,
        //    is a direct address even with no mention text.
        if is_interaction_payload(event) {
            return MentionDecision::Process;
        }

        // 3. Resolve whether THIS venue gates ambient chat at all. The host
        //    default governs by venue type; a DM is never gated under the
        //    default policy (`require_in_dms = false`).
        if !self.gates_venue(is_group) {
            return MentionDecision::Process;
        }

        // 4. Venue gates — the event must carry an engagement signal.
        // 4a. Native @-mention surfaced by the adapter.
        if event.message.is_mention == Some(true) {
            return MentionDecision::Process;
        }

        // 4b. A reply/quote, but ONLY when the adapter resolved the parent
        //     was the agent's own message. An arbitrary reply to another
        //     user — or one whose parent author could not be resolved — does
        //     NOT count.
        if event
            .reply_to
            .as_ref()
            .and_then(|r| r.replying_to_self)
            .unwrap_or(false)
        {
            return MentionDecision::Process;
        }

        // 4c. Pattern engagement: a `Pattern` wiring engages on a regex match
        //     against the message text. This is the per-group override — a
        //     wiring with `engage_pattern = ".*"` effectively un-gates the
        //     group; a narrow pattern engages only on matching text.
        if matches!(engage_mode, EngageMode::Pattern) {
            if let Some(pat) = engage_pattern {
                if pattern_matches(pat, event) {
                    return MentionDecision::Process;
                }
            }
        }

        MentionDecision::Drop("unmentioned")
    }

    /// Whether a venue of the given group-ness gates ambient chat under the
    /// host-wide default policy.
    fn gates_venue(self, is_group: bool) -> bool {
        if is_group {
            self.require_in_groups
        } else {
            self.require_in_dms
        }
    }
}

/// Whether the event carries an interaction / command payload that should
/// bypass mention gating. Matches the concrete content markers channel
/// adapters set: a Telegram-style `callback` object, a WhatsApp-style
/// `button` reply, or a slash `command`.
fn is_interaction_payload(event: &InboundEvent) -> bool {
    let content = &event.message.content;
    let Some(obj) = content.as_object() else {
        return false;
    };
    obj.contains_key("callback") || obj.contains_key("button") || obj.contains_key("command")
}

/// Best-effort regex match of `pattern` against the event's text content.
/// An invalid pattern never matches (logged at warn) — a broken operator
/// regex must not accidentally engage on every message.
fn pattern_matches(pattern: &str, event: &InboundEvent) -> bool {
    let Some(text) = event
        .message
        .content
        .get("text")
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    match regex::Regex::new(pattern) {
        Ok(re) => re.is_match(text),
        Err(err) => {
            tracing::warn!(%err, pattern, "invalid engage_pattern regex; not matching");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use copperclaw_types::{ChannelType, InboundMessage, ReplyTo};

    fn chat_event(text: &str) -> InboundEvent {
        InboundEvent {
            channel_type: ChannelType::new("cli"),
            platform_id: "c".into(),
            thread_id: None,
            message: InboundMessage {
                id: "m".into(),
                kind: MessageKind::Chat,
                content: serde_json::json!({ "text": text }),
                timestamp: Utc::now(),
                is_mention: None,
                is_group: Some(true),
            },
            reply_to: None,
            sender: None,
        }
    }

    #[test]
    fn default_is_groups_on_dms_off() {
        let g = MentionGate::default();
        assert!(g.require_in_groups);
        assert!(!g.require_in_dms);
    }

    #[test]
    fn unmentioned_group_chat_is_dropped() {
        let g = MentionGate::default();
        let ev = chat_event("hi all");
        assert_eq!(
            g.decide(&ev, true, EngageMode::Mention, None),
            MentionDecision::Drop("unmentioned")
        );
    }

    #[test]
    fn dm_is_never_gated() {
        let g = MentionGate::default();
        let ev = chat_event("hi");
        assert_eq!(
            g.decide(&ev, false, EngageMode::Mention, None),
            MentionDecision::Process
        );
    }

    #[test]
    fn native_mention_processes() {
        let g = MentionGate::default();
        let mut ev = chat_event("hey bot");
        ev.message.is_mention = Some(true);
        assert_eq!(
            g.decide(&ev, true, EngageMode::Mention, None),
            MentionDecision::Process
        );
    }

    #[test]
    fn reply_to_self_processes_but_reply_to_other_and_unknown_do_not() {
        let g = MentionGate::default();
        let mk = |flag: Option<bool>| {
            let mut ev = chat_event("re");
            ev.reply_to = Some(ReplyTo {
                channel_type: ChannelType::new("cli"),
                platform_id: "c".into(),
                thread_id: Some("p".into()),
                replying_to_self: flag,
            });
            ev
        };
        assert_eq!(
            g.decide(&mk(Some(true)), true, EngageMode::Mention, None),
            MentionDecision::Process
        );
        assert_eq!(
            g.decide(&mk(Some(false)), true, EngageMode::Mention, None),
            MentionDecision::Drop("unmentioned")
        );
        assert_eq!(
            g.decide(&mk(None), true, EngageMode::Mention, None),
            MentionDecision::Drop("unmentioned")
        );
    }

    #[test]
    fn pattern_match_processes_and_miss_drops() {
        let g = MentionGate::default();
        let ev = chat_event("please deploy now");
        assert_eq!(
            g.decide(&ev, true, EngageMode::Pattern, Some("(?i)deploy")),
            MentionDecision::Process
        );
        let miss = chat_event("good morning");
        assert_eq!(
            g.decide(&miss, true, EngageMode::Pattern, Some("(?i)deploy")),
            MentionDecision::Drop("unmentioned")
        );
    }

    #[test]
    fn invalid_pattern_never_matches() {
        let g = MentionGate::default();
        let ev = chat_event("anything at all");
        // Unbalanced bracket — invalid regex; must not engage.
        assert_eq!(
            g.decide(&ev, true, EngageMode::Pattern, Some("[unterminated")),
            MentionDecision::Drop("unmentioned")
        );
    }

    #[test]
    fn callback_button_command_payloads_bypass_gate() {
        let g = MentionGate::default();
        for key in ["callback", "button", "command"] {
            let mut ev = chat_event("x");
            ev.message.content = serde_json::json!({ "text": "x", key: {"v": 1} });
            assert_eq!(
                g.decide(&ev, true, EngageMode::Mention, None),
                MentionDecision::Process,
                "{key} payload must bypass the gate"
            );
        }
    }

    #[test]
    fn non_chat_kind_bypasses_gate() {
        let g = MentionGate::default();
        for kind in [
            MessageKind::Task,
            MessageKind::Webhook,
            MessageKind::System,
            MessageKind::Agent,
        ] {
            let mut ev = chat_event("x");
            ev.message.kind = kind;
            assert_eq!(
                g.decide(&ev, true, EngageMode::Mention, None),
                MentionDecision::Process,
                "{kind:?} must bypass the gate"
            );
        }
    }

    #[test]
    fn construction_override_disables_group_gating() {
        let g = MentionGate {
            require_in_groups: false,
            require_in_dms: false,
        };
        let ev = chat_event("ambient");
        assert_eq!(
            g.decide(&ev, true, EngageMode::Mention, None),
            MentionDecision::Process
        );
    }
}
