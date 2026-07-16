//! Local, in-container Chromium for the first-party `ui_screenshot` tool
//! (M20 D1 — see `docs/plans/m20-coding-and-design-capability-program.md`
//! decision (a)).
//!
//! [`container.rs`]/[`live.rs`] spawn a DEDICATED CHILD CONTAINER and speak
//! CDP to it over the Docker bridge — that is the host-side
//! `browser_render`/`browser_interact` path, and it needs a Docker socket the
//! in-container runner does not have (by design: the runner IS already
//! inside a container, and there is no Docker-in-Docker wiring). This module
//! is the OTHER path: launch chromium as a **local process inside the
//! session container the runner itself is running in**, and speak CDP to it
//! over loopback. No container spawn, no Docker socket, no host round-trip —
//! the screenshot never leaves the box the page is already running in.
//!
//! ## Why `--headless=new --no-sandbox` is safe here
//!
//! Chromium's OWN internal renderer sandbox (the SUID sandbox / user
//! namespaces it sets up for its renderer processes) typically needs a
//! setuid helper or namespace privileges an already-unprivileged container
//! doesn't have, so it is disabled with `--no-sandbox`. That is safe
//! specifically *because* the session container is itself the sandbox
//! boundary here: the agent already has an arbitrary `shell` tool and this
//! same chromium binary reachable from it (the prototyping image bakes
//! `chromium` — `copperclaw-types::image::PROTOTYPING_APT_PACKAGES`), so a
//! renderer that loses its own internal sandbox does not hand the agent any
//! capability it didn't already have. This tool ALSO refuses any
//! non-loopback navigation target (enforced one layer up, in the
//! `copperclaw-mcp` tool) so the browser this module drives can only ever
//! load a page the agent's own session is already serving — it is not a
//! general browsing capability the way `browser_render`/`browser_interact`
//! are, and it does not touch the container's egress posture.
//!
//! ## What lives here
//!
//!   * Chromium binary discovery ([`find_chromium_binary`]) — the minimal
//!     image profile has no chromium, so the caller probes at call time and
//!     turns `None` into one clean actionable error instead of a crash.
//!   * [`ChromiumSingleton`] — a lazy, idle-reaped local chromium process:
//!     the first `ui_screenshot` call spawns it, later calls in the same
//!     session reuse the warm process (a build loop takes many screenshots
//!     back to back), and a background reaper kills it after
//!     [`IDLE_TIMEOUT`] with no calls so a session that finished building
//!     doesn't hold a browser process open forever.
//!   * [`capture`] — the pure CDP command orchestration for one windowed
//!     screenshot (viewport size → navigate → optional wait → capture),
//!     driven purely against the [`CdpTransport`] seam so it is fully
//!     unit-tested with a mock transport, exactly like [`crate::cdp`]'s own
//!     tests. Deliberately **windowed**, not `captureBeyondViewport`: a
//!     full-page capture of a long page blows the 5 MB `view_image`-class
//!     cap the resulting image block is held to (see
//!     `copperclaw-mcp/src/tools/view_image.rs`). D2 (a later M20 card)
//!     generalises viewport presets / formats / full-page capture across
//!     this AND the host-side render path; this module only needs the one
//!     fixed default D1 requires.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::cdp::CdpTransport;
use crate::error::BrowserError;
use crate::live::{CdpConnector, WsCdpConnector};

/// Chromium-family binary names probed, in order. Debian trixie's `chromium`
/// apt package (the one the prototyping image bakes) installs a binary
/// literally named `chromium`; the aliases cover an operator's own
/// customised base image.
pub const CHROMIUM_BINARY_CANDIDATES: &[&str] =
    &["chromium", "chromium-browser", "google-chrome", "chrome"];

/// Loopback CDP remote-debugging port the in-container singleton listens on.
/// Distinct from the host-side child-container path's 9222 (a wholly
/// separate process, running inside the session container rather than a
/// spawned child container).
pub const DEFAULT_CDP_PORT: u16 = 9223;

