//! `browser_interact`: drive a headless browser through a bounded, scripted
//! interactive sequence (Phase 5b, **demand-pull, opt-in**), then read back the
//! post-interaction page. The interactive counterpart to the read-only
//! [`crate::tools::browser_render`].
//!
//! ## Security posture (what this tool enforces)
//!
//!   * **Stricter, SEPARATE opt-in — OFF by default.** This tool requires BOTH
//!     the base browser opt-in (`COPPERCLAW_BROWSER_ENABLED`) AND the stricter
//!     interactive opt-in (`COPPERCLAW_BROWSER_INTERACTIVE`) to be truthy. With
//!     the interactive flag unset the tool is **not even registered** (see
//!     [`crate::tools::build_tool_set`]), so a default deployment — and one
//!     running only the read-only browser — is byte-identical to today: the
//!     model never sees this verb.
//!   * **Demand-pull only.** One call runs one caller-scripted, bounded action
//!     sequence (capped at [`copperclaw_browser::MAX_ACTIONS`]) against one
//!     page, then tears the child container down. There is NO autonomous
//!     browsing loop and NO persistent session.
//!   * **SSRF guard re-run per navigation.** The initial target is pre-flighted
//!     through [`crate::tools::net_guard::guard_url`]; then, in
//!     [`copperclaw_browser::interact`], the settled `location.href` AND every
//!     redirect hop are re-guarded after EVERY action (a click / submit /
//!     scripted navigation to an internal address is refused mid-interaction,
//!     before the DOM is read).
//!   * **Untrusted provenance.** The interactively-driven DOM is external,
//!     attacker-influenceable content — the turn is tagged untrusted up front
//!     via [`ToolContext::mark_untrusted_context`], exactly like `browser_render`.
//!   * **Same locked-down child container.** No broker token, deny-default
//!     egress scoped to the navigation target, unprivileged user, hardened
//!     sandbox — reused verbatim from
//!     [`copperclaw_browser::build_browser_container_spec`].
//!
//! ## Non-goals (kept explicit)
//!
//! No always-on interactive browsing, and no browser-writes-memory. This tool
//! renders post-interaction content for the model to read; it never persists
//! anything.

use crate::context::ToolContext;
use crate::error::ToolError;
use crate::tools::browser_render::{
    EnvLookup, NavGuard, SystemEnv, egress_allow_for, preview_egress_allow, resolve_config,
    screenshot_dir, truthy,
};
use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args, success_json};
use copperclaw_browser::{
    BrowserContainerParams, BrowserError, InteractRequest, InteractiveAction, LiveRenderOptions,
    NavigationGuard, RenderMode, WsCdpConnector,
};
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::Deserialize;
use serde_json::json;

/// Stricter, SEPARATE opt-in for the INTERACTIVE browser. Distinct from
/// `COPPERCLAW_BROWSER_ENABLED` (the read-only enable): interactive click/type/
/// scroll is a higher-risk capability, so it gates independently. Unset / not
/// truthy → the tool is not registered and never runs.
pub const INTERACTIVE_ENABLE_ENV: &str = "COPPERCLAW_BROWSER_INTERACTIVE";

/// Whether the interactive browser is opted in: the base browser AND the
/// stricter interactive flag must BOTH be truthy. This gates both registration
/// (in [`crate::tools::build_tool_set`]) and, defensively, each call.
pub fn interactive_opt_in(env: &dyn EnvLookup) -> bool {
    resolve_config(env).enabled && env.get(INTERACTIVE_ENABLE_ENV).is_some_and(|v| truthy(&v))
}

/// Decoded tool input: an initial URL, a scripted action list, and what to read
/// back afterward.
#[derive(Debug, Clone, Deserialize)]
pub struct Input {
    pub url: String,
    /// The scripted actions to perform, in order (tagged by `action`).
    pub actions: Vec<InteractiveAction>,
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
            "browser_interact: unknown mode `{other}` \
             (expected screenshot | dom_text | aria_snapshot)"
        ))),
    }
}

