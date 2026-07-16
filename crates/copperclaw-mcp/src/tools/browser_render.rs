//! `browser_render`: render a web page in a headless browser, read-only.
//!
//! Phase 5a, **read-only subset**: navigate to a URL in a headless Chromium
//! and return either a screenshot path, the page's DOM text, or a read-only
//! ARIA snapshot. There is NO interactive click / type / scroll — that is
//! Phase 5b and out of scope.
//!
//! ## Security posture (what this tool enforces)
//!
//!   * **Opt-in, OFF by default.** The tool runs only when the operator sets
//!     `COPPERCLAW_BROWSER_ENABLED=1` (+ optionally an image / sandbox runtime).
//!     With it unset, the tool returns a clear "disabled" validation error and
//!     never touches the network or spawns a container — default deployments
//!     are unchanged.
//!   * **SSRF guard reuse.** The navigation target is pre-flighted through
//!     [`crate::tools::net_guard::guard_url`] (the same classifier `web_fetch`
//!     uses) before anything connects, and a [`NavGuard`] backed by
//!     [`crate::tools::net_guard::guard_redirect_url`] re-checks every redirect
//!     hop. Loopback / link-local (incl. `169.254.169.254`) / RFC1918 /
//!     unique-local targets are rejected.
//!   * **Untrusted provenance.** A rendered page is external,
//!     attacker-influenceable content. The turn is tagged untrusted up front
//!     via [`ToolContext::mark_untrusted_context`] (the Wave-4 `TurnTrust`
//!     path) so the runner's coarse gate blocks credentialed external actions
//!     for the rest of the turn absent fresh approval.
//!   * **Dedicated, locked-down child container.** The render runs in a
//!     dedicated child container with NO broker token, egress restricted to
//!     the navigation target, and a stronger-sandbox runtime requested
//!     ([`copperclaw_browser::build_browser_container_spec`]).
//!
//! ## Enforced here vs. the live runtime path
//!
//! Enforced + tested at this layer: opt-in gating, the SSRF pre-flight + the
//! per-redirect [`NavGuard`], the untrusted tagging, and construction of the
//! locked-down child-container spec. **Live runtime path** (behind the opt-in
//! gate): [`handle`] detects a container runtime and drives
//! [`copperclaw_browser::render_live`], which spawns the locked-down child,
//! connects a CDP session to the Chromium inside it, and renders read-only. A
//! real headless browser + Docker daemon is environment-dependent; where none
//! is reachable (e.g. the in-container runner, which has no Docker socket by
//! design), the tool reports unavailable cleanly after running every safety
//! step — it never fabricates page content and never panics.

use crate::context::ToolContext;
use crate::error::ToolError;
use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args, success_json};
use copperclaw_browser::{
    BrowserContainerParams, BrowserError, BrowserToolConfig, LiveRenderOptions, NavigationGuard,
    RenderMode, RenderRequest, WsCdpConnector,
};
use copperclaw_container_rt::SandboxRuntime;
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::Deserialize;
use serde_json::json;

/// Env var that opts the browser tool in. Unset / not truthy → disabled.
const ENABLE_ENV: &str = "COPPERCLAW_BROWSER_ENABLED";
/// Optional override for the child-container image.
const IMAGE_ENV: &str = "COPPERCLAW_BROWSER_IMAGE";
/// Optional override for the requested sandbox runtime
/// (`runsc` | `kata` | `firecracker` | `hardened-runc`).
const RUNTIME_ENV: &str = "COPPERCLAW_BROWSER_SANDBOX";
/// Host-supplied extra egress allow-list for the browser child (V4). A
/// comma-separated list of `host:port` entries the locked-down child must be
/// able to reach IN ADDITION to the navigation target, under deny-default
/// egress. The host sets this at browser-child spawn from the live
/// `PreviewEntry` (`container_ip:container_port`) so the child can reach the
/// prototype's own session container directly on the Docker bridge — see the
/// render-target rationale in [`prepare`]. Unset / empty → no extra entries,
/// byte-identical to pre-V4.
const PREVIEW_ALLOW_ENV: &str = "COPPERCLAW_BROWSER_PREVIEW_ALLOW";

/// Lookup table for environment variables. Production uses [`SystemEnv`];
/// tests build a [`MapEnv`]. (Mirrors the `web_search` pattern.)
pub trait EnvLookup: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
}

