//! Delta Chat channel adapter — talks to the `deltachat-rpc-server`
//! subprocess via stdio JSON-RPC.
//!
//! ## Overview
//!
//! [`DeltaChatFactory`] / [`DeltaChatAdapter`] back
//! `ChannelType::new("deltachat")`. The adapter:
//!
//! - Spawns (or wraps) a [`rpc::RpcTransport`] that speaks JSON-RPC 2.0 to
//!   `deltachat-rpc-server` over stdio.
//! - Runs a background forwarder task that polls `get_next_event` and turns
//!   `IncomingMsg` events into [`InboundEvent`](ironclaw_types::InboundEvent)s.
//! - Sends text and file messages via `send_msg`, reactions via
//!   `send_reaction`, and deletes via `delete_messages`.
//!
//! ## Configuration shape
//!
//! ```json
//! {
//!   "account_id": 1,
//!   "rpc_server_bin": "deltachat-rpc-server",
//!   "extra_args": [],
//!   "event_poll_ms": 200,
//!   "attachment_download": true,
//!   "blob_dir": null,
//!   "max_attachment_bytes": 52428800
//! }
//! ```
//!
//! `account_id` is required. The other fields default to the values shown
//! above.
//!
//! ## Account setup (v1)
//!
//! `deltachat-rpc-server` requires the underlying e-mail account to be
//! configured out-of-band (typically by running the binary once with
//! `--set-config` to set `addr` / `mail_pw` etc.) before this adapter is
//! started. v1 does **not** perform account creation or credential setup;
//! if the configured `account_id` is missing the adapter returns
//! [`AdapterError::BadRequest`] at init time.
//!
//! ## Inbound attachments
//!
//! Incoming messages whose `file` field points at a blob path go through
//! the full-download path: the adapter calls `download_full_msg` when the
//! message's `download_state` is anything other than `"Done"`, re-fetches
//! the message, and then verifies the resulting blob is readable. The
//! event is surfaced as [`MessageKind::Chat`](ironclaw_types::MessageKind::Chat)
//! with `content.attachment` carrying `{path, bytes_path, filename, mime,
//! size, view_type}`.
//!
//! Two knobs gate the behaviour:
//!
//! - `attachment_download` (default `true`) — when `false`, the adapter
//!   keeps the legacy behaviour of surfacing the raw blob `path` without
//!   running `download_full_msg` or reading the file off disk.
//! - `blob_dir` (default unset) — when set, blobs are read from
//!   `<blob_dir>/<basename(file)>`; useful when the agent container does
//!   not share the deltachat blob store with the host. When unset, the
//!   server-reported path is used verbatim.
//! - `max_attachment_bytes` (default 50 MiB) — files exceeding this cap
//!   (either by the server-reported `file_bytes` or by the on-disk size)
//!   are surfaced as [`MessageKind::System`](ironclaw_types::MessageKind::System)
//!   with `reason: "too_large"`. The same fallback applies when
//!   `download_full_msg`, `stat`, or `open` fails; the underlying error
//!   is captured under `content.attachment.error`.
//!
//! ## `platform_id` shape
//!
//! `"account/<account_id>/chat/<chat_id>"` — the adapter parses this
//! shape on `deliver` and emits it on every inbound event.
//!
//! ## Subprocess isolation
//!
//! All RPC traffic goes through the [`rpc::RpcTransport`] trait, so tests
//! exercise the adapter against a deterministic [`rpc::MockTransport`]
//! instead of spawning `deltachat-rpc-server`.

pub mod adapter;
pub mod api;
pub mod config;
pub mod factory;
pub mod parse;
pub mod rpc;

pub use adapter::DeltaChatAdapter;
pub use config::{
    DEFAULT_EVENT_POLL_MS, DEFAULT_MAX_ATTACHMENT_BYTES, DEFAULT_RPC_SERVER_BIN, DeltaChatConfig,
};
pub use factory::{CHANNEL_TYPE_STR, DeltaChatFactory, register};