/// The safety-critical, fully-testable core: gate on the stricter opt-in,
/// validate, tag the turn untrusted, run the SSRF pre-flight, and build the
/// locked-down child-container spec. Returns the prepared request + the child
/// spec the runtime path would spawn.
pub async fn prepare(
    input: Input,
    env: &dyn EnvLookup,
    guard: &dyn NavigationGuard,
    ctx: &dyn ToolContext,
) -> Result<Prepared, ToolError> {
    // 1. Stricter opt-in gate. OFF by default → hard validation error, no side
    //    effects. Requires BOTH the base browser AND the interactive flag.
    if !interactive_opt_in(env) {
        return Err(ToolError::Validation(
            "browser_interact is disabled (opt-in); the operator must set BOTH \
             COPPERCLAW_BROWSER_ENABLED and COPPERCLAW_BROWSER_INTERACTIVE to enable it"
                .to_string(),
        ));
    }
    // The base config supplies the image + sandbox runtime for the child spec.
    let cfg = resolve_config(env);

    // 2. Provenance: interactively-driven content is untrusted external content.
    //    Tag the turn up front (before any content could land in history).
    ctx.mark_untrusted_context(&format!("browser_interact:{}", input.url));

    // 3. Validate shape (URL + bounded action list + each action well-formed).
    let mode = parse_mode(input.mode.as_deref())?;
    let req = InteractRequest {
        url: input.url.clone(),
        actions: input.actions.clone(),
        mode,
        timeout_secs: input.timeout_secs,
    };
    req.validate()
        .map_err(|e| ToolError::Validation(e.to_string()))?;

    // 4. SSRF pre-flight on the INITIAL navigation target (reuses net_guard).
    //    The live orchestration additionally re-guards after every action.
    guard.guard_target(&req.url).await.map_err(|e| {
        copperclaw_metrics::inc_browser_ssrf_block("interactive_target_preflight");
        ToolError::Validation(e)
    })?;

    // 5. Build the locked-down child-container spec (identical to the read-only
    //    tool): no broker token, deny-default egress scoped to the navigation
    //    target (+ any host-injected preview host:port), stronger sandbox.
    let mut egress_allow = egress_allow_for(&req.url)?;
    let preview_extras = preview_egress_allow(env);
    if !preview_extras.is_empty() {
        copperclaw_metrics::inc_browser_render_preview_allow_injected();
    }
    for extra in preview_extras {
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

/// Output of [`prepare`]: the validated interactive request + the child spec.
#[derive(Debug)]
pub struct Prepared {
    pub req: InteractRequest,
    pub spec: copperclaw_container_rt::ContainerSpec,
}

pub fn schema() -> Tool {
    make_tool(
        "browser_interact",
        "Drive a headless browser through a SCRIPTED interactive sequence \
         (click / type / scroll / wait_for_selector) against a page, then read \
         back the post-interaction DOM text, a screenshot path, or an ARIA \
         snapshot. Demand-pull and bounded — one call, one page, one action \
         list; NOT an autonomous browsing loop. Requires a STRICTER separate \
         opt-in and is OFF by default. Output is treated as UNTRUSTED external \
         content. The initial target AND every navigation an action triggers \
         are SSRF-guarded (internal/metadata addresses are refused).",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["url", "actions"],
            "properties": {
                "url": { "type": "string", "minLength": 1 },
                "mode": { "type": ["string", "null"], "enum": ["screenshot", "dom_text", "aria_snapshot", null] },
                "timeout_secs": { "type": ["integer", "null"], "minimum": 1, "maximum": 120 },
                "actions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 32,
                    "items": {
                        "type": "object",
                        "required": ["action"],
                        "properties": {
                            "action": { "type": "string", "enum": ["click", "type", "scroll", "wait_for_selector"] },
                            "selector": { "type": "string" },
                            "text": { "type": "string" },
                            "dx": { "type": "number" },
                            "dy": { "type": "number" },
                            "timeout_ms": { "type": ["integer", "null"], "minimum": 1 }
                        }
                    }
                }
            }
        }),
    )
}

pub async fn handle(
    arguments: Option<JsonObject>,
    ctx: &dyn ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;
    let env = SystemEnv;
    let guard = NavGuard;
    // Every safety step (stricter opt-in gate, untrusted tagging, SSRF
    // pre-flight, locked-down child spec) runs in `prepare`. With the tool
    // disabled it returns the "disabled" validation error before the live path.
    let prepared = prepare(input, &env, &guard, ctx).await?;
    interact_prepared_live(prepared, &guard, &env).await
}

