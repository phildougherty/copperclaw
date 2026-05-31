use crate::id::{AgentGroupId, ApprovalId, MessagingGroupId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    Sender,
    Channel,
    InstallPackages,
    AddMcpServer,
    OneCli,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub id: ApprovalId,
    pub kind: ApprovalKind,
    pub messaging_group_id: Option<MessagingGroupId>,
    pub agent_group_id: Option<AgentGroupId>,
    pub requester: Option<UserId>,
    pub approver: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub payload: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_serde() {
        let a = Approval {
            id: ApprovalId::new(),
            kind: ApprovalKind::InstallPackages,
            messaging_group_id: None,
            agent_group_id: None,
            requester: None,
            approver: None,
            created_at: Utc::now(),
            payload: serde_json::json!({"apt":["jq"]}),
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: Approval = serde_json::from_str(&json).unwrap();
        assert_eq!(a.id, back.id);
    }
}
