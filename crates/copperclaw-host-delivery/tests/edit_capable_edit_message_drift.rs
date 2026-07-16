//! Edit-capability drift guard (M19 F1).
//!
//! `copperclaw_channels_core::EDIT_CAPABLE_CHANNELS` (surfaced via
//! [`copperclaw_channels_core::edit_capable_channels`]) promises the M18
//! Task HUD that each listed channel's adapter overrides the trait
//! [`copperclaw_channels_core::ChannelAdapter::edit_message`] with a real
//! in-place edit. The delivery dispatcher calls that **trait** method
//! (`dispatch.rs`); when a listed channel forgot to override it, the call
//! falls through to the trait default (`AdapterError::Unsupported`) and the
//! HUD silently never edits — exactly the `matrix`/`webex` lie F1 fixed.
//!
//! Core can't see the adapter crates (they depend on core, not the reverse),
//! so the reality check lives here in `host-delivery` — the crate that
//! *calls* `edit_message` and can read the sibling adapter sources. This is
//! the same spirit as the R0 tool-name-drift guard: pin the static claim to
//! the real impl so the class of lie can't recur. Adding a channel to
//! `EDIT_CAPABLE_CHANNELS` without a trait override fails this test.

use std::path::PathBuf;

/// `crates/copperclaw-channels`, resolved from this crate's manifest dir
/// (`crates/copperclaw-host-delivery`).
fn channels_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("host-delivery manifest dir has a parent (`crates/`)")
        .join("copperclaw-channels")
}

#[test]
fn every_edit_capable_channel_overrides_trait_edit_message() {
    let base = channels_dir();
    for ct in copperclaw_channels_core::capabilities::edit_capable_channels() {
        let src = base.join(ct).join("src").join("adapter.rs");
        let body = std::fs::read_to_string(&src).unwrap_or_else(|e| {
            panic!(
                "edit-capable channel `{ct}` (from EDIT_CAPABLE_CHANNELS) has no \
                 readable adapter at {}: {e} — if the channel dir moved, update this \
                 drift guard",
                src.display(),
            )
        });
        assert!(
            body.contains("async fn edit_message("),
            "channel `{ct}` is in EDIT_CAPABLE_CHANNELS (supports_message_edit == \
             true), so the HUD/approval edit path calls its trait `edit_message` — \
             but {} does not override it. The call would fall through to the trait \
             default (AdapterError::Unsupported) and the HUD would silently never \
             edit. Add the trait override, or drop the channel from \
             EDIT_CAPABLE_CHANNELS.",
            src.display(),
        );
    }
}
