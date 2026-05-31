//! `WeChat` Work (Work Weixin / Enterprise `WeChat`) channel adapter.
//!
//! Implements the M8 wechat channel for copperclaw against the documented
//! `WeChat` Work REST + webhook API set. This is the enterprise (corporate)
//! API surface — not the consumer-facing `WeChat` product, which has no
//! supported integration API.
//!
//! # Auth model
//!
//! Operator-configured credentials:
//!
//! - `corp_id` — the company tenant id.
//! - `corp_secret` — the per-app secret used to mint access tokens.
//! - `agent_id` — the numeric app id within the corp.
//! - `token` + `encoding_aes_key` — used to verify and decrypt inbound
//!   webhook callbacks. The AES key is a 43-character base64 string
//!   (decoded to 32 raw bytes).
//!
//! Access tokens are obtained via `GET /cgi-bin/gettoken` and cached in
//! memory until their `expires_in` window elapses (typically 7200s). The
//! [`api::TokenStore`] handles refresh on demand.
//!
//! # `platform_id` shape
//!
//! Three shapes are recognised for outbound delivery. Anything else is
//! `AdapterError::BadRequest`:
//!
//! | Prefix | Meaning | Mapped to |
//! |---|---|---|
//! | `user:<userid>` | 1-on-1 DM-style send to a corp user | `touser` |
//! | `party:<partyid>` | Send to a department / org unit | `toparty` |
//! | `tag:<tagid>` | Send to a labelled cohort | `totag` |
//!
//! Inbound `platform_id` is always `user:<FromUserName>` because Work
//! Weixin's callback only delivers DM-style events to the agent.
//!
//! # Outbound coverage (v1)
//!
//! - Text → `POST /cgi-bin/message/send` with `msgtype:"text"`.
//! - Files → upload via `POST /cgi-bin/media/upload?type=image|file` to
//!   obtain a `media_id`, then send `msgtype:"image"` or `msgtype:"file"`
//!   keyed by inferred MIME family.
//! - System action `edit` / `reaction` → `AdapterError::Unsupported`
//!   (Work Weixin has no message-edit or reaction endpoint).
//! - Cards (`template_card`) — a thin pass-through path is provided but
//!   not required to be exercised by the agent surface in v1.
//!
//! `set_typing` and `subscribe` are no-ops — Work Weixin exposes neither.
//!
//! # Inbound coverage (v1)
//!
//! Encrypted XML POSTs are signature-verified ([`signature`]) then AES
//! decrypted ([`signature::decrypt_payload`]) and parsed ([`parse`]).
//! Surfaced event kinds:
//!
//! - `text` → `MessageKind::Chat` with `{"text": "..."}`.
//! - `image` / `voice` / `video` / `file` → `MessageKind::Chat` with
//!   `{"text": "", "attachment": {...}}`.
//! - `event` → `MessageKind::System` with the event payload as content.
//!
//! `MsgId` is used for duplicate suppression via a small LRU ring.
//!
//! # Signature, not HMAC
//!
//! Work Weixin's webhook signature is **not** HMAC. It is a SHA1 over the
//! sorted-and-concatenated `[token, timestamp, nonce, encrypted_text]`
//! quartet. See [`signature::compute_msg_signature`]; callers grep for
//! HMAC and won't find any here.
//!
//! # Error mapping
//!
//! Work Weixin returns errors as `{"errcode": N, "errmsg": "..."}`. The
//! HTTP status is almost always 200 — error details ride in the JSON
//! envelope. We map:
//!
//! | `errcode` | Result |
//! |---|---|
//! | `40014` / `40001` / `40082` | `AdapterError::Auth` |
//! | `42001` / `42007` / `42009` | `AdapterError::Auth` (token expired) |
//! | `45009` / `45033` | `AdapterError::Rate { retry_after: None }` |
//! | `40036` / `40068` | `AdapterError::BadRequest` (bad agentid / signature) |
//! | other `>= 40000` | `AdapterError::BadRequest` |
//!
//! HTTP `401` / `403` → `Auth`, `429` → `Rate`, `4xx` → `BadRequest`,
//! `5xx` → `Transport`.

pub mod adapter;
pub mod api;
pub mod config;
pub mod events;
pub mod factory;
pub mod parse;
pub mod signature;

pub use adapter::WeChatAdapter;
pub use api::{TokenStore, WeChatApi};
pub use config::{WeChatConfig, WebhookConfig};
pub use events::router::{EventDedup, WeChatEventsState, build_events_router};
pub use factory::{CHANNEL_TYPE_STR, WeChatFactory, register};
pub use parse::{InboundXml, MsgType, parse_inbound_xml};
pub use signature::{SignatureError, compute_msg_signature, decrypt_payload, verify_msg_signature};
