//! Input types for [`crate::AgentProvider::query`].
//!
//! [`QueryInput`] is the structured payload the runner hands to a provider
//! to start one turn. It carries the system prompt, model, effort, tool
//! catalogue, and (separately) two notions of continuity:
//!
//! * [`QueryInput::previous_continuation`] — an opaque per-provider token
//!   the provider itself produced via [`copperclaw_types::ProviderEvent::Init`].
//!   For native-continuation providers this is enough to resume a session;
//!   for stateless providers (Anthropic Messages) it is treated as advisory
//!   and the real continuity comes from [`QueryInput::history`].
//! * [`QueryInput::history`] — the full ordered chat transcript. The runner
//!   persists this; the provider just replays it on every turn. Tool results
//!   live here as [`HistoryMessage::Tool`] entries.

use copperclaw_types::Effort;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A tool the model is allowed to invoke for this turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDef {
    /// Stable tool name (matches the name surfaced in
    /// [`copperclaw_types::ProviderEvent::ToolStart::name`]).
    pub name: String,
    /// One-line human description that providers may surface to the model.
    pub description: String,
    /// JSON Schema for the tool input. Providers forward this verbatim.
    pub input_schema: Value,
}

/// One entry in the chat transcript. The runner owns this history and feeds
/// it into every [`QueryInput`]; the provider implementation never mutates
/// it directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum HistoryMessage {
    /// A user-authored message (incoming chat text, or an instruction).
    User { content: String },
    /// A prior assistant turn (final text only — tool-use blocks live in
    /// [`Self::ToolUse`]).
    Assistant { content: String },
    /// An assistant tool invocation from a previous turn. The provider
    /// re-serializes this so the model can chain a follow-up tool result.
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// The result of executing a tool invocation. Surfaces in the transcript
    /// as the user role with a `tool_result` content block on Anthropic.
    Tool {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// An image supplied to the model — an inbound photo, or the result of
    /// a `view_image` tool call. Carried as base64 so the transcript is
    /// self-contained (no dependency on a file that may be cleaned up).
    /// Serializes as a `user`-role image content block; vision-capable
    /// models (e.g. minimax-m3) read it, text-only providers drop it to a
    /// placeholder.
    Image {
        /// MIME type, e.g. `image/png`, `image/jpeg`, `image/webp`.
        media_type: String,
        /// Base64-encoded image bytes, with no `data:` URI prefix.
        data: String,
    },
}

/// Input bundle for one provider turn. Construct via [`QueryInput::default`]
/// + field assignment or via [`QueryInput::new`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryInput {
    /// Static system prompt prepended to every turn. This is the
    /// persona / skills / safety preamble assembled once at runner
    /// startup — it does NOT vary turn-to-turn, which is what makes it
    /// the stable prefix the Anthropic provider can cache (see
    /// [`Self::system_context`] for the volatile counterpart).
    pub system: String,
    /// Optional volatile system addendum — the per-inbound
    /// "Conversation context" paragraph (venue shape, batch size,
    /// history depth, …) that changes on (almost) every turn. Held
    /// separate from [`Self::system`] so a caching provider can place a
    /// cache breakpoint AFTER the stable system prompt but BEFORE this
    /// volatile text, keeping the cached prefix byte-stable across
    /// inbounds. Providers that don't cache flatten the two back into
    /// one system string via [`Self::combined_system`], so the request
    /// they emit is byte-identical to the pre-split shape.
    #[serde(default)]
    pub system_context: Option<String>,
    /// Provider-native model identifier (e.g. `claude-sonnet-4-6`).
    pub model: String,
    /// Tier-of-effort hint.
    pub effort: Effort,
    /// Opaque continuation token returned by the provider on the previous
    /// turn. `None` means "fresh session".
    pub previous_continuation: Option<String>,
    /// Full chat transcript for this conversation. See [`HistoryMessage`].
    pub history: Vec<HistoryMessage>,
    /// Tools the model may call this turn.
    pub tools: Vec<ToolDef>,
    /// Maximum tokens to sample.
    pub max_tokens: u32,
    /// Sampling temperature, if set.
    pub temperature: Option<f32>,
    /// Display name of the assistant ("Claude", "Codex", …).
    pub assistant_name: Option<String>,
    /// Display name to pass through to the provider for human-facing logs.
    pub display_name: Option<String>,
}

impl Default for QueryInput {
    fn default() -> Self {
        Self {
            system: String::new(),
            system_context: None,
            model: String::new(),
            effort: Effort::Medium,
            previous_continuation: None,
            history: Vec::new(),
            tools: Vec::new(),
            max_tokens: 4096,
            temperature: None,
            assistant_name: None,
            display_name: None,
        }
    }
}

