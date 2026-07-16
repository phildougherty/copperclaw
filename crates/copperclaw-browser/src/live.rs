//! The privileged runtime path: spawn the locked-down browser child container,
//! connect a CDP session to the Chromium inside it, run the read-only render
//! through the SSRF orchestration, and always tear the container down.
//!
//! This is leg (b) of the live-browser work: [`crate::container`] builds the
//! child spec (pure, tested); here we actually `spawn` it via the
//! [`ContainerRuntime`] seam, resolve its bridge IP, and drive it. The spawn +
//! CDP connect need a real Docker daemon + Chromium image, so the live path is
//! exercised only behind the `COPPERCLAW_BROWSER_ENABLED` opt-in; the
//! orchestration itself (spawn → resolve → connect → render → **guaranteed
//! teardown**) is unit-tested against a mock runtime + a mock CDP connector.
//!
//! Every safety property is preserved: [`crate::driver::render`] runs the SSRF
//! target pre-flight and per-redirect re-guard, and the spawned spec is the
//! deny-default-egress, forbidden-env-stripped, unprivileged, hardened-sandbox
//! child from [`crate::build_browser_container_spec`].

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use copperclaw_container_rt::{ContainerRuntime, ContainerSpec};

use crate::cdp::{CdpBrowserDriver, CdpTransport, DEFAULT_NAV_TIMEOUT, WsCdpTransport};
use crate::driver::render as orchestrate;
use crate::error::BrowserError;
use crate::guard::NavigationGuard;
use crate::render::{RenderOutput, RenderRequest};

/// Opens a CDP transport to the child container's Chromium. Abstracted so the
/// live path (a real WebSocket) is swapped for a mock in tests.
#[async_trait]
pub trait CdpConnector: Send + Sync {
    /// Given the container's CDP HTTP base (`http://<ip>:<port>`), open a
    /// page-level CDP session and return the transport.
    async fn connect(&self, http_base: &str) -> Result<Box<dyn CdpTransport>, BrowserError>;
}

/// Tunables for one live render.
#[derive(Debug, Clone)]
pub struct LiveRenderOptions {
    /// Host-side directory screenshots are written into.
    pub screenshot_dir: PathBuf,
    /// The CDP remote-debugging port Chromium listens on inside the child
    /// container.
    pub cdp_port: u16,
    /// Navigation/load timeout.
    pub nav_timeout: Duration,
}

impl Default for LiveRenderOptions {
    fn default() -> Self {
        Self {
            screenshot_dir: std::env::temp_dir().join("copperclaw-browser"),
            cdp_port: 9222,
            nav_timeout: DEFAULT_NAV_TIMEOUT,
        }
    }
}

/// Run one read-only render live: spawn `spec`, connect CDP, render `req`
/// through the SSRF orchestration, and tear the container down unconditionally.
///
/// The caller MUST have already passed the opt-in gate and built `spec` from
/// [`crate::build_browser_container_spec`] (so the deny-default egress,
/// forbidden-env allow-list, unprivileged user, and hardened sandbox are baked
/// in). `guard` is the production `net_guard`-backed [`NavigationGuard`].
pub async fn render_live(
    req: &RenderRequest,
    spec: ContainerSpec,
    guard: &dyn NavigationGuard,
    runtime: &dyn ContainerRuntime,
    connector: &dyn CdpConnector,
    opts: &LiveRenderOptions,
) -> Result<RenderOutput, BrowserError> {
    req.validate()?;
    let name = spec.name.clone();

    // SSRF target pre-flight BEFORE we spend a container spawn on it. The
    // orchestration re-runs this (idempotent classification) and additionally
    // re-guards every redirect hop the page follows.
    guard.guard_target(&req.url).await.map_err(|e| {
        copperclaw_metrics::inc_browser_ssrf_block("target_preflight");
        BrowserError::Blocked(e)
    })?;

    // Spawn the locked-down child container. THIS is the previously-deferred
    // `runtime.spawn` call (leg b).
    match runtime.spawn(spec).await {
        Ok(_) => copperclaw_metrics::inc_browser_child_spawn("ok"),
        Err(e) => {
            copperclaw_metrics::inc_browser_child_spawn("error");
            return Err(BrowserError::Container(format!(
                "spawn browser child `{name}`: {e}"
            )));
        }
    }

    // Everything after the spawn must tear the container down, success or
    // failure — do it via an inner fn whose result we return after cleanup.
    let outcome = render_after_spawn(req, guard, runtime, connector, opts, &name).await;

    // Best-effort teardown; the container is labelled for the orphan sweep, so
    // even a failed remove here is eventually reclaimed.
    match runtime.remove(&name).await {
        Ok(()) => copperclaw_metrics::inc_browser_child_teardown("ok"),
        Err(e) => {
            copperclaw_metrics::inc_browser_child_teardown("error");
            tracing::warn!(container = %name, error = %e, "browser child teardown failed (orphan sweep will reclaim)");
        }
    }

    outcome
}