/// Production env lookup, backed by `std::env::var`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemEnv;

impl EnvLookup for SystemEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.is_empty())
    }
}

/// In-memory env, used by tests.
#[derive(Debug, Default, Clone)]
pub struct MapEnv(pub std::collections::HashMap<String, String>);

impl MapEnv {
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self(
            pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }
}

impl EnvLookup for MapEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}

/// Parse a truthy opt-in value (`1` / `true` / `on` / `yes`, case-insensitive).
fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "on" | "yes"
    )
}

/// Parse the requested sandbox runtime from the env override. Defaults to
/// gVisor (`runsc`), which degrades to hardened-runc when unavailable.
fn parse_runtime(raw: Option<&str>) -> SandboxRuntime {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("kata") => SandboxRuntime::Kata,
        Some("firecracker" | "fc") => SandboxRuntime::Firecracker,
        Some("hardened-runc" | "runc" | "hardened") => SandboxRuntime::HardenedRunc,
        // Default + explicit "runsc"/"gvisor".
        _ => SandboxRuntime::Runsc,
    }
}

/// Resolve the [`BrowserToolConfig`] from the ambient env. Disabled unless the
/// opt-in env is truthy.
pub fn resolve_config(env: &dyn EnvLookup) -> BrowserToolConfig {
    let enabled = env.get(ENABLE_ENV).is_some_and(|v| truthy(&v));
    if !enabled {
        return BrowserToolConfig::default(); // disabled
    }
    let image = env
        .get(IMAGE_ENV)
        .unwrap_or_else(|| "copperclaw-browser:latest".to_string());
    let runtime = parse_runtime(env.get(RUNTIME_ENV).as_deref());
    BrowserToolConfig::enabled(image, runtime)
}

/// A [`NavigationGuard`] backed by the existing `net_guard` SSRF classifier.
/// This is the concrete reuse of `web_fetch`'s guard for browser navigation.
pub struct NavGuard;

#[async_trait::async_trait]
impl NavigationGuard for NavGuard {
    async fn guard_target(&self, url: &str) -> Result<(), String> {
        crate::tools::net_guard::guard_url(url)
            .await
            .map_err(|e| e.to_string())
    }

    fn guard_redirect(&self, url: &str) -> Result<(), String> {
        crate::tools::net_guard::guard_redirect_url(url).map_err(|e| e.to_string())
    }
}

/// Decoded tool input.
#[derive(Debug, Clone, Deserialize)]
pub struct Input {
    pub url: String,
    /// `screenshot` | `dom_text` | `aria_snapshot`. Defaults to `dom_text`.
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

fn parse_mode(raw: Option<&str>) -> Result<RenderMode, ToolError> {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        None | Some("" | "dom_text" | "text") => Ok(RenderMode::DomText),
        Some("screenshot" | "png") => Ok(RenderMode::Screenshot),
        Some("aria_snapshot" | "aria" | "a11y") => Ok(RenderMode::AriaSnapshot),
        Some(other) => Err(ToolError::Validation(format!(
            "browser_render: unknown mode `{other}` \
             (expected screenshot | dom_text | aria_snapshot)"
        ))),
    }
}

/// Build the navigation-target egress allow-list (`host:port`) for the child
/// container from the request URL. The child gets exactly this one entry —
/// nothing broader. Returns a validation error for a URL with no host.
fn egress_allow_for(url: &str) -> Result<Vec<String>, ToolError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| ToolError::Validation(format!("browser_render: invalid url: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| ToolError::Validation("browser_render: url has no host".to_string()))?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    Ok(vec![format!("{host}:{port}")])
}

/// True for a well-formed `host:port` allow-list entry: a non-empty host and a
/// port that parses as a `u16`. Deliberately strict — a malformed entry is
/// dropped rather than widening the child's deny-default egress by accident.
/// IPv4 literals + DNS names only (the preview target is always an IPv4 bridge
/// address), so a single trailing `:port` split is sufficient.
fn is_host_port(s: &str) -> bool {
    match s.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && port.parse::<u16>().is_ok(),
        None => false,
    }
}

