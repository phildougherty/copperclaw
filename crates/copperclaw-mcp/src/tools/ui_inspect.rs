//! `ui_inspect`: console errors + element geometry for the agent's OWN
//! running app (M20 D5 — see
//! `docs/plans/m20-coding-and-design-capability-program.md`, Wave 3, card D5).
//!
//! `ui_screenshot` (M20 D1/D2) closes the vision-loop WRITE path but a
//! screenshot alone can't diagnose *why* something is broken: a JS crash on
//! load renders as a blank page with no visible cause, and a squashed/
//! off-screen element looks identical to a correctly-sized one in a still
//! image. `ui_inspect` closes that gap: it drives the SAME in-container
//! chromium (`copperclaw_browser::incontainer`) to (1) return the buffered
//! browser console — `console.*` calls, uncaught exceptions, and
//! `Log.entryAdded` diagnostics captured during this navigation — and (2),
//! when a `selector` is given, that element's box model (position/size) plus
//! a CURATED subset of its computed style (never the ~300-property CDP
//! dump), so a broken layout is measurable (overflow, zero height,
//! off-screen) rather than just visible-but-unexplained.
//!
//! `ui_screenshot` already folds a console-error count + the first error into
//! its own text response (M20 D5) so the common case needs no second call;
//! `ui_inspect` is for when the agent needs the FULL console buffer or an
//! element's geometry.
//!
//! ## Security posture
//!
//!   * **Loopback-only, hard-refused otherwise** — identical check to
//!     `ui_screenshot` (`crate::tools::ui_screenshot::validate_loopback_url`,
//!     reused verbatim rather than re-implemented): any non-`127.0.0.1` /
//!     `::1` / `localhost` URL is refused with a hint pointing at
//!     `browser_render`.
//!   * **No new privilege** — same argument as `ui_screenshot`
//!     (`copperclaw_browser::incontainer`'s module docs): the agent already
//!     has `shell` + this same chromium binary in the prototyping image;
//!     this tool spawns no new process class, widens no egress, and — being
//!     loopback-only — cannot reach the LAN, the host, or the public
//!     internet. That is the argument for registering it by default in the
//!     Coding/Full profiles, exactly like `ui_screenshot`. Recorded again
//!     here as this is M20's second `security-review`-rider card (after D1).
//!   * **ALWAYS tainted untrusted.** Unlike `ui_screenshot` (which only taints
//!     when it folds in a console error), this tool's entire purpose is to
//!     surface page-originated console text — the page's own `console.*`
//!     calls could echo fetched/attacker-influenced content into what looks
//!     like innocuous debug output. So `mark_untrusted_context` is called
//!     unconditionally, before the navigation runs, mirroring
//!     `browser_render`'s "tag the turn up front... before any content could
//!     land in history" ordering (`crate::tools::browser_render`) — a call
//!     that happens to see an empty console this time still marks the turn,
//!     since the NEXT identical call against the same live app could easily
//!     not be empty.
//!   * **Minimal-profile degradation.** Chromium is probed for at CALL TIME
//!     (`copperclaw_browser::find_chromium_binary`); its absence returns the
//!     same clean, actionable error `ui_screenshot` does
//!     (`crate::tools::ui_screenshot::chromium_missing_error`), naming the
//!     prototyping profile — never a crash or a hang.

use std::time::Duration;

use crate::context::ToolContext;
use crate::error::ToolError;
use crate::tools::ui_screenshot::{chromium_missing_error, validate_loopback_url};
use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args};
use copperclaw_browser::{ConsoleEntry, ConsoleLevel, ElementInspection};
use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde::Deserialize;
use serde_json::json;

/// Navigation/load timeout for the in-container chromium (mirrors
/// `ui_screenshot`'s).
const NAV_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Deserialize)]
struct Input {
    /// Must be a loopback URL (127.0.0.1 / `::1` / localhost). Anything else
    /// is refused — use `browser_render` for real web browsing.
    url: String,
    /// A CSS selector to also fetch the box model + curated computed style
    /// for. Omitted → console-only response.
    #[serde(default)]
    selector: Option<String>,
    /// Extra fixed delay (ms) after load before inspecting, e.g. to let a
    /// client-side render (and any console errors it throws) settle.
    /// Clamped to [`copperclaw_browser::UI_SCREENSHOT_MAX_WAIT_MS`].
    #[serde(default)]
    wait_ms: Option<u64>,
}