/// The render steps that run while the child container is up. Split out so
/// [`render_live`] can guarantee teardown regardless of the outcome.
async fn render_after_spawn(
    req: &RenderRequest,
    guard: &dyn NavigationGuard,
    runtime: &dyn ContainerRuntime,
    connector: &dyn CdpConnector,
    opts: &LiveRenderOptions,
    name: &str,
) -> Result<RenderOutput, BrowserError> {
    // Resolve the child's bridge IP — the host reaches Chromium's CDP port
    // directly on the Docker bridge (no published host port).
    let ip = runtime
        .container_ip(name)
        .await
        .map_err(|e| BrowserError::Container(format!("resolve browser child ip: {e}")))?
        .ok_or_else(|| {
            BrowserError::Container(format!(
                "browser child `{name}` has no bridge IP (cannot reach CDP endpoint)"
            ))
        })?;

    let http_base = format!("http://{ip}:{}", opts.cdp_port);
    let transport = connector.connect(&http_base).await.inspect_err(|_e| {
        copperclaw_metrics::inc_browser_cdp_connect_failure();
    })?;
    // M20 D2: `req.capture` defaults to `CaptureOptions::legacy_full_page()`
    // (an omitted wire field), so this stays byte-identical to the pre-D2
    // driver for every existing `browser_render` caller.
    let driver = CdpBrowserDriver::new(transport, opts.screenshot_dir.clone(), opts.nav_timeout)
        .with_capture(req.capture.clone());

    // Reuse the exact SSRF orchestration: target pre-flight + per-redirect
    // re-guard + untrusted provenance tagging.
    let started = std::time::Instant::now();
    let out = orchestrate(req, guard, &driver).await;
    copperclaw_metrics::observe_browser_render_duration_seconds(started.elapsed().as_secs_f64());
    out
}

/// Run one INTERACTIVE session live (Phase 5b): spawn `spec`, connect CDP,
/// drive `req` (navigate + scripted actions + read) through the interactive
/// SSRF orchestration ([`crate::interactive::interact`], which re-guards every
/// navigation), and tear the container down unconditionally.
///
/// Mirrors [`render_live`] exactly on the container lifecycle + safety
/// invariants — the caller MUST have passed the STRICTER interactive opt-in
/// gate (separate from the read-only enable flag) and built `spec` from
/// [`crate::build_browser_container_spec`]. `guard` is the production
/// `net_guard`-backed [`NavigationGuard`].
pub async fn interact_live(
    req: &crate::interactive::InteractRequest,
    spec: ContainerSpec,
    guard: &dyn NavigationGuard,
    runtime: &dyn ContainerRuntime,
    connector: &dyn CdpConnector,
    opts: &LiveRenderOptions,
) -> Result<RenderOutput, BrowserError> {
    req.validate()?;
    let name = spec.name.clone();

    // SSRF pre-flight on the INITIAL target BEFORE we spend a container spawn.
    // The orchestration re-runs this and additionally re-guards the settled URL
    // + every redirect hop after EACH action.
    guard.guard_target(&req.url).await.map_err(|e| {
        copperclaw_metrics::inc_browser_ssrf_block("interactive_target_preflight");
        BrowserError::Blocked(e)
    })?;

    match runtime.spawn(spec).await {
        Ok(_) => copperclaw_metrics::inc_browser_child_spawn("ok"),
        Err(e) => {
            copperclaw_metrics::inc_browser_child_spawn("error");
            return Err(BrowserError::Container(format!(
                "spawn browser child `{name}`: {e}"
            )));
        }
    }

    let outcome = interact_after_spawn(req, guard, runtime, connector, opts, &name).await;

    match runtime.remove(&name).await {
        Ok(()) => copperclaw_metrics::inc_browser_child_teardown("ok"),
        Err(e) => {
            copperclaw_metrics::inc_browser_child_teardown("error");
            tracing::warn!(container = %name, error = %e, "browser child teardown failed (orphan sweep will reclaim)");
        }
    }

    outcome
}