/// Parse the host-supplied preview egress allow-list from [`PREVIEW_ALLOW_ENV`].
/// Comma-separated `host:port` entries; blank / malformed entries are dropped.
/// Unset → empty (byte-identical to pre-V4). This is the V4 injection: the host
/// scopes the browser child's deny-default egress to the prototype's own
/// preview origin so the screenshot render can actually reach it.
fn preview_egress_allow(env: &dyn EnvLookup) -> Vec<String> {
    env.get(PREVIEW_ALLOW_ENV)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty() && is_host_port(s))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The safety-critical, fully-testable core: gate on opt-in, validate, tag the
/// turn untrusted, run the SSRF pre-flight, and build the locked-down
/// child-container spec. Returns the prepared request + the child spec the
/// runtime path would spawn. Driving the live browser is the deferred step.
pub async fn prepare(
    input: Input,
    env: &dyn EnvLookup,
    guard: &dyn NavigationGuard,
    ctx: &dyn ToolContext,
) -> Result<Prepared, ToolError> {
    // 1. Opt-in gate. OFF by default → hard validation error, no side effects.
    let cfg = resolve_config(env);
    cfg.ensure_enabled()
        .map_err(|e| ToolError::Validation(e.to_string()))?;

    // 2. Provenance: a rendered page is untrusted external content. Tag the
    //    turn up front (before any content could land in history) so the
    //    coarse gate blocks credentialed external actions this turn.
    ctx.mark_untrusted_context(&format!("browser_render:{}", input.url));

    // 3. Validate shape.
    let mode = parse_mode(input.mode.as_deref())?;
    let req = RenderRequest {
        url: input.url.clone(),
        mode,
        timeout_secs: input.timeout_secs,
    };
    req.validate()
        .map_err(|e| ToolError::Validation(e.to_string()))?;

    // 4. SSRF pre-flight on the navigation target (reuses net_guard).
    guard
        .guard_target(&req.url)
        .await
        .map_err(ToolError::Validation)?;

    // 5. Build the locked-down child-container spec (no broker token, egress
    //    restricted to the target, stronger sandbox requested). The host
    //    probes available sandbox runtimes; at this layer we pass an empty set
    //    so the spec resolves to the hardened-runc floor deterministically —
    //    the host's spawn path supplies the real availability probe.
    //
    //    V4 render-target decision: the P3 "prototype ready" screenshot renders
    //    the live preview by pointing the child at the prototype's OWN session
    //    container directly on the Docker bridge
    //    (`http://<container_ip>:<container_port>`) — NOT the host preview-proxy
    //    URL. The proxy (V1) binds a host port and 403s any request lacking the
    //    `cclaw_preview` cookie (minted only for the human user's external
    //    browser), so a cookieless automated render against the proxy URL would
    //    be refused; and the child + the app container already share the Docker
    //    bridge, so the direct hop needs no host round-trip. The host passes
    //    that `container_ip:container_port` (read from the live `PreviewEntry`)
    //    via `PREVIEW_ALLOW_ENV`; we inject it into the deny-default allow-list
    //    here so the child can reach it. Unset → target-only (pre-V4).
    let mut egress_allow = egress_allow_for(&req.url)?;
    for extra in preview_egress_allow(env) {
        if !egress_allow.contains(&extra) {
            egress_allow.push(extra);
        }
    }
    let params = BrowserContainerParams {
        name: "copperclaw-browser",
        install_slug: "copperclaw",
        egress_allow,
        available_runtimes: &[],
    };
    let spec = copperclaw_browser::build_browser_container_spec(&cfg, &params);

    Ok(Prepared { req, spec })
}

/// Output of [`prepare`]: the validated request + the child-container spec the
/// runtime path would spawn.
#[derive(Debug)]
pub struct Prepared {
    pub req: RenderRequest,
    pub spec: copperclaw_container_rt::ContainerSpec,
}

pub fn schema() -> Tool {
    make_tool(
        "browser_render",
        "Render a web page in a headless browser (READ-ONLY: no clicking/typing). \
         Returns a screenshot path, the page DOM text, or a read-only ARIA snapshot. \
         Opt-in and OFF by default; the operator must enable it. Output is treated as \
         UNTRUSTED external content. The navigation target and every redirect are \
         SSRF-guarded (internal/metadata addresses are refused).",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["url"],
            "properties": {
                "url":          { "type": "string", "minLength": 1 },
                "mode":         { "type": ["string", "null"], "enum": ["screenshot", "dom_text", "aria_snapshot", null] },
                "timeout_secs": { "type": ["integer", "null"], "minimum": 1, "maximum": 120 }
            }
        }),
    )
}

