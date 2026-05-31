//! Signal channel adapter — talks to a `signal-cli daemon --json-rpc`
//! subprocess over stdin/stdout.
//!
//! Implements [`SignalFactory`] / [`SignalAdapter`] backing
//! `ChannelType::new("signal")`.
//!
//! ## Configuration shape
//!
//! ```json
//! {
//!   "account":         "+15551234567",
//!   "signal_cli_bin":  "/usr/local/bin/signal-cli",
//!   "extra_args":      ["--config", "/etc/signal-cli"],
//!   "restart_on_exit": true
//! }
//! ```
//!
//! Only `account` is required.
//!
//! ## `platform_id` shapes
//!
//! - `"user:<e164>"` for 1:1 chats (e.g. `"user:+15551234"`).
//! - `"group:<base64>"` for group chats (the base64 group id as signal-cli
//!   reports it).
//!
//! ## Outbound system actions
//!
//! In addition to plain text + attachments, the host may emit `System`
//! messages whose `content` carries an `action` field:
//!
//! - `{ "action": "edit",     "target_id": "<ts>", "text": "..." }`
//! - `{ "action": "reaction", "target_id": "<ts>", "target_author": "+...",
//!      "emoji": "...", "remove": false }`
//! - `{ "action": "delete",   "target_id": "<ts>" }`
//!
//! `target_id` is the original message's `targetSentTimestamp` as either an
//! integer or string.
//!
//! ## Error mapping
//!
//! signal-cli emits JSON-RPC errors of the form `{"code": <int>,
//! "message": "..."}`. We map:
//!
//! - `-1` (`AuthorizationFailedException`, `MissingTokenException`)
//!   -> [`AdapterError::Auth`]
//! - `-3` (`RateLimitException`) -> [`AdapterError::Rate { retry_after: None }`]
//! - any other code -> [`AdapterError::BadRequest`]
//! - subprocess spawn/IO failure, stdin write failure, unparseable stdout
//!   line -> [`AdapterError::Transport`]
//!
//! [`AdapterError`]: copperclaw_channels_core::AdapterError

pub mod adapter;
pub mod api;
pub mod config;
pub mod factory;
pub mod parse;
pub mod rpc;

pub use adapter::SignalAdapter;
pub use config::{DEFAULT_RESTART_ON_EXIT, DEFAULT_SIGNAL_CLI_BIN, SignalConfig};
pub use factory::{CHANNEL_TYPE_STR, SignalFactory, register};
