use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Tier-of-effort hint passed to the underlying model. Each provider maps
/// this onto its own knob.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
}

/// What the agent provider sends back as it runs a turn. Consumed by the
/// container poll loop in `copperclaw-runner`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderEvent {
    /// Emitted once at the start. Contains the opaque continuation token
    /// the runner will persist to `session_state` and pass back next turn.
    Init {
        continuation: String,
    },
    /// Final assistant text for the turn (may be empty).
    Result {
        text: Option<String>,
    },
    Error {
        message: String,
        retryable: bool,
    },
    Progress {
        message: String,
    },
    /// Heartbeat — used to detect long-running tools.
    Activity,
    ToolStart {
        name: String,
        declared_timeout_ms: Option<u64>,
    },
    ToolEnd,
    /// A complete tool-use request from the model. Emitted at
    /// `content_block_stop` for a `tool_use` block, after the
    /// provider has reassembled the streamed `input_json_delta`
    /// chunks into the full input value. The runner dispatches
    /// to the registered tool handler and feeds the result back
    /// as a `HistoryMessage::Tool` on the next turn.
    ToolCall {
        /// Provider-side tool-use id. Must echo back as the
        /// matching `tool_result.tool_use_id` next turn.
        id: String,
        /// Tool name (matches a key in the runner's tool map).
        name: String,
        /// Fully-parsed input. Empty object if the model called
        /// the tool with no arguments.
        input: Value,
    },
    /// Per-turn token usage as reported by the provider. Emitted at
    /// least once before [`ProviderEvent::Result`] when the provider
    /// surfaces it (Anthropic's `message_delta.usage` field). The
    /// runner accumulates these into the `agent_turns` table for
    /// observability and per-group budgeting.
    ///
    /// `cache_read_tokens` / `cache_creation_tokens` mirror Anthropic's
    /// prompt-caching usage fields (`cache_read_input_tokens` /
    /// `cache_creation_input_tokens`). They are `0` for providers that
    /// don't report caching (the `#[serde(default)]` keeps older event
    /// payloads decodable). A non-zero `cache_read_tokens` is the signal
    /// that a cache breakpoint *hit* this turn — that's what makes the
    /// cost win observable. Anthropic bills cached reads at ~10% of the
    /// base input rate and cache writes at ~125%, so the two are tracked
    /// separately. Note: on the Anthropic wire `input_tokens` already
    /// EXCLUDES the cached prefix, so total prompt size is
    /// `input_tokens + cache_read_tokens + cache_creation_tokens`.
    Usage {
        input_tokens: u32,
        output_tokens: u32,
        /// Input tokens served from the prompt cache (a cache hit). `0`
        /// when caching is off or the prefix missed.
        #[serde(default)]
        cache_read_tokens: u32,
        /// Input tokens written into the prompt cache this turn (a cache
        /// write — billed at a premium, amortized over later hits). `0`
        /// when caching is off.
        #[serde(default)]
        cache_creation_tokens: u32,
    },
    /// The provider finished reassembling a `tool_use` block but the
    /// concatenated `input_json_delta` chunks did not parse as JSON.
    /// Rather than treating this as a terminal stream error (which
    /// would leave the user without a reply), the runner converts this
    /// into a synthetic `tool_result` with `is_error: true` and feeds
    /// it back into the next turn so the model can self-correct. See
    /// `copperclaw-runner::run::pump_events` for the recovery path.
    ToolInputParseError {
        /// Provider-side tool-use id the model assigned to this block.
        /// Must echo back as the matching `tool_result.tool_use_id`.
        tool_use_id: String,
        /// Tool the model was trying to invoke.
        tool_name: String,
        /// Raw concatenated `input_json_delta` payload that failed to
        /// parse. Captured for audit / debugging; the runner does not
        /// attempt to repair it.
        raw_input: String,
        /// The underlying `serde_json` error rendered as a string (e.g.
        /// "EOF while parsing an object at line 1 column 37").
        parse_error: String,
    },
    /// One completed thinking (or `redacted_thinking`) content block.
    /// Reasoning-capable providers (Anthropic extended thinking,
    /// `Kimi K2.6`, `Qwen QwQ`, `DeepSeek R1`) stream the chain-of-thought
    /// as `thinking_delta` events interleaved with `signature_delta`
    /// events; the provider accumulates them and emits this event
    /// once at the `content_block_stop` boundary so the runner can
    /// (a) record the reasoning into `agent_turns` for audit and
    /// (b) surface it to the user as a [`MessageKind::Thinking`] row
    /// when the per-group `surface_thinking` config knob is on.
    ///
    /// `text` is the accumulated thinking prose (empty when
    /// `redacted == true`); `redacted` mirrors the upstream
    /// `redacted_thinking` block type — the user must NOT see the
    /// raw blob, only a placeholder.
    Thinking {
        /// Accumulated thinking text. Empty for redacted blocks.
        text: String,
        /// `true` when the upstream block was `redacted_thinking`.
        redacted: bool,
    },
}

/// Provider config materialized into the container at spawn time. Stored in
/// `container_configs.provider` and friends; passed to the provider factory.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderRuntimeConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub assistant_name: Option<String>,
    pub max_messages_per_prompt: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_serde() {
        assert_eq!(serde_json::to_string(&Effort::Low).unwrap(), "\"low\"");
    }

    #[test]
    fn provider_event_serde() {
        let e = ProviderEvent::Init {
            continuation: "abc".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"type\":\"init\""));
        let back: ProviderEvent = serde_json::from_str(&json).unwrap();
        match back {
            ProviderEvent::Init { continuation } => assert_eq!(continuation, "abc"),
            _ => panic!("wrong variant"),
        }
    }
}