/// Optional override for where rendered screenshots are written.
/// V4: defaults to `<data_root>/screenshots` (the session `/data` dir in
/// production) so the in-container agent can read the PNG and relay it with
/// `send_file` for the P3 ritual. An explicit value still wins.
const OUTPUT_DIR_ENV: &str = "COPPERCLAW_BROWSER_OUTPUT_DIR";
/// Subdirectory under the session data root where screenshots land by default.
const SCREENSHOT_SUBDIR: &str = "screenshots";

/// Resolve the directory rendered screenshots are written into. An explicit
/// [`OUTPUT_DIR_ENV`] wins; otherwise (V4) they land under the session data
/// root — `/data/screenshots` in production — so the PNG is readable by the
/// in-container agent for `send_file`. Reuses the shared,
/// `COPPERCLAW_DATA_ROOT`-aware [`crate::tools::verify_gate::data_root`] so a
/// host/test override of the data root moves screenshots with it.
fn screenshot_dir(env: &dyn EnvLookup) -> std::path::PathBuf {
    env.get(OUTPUT_DIR_ENV).map_or_else(
        || crate::tools::verify_gate::data_root().join(SCREENSHOT_SUBDIR),
        std::path::PathBuf::from,
    )
}

pub async fn handle(
    arguments: Option<JsonObject>,
    ctx: &dyn ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;
    let env = SystemEnv;
    let guard = NavGuard;
    // `prepare` runs every safety step (opt-in gate, untrusted tagging, SSRF
    // target pre-flight, locked-down child spec). With the tool disabled it
    // returns the byte-identical "disabled" validation error before any of the
    // live path below is reached.
    let prepared = prepare(input, &env, &guard, ctx).await?;
    render_prepared_live(prepared, &guard, &env).await
}

/// Drive the live render for a fully-prepared request: detect a container
/// runtime, spawn the locked-down browser child, and render through the SSRF
/// orchestration. When no container runtime is reachable (e.g. the in-container
/// runner, which has no Docker socket by design), report unavailable cleanly —
/// never a panic, and every safety step already ran in [`prepare`].
async fn render_prepared_live(
    prepared: Prepared,
    guard: &dyn NavigationGuard,
    env: &dyn EnvLookup,
) -> Result<CallToolResult, ToolError> {
    let runtime = match copperclaw_container_rt::detect().await {
        Ok(rt) => rt,
        Err(e) => {
            return Err(ToolError::Internal(format!(
                "browser_render: navigation target `{}` passed the SSRF + opt-in checks and a \
                 locked-down child-container spec was constructed (egress={:?}), but no container \
                 runtime is reachable here to spawn the headless-browser child ({e}). The browser \
                 render runs where a Docker daemon is available.",
                prepared.req.url, prepared.spec.egress_allow,
            )));
        }
    };

    let nav_timeout =
        std::time::Duration::from_secs(prepared.req.timeout_secs.unwrap_or(30).clamp(1, 120));
    let opts = LiveRenderOptions {
        screenshot_dir: screenshot_dir(env),
        cdp_port: 9222,
        nav_timeout,
    };
    let connector = WsCdpConnector::new(nav_timeout);

    match copperclaw_browser::render_live(
        &prepared.req,
        prepared.spec,
        guard,
        runtime.as_ref(),
        &connector,
        &opts,
    )
    .await
    {
        Ok(out) => Ok(success_json(&out)),
        // A redirect that lands in a blocked (internal) range is a user-visible
        // refusal, not an internal fault.
        Err(BrowserError::Blocked(m)) => Err(ToolError::Validation(m)),
        Err(e) => Err(ToolError::Internal(format!("browser_render: {e}"))),
    }
}

struct Handler;

#[async_trait::async_trait]
impl ToolHandler for Handler {
    async fn call(
        &self,
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        handle(arguments, ctx).await
    }
}