/// Default (and D1's only) capture viewport: a typical laptop browser
/// window. Windowed, not full-page — see the module docs.
pub const DEFAULT_WIDTH: u32 = 1280;
pub const DEFAULT_HEIGHT: u32 = 800;

/// Hard cap on an explicit `wait_ms` sleep, so one screenshot call can't
/// turn into an unbounded stall.
pub const MAX_WAIT_MS: u64 = 10_000;

/// Default `wait_for_selector` poll budget when the caller doesn't specify
/// one via `nav_timeout`-adjacent knobs.
const DEFAULT_SELECTOR_TIMEOUT: Duration = Duration::from_secs(10);
const SELECTOR_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long a warm-but-unused chromium process is kept alive before the
/// idle reaper kills it. Long enough that a burst of screenshots inside one
/// see→fix loop (M20 D4) stays warm; short enough that a session that
/// finished building doesn't hold a browser process (and its RAM) forever.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const REAP_INTERVAL: Duration = Duration::from_secs(30);

// ── chromium binary discovery ────────────────────────────────────────────

/// Search `path_var` (a `PATH`-style `:`-joined list, as `OsStr`) for the
/// first of [`CHROMIUM_BINARY_CANDIDATES`] that resolves to an existing
/// file. Split out of [`find_chromium_binary`] so the scan logic is
/// unit-testable without touching the real process environment.
#[must_use]
pub fn find_chromium_binary_in(path_var: &std::ffi::OsStr) -> Option<PathBuf> {
    CHROMIUM_BINARY_CANDIDATES.iter().find_map(|name| {
        std::env::split_paths(path_var).find_map(|dir| {
            let candidate = dir.join(name);
            candidate.is_file().then_some(candidate)
        })
    })
}

/// Probe the live process `PATH` for an installed chromium-family binary.
/// `None` when absent — the minimal image profile's expected, clean-error
/// case (the caller turns this into one actionable message, never a crash).
#[must_use]
pub fn find_chromium_binary() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| find_chromium_binary_in(&p))
}

// ── pure screenshot capture orchestration (CdpTransport seam) ───────────

/// One `ui_screenshot` capture request against an already-connected CDP
/// transport. Pure data — [`capture`]'s orchestration over it is exercised
/// against a mock [`CdpTransport`] with no live chromium.
#[derive(Debug, Clone)]
pub struct ScreenshotRequest {
    /// The (already loopback-validated, by the caller) navigation target.
    pub url: String,
    /// Viewport width in CSS pixels.
    pub width: u32,
    /// Viewport height in CSS pixels.
    pub height: u32,
    /// Extra fixed delay after load before capturing, in milliseconds
    /// (clamped to [`MAX_WAIT_MS`]).
    pub wait_ms: Option<u64>,
    /// Poll until this CSS selector exists before capturing.
    pub wait_for_selector: Option<String>,
    /// Navigation/load timeout.
    pub nav_timeout: Duration,
}

/// Drive one windowed screenshot over `transport`: enable the domains the
/// wait needs, size the viewport, navigate, optionally wait, then capture a
/// PNG clipped to EXACTLY the viewport (never `captureBeyondViewport`).
/// Returns the raw (not base64) PNG bytes.
///
/// Pure CDP-command orchestration against the [`CdpTransport`] seam — no
/// process management here (see [`ChromiumSingleton`] for that), so this is
/// fully unit-tested with a mock transport.
pub async fn capture(
    transport: &dyn CdpTransport,
    req: &ScreenshotRequest,
) -> Result<Vec<u8>, BrowserError> {
    transport.send("Page.enable", json!({})).await?;
    transport.send("Network.enable", json!({})).await?;
    transport
        .send(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": req.width,
                "height": req.height,
                "deviceScaleFactor": 1,
                "mobile": false,
            }),
        )
        .await?;

    let nav = transport
        .send("Page.navigate", json!({ "url": req.url }))
        .await?;
    if let Some(err) = nav.get("errorText").and_then(Value::as_str) {
        if !err.is_empty() {
            return Err(BrowserError::Driver(format!(
                "navigation to `{}` failed: {err}",
                req.url
            )));
        }
    }
    transport.wait_for_load(req.nav_timeout).await?;

    if let Some(selector) = &req.wait_for_selector {
        wait_for_selector(transport, selector, DEFAULT_SELECTOR_TIMEOUT).await?;
    }
    if let Some(ms) = req.wait_ms {
        tokio::time::sleep(Duration::from_millis(ms.min(MAX_WAIT_MS))).await;
    }

    let out = transport
        .send(
            "Page.captureScreenshot",
            json!({ "format": "png", "captureBeyondViewport": false }),
        )
        .await?;
    let b64 = out
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| BrowserError::Driver("Page.captureScreenshot returned no data".into()))?;
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| BrowserError::Driver(format!("screenshot base64 decode: {e}")))
}

