//! Shared markdown → per-platform renderer (M18/C5b).
//!
//! One place for two jobs every channel adapter otherwise duplicates:
//!
//! - [`render`] turns the agent's canonical Markdown into each platform's
//!   flavor ([`Flavor`]) — Telegram HTML, Slack `mrkdwn`,
//!   Discord/Mattermost `CommonMark`, `WhatsApp`'s `*bold*`, Signal
//!   plaintext. Adapters migrate onto it over time, replacing their
//!   bespoke formatters (telegram `markdown_to_html`, slack mrkdwn,
//!   discord escaping).
//! - [`effective_max`] shrinks a channel's declared cap by a render-
//!   headroom margin, because adapters render *after* the host splits and
//!   rendering only grows the string.
//! - [`split_into_chunks`] / [`is_balanced`] do fence-aware chunking of a
//!   long reply into cap-sized pieces that each parse with balanced code
//!   fences. This logic was migrated here from
//!   `copperclaw-host-delivery` (C2's `fence.rs` + splitter) so the
//!   renderer owns fences end-to-end; the delivery loop now consumes it.

mod fence;
mod render;
mod split;

pub use fence::is_balanced;
pub use render::{Flavor, render};
pub use split::{
    RENDER_HEADROOM_PERCENT, effective_max, split_into_chunks, split_into_chunks_within_bytes,
};
