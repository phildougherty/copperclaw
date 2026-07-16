//! `copperclaw-browser` — headless-browser core (Phase 5a render + Phase 5b
//! interactive).
//!
//! This crate carries the pure, host-side logic for the headless browser tool.
//! Phase 5a (read-only): render a page and return a screenshot path, the DOM
//! text, or a read-only ARIA snapshot. Phase 5b ([`interactive`], demand-pull,
//! behind a *stricter separate* opt-in flag): a bounded, caller-scripted
//! click / type / scroll / wait-for-selector sequence that returns the
//! post-interaction DOM — an incremental extension of the same live path, NOT
//! an autonomous browsing loop and NOT a memory-writing capability. Both phases
//! tag output [`Provenance::Untrusted`] and re-run the SSRF
//! [`NavigationGuard`] on every navigation (Phase 5b re-guards after every
//! action; see [`interactive::interact`]).
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
//! The **live runtime path** (implemented; environment-dependent, opt-in):
//!   * [`CdpBrowserDriver`] — the concrete Chromium / CDP [`BrowserDriver`],
//!     speaking CDP over a WebSocket ([`WsCdpTransport`]) to the child
//!     container's Chromium. The command sequence is unit-tested against a mock
//!     transport; the live WebSocket needs a real Chromium.
//!   * [`render_live`] — the privileged spawn path: it `spawn`s the child
//!     container spec, resolves its bridge IP, connects a CDP session, drives
//!     the read-only render through the same SSRF [`render`] orchestration, and
//!     tears the container down unconditionally. The spawn/CDP-connect need a
//!     live Docker daemon + Chromium image, so the live path runs only behind
//!     the `COPPERCLAW_BROWSER_ENABLED` opt-in; the spawn→resolve→connect→
//!     render→teardown orchestration is unit-tested against a mock runtime +
//!     mock connector.
//!
//! Default deployments are unaffected: with the tool disabled, nothing here
//! spawns a container or opens a browser.
//!
//! ## M20 D1: the in-container `ui_screenshot` path
//!
//! [`incontainer`] is a THIRD path, distinct from both the above: instead of
//! a dedicated child container reached over Docker, it launches chromium as
//! a **local process inside the session container the runner itself runs
//! in** and speaks CDP to it over loopback — see the module docs for why
//! that's the right shape for a tool that only ever screenshots the agent's
//! own already-running app (never a general browsing capability).
//!
//! ## SSRF guard reuse
//!
//! Navigation is guarded by the [`NavigationGuard`] trait. The production
//! implementation (wired in the `copperclaw-mcp` browser tool) delegates to
//! `copperclaw_mcp::tools::net_guard` so the navigation target AND every
//! redirect hop are classified by the exact same SSRF rules `web_fetch` uses.
//! The trait keeps that reuse testable here without a dependency cycle.

#![forbid(unsafe_code)]

pub mod capture;
pub mod cdp;
pub mod container;
pub mod driver;
pub mod error;
pub mod guard;
pub mod incontainer;
pub mod interactive;
pub mod live;
pub mod render;

pub use crate::capture::{CaptureOptions, DOWNGRADE_JPEG_QUALITY, ImageFormat, ViewportPreset};
pub use crate::cdp::{
    CdpBrowserDriver, CdpTransport, DEFAULT_NAV_TIMEOUT, WsCdpTransport, serialize_ax_tree,
};
pub use crate::container::{
    BrowserContainerParams, BrowserToolConfig, FORBIDDEN_ENV_KEYS, browser_env,
    build_browser_container_spec,
};
pub use crate::driver::{BrowserDriver, DriverRender, Navigation, RenderedArtifact, render};
pub use crate::error::BrowserError;
pub use crate::guard::{GuardResult, NavigationGuard};
pub use crate::incontainer::{
    CHROMIUM_BINARY_CANDIDATES, ChromiumSingleton, DEFAULT_CDP_PORT,
    MAX_WAIT_MS as UI_SCREENSHOT_MAX_WAIT_MS, SIZE_SAFETY_CAP_BYTES, ScreenshotRequest,
    SizeSafeCapture, capture, capture_with_size_safety, find_chromium_binary,
    find_chromium_binary_in, global as chromium_singleton,
};
pub use crate::interactive::{
    InteractRequest, InteractiveAction, InteractiveDriver, MAX_ACTIONS, MAX_TYPE_LEN, interact,
};
pub use crate::live::{
    CdpConnector, LiveRenderOptions, WsCdpConnector, interact_live, render_live,
};
pub use crate::render::{Provenance, RenderMode, RenderOutput, RenderRequest};
