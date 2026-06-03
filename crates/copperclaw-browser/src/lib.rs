//! `copperclaw-browser` — headless-browser render core (Phase 5a, read-only).
//!
//! This crate carries the pure, host-side logic for the read-only headless
//! browser tool: render a page and return a screenshot path, the DOM text, or
//! a read-only ARIA snapshot. It is deliberately **read-only** — no
//! interactive click / type / scroll (that is Phase 5b, out of scope).
//!
//! ## What lives here (pure + tested) vs. the runtime path
//!
//! Pure, fully unit-tested:
//!   * [`RenderRequest`] / [`RenderOutput`] / [`RenderMode`] and the
//!     [`Provenance::Untrusted`] tag every render output carries.
//!   * [`render`] orchestration — SSRF pre-flight + per-redirect re-guard +
//!     provenance tagging — driven against a mock [`BrowserDriver`].
//!   * [`build_browser_container_spec`] — the dedicated child-container spec:
//!     **no broker token**, **egress-restricted** (deny-default + narrow
//!     allow-list), **stronger sandbox requested** (gVisor / Kata /
//!     Firecracker, falling back to hardened `runc`).
//!   * [`BrowserToolConfig`] opt-in gating — **OFF by default**.
//!
//! The **runtime path** (deferred, environment-dependent):
//!   * the real Chromium / CDP [`BrowserDriver`] implementation (needs a live
//!     child container + Chromium binary);
//!   * the privileged spawn of the child container with the requested microVM
//!     / gVisor runtime (availability is environment-dependent — see
//!     [`copperclaw_container_rt::select_sandbox_runtime`]).
//!
//! Default deployments are unaffected: with the tool disabled, nothing here
//! spawns a container or opens a browser.
//!
//! ## SSRF guard reuse
//!
//! Navigation is guarded by the [`NavigationGuard`] trait. The production
//! implementation (wired in the `copperclaw-mcp` browser tool) delegates to
//! `copperclaw_mcp::tools::net_guard` so the navigation target AND every
//! redirect hop are classified by the exact same SSRF rules `web_fetch` uses.
//! The trait keeps that reuse testable here without a dependency cycle.

#![forbid(unsafe_code)]

pub mod container;
pub mod driver;
pub mod error;
pub mod guard;
pub mod render;

pub use crate::container::{
    BrowserContainerParams, BrowserToolConfig, FORBIDDEN_ENV_KEYS, browser_env,
    build_browser_container_spec,
};
pub use crate::driver::{BrowserDriver, DriverRender, Navigation, RenderedArtifact, render};
pub use crate::error::BrowserError;
pub use crate::guard::{GuardResult, NavigationGuard};
pub use crate::render::{Provenance, RenderMode, RenderOutput, RenderRequest};
