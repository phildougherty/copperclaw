//! `DmHandle` returned by `ChannelAdapter::open_dm`.
//!
//! Note: a `DmHandle` shape also lives in `copperclaw-types::channel`; that one
//! describes an already-known DM thread on a platform. The handle returned by
//! `open_dm` here is the *result* of opening that DM, i.e. once we know the
//! platform-side address we can deliver to. Channels that don't support DMs
//! return `Ok(None)` instead.

use copperclaw_types::ChannelType;

/// Result of `ChannelAdapter::open_dm` on a platform that supports DMs.
///
/// `user_id` is the copperclaw-side user reference passed to `open_dm`.
/// `platform_id` is the platform's identifier for the freshly opened DM
/// thread (e.g. Telegram chat id, Slack channel id).
/// `channel_type` is the channel that produced the handle — included so
/// the host can route the next delivery without a separate lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmHandle {
    pub user_id: String,
    pub platform_id: String,
    pub channel_type: ChannelType,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_constructs_and_clones() {
        let h = DmHandle {
            user_id: "u".into(),
            platform_id: "p".into(),
            channel_type: ChannelType::new("telegram"),
        };
        let h2 = h.clone();
        assert_eq!(h, h2);
    }

    #[test]
    fn debug_format_includes_user_id() {
        let h = DmHandle {
            user_id: "alice".into(),
            platform_id: "123".into(),
            channel_type: ChannelType::new("slack"),
        };
        assert!(format!("{h:?}").contains("alice"));
    }
}
