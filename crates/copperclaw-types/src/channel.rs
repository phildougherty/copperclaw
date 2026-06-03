use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::fmt;

/// An open-typed channel identifier. Channel implementations register a
/// `ChannelType` (e.g. `"telegram"`, `"slack"`, `"cli"`, `"agent"`) at
/// startup; the router and delivery loop match on the string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChannelType(pub String);

impl ChannelType {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Synthetic channel type used when an agent sends a message to another
    /// agent rather than an external platform.
    pub const AGENT: &'static str = "agent";

    /// CLI / stdio channel — used for tests and local REPL.
    pub const CLI: &'static str = "cli";
}

impl fmt::Display for ChannelType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ChannelType {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for ChannelType {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl Borrow<str> for ChannelType {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// Platform identity of the sender of an inbound event.
///
/// `channel_type + identity` is the unique key for a platform user;
/// `display_name` is best-effort and may be missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderIdentity {
    pub channel_type: ChannelType,
    pub identity: String,
    pub display_name: Option<String>,
}

/// Optional reply-routing override for inbound events. Used when the host's
/// admin CLI synthesizes an event on behalf of a user, and when a channel
/// adapter surfaces that an inbound message is a reply/quote of an earlier
/// message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyTo {
    pub channel_type: ChannelType,
    pub platform_id: String,
    pub thread_id: Option<String>,
    /// Whether the message this is a reply/quote of was authored by the
    /// agent (this bot) itself.
    ///
    /// Only the channel adapter can answer this — it alone knows its own
    /// platform identity (bot user id / username). The mention gate treats
    /// `Some(true)` as an implicit mention (a user replying to the agent's
    /// own message is engaging it). `Some(false)` (reply to *another*
    /// user's message) and `None` (parent author could not be resolved)
    /// are deliberately NOT treated as a mention — an arbitrary reply must
    /// never count as engaging the agent.
    #[serde(default)]
    pub replying_to_self: Option<bool>,
}

impl ReplyTo {
    /// Construct a `ReplyTo` whose parent author is unknown
    /// (`replying_to_self = None`). Adapters that can resolve the parent's
    /// author set [`Self::replying_to_self`] afterwards.
    #[must_use]
    pub fn new(channel_type: ChannelType, platform_id: String, thread_id: Option<String>) -> Self {
        Self {
            channel_type,
            platform_id,
            thread_id,
            replying_to_self: None,
        }
    }
}

/// Handle for an open DM thread on a platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmHandle {
    pub channel_type: ChannelType,
    pub platform_id: String,
    pub thread_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_type_serde_is_transparent() {
        let ct = ChannelType::new("telegram");
        let json = serde_json::to_string(&ct).unwrap();
        assert_eq!(json, "\"telegram\"");
        let back: ChannelType = serde_json::from_str(&json).unwrap();
        assert_eq!(ct, back);
    }

    #[test]
    fn channel_type_lookup_by_str_borrow() {
        use std::collections::HashMap;
        let mut m: HashMap<ChannelType, u32> = HashMap::new();
        m.insert(ChannelType::new("slack"), 1);
        assert_eq!(m.get("slack"), Some(&1));
    }
}