pub fn schema() -> Tool {
    make_tool(
        "ui_inspect",
        "Inspect YOUR OWN running app (loopback only, like `ui_screenshot`): returns the browser \
         console — `console.*` calls, uncaught JS exceptions, and browser-level Log entries \
         captured during this navigation, capped, and treated as UNTRUSTED (page-originated) \
         content — plus, when `selector` is given, that element's box model (position/size) and \
         a CURATED subset of its computed style (display, position, overflow, width, height, \
         font-family — never the full ~300-property CDP dump). Use this to diagnose WHY a page \
         renders blank (a JS crash on load, visible here even though the screenshot alone can't \
         show it) or WHY a layout looks broken (overflow, zero height, an off-screen element). \
         `ui_screenshot` already folds a console-error count + the first error into its own \
         response for the common case — reach for `ui_inspect` when you need the full buffer or \
         an element's geometry. `url` MUST be a loopback address (127.0.0.1 / ::1 / localhost) — \
         this is NOT a general web-browsing tool; for a real external site use `browser_render` \
         instead. Requires chromium, which is baked into the `prototyping` image profile; on a \
         minimal-profile container this returns a clear, actionable error instead of failing \
         silently.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["url"],
            "properties": {
                "url": { "type": "string", "minLength": 1 },
                "selector": { "type": ["string", "null"], "minLength": 1 },
                "wait_ms": { "type": ["integer", "null"], "minimum": 0, "maximum": 10000 }
            }
        }),
    )
}

/// Format the console section of the response: counts by level, then every
/// buffered entry (already capped upstream at
/// [`copperclaw_browser::CONSOLE_BUFFER_CAP`]). Pure so it's unit-tested
/// without a live transport.
fn format_console_section(entries: &[ConsoleEntry]) -> String {
    if entries.is_empty() {
        return "Console: no entries captured during this navigation.".to_string();
    }
    let errors = entries
        .iter()
        .filter(|e| e.level == ConsoleLevel::Error)
        .count();
    let warnings = entries
        .iter()
        .filter(|e| e.level == ConsoleLevel::Warning)
        .count();
    let mut lines = vec![format!(
        "Console (untrusted, page-originated): {errors} error(s), {warnings} warning(s), {} \
         total:",
        entries.len()
    )];
    for entry in entries {
        lines.push(format!("  [{}] {}", entry.level.as_str(), entry.text));
    }
    lines.join("\n")
}

/// Format the element-inspection section: box model dimensions + the curated
/// computed-style whitelist. Pure so it's unit-tested without a live
/// transport.
fn format_element_section(el: &ElementInspection) -> String {
    let b = &el.box_model;
    let mut lines = vec![format!(
        "Element `{}`: box {w}x{h}px",
        el.selector,
        w = b.width,
        h = b.height
    )];
    if el.computed_style.is_empty() {
        lines.push("Computed style: (none of the curated properties were present)".to_string());
    } else {
        lines.push(
            "Computed style (curated — display/position/overflow/width/height/font-family only):"
                .to_string(),
        );
        for (name, value) in &el.computed_style {
            lines.push(format!("  {name}: {value}"));
        }
    }
    lines.join("\n")
}

#[derive(Debug)]
struct Prepared {
    url: reqwest::Url,
    selector: Option<String>,
    wait_ms: Option<u64>,
}

/// Everything up to (but not including) driving a real chromium: parse and
/// loopback-validate. Split out so validation is unit-tested without
/// touching a real chromium binary — mirrors `ui_screenshot::prepare`.
fn prepare(input: &Input) -> Result<Prepared, ToolError> {
    let url = validate_loopback_url(&input.url)?;
    Ok(Prepared {
        url,
        selector: input.selector.clone(),
        wait_ms: input.wait_ms,
    })
}

