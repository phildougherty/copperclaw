//! End-user slash commands, parsed router-side (M18 R1).
//!
//! Four commands are recognised when a plain `Chat` message's text is
//! EXACTLY one slash token (case-insensitive, whitespace-trimmed, an
//! optional Telegram-style `@BotName` suffix stripped):
//!
//! - `/stop` (alias `/cancel`) — persisted as a **control row** in the
//!   session's `messages_in` (see [control-row contract](#control-row-contract)
//!   below). The row lands even while a runner turn is in flight; the
//!   M18 R2 card consumes it mid-turn to stop the turn cleanly.
//! - `/status` — answered by the HOST from central-DB state. No inbound
//!   row is written and the runner is never woken; the reply is written
//!   straight into the session's `messages_out` for the delivery loop.
//! - `/compact` / `/clear` (aliases `/reset`, `/new`) — passed through
//!   as normal chat rows so the runner's existing slash-command
//!   sentinels (`copperclaw-runner/src/run/mod.rs`) handle them. The
//!   router's contribution is (a) bypassing the group-chat mention gate
//!   and (b) normalising the text (case, `@bot` suffix, aliases) to the
//!   canonical form the runner sentinel matches.
//!
//! Anything else that starts with `/` — unknown commands, or a known
//! command followed by more text ("/clear and then...") — falls through
//! to the agent unchanged, byte-identical to today's routing. People
//! type `/s` in prose; the model can handle it.
//!
//! Recognised commands bypass the mention gate: a slash command in a
//! mention-gated group chat is a direct address, exactly like the
//! `content.command` interaction payloads [`crate::mention`] already
//! whitelists. The router treats a detected command as equivalent to an
//! adapter-stamped command payload.
//!
//! # Control-row contract
//!
//! The `/stop` control row (consumed by M18 R2) is shaped as:
//!
//! ```json
//! {
//!   "kind": "system",
//!   "trigger": false,
//!   "on_wake": false,
//!   "content": {
//!     "control": { "op": "stop" },
//!     "command": "stop",
//!     "text": "/stop"
//!   }
//! }
//! ```
//!
//! - `kind = "system"`: not user chat; the runner's formatter renders
//!   it as a system line if a pre-R2 runner ever batches it.
//! - `trigger = false`: the row must NOT spawn a container
//!   (`messages_in::count_due` filters on `trigger = 1`, so the
//!   container manager's spawn classifier ignores it). Stopping an idle
//!   session is a no-op and must stay one.
//! - `content.control.op`: the machine-readable operation. R2 detects
//!   control rows by the presence of the `control` object.
//! - `content.command` mirrors the interaction-payload marker
//!   convention from [`crate::mention::MentionGate`].
//! - `content.text` carries the canonical command so a pre-R2 runner
//!   that batches the row surfaces something legible to the model.
//! - `status` stays `pending` until the consumer (R2) marks it
//!   completed.

use copperclaw_types::{InboundEvent, MessageKind};

/// A recognised end-user slash command. See the module docs for the
/// per-command routing behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    /// `/stop` or `/cancel` — persist a `control{op:stop}` row.
    Stop,
    /// `/status` — host-side synthesized reply; runner untouched.
    Status,
    /// `/compact` — pass through to the runner's compaction sentinel.
    Compact,
    /// `/clear`, `/reset`, or `/new` — pass through to the runner's
    /// clear-history sentinel.
    Clear,
}