/// Drive the live interactive session for a fully-prepared request: detect a
/// container runtime, spawn the locked-down child, and drive the actions
/// through the SSRF orchestration. When no container runtime is reachable,
/// report unavailable cleanly — never a panic; every safety step already ran.
async fn interact_prepared_live(
    prepared: Prepared,
    guard: &dyn NavigationGuard,
    env: &dyn EnvLookup,
) -> Result<CallToolResult, ToolError> {
    let mode_label = "interactive";

    let runtime = match copperclaw_container_rt::detect().await {
        Ok(rt) => rt,
        Err(e) => {
            copperclaw_metrics::inc_browser_render(mode_label, "unavailable");
            return Err(ToolError::Internal(format!(
                "browser_interact: target `{}` passed the SSRF + opt-in checks and a locked-down \
                 child-container spec was constructed (egress={:?}), but no container runtime is \
                 reachable here to spawn the headless-browser child ({e}). This is the \
                 in-container runner, which has no Docker socket by design (M18 V4's host-side \
                 screenshot injection is superseded — see M20 D1). If you want to SEE your own \
                 app's UI, use `ui_screenshot` instead: it renders locally in this container over \
                 loopback and needs no container runtime (it is read-only, so it can't replace \
                 click/type/scroll — for those, `browser_interact` itself runs only where the \
                 operator has wired a reachable Docker daemon).",
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

    let result = copperclaw_browser::interact_live(
        &prepared.req,
        prepared.spec,
        guard,
        runtime.as_ref(),
        &connector,
        &opts,
    )
    .await;

    let outcome = match &result {
        Ok(_) => "ok",
        Err(BrowserError::Blocked(_)) => "blocked",
        Err(_) => "driver_error",
    };
    copperclaw_metrics::inc_browser_render(mode_label, outcome);

    match result {
        Ok(out) => Ok(success_json(&out)),
        Err(BrowserError::Blocked(m)) => Err(ToolError::Validation(m)),
        Err(e) => Err(ToolError::Internal(format!("browser_interact: {e}"))),
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
    use crate::tools::browser_render::MapEnv;
    use std::sync::Mutex;

    fn env(pairs: &[(&str, &str)]) -> MapEnv {
        MapEnv::from_pairs(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
        )
    }

    /// Records `mark_untrusted_context` calls so the provenance test can assert
    /// the turn was tainted.
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

    fn click_input(url: &str) -> Input {
        Input {
            url: url.into(),
            actions: vec![InteractiveAction::Click {
                selector: "#go".into(),
            }],
            mode: None,
            timeout_secs: None,
        }
    }

    // ── opt-in gating (the byte-stability boundary) ──────────────────────

    #[test]
    fn interactive_disabled_by_default() {
        // Nothing set.
        assert!(!interactive_opt_in(&env(&[])));
        // Base browser on, interactive off → still disabled.
        assert!(!interactive_opt_in(&env(&[(
            "COPPERCLAW_BROWSER_ENABLED",
            "1"
        )])));
        // Interactive on, base browser off → still disabled (needs both).
        assert!(!interactive_opt_in(&env(&[(
            "COPPERCLAW_BROWSER_INTERACTIVE",
            "1"
        )])));
    }

    #[test]
    fn interactive_enabled_requires_both_flags() {
        assert!(interactive_opt_in(&env(&[
            ("COPPERCLAW_BROWSER_ENABLED", "1"),
            ("COPPERCLAW_BROWSER_INTERACTIVE", "1"),
        ])));
    }

    #[test]
    fn interactive_non_truthy_stays_disabled() {
        for v in ["0", "false", "off", "no", "maybe", ""] {
            assert!(
                !interactive_opt_in(&env(&[
                    ("COPPERCLAW_BROWSER_ENABLED", "1"),
                    ("COPPERCLAW_BROWSER_INTERACTIVE", v),
                ])),
                "value {v:?} must not enable interactive"
            );
        }
    }

    #[tokio::test]
    async fn prepare_disabled_errors_without_side_effects() {
        let ctx = TaintRecordingCtx::default();
        // Base browser enabled but interactive NOT → disabled, no taint.
        let err = prepare(
            click_input("https://example.com"),
            &env(&[("COPPERCLAW_BROWSER_ENABLED", "1")]),
            &NavGuard,
            &ctx,
        )
        .await
        .unwrap_err();
        match err {
            ToolError::Validation(m) => assert!(m.to_lowercase().contains("disabled"), "{m}"),
            other => panic!("expected disabled validation error, got {other:?}"),
        }
        assert!(
            ctx.sources.lock().unwrap().is_empty(),
            "disabled tool must not taint the turn"
        );
    }

    // ── untrusted provenance tagging ─────────────────────────────────────

    #[tokio::test]
    async fn prepare_taints_turn_untrusted_when_enabled() {
        let ctx = TaintRecordingCtx::default();
        let out = prepare(
            click_input("https://8.8.8.8/"),
            &env(&[
                ("COPPERCLAW_BROWSER_ENABLED", "1"),
                ("COPPERCLAW_BROWSER_INTERACTIVE", "1"),
            ]),
            &NavGuard,
            &ctx,
        )
        .await;
        out.expect("public target must pass pre-flight");
        let sources = ctx.sources.lock().unwrap();
        assert_eq!(sources.len(), 1, "must taint exactly once");
        assert!(
            sources[0].starts_with("browser_interact:"),
            "taint source names the tool + url: {}",
            sources[0]
        );
    }

    // ── SSRF rejection on the initial navigation target ──────────────────

    #[tokio::test]
    async fn prepare_blocks_metadata_target() {
        let ctx = TaintRecordingCtx::default();
        let err = prepare(
            click_input("http://169.254.169.254/latest/meta-data/"),
            &env(&[
                ("COPPERCLAW_BROWSER_ENABLED", "1"),
                ("COPPERCLAW_BROWSER_INTERACTIVE", "1"),
            ]),
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
            click_input("http://10.0.0.5/"),
            &env(&[
                ("COPPERCLAW_BROWSER_ENABLED", "1"),
                ("COPPERCLAW_BROWSER_INTERACTIVE", "1"),
            ]),
            &NavGuard,
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    // ── empty / malformed action list is refused ─────────────────────────

    #[tokio::test]
    async fn prepare_rejects_empty_action_list() {
        let ctx = TaintRecordingCtx::default();
        let input = Input {
            url: "https://8.8.8.8/".into(),
            actions: vec![],
            mode: None,
            timeout_secs: None,
        };
        let err = prepare(
            input,
            &env(&[
                ("COPPERCLAW_BROWSER_ENABLED", "1"),
                ("COPPERCLAW_BROWSER_INTERACTIVE", "1"),
            ]),
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
            click_input("https://93.184.216.34/"),
            &env(&[
                ("COPPERCLAW_BROWSER_ENABLED", "1"),
                ("COPPERCLAW_BROWSER_INTERACTIVE", "1"),
            ]),
            &NavGuard,
            &ctx,
        )
        .await
        .expect("public literal passes");
        let spec = &prepared.spec;
        let keys: Vec<&str> = spec.env.iter().map(|(k, _)| k.as_str()).collect();
        for forbidden in copperclaw_browser::FORBIDDEN_ENV_KEYS {
            assert!(!keys.contains(forbidden), "leaked {forbidden}");
        }
        assert_eq!(spec.egress_allow, vec!["93.184.216.34:443".to_string()]);
        assert_eq!(
            spec.egress_mode,
            copperclaw_container_rt::EgressMode::DenyDefault
        );
        let sb = spec.sandbox.as_ref().expect("sandbox requested");
        assert!(sb.cap_drop_all);
        assert!(sb.no_new_privileges);
    }

    // ── mode parsing + schema ────────────────────────────────────────────

    #[test]
    fn parse_mode_defaults_and_aliases() {
        assert_eq!(parse_mode(None).unwrap(), RenderMode::DomText);
        assert_eq!(
            parse_mode(Some("screenshot")).unwrap(),
            RenderMode::Screenshot
        );
        assert_eq!(parse_mode(Some("aria")).unwrap(), RenderMode::AriaSnapshot);
    }

    #[test]
    fn schema_advertises_interactive_bounded_untrusted() {
        let tool = schema();
        assert_eq!(tool.name, "browser_interact");
        let desc = tool
            .description
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(desc.contains("untrusted"), "{desc}");
        assert!(
            desc.contains("opt-in") || desc.contains("off by default"),
            "{desc}"
        );
        assert!(
            desc.contains("not an autonomous") || desc.contains("demand-pull"),
            "must advertise the non-goal boundary: {desc}"
        );
    }

    #[test]
    fn input_deserializes_tagged_actions() {
        let input: Input = serde_json::from_value(json!({
            "url": "https://example.com",
            "actions": [
                { "action": "click", "selector": "#a" },
                { "action": "type", "selector": "#b", "text": "hi" },
                { "action": "scroll", "dy": 400 },
                { "action": "wait_for_selector", "selector": ".done", "timeout_ms": 2000 }
            ]
        }))
        .unwrap();
        assert_eq!(input.actions.len(), 4);
        assert_eq!(input.actions[0].kind(), "click");
        assert_eq!(input.actions[3].kind(), "wait_for_selector");
    }
}
