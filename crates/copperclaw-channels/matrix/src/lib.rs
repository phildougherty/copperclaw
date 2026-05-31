//! Matrix channel adapter — Matrix Client-Server API.
//!
//! Implements [`MatrixFactory`] / [`MatrixAdapter`] backing
//! `ChannelType::new("matrix")`. The adapter:
//!
//! - Runs a `/sync` long-poll background task that translates timeline
//!   events into [`InboundEvent`](copperclaw_types::InboundEvent)s.
//! - Sends text, HTML, threaded replies, edits, reactions, and media via
//!   the Client-Server REST API.
//! - Persists the rolling `next_batch` token to `data_dir/next_batch.txt`
//!   so restarts resume rather than replay.
//!
//! ## Configuration shape
//!
//! ```json
//! {
//!   "homeserver_url": "https://matrix.org",
//!   "access_token":   "<bearer token>",
//!   "user_id":        "@bot:matrix.org",
//!   "rooms":          ["!abc:matrix.org", "#alias:matrix.org"],
//!   "sync_timeout_ms": 30000,
//!   "txn_prefix":     "copperclaw"
//! }
//! ```
//!
//! `homeserver_url`, `access_token`, and `user_id` are required. The
//! channel does *not* perform interactive login — the caller supplies the
//! `access_token` from a prior login.
//!
//! ## Subscribe semantics (v1)
//!
//! `subscribe(platform_id, _)` resolves the room (alias if needed) and
//! adds it to an in-memory "rooms of interest" set used by the next
//! `/sync` filter. Aliases are cached in memory. The running `/sync` call
//! is *not* interrupted; the new filter applies on the next iteration.

pub mod adapter;
pub mod api;
pub mod config;
pub mod factory;
pub mod parse;
pub mod sync;

pub use adapter::MatrixAdapter;
pub use api::MatrixApi;
pub use config::{DEFAULT_SYNC_TIMEOUT_MS, DEFAULT_TXN_PREFIX, MatrixConfig};
pub use factory::{CHANNEL_TYPE_STR, MatrixFactory, register};