/// The interactive render steps that run while the child container is up. Split
/// out so [`interact_live`] can guarantee teardown regardless of the outcome.
async fn interact_after_spawn(
    req: &crate::interactive::InteractRequest,
    guard: &dyn NavigationGuard,
    runtime: &dyn ContainerRuntime,
    connector: &dyn CdpConnector,
    opts: &LiveRenderOptions,
    name: &str,
) -> Result<RenderOutput, BrowserError> {
    let ip = runtime
        .container_ip(name)
        .await
        .map_err(|e| BrowserError::Container(format!("resolve browser child ip: {e}")))?
        .ok_or_else(|| {
            BrowserError::Container(format!(
                "browser child `{name}` has no bridge IP (cannot reach CDP endpoint)"
            ))
        })?;

    let http_base = format!("http://{ip}:{}", opts.cdp_port);
    let transport = connector.connect(&http_base).await.inspect_err(|_e| {
        copperclaw_metrics::inc_browser_cdp_connect_failure();
    })?;
    let driver = CdpBrowserDriver::new(transport, opts.screenshot_dir.clone(), opts.nav_timeout);

    let started = std::time::Instant::now();
    let out = crate::interactive::interact(req, guard, &driver).await;
    copperclaw_metrics::observe_browser_render_duration_seconds(started.elapsed().as_secs_f64());
    out
}

/// The production [`CdpConnector`]: discovers a page-level CDP WebSocket via the
/// browser's HTTP `/json/new` endpoint, then connects a [`WsCdpTransport`].
pub struct WsCdpConnector {
    /// Per-command response timeout for the opened transport.
    pub call_timeout: Duration,
}

impl Default for WsCdpConnector {
    fn default() -> Self {
        Self {
            call_timeout: DEFAULT_NAV_TIMEOUT,
        }
    }
}

impl WsCdpConnector {
    #[must_use]
    pub fn new(call_timeout: Duration) -> Self {
        Self { call_timeout }
    }
}

