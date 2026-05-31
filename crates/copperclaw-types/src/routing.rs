use crate::channel::ChannelType;
use crate::id::AgentGroupId;
use serde::{Deserialize, Serialize};

/// Per-session reply-in-place routing. Written by the host on every container
/// wake; read by the runner whenever the agent's response doesn't carry an
/// explicit destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRouting {
    pub channel_type: Option<ChannelType>,
    pub platform_id: Option<String>,
    pub thread_id: Option<String>,
}

/// A destination the agent can address by name (in `send_message(to=..)`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestinationRow {
    pub name: String,
    pub display_name: String,
    pub kind: DestinationKind,
    pub channel_type: Option<ChannelType>,
    pub platform_id: Option<String>,
    pub agent_group_id: Option<AgentGroupId>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DestinationKind {
    Channel,
    Agent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_kind_serde() {
        assert_eq!(serde_json::to_string(&DestinationKind::Channel).unwrap(), "\"channel\"");
        assert_eq!(serde_json::to_string(&DestinationKind::Agent).unwrap(), "\"agent\"");
    }
}
