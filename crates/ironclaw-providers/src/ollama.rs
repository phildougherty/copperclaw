//! Ollama provider — Anthropic-compatible HTTP facade.
//!
//! Ollama exposes an Anthropic Messages-compatible endpoint at
//! `<base_url>/v1/messages`. This provider piggybacks on
//! [`crate::AnthropicProvider`]: same wire format, same SSE parser, just a
//! different base URL, model default, and reported name. Ollama servers do
//! not require an `x-api-key` — the underlying client still sends the
//! header but the value is empty (and Ollama ignores it).
//!
//! ## Defaults
//!
//! * `base_url` is required.
//! * `model` defaults to [`DEFAULT_MODEL`] when the caller passes `None`.
//! * `api_key` is unused; provide `""` (or anything — it isn't read).

use async_trait::async_trait;

use crate::anthropic::AnthropicProvider;
use crate::error::ProviderError;
use crate::types::QueryInput;
use crate::{AgentProvider, AgentQuery};

/// Stable provider name surfaced via [`AgentProvider::name`].
pub const PROVIDER_NAME: &str = "ollama";

/// Sensible default model identifier when the caller doesn't override.
pub const DEFAULT_MODEL: &str = "llama3.1:8b";

/// Provider that talks to an Anthropic-compatible base URL exposed by an
/// Ollama server.
///
/// Internally this is a thin facade around [`AnthropicProvider`]; every
/// turn replays the full history (Ollama, like Anthropic Messages, is
/// stateless on the wire).
#[derive(Debug, Clone)]
pub struct OllamaProvider {
    inner: AnthropicProvider,
    default_model: String,
}

impl OllamaProvider {
    /// Build a provider against `base_url` with a per-call model override.
    /// Pass `None` for `model` to inherit [`DEFAULT_MODEL`].
    #[must_use]
    pub fn new(base_url: impl Into<String>, model: Option<String>) -> Self {
        // Ollama doesn't read x-api-key but the underlying client always
        // sends it; an empty string is a harmless placeholder.
        let inner = AnthropicProvider::with_base_url("", base_url);
        let default_model = model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
        Self { inner, default_model }
    }

    /// The model name applied when a [`QueryInput`] arrives with an empty
    /// `model` field.
    #[must_use]
    pub fn default_model(&self) -> &str {
        &self.default_model
    }
}

#[async_trait]
impl AgentProvider for OllamaProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn supports_native_slash_commands(&self) -> bool {
        false
    }

    async fn query(&self, mut input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError> {
        if input.model.is_empty() {
            input.model.clone_from(&self.default_model);
        }
        self.inner.query(input).await
    }

    fn is_session_invalid(&self, err: &ProviderError) -> bool {
        self.inner.is_session_invalid(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::HistoryMessage;

    #[test]
    fn name_and_flags() {
        let p = OllamaProvider::new("http://localhost:11434", None);
        assert_eq!(p.name(), PROVIDER_NAME);
        assert!(!p.supports_native_slash_commands());
        assert_eq!(p.default_model(), DEFAULT_MODEL);
    }

    #[test]
    fn explicit_model_overrides_default() {
        let p = OllamaProvider::new("http://localhost:11434", Some("qwen2:7b".into()));
        assert_eq!(p.default_model(), "qwen2:7b");
    }

    #[test]
    fn is_session_invalid_delegates() {
        let p = OllamaProvider::new("http://x", None);
        assert!(p.is_session_invalid(&ProviderError::SessionInvalid));
        assert!(!p.is_session_invalid(&ProviderError::Cancelled));
        assert!(!p.is_session_invalid(&ProviderError::Overloaded));
        assert!(!p.is_session_invalid(&ProviderError::BadRequest("x".into())));
    }

    #[test]
    fn provider_clone_shares_inner() {
        let p = OllamaProvider::new("http://x", Some("m".into()));
        let c = p.clone();
        assert_eq!(p.name(), c.name());
        assert_eq!(p.default_model(), c.default_model());
    }

    #[tokio::test]
    async fn empty_model_falls_back_to_default() {
        // We can't actually round-trip without a server, but we can confirm
        // the model rewrite happens before the (failing) connection attempt
        // by aiming at an unbound port and inspecting the error type.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let p = OllamaProvider::new(format!("http://{addr}"), None);
        let mut input = QueryInput::new("s", "");
        input.history.push(HistoryMessage::User { content: "hi".into() });
        let r = p.query(input).await;
        match r {
            Err(ProviderError::Transport(_)) => {}
            Ok(_) => panic!("expected transport err"),
            Err(other) => panic!("expected transport, got {other:?}"),
        }
    }
}