impl SlashCommand {
    /// Parse `text` as a pure slash command. Returns `None` unless the
    /// ENTIRE trimmed text is a single recognised slash token — a
    /// command with trailing words is prose for the model, not a
    /// command (mirrors the runner's `SlashCommand::parse` contract).
    ///
    /// A Telegram-style `@BotName` suffix on the token (`/stop@MyBot`)
    /// is stripped before matching, since group-chat clients append it
    /// when the user picks a command from the bot menu.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let trimmed = text.trim();
        if !trimmed.starts_with('/') {
            return None;
        }
        // Exactly one whitespace-delimited token.
        let mut tokens = trimmed.split_whitespace();
        let token = tokens.next()?;
        if tokens.next().is_some() {
            return None;
        }
        // Strip an optional @BotName suffix: "/stop@MyBot" -> "/stop".
        let bare = token.split('@').next().unwrap_or(token);
        match bare.to_ascii_lowercase().as_str() {
            "/stop" | "/cancel" => Some(Self::Stop),
            "/status" => Some(Self::Status),
            "/compact" => Some(Self::Compact),
            "/clear" | "/reset" | "/new" => Some(Self::Clear),
            _ => None,
        }
    }

    /// Detect a slash command on an inbound event. Only plain `Chat`
    /// messages qualify — a webhook or task whose payload text happens
    /// to start with `/` is never a command.
    #[must_use]
    pub fn detect(event: &InboundEvent) -> Option<Self> {
        if event.message.kind != MessageKind::Chat {
            return None;
        }
        let text = event
            .message
            .content
            .get("text")
            .and_then(serde_json::Value::as_str)?;
        Self::parse(text)
    }

    /// Canonical text form — what the runner's sentinel matches
    /// (`/clear`, `/compact`) and what the control row's `text` carries.
    #[must_use]
    pub fn canonical(self) -> &'static str {
        match self {
            Self::Stop => "/stop",
            Self::Status => "/status",
            Self::Compact => "/compact",
            Self::Clear => "/clear",
        }
    }

    /// Machine label for `content.command` / `content.control.op`.
    #[must_use]
    pub fn op(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Status => "status",
            Self::Compact => "compact",
            Self::Clear => "clear",
        }
    }
}

/// Build the `messages_in.content` for a `/stop` control row. See the
/// module docs for the full contract.
#[must_use]
pub fn control_content(cmd: SlashCommand, original_text: &str) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "control": { "op": cmd.op() },
        "command": cmd.op(),
        "text": cmd.canonical(),
    });
    preserve_original(&mut obj, cmd, original_text);
    obj
}

/// Build the `messages_in.content` for a `/compact` / `/clear`
/// passthrough row: the original content object with `text` normalised
/// to the canonical command (so the runner sentinel matches even for
/// `/CLEAR`, `/reset`, or `/compact@MyBot`) and a `command` marker
/// mirroring the interaction-payload convention the mention gate
/// whitelists.
#[must_use]
pub fn passthrough_content(
    cmd: SlashCommand,
    original: &serde_json::Value,
    original_text: &str,
) -> serde_json::Value {
    let mut obj = match original {
        serde_json::Value::Object(map) => serde_json::Value::Object(map.clone()),
        other => serde_json::json!({ "raw": other }),
    };
    if let Some(map) = obj.as_object_mut() {
        map.insert(
            "text".into(),
            serde_json::Value::String(cmd.canonical().to_owned()),
        );
        map.insert(
            "command".into(),
            serde_json::Value::String(cmd.op().to_owned()),
        );
    }
    preserve_original(&mut obj, cmd, original_text);
    obj
}

/// When the user's trimmed text differs from the canonical command
/// (alias, `@bot` suffix, odd casing), keep what they actually typed
/// under `original_text` so nothing is silently lost from the record.
fn preserve_original(obj: &mut serde_json::Value, cmd: SlashCommand, original_text: &str) {
    let trimmed = original_text.trim();
    if trimmed != cmd.canonical() {
        if let Some(map) = obj.as_object_mut() {
            map.insert(
                "original_text".into(),
                serde_json::Value::String(trimmed.to_owned()),
            );
        }
    }
}