pub fn entry() -> ToolEntry {
    ToolEntry {
        tool: schema(),
        handler: Box::new(Handler),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn env(pairs: &[(&str, &str)]) -> MapEnv {
        MapEnv::from_pairs(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
        )
    }

    /// Records `mark_untrusted_context` calls so the provenance test can
    /// assert the turn was tainted.
    #[derive(Default)]
    struct TaintRecordingCtx {
        sources: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl ToolContext for TaintRecordingCtx {
        async fn emit_outbound(
            &self,
            _e: crate::context::OutboundToolEffect,
        ) -> Result<crate::context::ToolEffectAck, ToolError> {
            Ok(crate::context::ToolEffectAck::Accepted)
        }
        async fn list_tasks(&self) -> Result<Vec<crate::context::TaskSummary>, ToolError> {
            Ok(Vec::new())
        }
        fn mark_untrusted_context(&self, source: &str) {
            self.sources.lock().unwrap().push(source.to_string());
        }
    }

    fn input(url: &str, mode: Option<&str>) -> Input {
        Input {
            url: url.into(),
            mode: mode.map(str::to_string),
            timeout_secs: None,
        }
    }

    // ── opt-in gating ────────────────────────────────────────────────────

    #[test]
    fn config_disabled_by_default() {
        let cfg = resolve_config(&env(&[]));
        assert!(!cfg.enabled);
    }

    #[test]
    fn config_enabled_with_truthy_env() {
        let cfg = resolve_config(&env(&[("COPPERCLAW_BROWSER_ENABLED", "1")]));
        assert!(cfg.enabled);
        assert_eq!(cfg.requested_runtime, SandboxRuntime::Runsc);
    }

    #[test]
    fn config_runtime_override_parsed() {
        let cfg = resolve_config(&env(&[
            ("COPPERCLAW_BROWSER_ENABLED", "true"),
            ("COPPERCLAW_BROWSER_SANDBOX", "kata"),
        ]));
        assert_eq!(cfg.requested_runtime, SandboxRuntime::Kata);
    }

    #[test]
    fn config_non_truthy_stays_disabled() {
        for v in ["0", "false", "off", "no", "maybe", ""] {
            let cfg = resolve_config(&env(&[("COPPERCLAW_BROWSER_ENABLED", v)]));
            assert!(!cfg.enabled, "value {v:?} must not enable");
        }
    }

    #[tokio::test]
    async fn prepare_disabled_errors_without_side_effects() {
        let ctx = TaintRecordingCtx::default();
        let err = prepare(
            input("https://example.com", None),
            &env(&[]), // disabled
            &NavGuard,
            &ctx,
        )
        .await
        .unwrap_err();
        match err {
            ToolError::Validation(m) => assert!(m.to_lowercase().contains("disabled"), "{m}"),
            other => panic!("expected disabled validation error, got {other:?}"),
        }
        // Disabled → no provenance taint, no network: the gate ran first.
        assert!(
            ctx.sources.lock().unwrap().is_empty(),
            "disabled tool must not taint the turn"
        );
    }

    // ── untrusted provenance tagging ─────────────────────────────────────

    #[tokio::test]
    async fn prepare_taints_turn_untrusted_when_enabled() {
        let ctx = TaintRecordingCtx::default();
        // Use a public IP literal so the SSRF pre-flight passes without DNS.
        let out = prepare(
            input("https://8.8.8.8/", None),
            &env(&[("COPPERCLAW_BROWSER_ENABLED", "1")]),
            &NavGuard,
            &ctx,
        )
        .await;
        out.expect("public target must pass pre-flight");
        let sources = ctx.sources.lock().unwrap();
        assert_eq!(sources.len(), 1, "must taint exactly once");
        assert!(
            sources[0].starts_with("browser_render:"),
            "taint source names the tool + url: {}",
            sources[0]
        );
    }

    // ── SSRF rejection on navigation ─────────────────────────────────────

    #[tokio::test]
    async fn prepare_blocks_metadata_target() {
        let ctx = TaintRecordingCtx::default();
        let err = prepare(
            input("http://169.254.169.254/latest/meta-data/", None),
            &env(&[("COPPERCLAW_BROWSER_ENABLED", "1")]),
            &NavGuard,
            &ctx,
        )
        .await
        .unwrap_err();
        match err {
            ToolError::Validation(m) => {
                assert!(m.contains("link-local") || m.contains("SSRF"), "{m}");
            }
            other => panic!("expected SSRF validation error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn prepare_blocks_rfc1918_target() {
        let ctx = TaintRecordingCtx::default();
        let err = prepare(
            input("http://10.0.0.5/", None),
            &env(&[("COPPERCLAW_BROWSER_ENABLED", "1")]),
            &NavGuard,
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    // ── child-container spec built by prepare ────────────────────────────

    #[tokio::test]
    async fn prepare_builds_locked_down_child_spec() {
        let ctx = TaintRecordingCtx::default();
        let prepared = prepare(
            input("https://93.184.216.34/", Some("screenshot")),
            &env(&[("COPPERCLAW_BROWSER_ENABLED", "1")]),
            &NavGuard,
            &ctx,
        )
        .await
        .expect("public literal passes");
        let spec = &prepared.spec;
        // No broker token / no ANTHROPIC_* in the child env.
        let keys: Vec<&str> = spec.env.iter().map(|(k, _)| k.as_str()).collect();
        for forbidden in copperclaw_browser::FORBIDDEN_ENV_KEYS {
            assert!(!keys.contains(forbidden), "leaked {forbidden}");
        }
        // Egress restricted to the navigation target.
        assert_eq!(spec.egress_allow, vec!["93.184.216.34:443".to_string()]);
        assert_eq!(
            spec.egress_mode,
            copperclaw_container_rt::EgressMode::DenyDefault
        );
        // Stronger sandbox requested (hardened-runc floor here since the empty
        // availability set means no microVM runtime is honoured at this layer).
        let sb = spec.sandbox.as_ref().expect("sandbox requested");
        assert!(sb.cap_drop_all);
        assert!(sb.no_new_privileges);
        assert_eq!(sb.userns_remap.as_deref(), Some("copperclaw-browser"));
        assert_eq!(prepared.req.mode, RenderMode::Screenshot);
    }

    // ── mode parsing ─────────────────────────────────────────────────────

    #[test]
    fn parse_mode_defaults_and_aliases() {
        assert_eq!(parse_mode(None).unwrap(), RenderMode::DomText);
        assert_eq!(parse_mode(Some("text")).unwrap(), RenderMode::DomText);
        assert_eq!(
            parse_mode(Some("screenshot")).unwrap(),
            RenderMode::Screenshot
        );
        assert_eq!(parse_mode(Some("png")).unwrap(), RenderMode::Screenshot);
        assert_eq!(parse_mode(Some("aria")).unwrap(), RenderMode::AriaSnapshot);
        assert_eq!(
            parse_mode(Some("aria_snapshot")).unwrap(),
            RenderMode::AriaSnapshot
        );
    }

    #[test]
    fn parse_mode_rejects_unknown() {
        assert!(matches!(
            parse_mode(Some("clickbait")),
            Err(ToolError::Validation(_))
        ));
    }

    #[test]
    fn egress_allow_for_uses_target_host_port() {
        assert_eq!(
            egress_allow_for("https://example.com/path").unwrap(),
            vec!["example.com:443".to_string()]
        );
        assert_eq!(
            egress_allow_for("http://example.com:8080/").unwrap(),
            vec!["example.com:8080".to_string()]
        );
    }

    // ── V4: preview egress allow-list injection ──────────────────────────

    #[test]
    fn is_host_port_accepts_valid_and_rejects_malformed() {
        assert!(is_host_port("127.0.0.1:8080"));
        assert!(is_host_port("app.local:80"));
        assert!(is_host_port("172.17.0.9:5173"));
        // No port / empty host / out-of-range port / junk → rejected.
        assert!(!is_host_port("127.0.0.1"));
        assert!(!is_host_port(":8080"));
        assert!(!is_host_port("host:99999")); // > u16::MAX
        assert!(!is_host_port("host:notaport"));
        assert!(!is_host_port(""));
    }

    #[test]
    fn preview_egress_allow_parses_filters_and_trims() {
        let e = env(&[(
            "COPPERCLAW_BROWSER_PREVIEW_ALLOW",
            " 127.0.0.1:8080 , junk , host:99999 , ok.host:443 ",
        )]);
        assert_eq!(
            preview_egress_allow(&e),
            vec!["127.0.0.1:8080".to_string(), "ok.host:443".to_string()]
        );
    }

    #[test]
    fn preview_egress_allow_empty_when_unset() {
        assert!(preview_egress_allow(&env(&[])).is_empty());
    }

    /// The deny-default-egress fixture: with the host-supplied preview allow
    /// entry present, `prepare` builds a child spec whose deny-default egress
    /// permits BOTH the navigation target and the injected preview host:port —
    /// proving the allow-list injection lands on the spec the spawn path
    /// consumes.
    #[tokio::test]
    async fn prepare_injects_preview_allow_into_deny_default_egress() {
        let ctx = TaintRecordingCtx::default();
        let prepared = prepare(
            input("https://8.8.8.8/", Some("screenshot")),
            &env(&[
                ("COPPERCLAW_BROWSER_ENABLED", "1"),
                ("COPPERCLAW_BROWSER_PREVIEW_ALLOW", "172.17.0.9:5173"),
            ]),
            &NavGuard,
            &ctx,
        )
        .await
        .expect("public literal passes");
        assert_eq!(
            prepared.spec.egress_mode,
            copperclaw_container_rt::EgressMode::DenyDefault
        );
        assert_eq!(
            prepared.spec.egress_allow,
            vec!["8.8.8.8:443".to_string(), "172.17.0.9:5173".to_string()]
        );
    }

    #[tokio::test]
    async fn prepare_without_preview_allow_is_target_only() {
        let ctx = TaintRecordingCtx::default();
        let prepared = prepare(
            input("https://8.8.8.8/", Some("screenshot")),
            &env(&[("COPPERCLAW_BROWSER_ENABLED", "1")]),
            &NavGuard,
            &ctx,
        )
        .await
        .expect("public literal passes");
        // Byte-identical to pre-V4: exactly the navigation target.
        assert_eq!(prepared.spec.egress_allow, vec!["8.8.8.8:443".to_string()]);
    }

    #[tokio::test]
    async fn prepare_dedupes_preview_allow_equal_to_target() {
        let ctx = TaintRecordingCtx::default();
        // Preview allow == the navigation target → not duplicated. (Uses a
        // public literal so the SSRF pre-flight passes; the real preview target
        // is an RFC1918 bridge IP, but that would be refused by the guard, and
        // dedupe is host-agnostic.)
        let prepared = prepare(
            input("http://93.184.216.34:5173/", Some("screenshot")),
            &env(&[
                ("COPPERCLAW_BROWSER_ENABLED", "1"),
                ("COPPERCLAW_BROWSER_PREVIEW_ALLOW", "93.184.216.34:5173"),
            ]),
            &NavGuard,
            &ctx,
        )
        .await
        .expect("public literal passes");
        assert_eq!(
            prepared.spec.egress_allow,
            vec!["93.184.216.34:5173".to_string()]
        );
    }

    // ── V4: screenshot output dir defaults under the session data root ───

    #[test]
    fn screenshot_dir_prefers_explicit_override() {
        let e = env(&[("COPPERCLAW_BROWSER_OUTPUT_DIR", "/custom/shots")]);
        assert_eq!(
            screenshot_dir(&e),
            std::path::PathBuf::from("/custom/shots")
        );
    }

    #[test]
    fn screenshot_dir_defaults_under_data_root() {
        // With no explicit override, screenshots land under the shared data
        // root (so the in-container agent can send_file them). The concrete
        // root is whatever `verify_gate::data_root()` resolves to in this
        // process; the invariant is the `screenshots` leaf under it.
        let got = screenshot_dir(&env(&[]));
        let want = crate::tools::verify_gate::data_root().join(SCREENSHOT_SUBDIR);
        assert_eq!(got, want);
        assert_eq!(
            got.file_name().and_then(|s| s.to_str()),
            Some("screenshots")
        );
    }

    // ── schema ───────────────────────────────────────────────────────────

    #[test]
    fn schema_is_read_only_and_opt_in() {
        let tool = schema();
        assert_eq!(tool.name, "browser_render");
        let desc = tool
            .description
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(
            desc.contains("read-only"),
            "must advertise read-only: {desc}"
        );
        assert!(
            desc.contains("opt-in") || desc.contains("off by default"),
            "{desc}"
        );
        assert!(desc.contains("untrusted"), "{desc}");
    }

    // ── handle() returns the honest deferred-runtime error after safety ──

    #[tokio::test]
    async fn handle_disabled_is_validation_error() {
        let ctx = TaintRecordingCtx::default();
        // No enable env in the process → SystemEnv-backed handle is disabled.
        // Use the lower-level prepare with an explicit disabled env to keep
        // this deterministic regardless of the test process environment.
        let err = prepare(input("https://8.8.8.8/", None), &env(&[]), &NavGuard, &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }
}