#[async_trait]
impl CdpConnector for WsCdpConnector {
    async fn connect(&self, http_base: &str) -> Result<Box<dyn CdpTransport>, BrowserError> {
        let client = reqwest::Client::builder()
            .timeout(self.call_timeout)
            .build()
            .map_err(|e| BrowserError::Container(format!("CDP http client: {e}")))?;

        // `/json/new` opens a fresh tab and returns its `webSocketDebuggerUrl`.
        // Recent Chromium requires PUT; older builds accept GET — try PUT, fall
        // back to GET so both are supported.
        let new_url = format!("{http_base}/json/new");
        let resp = match client.put(&new_url).send().await {
            Ok(r) if r.status().is_success() => r,
            _ => client
                .get(&new_url)
                .send()
                .await
                .map_err(|e| BrowserError::Container(format!("CDP /json/new: {e}")))?,
        };
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| BrowserError::Container(format!("CDP /json/new decode: {e}")))?;
        let ws_url = body
            .get("webSocketDebuggerUrl")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                BrowserError::Container("CDP /json/new returned no webSocketDebuggerUrl".into())
            })?;

        let transport = WsCdpTransport::connect(ws_url, self.call_timeout).await?;
        Ok(Box::new(transport))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;
    use copperclaw_container_rt::{ContainerHandle, ImageBuildSpec, RtError};
    use serde_json::{Value, json};

    use crate::cdp::CdpTransport;
    use crate::container::{
        BrowserContainerParams, BrowserToolConfig, build_browser_container_spec,
    };
    use crate::guard::test_guards::RecordingGuard;
    use crate::render::RenderMode;

    // ── mock container runtime ───────────────────────────────────────────

    #[derive(Default)]
    struct MockRuntime {
        spawned: Mutex<Vec<String>>,
        removed: Mutex<Vec<String>>,
        ip: Option<String>,
        fail_spawn: bool,
    }

    #[async_trait]
    impl ContainerRuntime for MockRuntime {
        async fn ensure_running(&self) -> Result<(), RtError> {
            Ok(())
        }
        async fn cleanup_orphans(&self, _slug: &str) -> Result<(), RtError> {
            Ok(())
        }
        async fn spawn(&self, spec: ContainerSpec) -> Result<ContainerHandle, RtError> {
            if self.fail_spawn {
                return Err(RtError::Container("boom".into()));
            }
            self.spawned.lock().unwrap().push(spec.name.clone());
            Ok(ContainerHandle::new("id-1", spec.name))
        }
        async fn stop(&self, _name: &str, _grace: Duration) -> Result<(), RtError> {
            Ok(())
        }
        async fn remove(&self, name: &str) -> Result<(), RtError> {
            self.removed.lock().unwrap().push(name.to_string());
            Ok(())
        }
        async fn build_image(&self, _spec: ImageBuildSpec) -> Result<String, RtError> {
            Ok("img:test".into())
        }
        async fn container_ip(&self, _name: &str) -> Result<Option<String>, RtError> {
            Ok(self.ip.clone())
        }
    }

    // ── mock CDP transport + connector ───────────────────────────────────

    struct MockTransport {
        results: std::collections::HashMap<String, Value>,
    }

    #[async_trait]
    impl CdpTransport for MockTransport {
        async fn send(&self, method: &str, _params: Value) -> Result<Value, BrowserError> {
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

    struct MockConnector {
        connected: Mutex<Vec<String>>,
        fail: bool,
    }

    #[async_trait]
    impl CdpConnector for MockConnector {
        async fn connect(&self, http_base: &str) -> Result<Box<dyn CdpTransport>, BrowserError> {
            self.connected.lock().unwrap().push(http_base.to_string());
            if self.fail {
                return Err(BrowserError::Container("connect refused".into()));
            }
            let mut results = std::collections::HashMap::new();
            results.insert(
                "Runtime.evaluate".to_string(),
                json!({ "result": { "value": "rendered body" } }),
            );
            Ok(Box::new(MockTransport { results }))
        }
    }

    fn enabled_spec() -> ContainerSpec {
        let cfg =
            BrowserToolConfig::enabled("img:test", copperclaw_container_rt::SandboxRuntime::Runsc);
        let params = BrowserContainerParams {
            name: "copperclaw-browser-sess1",
            install_slug: "slug",
            egress_allow: vec!["example.com:443".into()],
            available_runtimes: &[],
        };
        build_browser_container_spec(&cfg, &params)
    }

    fn req() -> RenderRequest {
        RenderRequest {
            url: "https://example.com".into(),
            mode: RenderMode::DomText,
            timeout_secs: None,
            capture: crate::capture::CaptureOptions::default(),
        }
    }

    fn opts() -> LiveRenderOptions {
        LiveRenderOptions {
            screenshot_dir: std::env::temp_dir().join("copperclaw-browser-live-test"),
            cdp_port: 9222,
            nav_timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn render_live_happy_path_spawns_renders_and_tears_down() {
        let runtime = MockRuntime {
            ip: Some("172.17.0.9".into()),
            ..Default::default()
        };
        let connector = MockConnector {
            connected: Mutex::new(Vec::new()),
            fail: false,
        };
        let guard = RecordingGuard::default();
        let out = render_live(
            &req(),
            enabled_spec(),
            &guard,
            &runtime,
            &connector,
            &opts(),
        )
        .await
        .unwrap();
        assert!(out.is_untrusted());
        assert_eq!(out.text.as_deref(), Some("rendered body"));
        // Spawned once, connected to the resolved bridge IP, torn down once.
        assert_eq!(runtime.spawned.lock().unwrap().len(), 1);
        assert_eq!(
            connector.connected.lock().unwrap()[0],
            "http://172.17.0.9:9222"
        );
        assert_eq!(runtime.removed.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn render_live_tears_down_when_connect_fails() {
        let runtime = MockRuntime {
            ip: Some("172.17.0.9".into()),
            ..Default::default()
        };
        let connector = MockConnector {
            connected: Mutex::new(Vec::new()),
            fail: true,
        };
        let guard = RecordingGuard::default();
        let err = render_live(
            &req(),
            enabled_spec(),
            &guard,
            &runtime,
            &connector,
            &opts(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, BrowserError::Container(_)));
        // Teardown still happened despite the mid-flight failure.
        assert_eq!(runtime.removed.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn render_live_errors_when_no_bridge_ip() {
        let runtime = MockRuntime {
            ip: None,
            ..Default::default()
        };
        let connector = MockConnector {
            connected: Mutex::new(Vec::new()),
            fail: false,
        };
        let guard = RecordingGuard::default();
        let err = render_live(
            &req(),
            enabled_spec(),
            &guard,
            &runtime,
            &connector,
            &opts(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, BrowserError::Container(_)));
        // Never connected, but the spawned container was still cleaned up.
        assert!(connector.connected.lock().unwrap().is_empty());
        assert_eq!(runtime.removed.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn render_live_does_not_teardown_when_spawn_fails() {
        let runtime = MockRuntime {
            fail_spawn: true,
            ..Default::default()
        };
        let connector = MockConnector {
            connected: Mutex::new(Vec::new()),
            fail: false,
        };
        let guard = RecordingGuard::default();
        let err = render_live(
            &req(),
            enabled_spec(),
            &guard,
            &runtime,
            &connector,
            &opts(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, BrowserError::Container(_)));
        // Nothing to tear down — spawn never succeeded.
        assert!(runtime.removed.lock().unwrap().is_empty());
    }

    /// A minimal 1x1 PNG, base64-encoded — what a real `Page.captureScreenshot`
    /// hands back in its `data` field. The driver base64-decodes and writes the
    /// bytes verbatim, so this is enough to prove a PNG lands on disk.
    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwC\
AAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    /// A CDP transport that additionally answers `Page.captureScreenshot` with a
    /// PNG payload, so the screenshot render path writes a real file.
    struct ScreenshotTransport;

    #[async_trait]
    impl CdpTransport for ScreenshotTransport {
        async fn send(&self, method: &str, _params: Value) -> Result<Value, BrowserError> {
            match method {
                "Page.captureScreenshot" => Ok(json!({ "data": TINY_PNG_B64 })),
                // final_url eval + any other command.
                "Runtime.evaluate" => Ok(json!({ "result": { "value": "https://preview.test/" } })),
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

    struct ScreenshotConnector;

    #[async_trait]
    impl CdpConnector for ScreenshotConnector {
        async fn connect(&self, _http_base: &str) -> Result<Box<dyn CdpTransport>, BrowserError> {
            Ok(Box::new(ScreenshotTransport))
        }
    }

    /// End-to-end with the mock driver stack (V3's mock runtime + mock CDP
    /// transport), screenshot mode: spawn → render → a PNG file exists under the
    /// configured output dir (which V4 defaults to the session `/data` dir), and
    /// the render output carries its path. This is the supply the P3 ritual
    /// relays with `send_file`.
    #[tokio::test]
    async fn render_live_screenshot_writes_png_under_output_dir() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        // Stand in for the session `/data/screenshots` dir.
        let out_dir = std::env::temp_dir().join(format!("copperclaw-v4-shot-{nanos}"));

        let runtime = MockRuntime {
            ip: Some("172.17.0.9".into()),
            ..Default::default()
        };
        let guard = RecordingGuard::default();
        let req = RenderRequest {
            url: "http://172.17.0.9:5173/".into(),
            mode: RenderMode::Screenshot,
            timeout_secs: None,
            capture: crate::capture::CaptureOptions::default(),
        };
        let opts = LiveRenderOptions {
            screenshot_dir: out_dir.clone(),
            cdp_port: 9222,
            nav_timeout: Duration::from_secs(5),
        };

        let out = render_live(
            &req,
            enabled_spec(),
            &guard,
            &runtime,
            &ScreenshotConnector,
            &opts,
        )
        .await
        .expect("screenshot render succeeds");

        assert!(out.is_untrusted());
        let path = out
            .screenshot_path
            .as_deref()
            .expect("screenshot mode yields a path");
        let path = std::path::Path::new(path);
        assert!(path.exists(), "PNG must exist at {}", path.display());
        assert!(
            path.starts_with(&out_dir),
            "PNG must land under the configured output dir"
        );
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("png"));
        // The bytes are the decoded PNG, not empty.
        let bytes = std::fs::read(path).expect("read back the PNG");
        assert!(bytes.starts_with(b"\x89PNG"), "wrote a real PNG header");

        // Cleanup.
        let _ = std::fs::remove_dir_all(&out_dir);
    }

    // ── interactive live path (Phase 5b) ────────────────────────────────

    #[tokio::test]
    async fn interact_live_happy_path_spawns_drives_and_tears_down() {
        use crate::interactive::{InteractRequest, InteractiveAction};
        let runtime = MockRuntime {
            ip: Some("172.17.0.9".into()),
            ..Default::default()
        };
        // MockConnector's transport answers Runtime.evaluate with "rendered
        // body" for every eval (location read, click effect, innerText read).
        let connector = MockConnector {
            connected: Mutex::new(Vec::new()),
            fail: false,
        };
        let guard = RecordingGuard::default();
        let req = InteractRequest {
            url: "https://example.com".into(),
            actions: vec![InteractiveAction::Click {
                selector: "#go".into(),
            }],
            mode: RenderMode::DomText,
            timeout_secs: None,
        };
        let out = interact_live(&req, enabled_spec(), &guard, &runtime, &connector, &opts())
            .await
            .unwrap();
        assert!(out.is_untrusted());
        assert_eq!(out.text.as_deref(), Some("rendered body"));
        assert_eq!(runtime.spawned.lock().unwrap().len(), 1);
        assert_eq!(runtime.removed.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn interact_live_blocks_ssrf_target_before_spawning() {
        use crate::interactive::{InteractRequest, InteractiveAction};
        let runtime = MockRuntime {
            ip: Some("172.17.0.9".into()),
            ..Default::default()
        };
        let connector = MockConnector {
            connected: Mutex::new(Vec::new()),
            fail: false,
        };
        let guard = RecordingGuard::blocking(&["169.254.169.254"]);
        let req = InteractRequest {
            url: "http://169.254.169.254/latest/meta-data/".into(),
            actions: vec![InteractiveAction::Click {
                selector: "#go".into(),
            }],
            mode: RenderMode::DomText,
            timeout_secs: None,
        };
        let err = interact_live(&req, enabled_spec(), &guard, &runtime, &connector, &opts())
            .await
            .unwrap_err();
        assert!(matches!(err, BrowserError::Blocked(_)));
        // Never spawned, never connected, nothing to tear down.
        assert!(runtime.spawned.lock().unwrap().is_empty());
        assert!(connector.connected.lock().unwrap().is_empty());
        assert!(runtime.removed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn interact_live_tears_down_when_connect_fails() {
        use crate::interactive::{InteractRequest, InteractiveAction};
        let runtime = MockRuntime {
            ip: Some("172.17.0.9".into()),
            ..Default::default()
        };
        let connector = MockConnector {
            connected: Mutex::new(Vec::new()),
            fail: true,
        };
        let guard = RecordingGuard::default();
        let req = InteractRequest {
            url: "https://example.com".into(),
            actions: vec![InteractiveAction::Click {
                selector: "#go".into(),
            }],
            mode: RenderMode::DomText,
            timeout_secs: None,
        };
        let err = interact_live(&req, enabled_spec(), &guard, &runtime, &connector, &opts())
            .await
            .unwrap_err();
        assert!(matches!(err, BrowserError::Container(_)));
        assert_eq!(runtime.removed.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn render_live_blocks_ssrf_target_before_spawning() {
        // The SSRF target pre-flight runs BEFORE spawn: a refused target never
        // spends a container spawn.
        let runtime = MockRuntime {
            ip: Some("172.17.0.9".into()),
            ..Default::default()
        };
        let connector = MockConnector {
            connected: Mutex::new(Vec::new()),
            fail: false,
        };
        let guard = RecordingGuard::blocking(&["169.254.169.254"]);
        let bad = RenderRequest {
            url: "http://169.254.169.254/latest/meta-data/".into(),
            mode: RenderMode::DomText,
            timeout_secs: None,
            capture: crate::capture::CaptureOptions::default(),
        };
        let err = render_live(&bad, enabled_spec(), &guard, &runtime, &connector, &opts())
            .await
            .unwrap_err();
        assert!(matches!(err, BrowserError::Blocked(_)));
        // Never spawned, never connected, nothing to tear down.
        assert!(runtime.spawned.lock().unwrap().is_empty());
        assert!(connector.connected.lock().unwrap().is_empty());
        assert!(runtime.removed.lock().unwrap().is_empty());
    }
}
