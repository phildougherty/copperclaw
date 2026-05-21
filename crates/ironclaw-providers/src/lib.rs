//! Agent provider trait + implementations.
//!
//! This crate defines the contract between the ironclaw runner and any
//! upstream agent backend (Anthropic, and later Codex / `OpenCode` /
//! Ollama). The two key abstractions are:
//!
//! * [`AgentProvider`] — a factory that opens a new turn given a
//!   [`QueryInput`]. Cheap to clone, safe to share.
//! * [`AgentQuery`] — an active turn that pumps [`ProviderEvent`]s back to
//!   the runner via [`AgentQuery::next_event`].
//!
//! See `PLAN.md` § 5.2 for the architectural picture.

use async_trait::async_trait;
use ironclaw_types::ProviderEvent;

pub mod anthropic;
pub mod codex;
pub mod error;
pub mod ollama;
pub mod opencode;
pub mod subprocess;
pub mod types;

pub use anthropic::AnthropicProvider;
pub use codex::CodexProvider;
pub use error::ProviderError;
pub use ollama::OllamaProvider;
pub use opencode::OpenCodeProvider;
pub use subprocess::{PushPolicy, SubprocessConfig, SubprocessProvider};
pub use types::{HistoryMessage, QueryInput, ToolDef};

/// Factory for [`AgentQuery`] instances. One per configured provider; cheap
/// to clone, safe to share across tasks.
#[async_trait]
pub trait AgentProvider: Send + Sync {
    /// Stable identifier (e.g. `"anthropic"`). Used in logs and config.
    fn name(&self) -> &str;

    /// True when the provider parses and acts on `/slash` commands natively
    /// (as opposed to the runner intercepting them). Defaults to `false`.
    fn supports_native_slash_commands(&self) -> bool {
        false
    }

    /// Open a new turn. The returned [`AgentQuery`] streams
    /// [`ProviderEvent`]s until it produces
    /// [`ProviderEvent::Result`] or [`ProviderEvent::Error`].
    async fn query(&self, input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError>;

    /// Classify an error: did the upstream tell us the session/continuation
    /// token is no longer usable? The runner uses this to decide whether to
    /// drop the persisted continuation and start fresh.
    fn is_session_invalid(&self, err: &ProviderError) -> bool;
}

/// An in-flight provider turn.
#[async_trait]
pub trait AgentQuery: Send {
    /// Push an additional user-side message into the open turn. Most
    /// stateless HTTP providers (Anthropic Messages) reject this — the
    /// caller is expected to append the message to
    /// [`QueryInput::history`] and start a new query.
    async fn push(&mut self, message: String) -> Result<(), ProviderError>;

    /// Signal that no more input will be pushed. Idempotent.
    async fn end(&mut self) -> Result<(), ProviderError>;

    /// Pump the next provider event. Returns `None` once the stream is
    /// exhausted.
    async fn next_event(&mut self) -> Option<ProviderEvent>;

    /// Cancel the turn. Idempotent. Must drop any background work the
    /// provider spawned.
    async fn abort(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ironclaw_types::ProviderEvent;

    /// Confirms a hand-rolled provider compiles against the trait surface,
    /// covering the default `supports_native_slash_commands` impl.
    struct DummyProvider;

    #[async_trait]
    impl AgentProvider for DummyProvider {
        #[allow(clippy::unnecessary_literal_bound)]
        fn name(&self) -> &str {
            "dummy"
        }
        async fn query(&self, _input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError> {
            Ok(Box::new(DummyQuery::default()))
        }
        fn is_session_invalid(&self, err: &ProviderError) -> bool {
            matches!(err, ProviderError::SessionInvalid)
        }
    }

    #[derive(Default)]
    struct DummyQuery {
        events: Vec<ProviderEvent>,
        ended: bool,
        aborted: bool,
    }

    #[async_trait]
    impl AgentQuery for DummyQuery {
        async fn push(&mut self, _message: String) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), ProviderError> {
            self.ended = true;
            Ok(())
        }
        async fn next_event(&mut self) -> Option<ProviderEvent> {
            self.events.pop()
        }
        async fn abort(&mut self) {
            self.aborted = true;
        }
    }

    #[tokio::test]
    async fn dummy_provider_defaults() {
        let p = DummyProvider;
        assert_eq!(p.name(), "dummy");
        assert!(!p.supports_native_slash_commands());
        let mut q = p.query(QueryInput::default()).await.unwrap();
        q.push("hi".into()).await.unwrap();
        q.end().await.unwrap();
        assert!(q.next_event().await.is_none());
        q.abort().await;
        assert!(p.is_session_invalid(&ProviderError::SessionInvalid));
        assert!(!p.is_session_invalid(&ProviderError::Cancelled));
    }
}
