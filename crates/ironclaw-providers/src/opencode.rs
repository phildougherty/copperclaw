//! `OpenCode` subprocess provider.
//!
//! Same wire shape as [`crate::CodexProvider`] — see
//! [`crate::subprocess`] for the JSON-Lines protocol — but spawns the
//! `opencode` CLI and accepts mid-turn `push` lines by default because
//! `opencode` runs an interactive session per spawn.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::error::ProviderError;
use crate::subprocess::{PushPolicy, SubprocessConfig, SubprocessProvider};
use crate::types::QueryInput;
use crate::{AgentProvider, AgentQuery};

/// Stable provider name surfaced via [`AgentProvider::name`].
pub const PROVIDER_NAME: &str = "opencode";

/// Subprocess-bridge provider for the `opencode` CLI.
#[derive(Debug, Clone)]
pub struct OpenCodeProvider {
    inner: SubprocessProvider,
}

impl OpenCodeProvider {
    /// Build an `OpenCode` provider that accepts mid-turn pushes by default.
    #[must_use]
    pub fn new(binary_path: PathBuf, extra_args: Vec<String>) -> Self {
        let cfg = SubprocessConfig::new(PROVIDER_NAME, binary_path)
            .with_args(extra_args)
            .with_push_policy(PushPolicy::Accept);
        Self {
            inner: SubprocessProvider::new(cfg),
        }
    }

    /// Build an `OpenCode` provider with [`PushPolicy::Reject`]. Use when the
    /// configured binary is run in batch (one-shot) mode.
    #[must_use]
    pub fn new_oneshot(binary_path: PathBuf, extra_args: Vec<String>) -> Self {
        let cfg = SubprocessConfig::new(PROVIDER_NAME, binary_path)
            .with_args(extra_args)
            .with_push_policy(PushPolicy::Reject);
        Self {
            inner: SubprocessProvider::new(cfg),
        }
    }

    /// Accessor — inner subprocess config.
    #[must_use]
    pub fn config(&self) -> &SubprocessConfig {
        self.inner.config()
    }
}

#[async_trait]
impl AgentProvider for OpenCodeProvider {
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
    use ironclaw_types::ProviderEvent;

    #[test]
    fn new_defaults_to_accept_push() {
        let p = OpenCodeProvider::new(PathBuf::from("/usr/bin/opencode"), vec!["--json".into()]);
        assert_eq!(p.name(), PROVIDER_NAME);
        assert_eq!(p.config().binary(), PathBuf::from("/usr/bin/opencode"));
        assert_eq!(p.config().args(), &["--json".to_string()]);
        assert_eq!(p.config().push_policy(), PushPolicy::Accept);
    }

    #[test]
    fn new_oneshot_uses_reject_push() {
        let p = OpenCodeProvider::new_oneshot(PathBuf::from("/bin/opencode"), Vec::new());
        assert_eq!(p.config().push_policy(), PushPolicy::Reject);
    }

    #[test]
    fn provider_clone_shares_config() {
        let p = OpenCodeProvider::new(PathBuf::from("/usr/bin/opencode"), Vec::new());
        let c = p.clone();
        assert_eq!(p.name(), c.name());
    }

    #[test]
    fn provider_flags() {
        let p = OpenCodeProvider::new(PathBuf::from("/usr/bin/opencode"), Vec::new());
        assert!(!p.supports_native_slash_commands());
        assert!(p.is_session_invalid(&ProviderError::SessionInvalid));
        assert!(!p.is_session_invalid(&ProviderError::Cancelled));
    }

    #[tokio::test]
    async fn query_through_sh_emits_result() {
        let script = "cat > /dev/null; \
             printf '%s\\n' '{\"type\":\"init\",\"continuation\":\"oc_1\"}'; \
             printf '%s\\n' '{\"type\":\"result\",\"text\":\"open\"}'";
        let p = OpenCodeProvider::new_oneshot(
            PathBuf::from("/bin/sh"),
            vec!["-c".into(), script.into()],
        );
        let mut q = p.query(QueryInput::new("s", "m")).await.unwrap();
        q.end().await.unwrap();
        let _ = q.next_event().await.unwrap();
        let result = q.next_event().await.unwrap();
        match result {
            ProviderEvent::Result { text } => assert_eq!(text.as_deref(), Some("open")),
            other => panic!("expected result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn push_accepted_by_default() {
        let script = "read q; read p; \
             printf '%s\\n' '{\"type\":\"init\",\"continuation\":\"oc_p\"}'; \
             printf '%s\\n' '{\"type\":\"result\",\"text\":\"pushed\"}'";
        let p = OpenCodeProvider::new(
            PathBuf::from("/bin/sh"),
            vec!["-c".into(), script.into()],
        );
        let mut q = p.query(QueryInput::new("s", "m")).await.unwrap();
        q.push("hello".into()).await.unwrap();
        q.end().await.unwrap();
        let mut saw_result = false;
        while let Some(ev) = q.next_event().await {
            if let ProviderEvent::Result { text } = ev {
                assert_eq!(text.as_deref(), Some("pushed"));
                saw_result = true;
                break;
            }
        }
        assert!(saw_result);
    }
}