pub async fn handle(
    arguments: Option<JsonObject>,
    ctx: &dyn ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;
    let prepared = prepare(&input)?;

    // Probe for chromium AT CALL TIME (not registration time — the tool is
    // always registered under Coding/Full; the minimal profile degrades
    // here, cleanly), same as `ui_screenshot`.
    let binary = copperclaw_browser::find_chromium_binary().ok_or_else(chromium_missing_error)?;

    // M20 D5: this tool's whole purpose is to surface page-originated
    // console text, so it taints the turn unconditionally, before driving
    // the navigation — mirroring `browser_render`'s "tag up front" ordering.
    // An empty console THIS call is not a guarantee the next identical call
    // against the same live app stays empty.
    ctx.mark_untrusted_context(&format!("ui_inspect:{}", prepared.url));

    let transport = copperclaw_browser::chromium_singleton()
        .get_transport(&binary, NAV_TIMEOUT)
        .await
        .map_err(|e| {
            ToolError::Internal(format!(
                "ui_inspect: could not start the local chromium: {e}"
            ))
        })?;

    let req = copperclaw_browser::InspectRequest {
        url: prepared.url.to_string(),
        selector: prepared.selector.clone(),
        wait_ms: prepared.wait_ms,
        wait_for_selector: None,
        nav_timeout: NAV_TIMEOUT,
    };
    let outcome = copperclaw_browser::inspect(transport.as_ref(), &req)
        .await
        .map_err(|e| ToolError::Internal(format!("ui_inspect: {e}")))?;

    let mut sections = vec![format_console_section(&outcome.console)];
    if let Some(element) = &outcome.element {
        sections.push(format_element_section(element));
    }

    Ok(CallToolResult::success(vec![Content::text(
        sections.join("\n\n"),
    )]))
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

    fn bare_input(url: &str) -> Input {
        Input {
            url: url.to_string(),
            selector: None,
            wait_ms: None,
        }
    }

    // ── loopback URL validation (reused from ui_screenshot) ──────────────

    #[test]
    fn prepare_accepts_loopback_url() {
        assert!(prepare(&bare_input("http://127.0.0.1:5173/")).is_ok());
    }

    #[test]
    fn prepare_rejects_non_loopback_before_touching_chromium() {
        let err = prepare(&bare_input("http://example.com/")).unwrap_err();
        match err {
            ToolError::Validation(m) => {
                assert!(m.contains("loopback"), "{m}");
                assert!(m.contains("browser_render"), "{m}");
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn prepare_carries_selector_and_wait_ms() {
        let mut input = bare_input("http://127.0.0.1:5173/");
        input.selector = Some("#app".to_string());
        input.wait_ms = Some(250);
        let prepared = prepare(&input).unwrap();
        assert_eq!(prepared.selector.as_deref(), Some("#app"));
        assert_eq!(prepared.wait_ms, Some(250));
    }

    // ── schema ────────────────────────────────────────────────────────────

    #[test]
    fn schema_advertises_loopback_only_and_prototyping_requirement() {
        let tool = schema();
        assert_eq!(tool.name, "ui_inspect");
        let desc = tool
            .description
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(desc.contains("loopback"), "{desc}");
        assert!(desc.contains("browser_render"), "{desc}");
        assert!(desc.contains("prototyping"), "{desc}");
        assert!(desc.contains("untrusted"), "{desc}");
    }

    #[test]
    fn schema_advertises_selector_and_wait_ms_args() {
        let tool = schema();
        let v: serde_json::Value = serde_json::to_value(&*tool.input_schema).unwrap();
        let props = v.get("properties").unwrap();
        for key in ["url", "selector", "wait_ms"] {
            assert!(props.get(key).is_some(), "schema missing `{key}`");
        }
    }

    // ── console section formatting ───────────────────────────────────────

    #[test]
    fn format_console_section_reports_no_entries() {
        let text = format_console_section(&[]);
        assert!(text.contains("no entries"), "{text}");
    }

    #[test]
    fn format_console_section_counts_and_lists_every_capped_entry() {
        let entries = vec![
            ConsoleEntry {
                level: ConsoleLevel::Error,
                text: "TypeError: boom".to_string(),
            },
            ConsoleEntry {
                level: ConsoleLevel::Warning,
                text: "deprecated".to_string(),
            },
            ConsoleEntry {
                level: ConsoleLevel::Log,
                text: "booted".to_string(),
            },
        ];
        let text = format_console_section(&entries);
        assert!(text.contains("1 error(s)"), "{text}");
        assert!(text.contains("1 warning(s)"), "{text}");
        assert!(text.contains('3'), "{text}"); // total
        assert!(text.contains("TypeError: boom"), "{text}");
        assert!(text.contains("deprecated"), "{text}");
        assert!(text.contains("booted"), "{text}");
        assert!(text.contains("untrusted"), "{text}");
    }

    // ── element section formatting: the curated whitelist, not the dump ──

    #[test]
    fn format_element_section_reports_box_and_curated_style_only() {
        let el = ElementInspection {
            selector: "#app".to_string(),
            box_model: copperclaw_browser::BoxModel {
                width: 200.0,
                height: 100.0,
                content: vec![],
                padding: vec![],
                border: vec![],
                margin: vec![],
            },
            computed_style: vec![
                ("display".to_string(), "flex".to_string()),
                ("font-family".to_string(), "Inter".to_string()),
            ],
        };
        let text = format_element_section(&el);
        assert!(text.contains("#app"), "{text}");
        assert!(text.contains("200"), "{text}");
        assert!(text.contains("100"), "{text}");
        assert!(text.contains("display: flex"), "{text}");
        assert!(text.contains("font-family: Inter"), "{text}");
        // Never a stray full-dump property this test didn't put there.
        assert!(!text.contains("z-index"), "{text}");
    }

    #[test]
    fn format_element_section_handles_no_curated_properties_present() {
        let el = ElementInspection {
            selector: "#app".to_string(),
            box_model: copperclaw_browser::BoxModel {
                width: 0.0,
                height: 0.0,
                content: vec![],
                padding: vec![],
                border: vec![],
                margin: vec![],
            },
            computed_style: vec![],
        };
        let text = format_element_section(&el);
        assert!(text.contains("none of the curated"), "{text}");
    }

    // ── mark_untrusted_context: unconditional taint ──────────────────────

    /// Records `mark_untrusted_context` calls so the provenance test can
    /// assert `ui_inspect` always taints, mirroring `browser_render`'s own
    /// `TaintRecordingCtx` test double.
    #[derive(Default)]
    struct TaintRecordingCtx {
        sources: std::sync::Mutex<Vec<String>>,
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

    #[tokio::test]
    async fn non_loopback_url_is_refused_before_any_taint_or_chromium_probe() {
        // A refused call carries no page-content risk — nothing to taint.
        let ctx = TaintRecordingCtx::default();
        let mut args = JsonObject::new();
        args.insert("url".into(), "http://example.com/".into());
        let err = handle(Some(args), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
        assert!(
            ctx.sources.lock().unwrap().is_empty(),
            "a refused (non-loopback) call must not taint the turn"
        );
    }

    // ── live in-container acceptance (Docker/prototyping-image-gated) ────

    /// Live in-container acceptance (M20 D5): a page that throws on load —
    /// `ui_inspect` surfaces the full console detail (mirroring
    /// `ui_screenshot`'s fold-in count), and a selector query against a known
    /// element returns its box + curated style. Needs the prototyping
    /// image's baked chromium AND a real dev server —
    /// `#[ignore]`d per the `ui_screenshot_docker_end_to_end` precedent.
    /// Opt in with `cargo test -p copperclaw-mcp -- --ignored ui_inspect`.
    #[tokio::test]
    #[ignore = "requires the prototyping image's chromium + a running dev server on 127.0.0.1; opt in with --ignored"]
    async fn ui_inspect_docker_end_to_end() {
        let mut args = JsonObject::new();
        args.insert("url".into(), "http://127.0.0.1:5173/".into());
        args.insert("selector".into(), "body".into());
        let ctx = crate::context::MockToolContext::new();
        let res = handle(Some(args), &ctx)
            .await
            .expect("ui_inspect should succeed against a live dev server");
        assert_eq!(res.is_error, Some(false));
        let text = res
            .content
            .iter()
            .find_map(|c| match &c.raw {
                rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .expect("a text part");
        assert!(text.contains("Console"), "{text}");
        assert!(text.contains("box"), "{text}");
    }
}