/// Poll until an element matching `selector` exists in the page, or
/// `timeout` elapses. Mirrors [`crate::cdp::CdpBrowserDriver`]'s own
/// wait-for-selector helper; kept as an independent (short) copy here so
/// this module has no dependency on `cdp.rs`'s private surface — only the
/// public [`CdpTransport`] seam.
async fn wait_for_selector(
    transport: &dyn CdpTransport,
    selector: &str,
    timeout: Duration,
) -> Result<(), BrowserError> {
    // JSON-encoding the selector into the expression is the same injection
    // guard `cdp.rs` uses: a JSON string is a valid JS string literal, so the
    // selector can never break out of it.
    let literal = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".to_string());
    let expr = format!("!!document.querySelector({literal})");
    let deadline = Instant::now() + timeout;
    loop {
        let out = transport
            .send(
                "Runtime.evaluate",
                json!({ "expression": expr, "returnByValue": true }),
            )
            .await?;
        let present = out
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(Value::as_bool)
            == Some(true);
        if present {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(BrowserError::Driver(format!(
                "wait_for_selector `{selector}` timed out after {timeout:?}"
            )));
        }
        tokio::time::sleep(SELECTOR_POLL_INTERVAL).await;
    }
}

// ── chromium process lifecycle (lazy singleton, idle-reaped) ────────────

/// The chromium CLI args for the in-container singleton. Pulled into a pure
/// function so the flag set (notably `--no-sandbox` — see the module docs
/// for why that's safe here) is unit-tested without spawning a process.
#[must_use]
pub fn chromium_args(port: u16, user_data_dir: &Path) -> Vec<String> {
    vec![
        "--headless=new".to_string(),
        "--no-sandbox".to_string(),
        "--disable-gpu".to_string(),
        "--disable-dev-shm-usage".to_string(),
        "--disable-extensions".to_string(),
        "--no-first-run".to_string(),
        format!("--remote-debugging-port={port}"),
        "--remote-debugging-address=127.0.0.1".to_string(),
        format!("--user-data-dir={}", user_data_dir.display()),
    ]
}

fn spawn_chromium(binary: &Path, port: u16) -> Result<Child, BrowserError> {
    let user_data_dir = std::env::temp_dir().join(format!("copperclaw-ui-screenshot-{port}"));
    Command::new(binary)
        .args(chromium_args(port, &user_data_dir))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| BrowserError::Container(format!("spawn chromium `{}`: {e}", binary.display())))
}

