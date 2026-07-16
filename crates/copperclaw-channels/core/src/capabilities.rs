//! Static, per-channel-type capability knowledge for out-of-process
//! consumers.
//!
//! The in-container runner (`copperclaw-runner`) is a separate process
//! from the host and never holds a live [`crate::ChannelAdapter`]
//! handle, so it cannot ask an adapter "do you support in-place message
//! edits?" or "is your typing indicator visible on this surface?". The
//! M18 Task HUD needs both answers *before* it decides whether to run
//! the self-editing HUD (rich channels) or fall back to periodic status
//! rows (bare channels) — emitting HUD edits at a channel whose
//! `edit_message` returns `Unsupported` would make the delivery loop
//! degrade every edit into a fresh message, which is exactly the
//! new-message spam the HUD exists to kill.
//!
//! This module is the compiled-in mirror of the adapter trait impls:
//!
//! - [`supports_message_edit`] mirrors "this adapter overrides
//!   [`crate::ChannelAdapter::edit_message`] with a real
//!   implementation" (the trait default returns `Unsupported`).
//! - [`typing_indicator_visible`] mirrors
//!   [`crate::ChannelAdapter::typing_indicator_visible`] — trait
//!   default `true`; Slack overrides it to report the assistant-thread
//!   rule (`assistant.threads.setStatus` only renders inside a thread
//!   in the bot's `D…`-prefixed DM).
//!
//! Keep-in-sync rule: when an adapter gains or loses `edit_message`
//! (or overrides `typing_indicator_visible`), update the matching
//! table here in the same PR. Drift is asymmetric by design: a
//! missing edit-capable entry only degrades that channel's HUD to the
//! safe periodic-status fallback; a stale entry would spam, so the
//! list is kept conservative (only adapters whose `edit_message` is
//! verified in-tree are listed).

/// Channel types whose adapters implement a real
/// [`crate::ChannelAdapter::edit_message`] (an in-place edit API), as
/// verified against the in-tree adapter sources. Everything else falls
/// back to the trait default (`AdapterError::Unsupported`).
const EDIT_CAPABLE_CHANNELS: [&str; 7] = [
    "telegram",
    "slack",
    "discord",
    "matrix",
    "webex",
    "signal",
    "mattermost",
];

/// True when the named channel type's adapter can edit a previously
/// delivered message in place (see module docs for the sync rule).
/// Unknown channel types return `false` — the safe degradation is
/// "no in-place HUD, periodic status rows instead".
#[must_use]
pub fn supports_message_edit(channel_type: &str) -> bool {
    EDIT_CAPABLE_CHANNELS.contains(&channel_type)
}

/// The full edit-capable channel-type list, exposed so out-of-crate
/// drift guards can iterate it. The F1 drift guard
/// (`copperclaw-host-delivery/tests/edit_capable_edit_message_drift.rs`)
/// walks this to assert every listed channel's adapter really overrides
/// the trait `edit_message` — core itself can't see the adapter crates,
/// so the reality check lives in `host-delivery`, which does.
#[must_use]
pub fn edit_capable_channels() -> &'static [&'static str] {
    &EDIT_CAPABLE_CHANNELS
}

/// Static mirror of [`crate::ChannelAdapter::typing_indicator_visible`]
/// for out-of-process consumers.
///
/// Slack renders a typing/status signal only on assistant-thread
/// surfaces — a thread (`thread_id.is_some()`) inside the bot's DM
/// (Slack DM channel ids start with `D`). That predicate mirrors
/// `is_assistant_thread_surface` in the Slack adapter. Every other
/// channel keeps the trait default: `true`.
///
/// Consumers use `false` as "the platform shows no working signal at
/// all", which the Task HUD treats as a reason to force full HUD
/// behaviour and a tighter edit cadence.
#[must_use]
pub fn typing_indicator_visible(
    channel_type: &str,
    platform_id: &str,
    thread_id: Option<&str>,
) -> bool {
    match channel_type {
        "slack" => thread_id.is_some() && platform_id.starts_with('D'),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_capable_channels_match_in_tree_adapters() {
        // The adapters with a real in-place edit impl (see the
        // module-level sync rule). signal (`sendEditMessage`) and
        // mattermost (`PUT /posts/{id}/patch`) were raised to the
        // rich-surface floor in M18-C5.
        for ct in [
            "telegram",
            "slack",
            "discord",
            "matrix",
            "webex",
            "signal",
            "mattermost",
        ] {
            assert!(supports_message_edit(ct), "{ct} implements edit_message");
        }
        // Bare / webhook-ish channels degrade to periodic status rows.
        // whatsapp-cloud is explicitly NOT edit-capable — the Cloud API
        // cannot edit a previously sent message.
        for ct in [
            "cli",
            "webhooks",
            "github",
            "email",
            "whatsapp-cloud",
            "unknown-new-channel",
        ] {
            assert!(!supports_message_edit(ct), "{ct} has no edit_message");
        }
    }

    #[test]
    fn typing_visible_defaults_true_off_slack() {
        assert!(typing_indicator_visible("telegram", "12345", None));
        assert!(typing_indicator_visible("cli", "stdin", None));
        assert!(typing_indicator_visible("discord", "C1", Some("t")));
    }

    #[test]
    fn typing_visible_slack_only_on_assistant_thread_surface() {
        // Mirrors `typing_indicator_visible_only_on_assistant_thread_surface`
        // in the Slack adapter tests.
        assert!(typing_indicator_visible("slack", "D1", Some("99.0")));
        assert!(!typing_indicator_visible("slack", "D1", None));
        assert!(!typing_indicator_visible("slack", "C1", Some("99.0")));
        assert!(!typing_indicator_visible("slack", "C1", None));
        assert!(!typing_indicator_visible("slack", "G1", Some("99.0")));
    }
}
