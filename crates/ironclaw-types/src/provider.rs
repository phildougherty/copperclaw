use serde::{Deserialize, Serialize};

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
/// container poll loop in `ironclaw-runner`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderEvent {
    /// Emitted once at the start. Contains the opaque continuation token
    /// the runner will persist to `session_state` and pass back next turn.
    Init { continuation: String },
    /// Final assistant text for the turn (may be empty).
    Result { text: Option<String> },
    Error { message: String, retryable: bool },
    Progress { message: String },
    /// Heartbeat — used to detect long-running tools.
    Activity,
    ToolStart {
        name: String,
        declared_timeout_ms: Option<u64>,
    },
    ToolEnd,
    /// Per-turn token usage as reported by the provider. Emitted at
    /// least once before [`ProviderEvent::Result`] when the provider
    /// surfaces it (Anthropic's `message_delta.usage` field). The
    /// runner accumulates these into the `agent_turns` table for
    /// observability and per-group budgeting.
    Usage {
        input_tokens: u32,
        output_tokens: u32,
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
        let e = ProviderEvent::Init { continuation: "abc".into() };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"type\":\"init\""));
        let back: ProviderEvent = serde_json::from_str(&json).unwrap();
        match back {
            ProviderEvent::Init { continuation } => assert_eq!(continuation, "abc"),
            _ => panic!("wrong variant"),
        }
    }
}