/// Render the `/status` reply text from host-side state. Everything in
/// here comes from the central DB plus a `count_due` against the
/// session's `inbound.db` — the runner is never involved.
#[must_use]
pub fn render_status(
    agent_group_name: &str,
    session: &copperclaw_types::Session,
    queued_messages: i64,
) -> String {
    format!(
        "Agent status\n\
         - agent group: {name}\n\
         - session: {sid}\n\
         - session state: {state} (container {container})\n\
         - queued messages: {queued}\n\
         - last active: {last_active}\n\
         - created: {created}",
        name = agent_group_name,
        sid = session.id.as_uuid(),
        state = session.status.as_str(),
        container = session.container_status.as_str(),
        queued = queued_messages,
        last_active = session.last_active.to_rfc3339(),
        created = session.created_at.to_rfc3339(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use copperclaw_types::{ChannelType, InboundMessage};

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
                is_group: None,
            },
            reply_to: None,
            sender: None,
        }
    }

    #[test]
    fn parse_recognises_all_commands_and_aliases() {
        assert_eq!(SlashCommand::parse("/stop"), Some(SlashCommand::Stop));
        assert_eq!(SlashCommand::parse("/cancel"), Some(SlashCommand::Stop));
        assert_eq!(SlashCommand::parse("/status"), Some(SlashCommand::Status));
        assert_eq!(SlashCommand::parse("/compact"), Some(SlashCommand::Compact));
        assert_eq!(SlashCommand::parse("/clear"), Some(SlashCommand::Clear));
        assert_eq!(SlashCommand::parse("/reset"), Some(SlashCommand::Clear));
        assert_eq!(SlashCommand::parse("/new"), Some(SlashCommand::Clear));
    }

    #[test]
    fn parse_is_case_insensitive_and_trims() {
        assert_eq!(SlashCommand::parse("  /STOP  "), Some(SlashCommand::Stop));
        assert_eq!(SlashCommand::parse("/Status"), Some(SlashCommand::Status));
    }

    #[test]
    fn parse_strips_telegram_bot_suffix() {
        assert_eq!(
            SlashCommand::parse("/stop@ReplayBot"),
            Some(SlashCommand::Stop)
        );
        assert_eq!(
            SlashCommand::parse("/compact@MyBot"),
            Some(SlashCommand::Compact)
        );
    }

    #[test]
    fn parse_rejects_prose_unknowns_and_args() {
        assert_eq!(SlashCommand::parse("stop"), None);
        assert_eq!(SlashCommand::parse("/frobnicate"), None);
        assert_eq!(SlashCommand::parse("/stop the build"), None);
        assert_eq!(SlashCommand::parse("/clear and also..."), None);
        assert_eq!(SlashCommand::parse(""), None);
        assert_eq!(SlashCommand::parse("/"), None);
        // /help stays runner-side (the runner sentinel answers it in
        // DMs); the router does not treat it as a command.
        assert_eq!(SlashCommand::parse("/help"), None);
    }

    #[test]
    fn detect_only_matches_chat_kind() {
        let mut ev = chat_event("/stop");
        assert_eq!(SlashCommand::detect(&ev), Some(SlashCommand::Stop));
        ev.message.kind = MessageKind::Webhook;
        assert_eq!(SlashCommand::detect(&ev), None);
    }

    #[test]
    fn detect_requires_text_content() {
        let mut ev = chat_event("/stop");
        ev.message.content = serde_json::json!({ "callback": {"id": "x"} });
        assert_eq!(SlashCommand::detect(&ev), None);
    }

    #[test]
    fn control_content_shape_is_pinned() {
        let c = control_content(SlashCommand::Stop, "/stop");
        assert_eq!(c["control"]["op"], "stop");
        assert_eq!(c["command"], "stop");
        assert_eq!(c["text"], "/stop");
        assert!(c.get("original_text").is_none());
    }

    #[test]
    fn control_content_preserves_non_canonical_original() {
        let c = control_content(SlashCommand::Stop, " /cancel ");
        assert_eq!(c["text"], "/stop");
        assert_eq!(c["original_text"], "/cancel");
    }

    #[test]
    fn passthrough_content_normalises_text_and_stamps_command() {
        let original = serde_json::json!({ "text": "/CLEAR@Bot", "extra": 7 });
        let c = passthrough_content(SlashCommand::Clear, &original, "/CLEAR@Bot");
        assert_eq!(c["text"], "/clear");
        assert_eq!(c["command"], "clear");
        assert_eq!(c["original_text"], "/CLEAR@Bot");
        // Unrelated keys on the original content survive.
        assert_eq!(c["extra"], 7);
    }

    #[test]
    fn passthrough_content_canonical_input_adds_no_original() {
        let original = serde_json::json!({ "text": "/compact" });
        let c = passthrough_content(SlashCommand::Compact, &original, "/compact");
        assert_eq!(c["text"], "/compact");
        assert_eq!(c["command"], "compact");
        assert!(c.get("original_text").is_none());
    }

    #[test]
    fn render_status_includes_every_field() {
        let session = copperclaw_types::Session {
            id: copperclaw_types::SessionId::new(),
            agent_group_id: copperclaw_types::AgentGroupId::new(),
            messaging_group_id: None,
            thread_id: None,
            agent_provider: None,
            status: copperclaw_types::SessionStatus::Active,
            container_status: copperclaw_types::ContainerStatus::Stopped,
            last_active: Utc::now(),
            created_at: Utc::now(),
            source_session_id: None,
        };
        let text = render_status("Replay", &session, 3);
        assert!(text.contains("agent group: Replay"));
        assert!(text.contains(&session.id.as_uuid().to_string()));
        assert!(text.contains("session state: active (container stopped)"));
        assert!(text.contains("queued messages: 3"));
        assert!(text.contains("last active: "));
        assert!(text.contains("created: "));
    }
}