/// Poll `http://127.0.0.1:<port>/json/version` until chromium's CDP HTTP
/// endpoint answers, or `timeout` elapses.
async fn wait_ready(port: u16, timeout: Duration) -> Result<(), BrowserError> {
    let deadline = Instant::now() + timeout;
    let url = format!("http://127.0.0.1:{port}/json/version");
    loop {
        if let Ok(resp) = reqwest::get(&url).await {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(BrowserError::Container(format!(
                "chromium did not become ready on 127.0.0.1:{port} within {timeout:?}"
            )));
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// True when a process last used at `last_used` is idle at `now` given
/// `idle_timeout`. Pure so the reaper's decision is unit-tested without a
/// real timer / process.
fn is_idle(last_used: Instant, now: Instant, idle_timeout: Duration) -> bool {
    now.saturating_duration_since(last_used) >= idle_timeout
}

struct RunningChromium {
    child: Child,
    port: u16,
    last_used: Instant,
}

/// Lazy, idle-reaped chromium process singleton — one per runner process.
/// Since the runner is a single long-lived process per session container,
/// this is naturally "one per session" (decision (a)'s requirement) without
/// any session-id bookkeeping: there is only ever one runner process reading
/// this static per container.
///
/// The first `ui_screenshot` call spawns chromium; later calls in the same
/// session reuse the warm process (opening a fresh CDP tab each time via
/// [`WsCdpConnector`]); a background reaper task kills it after
/// [`IDLE_TIMEOUT`] with no calls.
pub struct ChromiumSingleton {
    state: Mutex<Option<RunningChromium>>,
    reaper_started: AtomicBool,
}

impl Default for ChromiumSingleton {
    fn default() -> Self {
        Self {
            state: Mutex::new(None),
            reaper_started: AtomicBool::new(false),
        }
    }
}

impl ChromiumSingleton {
    /// Ensure a chromium process is running (spawning it — and waiting for
    /// its CDP endpoint to answer — on the first call, or after an idle
    /// reap) and open a fresh CDP tab against it. `binary` is the resolved
    /// chromium executable path (the caller probes for it so a "chromium
    /// missing" error can be tailored per-caller).
    pub async fn get_transport(
        &'static self,
        binary: &Path,
        call_timeout: Duration,
    ) -> Result<Box<dyn CdpTransport>, BrowserError> {
        let port = self.ensure_running(binary).await?;
        self.start_reaper();
        let http_base = format!("http://127.0.0.1:{port}");
        WsCdpConnector::new(call_timeout).connect(&http_base).await
    }

    async fn ensure_running(&self, binary: &Path) -> Result<u16, BrowserError> {
        let mut guard = self.state.lock().await;
        if let Some(running) = guard.as_mut() {
            if matches!(running.child.try_wait(), Ok(None)) {
                // Still alive — reuse it.
                running.last_used = Instant::now();
                return Ok(running.port);
            }
            // Died between calls (e.g. OOM-killed) — fall through and
            // respawn on the same port.
        }
        let port = DEFAULT_CDP_PORT;
        let child = spawn_chromium(binary, port)?;
        wait_ready(port, Duration::from_secs(15)).await?;
        *guard = Some(RunningChromium {
            child,
            port,
            last_used: Instant::now(),
        });
        Ok(port)
    }

    /// Start the background idle reaper the first time this singleton is
    /// used (idempotent — later calls are a no-op).
    fn start_reaper(&'static self) {
        if self.reaper_started.swap(true, Ordering::SeqCst) {
            return;
        }
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(REAP_INTERVAL).await;
                let mut guard = self.state.lock().await;
                let should_reap = guard
                    .as_ref()
                    .is_some_and(|r| is_idle(r.last_used, Instant::now(), IDLE_TIMEOUT));
                if should_reap {
                    if let Some(mut running) = guard.take() {
                        let _ = running.child.start_kill();
                        let _ = running.child.wait().await;
                        tracing::info!(
                            port = running.port,
                            "ui_screenshot: idle-reaped the local chromium process"
                        );
                    }
                }
            }
        });
    }
}

