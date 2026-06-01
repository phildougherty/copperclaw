use crate::id::{AgentGroupId, MessagingGroupId, SessionId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How sessions are partitioned for a given wiring.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionMode {
    /// One session per `(agent_group, messaging_group)` — threads share.
    Shared,
    /// One session per `(agent_group, messaging_group, thread_id)`.
    PerThread,
    /// One session per `agent_group` — all messaging groups share.
    AgentShared,
}

/// Engagement rule for an agent on a messaging group.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngageMode {
    /// Match a regex against message text.
    Pattern,
    /// Engage when explicitly @-mentioned.
    Mention,
    /// Engage on mention; stay engaged within the same thread.
    MentionSticky,
}

/// Container lifecycle status.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerStatus {
    Idle,
    Running,
    Stopped,
}

impl ContainerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Stopped => "stopped",
        }
    }
}

/// Lifecycle status of a session as seen by the host.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Active,
    Stopped,
    Archived,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Stopped => "stopped",
            Self::Archived => "archived",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub agent_group_id: AgentGroupId,
    pub messaging_group_id: Option<MessagingGroupId>,
    pub thread_id: Option<String>,
    pub agent_provider: Option<String>,
    pub status: SessionStatus,
    pub container_status: ContainerStatus,
    pub last_active: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    /// Session id of the agent that spawned this one (via `create_agent`).
    /// `None` for root sessions (a real user channel kicked them off).
    /// Used by the runtime to route a child's default `send_message`
    /// (`to: None`) back to the parent's `inbound.db` instead of the
    /// user's chat — see `docs/plans/agent-to-agent-routing.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session_id: Option<SessionId>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::*;

    #[test]
    fn session_mode_serde() {
        for m in [
            SessionMode::Shared,
            SessionMode::PerThread,
            SessionMode::AgentShared,
        ] {
            let json = serde_json::to_string(&m).unwrap();
            let back: SessionMode = serde_json::from_str(&json).unwrap();
            assert_eq!(m, back);
        }
        assert_eq!(
            serde_json::to_string(&SessionMode::PerThread).unwrap(),
            "\"per-thread\""
        );
    }

    #[test]
    fn session_roundtrip() {
        let s = Session {
            id: SessionId::new(),
            agent_group_id: AgentGroupId::new(),
            messaging_group_id: None,
            thread_id: None,
            agent_provider: Some("claude".into()),
            status: SessionStatus::Active,
            container_status: ContainerStatus::Idle,
            last_active: Utc::now(),
            created_at: Utc::now(),
            source_session_id: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(s.id, back.id);
        assert_eq!(s.status, back.status);
    }

    #[test]
    fn session_with_source_id_roundtrip() {
        let parent_id = SessionId::new();
        let s = Session {
            id: SessionId::new(),
            agent_group_id: AgentGroupId::new(),
            messaging_group_id: None,
            thread_id: None,
            agent_provider: Some("claude".into()),
            status: SessionStatus::Active,
            container_status: ContainerStatus::Idle,
            last_active: Utc::now(),
            created_at: Utc::now(),
            source_session_id: Some(parent_id),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source_session_id, Some(parent_id));
    }
}
