//! Codex subprocess provider.
//!
//! Spawns the `codex` CLI as a child process and bridges
//! [`crate::QueryInput`] / [`copperclaw_types::ProviderEvent`] over its
//! stdin/stdout JSON-Lines protocol. See [`crate::subprocess`] for the wire
//! format.
//!
//! Codex runs one turn per spawn — the runner is expected to start a fresh
//! query for every follow-up message — so [`crate::AgentQuery::push`]
//! returns [`crate::ProviderError::BadRequest`] by default.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::error::ProviderError;
use crate::subprocess::{PushPolicy, SubprocessConfig, SubprocessProvider};
use crate::types::QueryInput;
use crate::{AgentProvider, AgentQuery};

/// Stable provider name surfaced via [`AgentProvider::name`].
pub const PROVIDER_NAME: &str = "codex";

/// Subprocess-bridge provider for the `codex` CLI.
#[derive(Debug, Clone)]
pub struct CodexProvider {
    inner: SubprocessProvider,
}

impl CodexProvider {
    /// Build a Codex provider that will spawn `binary_path` with the given
    /// extra arguments on every [`AgentProvider::query`]. Push is rejected
    /// — Codex is single-turn-per-spawn.
    #[must_use]
    pub fn new(binary_path: PathBuf, extra_args: Vec<String>) -> Self {
        let cfg = SubprocessConfig::new(PROVIDER_NAME, binary_path)
            .with_args(extra_args)
            .with_push_policy(PushPolicy::Reject);
        Self {
            inner: SubprocessProvider::new(cfg),
        }
    }

    /// Build a Codex provider whose `push` accepts mid-turn user messages.
    /// Use when the configured Codex binary supports it.
    #[must_use]
    pub fn new_interactive(binary_path: PathBuf, extra_args: Vec<String>) -> Self {
        let cfg = SubprocessConfig::new(PROVIDER_NAME, binary_path)
            .with_args(extra_args)
            .with_push_policy(PushPolicy::Accept);
        Self {
            inner: SubprocessProvider::new(cfg),
        }
    }

    /// Accessor — exposed mainly so tests can inspect the inner config.
    #[must_use]
    pub fn config(&self) -> &SubprocessConfig {
        self.inner.config()
    }
}

#[async_trait]
impl AgentProvider for CodexProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn supports_native_slash_commands(&self) -> bool {
        self.inner.supports_native_slash_commands()
    }

    async fn query(&self, input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError> {
        self.inner.query(input).await
    }

    fn is_session_invalid(&self, err: &ProviderError) -> bool {
        self.inner.is_session_invalid(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::ProviderEvent;

    #[test]
    fn new_defaults_to_reject_push() {
        let p = CodexProvider::new(PathBuf::from("/usr/bin/codex"), vec!["--json".into()]);
        assert_eq!(p.name(), PROVIDER_NAME);
        assert_eq!(p.config().binary(), PathBuf::from("/usr/bin/codex"));
        assert_eq!(p.config().args(), &["--json".to_string()]);
        assert_eq!(p.config().push_policy(), PushPolicy::Reject);
    }

    #[test]
    fn new_interactive_uses_accept_push() {
        let p = CodexProvider::new_interactive(PathBuf::from("/bin/codex"), Vec::new());
        assert_eq!(p.config().push_policy(), PushPolicy::Accept);
    }

    #[test]
    fn provider_clone_shares_config() {
        let p = CodexProvider::new(PathBuf::from("/usr/bin/codex"), Vec::new());
        let c = p.clone();
        assert_eq!(p.name(), c.name());
    }

    #[test]
    fn provider_flags() {
        let p = CodexProvider::new(PathBuf::from("/usr/bin/codex"), Vec::new());
        assert!(!p.supports_native_slash_commands());
        assert!(p.is_session_invalid(&ProviderError::SessionInvalid));
        assert!(!p.is_session_invalid(&ProviderError::Cancelled));
    }

    #[tokio::test]
    async fn query_through_sh_emits_result() {
        // Use /bin/sh as a stand-in for `codex` to exercise the query path
        // end-to-end via the wrapper.
        let script = "cat > /dev/null; \
             printf '%s\\n' '{\"type\":\"init\",\"continuation\":\"cx_codex\"}'; \
             printf '%s\\n' '{\"type\":\"result\",\"text\":\"hi\"}'";
        let p = CodexProvider::new(PathBuf::from("/bin/sh"), vec!["-c".into(), script.into()]);
        let mut q = p.query(QueryInput::new("s", "m")).await.unwrap();
        q.end().await.unwrap();
        let first = q.next_event().await.unwrap();
        match first {
            ProviderEvent::Init { continuation } => assert_eq!(continuation, "cx_codex"),
            other => panic!("expected init, got {other:?}"),
        }
        let result = q.next_event().await.unwrap();
        match result {
            ProviderEvent::Result { text } => assert_eq!(text.as_deref(), Some("hi")),
            other => panic!("expected result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn push_rejected_by_default() {
        let script = "sleep 5";
        let p = CodexProvider::new(PathBuf::from("/bin/sh"), vec!["-c".into(), script.into()]);
        let mut q = p.query(QueryInput::new("s", "m")).await.unwrap();
        let err = q.push("hi".into()).await.unwrap_err();
        assert!(matches!(err, ProviderError::BadRequest(_)));
        q.abort().await;
    }
}