/// The process-wide (== per-session, see [`ChromiumSingleton`]'s docs)
/// chromium singleton.
pub fn global() -> &'static ChromiumSingleton {
    static SINGLETON: std::sync::OnceLock<ChromiumSingleton> = std::sync::OnceLock::new();
    SINGLETON.get_or_init(ChromiumSingleton::default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    // ── chromium binary discovery ─────────────────────────────────────

    #[test]
    fn finds_first_candidate_present_on_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("chromium-browser"), b"#!/bin/sh\n").unwrap();
        let path_var = dir.path().as_os_str().to_owned();
        let found = find_chromium_binary_in(&path_var).unwrap();
        assert_eq!(found, dir.path().join("chromium-browser"));
    }

    #[test]
    fn prefers_earlier_candidate_when_both_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("chromium"), b"").unwrap();
        std::fs::write(dir.path().join("google-chrome"), b"").unwrap();
        let path_var = dir.path().as_os_str().to_owned();
        let found = find_chromium_binary_in(&path_var).unwrap();
        assert_eq!(found, dir.path().join("chromium"));
    }

    #[test]
    fn none_when_no_candidate_on_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("not-a-browser"), b"").unwrap();
        let path_var = dir.path().as_os_str().to_owned();
        assert!(find_chromium_binary_in(&path_var).is_none());
    }

    #[test]
    fn a_directory_named_like_a_candidate_does_not_count() {
        // A directory entry (not a file) must not be mistaken for the binary.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("chromium")).unwrap();
        let path_var = dir.path().as_os_str().to_owned();
        assert!(find_chromium_binary_in(&path_var).is_none());
    }

    // ── chromium args (pure) ────────────────────────────────────────────

    #[test]
    fn args_include_no_sandbox_and_headless_and_port() {
        let dir = Path::new("/tmp/profile-dir");
        let args = chromium_args(9333, dir);
        assert!(args.contains(&"--no-sandbox".to_string()));
        assert!(args.contains(&"--headless=new".to_string()));
        assert!(args.contains(&"--remote-debugging-port=9333".to_string()));
        assert!(args.contains(&"--remote-debugging-address=127.0.0.1".to_string()));
        assert!(args.iter().any(|a| a == "--user-data-dir=/tmp/profile-dir"));
    }

    // ── idle-reap decision (pure) ────────────────────────────────────────

    #[test]
    fn idle_after_timeout_elapses() {
        let last_used = Instant::now();
        let later = last_used + IDLE_TIMEOUT + Duration::from_secs(1);
        assert!(is_idle(last_used, later, IDLE_TIMEOUT));
    }

    #[test]
    fn not_idle_before_timeout_elapses() {
        let last_used = Instant::now();
        let soon = last_used + Duration::from_secs(1);
        assert!(!is_idle(last_used, soon, IDLE_TIMEOUT));
    }

    // ── capture() orchestration against a mock transport ────────────────

    #[derive(Default)]
    struct MockTransport {
        results: HashMap<String, Value>,
        calls: StdMutex<Vec<String>>,
        metrics_calls: StdMutex<Vec<Value>>,
    }

    impl MockTransport {
        fn with(mut self, method: &str, result: Value) -> Self {
            self.results.insert(method.to_string(), result);
            self
        }
    }

    #[async_trait]
    impl CdpTransport for MockTransport {
        async fn send(&self, method: &str, params: Value) -> Result<Value, BrowserError> {
            self.calls.lock().unwrap().push(method.to_string());
            if method == "Emulation.setDeviceMetricsOverride" {
                self.metrics_calls.lock().unwrap().push(params);
            }
            Ok(self
                .results
                .get(method)
                .cloned()
                .unwrap_or_else(|| json!({})))
        }
        async fn wait_for_load(&self, _timeout: Duration) -> Result<(), BrowserError> {
            Ok(())
        }
        fn redirect_hops(&self) -> Vec<String> {
            Vec::new()
        }
        fn main_status(&self) -> Option<u16> {
            Some(200)
        }
    }

    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwC\
AAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    fn req(url: &str) -> ScreenshotRequest {
        ScreenshotRequest {
            url: url.to_string(),
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            wait_ms: None,
            wait_for_selector: None,
            nav_timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn capture_sizes_viewport_and_returns_png_bytes() {
        let transport = MockTransport::default()
            .with("Page.captureScreenshot", json!({ "data": TINY_PNG_B64 }));
        let bytes = capture(&transport, &req("http://127.0.0.1:5173"))
            .await
            .unwrap();
        assert!(bytes.starts_with(b"\x89PNG"));

        let calls = transport.calls.lock().unwrap().clone();
        assert!(calls.contains(&"Emulation.setDeviceMetricsOverride".to_string()));
        assert!(calls.contains(&"Page.navigate".to_string()));
        assert!(calls.contains(&"Page.captureScreenshot".to_string()));

        let metrics = transport.metrics_calls.lock().unwrap().clone();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0]["width"], json!(DEFAULT_WIDTH));
        assert_eq!(metrics[0]["height"], json!(DEFAULT_HEIGHT));
        assert_eq!(metrics[0]["mobile"], json!(false));
    }

    #[tokio::test]
    async fn capture_never_requests_beyond_viewport() {
        // The one D1 acceptance property: never full-page (blows the 5 MB
        // cap). D2 will make this configurable; D1's default must be false.
        struct RecordingParams(StdMutex<Option<Value>>);
        #[async_trait]
        impl CdpTransport for RecordingParams {
            async fn send(&self, method: &str, params: Value) -> Result<Value, BrowserError> {
                if method == "Page.captureScreenshot" {
                    *self.0.lock().unwrap() = Some(params);
                    return Ok(json!({ "data": TINY_PNG_B64 }));
                }
                Ok(json!({}))
            }
            async fn wait_for_load(&self, _timeout: Duration) -> Result<(), BrowserError> {
                Ok(())
            }
            fn redirect_hops(&self) -> Vec<String> {
                Vec::new()
            }
            fn main_status(&self) -> Option<u16> {
                Some(200)
            }
        }
        let recorder = RecordingParams(StdMutex::new(None));
        capture(&recorder, &req("http://127.0.0.1:5173"))
            .await
            .unwrap();
        let params = recorder.0.lock().unwrap().clone().unwrap();
        assert_eq!(params["captureBeyondViewport"], json!(false));
    }

    #[tokio::test]
    async fn capture_surfaces_navigation_error() {
        let transport = MockTransport::default().with(
            "Page.navigate",
            json!({ "errorText": "net::ERR_CONNECTION_REFUSED" }),
        );
        let err = capture(&transport, &req("http://127.0.0.1:5173"))
            .await
            .unwrap_err();
        match err {
            BrowserError::Driver(m) => assert!(m.contains("ERR_CONNECTION_REFUSED"), "{m}"),
            other => panic!("expected driver error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn capture_waits_for_selector_before_capturing() {
        struct SelectorTransport {
            polls: StdMutex<u32>,
        }
        #[async_trait]
        impl CdpTransport for SelectorTransport {
            async fn send(&self, method: &str, _params: Value) -> Result<Value, BrowserError> {
                match method {
                    "Page.captureScreenshot" => Ok(json!({ "data": TINY_PNG_B64 })),
                    "Runtime.evaluate" => {
                        let mut polls = self.polls.lock().unwrap();
                        *polls += 1;
                        // Present only from the second poll onward.
                        Ok(json!({ "result": { "value": *polls >= 2 } }))
                    }
                    _ => Ok(json!({})),
                }
            }
            async fn wait_for_load(&self, _timeout: Duration) -> Result<(), BrowserError> {
                Ok(())
            }
            fn redirect_hops(&self) -> Vec<String> {
                Vec::new()
            }
            fn main_status(&self) -> Option<u16> {
                Some(200)
            }
        }
        let transport = SelectorTransport {
            polls: StdMutex::new(0),
        };
        let mut r = req("http://127.0.0.1:5173");
        r.wait_for_selector = Some("#app".to_string());
        let bytes = capture(&transport, &r).await.unwrap();
        assert!(bytes.starts_with(b"\x89PNG"));
        assert!(*transport.polls.lock().unwrap() >= 2);
    }

    #[tokio::test]
    async fn capture_wait_for_selector_times_out_cleanly() {
        struct NeverPresent;
        #[async_trait]
        impl CdpTransport for NeverPresent {
            async fn send(&self, method: &str, _params: Value) -> Result<Value, BrowserError> {
                match method {
                    "Runtime.evaluate" => Ok(json!({ "result": { "value": false } })),
                    _ => Ok(json!({})),
                }
            }
            async fn wait_for_load(&self, _timeout: Duration) -> Result<(), BrowserError> {
                Ok(())
            }
            fn redirect_hops(&self) -> Vec<String> {
                Vec::new()
            }
            fn main_status(&self) -> Option<u16> {
                Some(200)
            }
        }
        let err = wait_for_selector(&NeverPresent, "#never", Duration::from_millis(250))
            .await
            .unwrap_err();
        assert!(matches!(err, BrowserError::Driver(_)));
    }
}
