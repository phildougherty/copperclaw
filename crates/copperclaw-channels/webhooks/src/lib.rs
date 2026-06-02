//! `webhooks` — generic HTTP-inbound channel adapter.
//!
//! One configured instance binds an axum HTTP listener; any service that
//! can issue an HTTP POST becomes a message source. The `platform_id`
//! is derived from the URL suffix under the configured base path so a
//! single deployment can route Stripe to one agent, Grafana to another,
//! and a hand-rolled CI hook to a third — all without per-service
//! adapter code.
//!
//! # Ingress
//!
//! - `POST <path>` → emits an [`copperclaw_types::InboundEvent`] with
//!   `channel_type = "webhooks"`, `platform_id = "default"`,
//!   `kind = MessageKind::Webhook`, `content = <JSON body>`.
//! - `POST <path>/<suffix>` → same, but `platform_id = "<suffix>"`.
//!   Sub-segments are preserved (`<path>/stripe/invoices` →
//!   `"stripe/invoices"`).
//! - Body MUST be JSON; non-JSON returns `415 Unsupported Media Type`.
//! - When [`crate::config::WebhooksConfig::secret`] is set, requests
//!   without a matching HMAC-SHA256 signature header are rejected with
//!   `401`. The header name + optional prefix are configurable to
//!   match common provider conventions (GitHub's `sha256=`,
//!   Shopify's plain hex, etc.).
//!
//! # Egress
//!
//! Inbound-only. `deliver` returns
//! [`copperclaw_channels_core::AdapterError::Unsupported`] — there is no
//! reply address bound to a webhook event. Configure a separate
//! outbound channel (resend, slack, etc.) if you want the agent to
//! reach back out.
//!
//! # Why it ships separately
//!
//! Compared to the `OpenClaw`-style "one extension per service" pattern,
//! a single generic webhooks adapter subsumes most of that surface
//! (`Stripe`, `Shopify`, `GitHub Actions`, `Grafana`, `Sentry`,
//! `Vercel`, `IoT` devices) because every modern provider already
//! speaks signed JSON over HTTP POST.

#![forbid(unsafe_code)]

pub mod adapter;
pub mod config;
pub mod factory;
pub mod router;
pub mod signature;

pub use adapter::WebhooksAdapter;
pub use config::{ChannelConfigError, WebhooksConfig};
pub use factory::{CHANNEL_TYPE_STR, WebhooksFactory, register};
pub use router::{WebhooksRouterState, build_router};
pub use signature::{SignatureOutcome, compute_hex, verify};