impl QueryInput {
    /// Minimal constructor. Use struct-update syntax to fill the rest.
    #[must_use]
    pub fn new(system: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            system: system.into(),
            model: model.into(),
            ..Self::default()
        }
    }

    /// Flatten [`Self::system`] (stable) and [`Self::system_context`]
    /// (volatile) back into the single system string that every
    /// non-caching provider sends on the wire. The concatenation is
    /// byte-identical to the pre-split runner join the runner used to
    /// perform inline before handing us one combined `system` string:
    /// a `\n\n` paragraph break between persona and context, the static
    /// system's trailing newlines trimmed first so the gap is always
    /// exactly one blank line. When the context is absent/empty this
    /// returns the static system verbatim; when the static system is
    /// empty it returns just the context.
    #[must_use]
    pub fn combined_system(&self) -> String {
        match self.system_context.as_deref() {
            Some(ctx) if !ctx.is_empty() => {
                if self.system.is_empty() {
                    ctx.to_string()
                } else {
                    let trimmed = self.system.trim_end_matches('\n');
                    format!("{trimmed}\n\n{ctx}")
                }
            }
            _ => self.system.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_query_input() {
        let q = QueryInput::default();
        assert_eq!(q.system, "");
        assert!(q.system_context.is_none());
        assert_eq!(q.model, "");
        assert_eq!(q.effort, Effort::Medium);
        assert!(q.previous_continuation.is_none());
        assert!(q.history.is_empty());
        assert!(q.tools.is_empty());
        assert_eq!(q.max_tokens, 4096);
        assert!(q.temperature.is_none());
        assert!(q.assistant_name.is_none());
        assert!(q.display_name.is_none());
    }

    #[test]
    fn new_sets_system_and_model() {
        let q = QueryInput::new("you are helpful", "claude-sonnet-4-6");
        assert_eq!(q.system, "you are helpful");
        assert_eq!(q.model, "claude-sonnet-4-6");
    }

    #[test]
    fn tool_def_roundtrip() {
        let t = ToolDef {
            name: "weather".into(),
            description: "look up the weather".into(),
            input_schema: serde_json::json!({ "type": "object" }),
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: ToolDef = serde_json::from_str(&s).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn history_message_user_roundtrip() {
        let m = HistoryMessage::User {
            content: "hi".into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"role\":\"user\""));
        let back: HistoryMessage = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn history_message_assistant_roundtrip() {
        let m = HistoryMessage::Assistant {
            content: "ok".into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"role\":\"assistant\""));
        let back: HistoryMessage = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn history_message_tool_use_roundtrip() {
        let m = HistoryMessage::ToolUse {
            id: "tu_1".into(),
            name: "weather".into(),
            input: serde_json::json!({ "loc": "sf" }),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"role\":\"tool_use\""));
        let back: HistoryMessage = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn history_message_tool_roundtrip() {
        let m = HistoryMessage::Tool {
            tool_use_id: "tu_1".into(),
            content: "sunny".into(),
            is_error: false,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"role\":\"tool\""));
        let back: HistoryMessage = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn query_input_full_serde() {
        let q = QueryInput {
            system: "S".into(),
            system_context: Some("CTX".into()),
            model: "M".into(),
            effort: Effort::High,
            previous_continuation: Some("c1".into()),
            history: vec![HistoryMessage::User {
                content: "hi".into(),
            }],
            tools: vec![ToolDef {
                name: "t".into(),
                description: "d".into(),
                input_schema: serde_json::json!({}),
            }],
            max_tokens: 2048,
            temperature: Some(0.7),
            assistant_name: Some("Claude".into()),
            display_name: Some("Bot".into()),
        };
        let s = serde_json::to_string(&q).unwrap();
        let back: QueryInput = serde_json::from_str(&s).unwrap();
        assert_eq!(back.system, "S");
        assert_eq!(back.system_context.as_deref(), Some("CTX"));
        assert_eq!(back.model, "M");
        assert_eq!(back.effort, Effort::High);
        assert_eq!(back.previous_continuation.as_deref(), Some("c1"));
        assert_eq!(back.history.len(), 1);
        assert_eq!(back.tools.len(), 1);
        assert_eq!(back.max_tokens, 2048);
        assert!((back.temperature.unwrap() - 0.7).abs() < 1e-6);
        assert_eq!(back.assistant_name.as_deref(), Some("Claude"));
        assert_eq!(back.display_name.as_deref(), Some("Bot"));
    }

    #[test]
    fn combined_system_joins_static_and_context_with_blank_line() {
        let mut q = QueryInput::new("you are helpful", "m");
        q.system_context = Some("Conversation context: x.".into());
        assert_eq!(
            q.combined_system(),
            "you are helpful\n\nConversation context: x."
        );
    }

    #[test]
    fn combined_system_trims_trailing_newlines_on_static_before_joining() {
        // The static prompt may already end in a newline; the join must
        // still produce exactly one blank line, never two.
        let mut q = QueryInput::new("you are helpful\n", "m");
        q.system_context = Some("ctx".into());
        assert_eq!(q.combined_system(), "you are helpful\n\nctx");
    }

    #[test]
    fn combined_system_without_context_returns_static_verbatim() {
        let q = QueryInput::new("you are helpful", "m");
        assert_eq!(q.combined_system(), "you are helpful");
    }

    #[test]
    fn combined_system_empty_context_returns_static_verbatim() {
        let mut q = QueryInput::new("you are helpful", "m");
        q.system_context = Some(String::new());
        assert_eq!(q.combined_system(), "you are helpful");
    }

    #[test]
    fn combined_system_empty_static_returns_just_context() {
        let mut q = QueryInput::new("", "m");
        q.system_context = Some("Conversation context: x.".into());
        assert_eq!(q.combined_system(), "Conversation context: x.");
    }
}
