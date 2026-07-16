//! Concrete Chrome `DevTools` Protocol (CDP) driver behind [`BrowserDriver`].
//!
//! The headless Chromium runs in the dedicated, locked-down child container
//! (see [`crate::container`]); this module speaks CDP to it over a WebSocket to
//! perform the read-only render the orchestration ([`crate::driver::render`])
//! asked for. The command/response logic is expressed against a small
//! [`CdpTransport`] seam so it is **fully unit-testable with a mock transport**
//! — the acceptance intent — while the live [`WsCdpTransport`] carries the real
//! WebSocket session (exercised only behind the `COPPERCLAW_BROWSER_ENABLED`
//! gate, since it needs a real Chromium + container).
//!
//! ## Why a hand-rolled CDP client rather than `chromiumoxide` / `headless_chrome`
//!
//! Both of those crates are process *launchers*: they spawn a Chromium binary
//! on the local host and drive it. Copperclaw runs Chromium in a dedicated,
//! egress-restricted, stronger-sandboxed **child container** — so the driver
//! must *connect* to a remote CDP endpoint on the container's bridge IP, not
//! spawn a local binary. The read-only render needs only a handful of CDP
//! methods (`Page.navigate`, `Page.captureScreenshot`, `Runtime.evaluate`,
//! `Accessibility.getFullAXTree`, plus the `Network`/`Page` enable + the
//! redirect events). Speaking those directly over `tokio-tungstenite` (already
//! in the workspace lock via the discord adapter) keeps the dependency
//! footprint tiny, keeps the crate `unsafe`-free and clippy-clean, and — the
//! decisive reason — puts the whole command sequence behind a mockable trait so
//! the driver is unit-tested without a live browser. A `chromiumoxide` wrapper
//! would be opaque to unit tests and drag in its generated-CDP-types codegen
//! tree for the five methods we actually use.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};

use crate::capture::{self, CaptureOptions};
use crate::driver::{BrowserDriver, DriverRender, Navigation, RenderedArtifact};
use crate::error::BrowserError;
use crate::interactive::InteractiveDriver;
use crate::render::RenderMode;

/// Default navigation/idle timeout when the caller does not specify one.
pub const DEFAULT_NAV_TIMEOUT: Duration = Duration::from_secs(30);

// ─────────────────────────────────────────────────────────────────────────────
// M20 D5: console buffering (`ui_screenshot`'s fold-in + `ui_inspect`'s full
// detail) and element inspection (`ui_inspect`'s selector query).
// ─────────────────────────────────────────────────────────────────────────────

/// Cap on buffered console entries per in-container CDP session (one
/// `ui_screenshot`/`ui_inspect` call — see `crate::incontainer`, which opens a
/// fresh tab per call, so this bounds ONE navigation's console noise, not a
/// whole runner lifetime). Keeps the MOST RECENT entries: a late crash matters
/// more than early noise, and this is small enough that a chatty page can
/// never turn a screenshot call into an unbounded-memory footgun.
pub const CONSOLE_BUFFER_CAP: usize = 50;

/// Severity of one buffered console entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleLevel {
    Error,
    Warning,
    Log,
}

impl ConsoleLevel {
    /// Stable lower-case token, used in `ui_inspect`'s formatted output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ConsoleLevel::Error => "error",
            ConsoleLevel::Warning => "warning",
            ConsoleLevel::Log => "log",
        }
    }
}

/// One buffered browser console entry: a `console.*` call, an uncaught JS
/// exception, or a `Log.entryAdded` browser-level diagnostic. This is
/// PAGE-ORIGINATED content — a page can echo fetched/attacker-influenced text
/// into its own console — so every call site that returns these to the model
/// (`ui_inspect`, and `ui_screenshot`'s fold-in when an error is present) must
/// `mark_untrusted_context` before doing so (M20 plan rule 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleEntry {
    pub level: ConsoleLevel,
    pub text: String,
}

/// Parse one raw CDP event (`method` + its `params`) into a [`ConsoleEntry`],
/// or `None` when it isn't one of the three console-related methods this
/// module buffers, or carries no readable text. Pure so the mapping is
/// unit-tested against canned CDP JSON, no live transport needed.
#[must_use]
pub fn parse_console_event(method: &str, params: &Value) -> Option<ConsoleEntry> {
    match method {
        "Runtime.consoleAPICalled" => {
            let kind = params.get("type").and_then(Value::as_str).unwrap_or("log");
            let level = match kind {
                "error" => ConsoleLevel::Error,
                "warning" => ConsoleLevel::Warning,
                _ => ConsoleLevel::Log,
            };
            let text = params
                .get("args")
                .and_then(Value::as_array)
                .map(|args| {
                    args.iter()
                        .filter_map(|a| {
                            a.get("value")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .or_else(|| {
                                    a.get("description")
                                        .and_then(Value::as_str)
                                        .map(str::to_string)
                                })
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "(console call with no readable args)".to_string());
            Some(ConsoleEntry { level, text })
        }
        "Runtime.exceptionThrown" => {
            let details = params.get("exceptionDetails")?;
            let text = details
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .or_else(|| details.get("text").and_then(Value::as_str))
                .unwrap_or("uncaught exception")
                .to_string();
            Some(ConsoleEntry {
                level: ConsoleLevel::Error,
                text,
            })
        }
        "Log.entryAdded" => {
            let entry = params.get("entry")?;
            let level = match entry.get("level").and_then(Value::as_str).unwrap_or("info") {
                "error" => ConsoleLevel::Error,
                "warning" => ConsoleLevel::Warning,
                _ => ConsoleLevel::Log,
            };
            let text = entry
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(ConsoleEntry { level, text })
        }
        _ => None,
    }
}

/// Push `item` onto `buf`, evicting the oldest entry first once `buf` is
/// already at `cap` — the buffering policy [`CONSOLE_BUFFER_CAP`] enforces.
/// Pure so the eviction behavior is unit-tested without a live socket.
pub fn push_capped<T>(buf: &mut Vec<T>, item: T, cap: usize) {
    if cap == 0 {
        return;
    }
    if buf.len() >= cap {
        buf.remove(0);
    }
    buf.push(item);
}

/// Counts + first error text distilled from a slice of [`ConsoleEntry`] — the
/// shape `ui_screenshot`'s fold-in note (M20 D5) needs without dumping every
/// entry into a screenshot's text part.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConsoleSummary {
    pub error_count: usize,
    pub warning_count: usize,
    pub first_error: Option<String>,
}

/// Summarize buffered console entries: counts by level and the first error's
/// text, if any. Pure so `ui_screenshot`'s fold-in text is unit-tested without
/// a live transport.
#[must_use]
pub fn summarize_console(entries: &[ConsoleEntry]) -> ConsoleSummary {
    ConsoleSummary {
        error_count: entries
            .iter()
            .filter(|e| e.level == ConsoleLevel::Error)
            .count(),
        warning_count: entries
            .iter()
            .filter(|e| e.level == ConsoleLevel::Warning)
            .count(),
        first_error: entries
            .iter()
            .find(|e| e.level == ConsoleLevel::Error)
            .map(|e| e.text.clone()),
    }
}

/// The exact whitelist of computed-style properties [`inspect_element`]
/// surfaces for a selector (M20 D5) — never the ~300-property full dump
/// `CSS.getComputedStyleForNode` returns. Fixed order; a missing property
/// (rare — these are all universally-computed CSS properties) is simply
/// absent from the output rather than padded.
pub const CURATED_STYLE_PROPS: &[&str] = &[
    "display",
    "position",
    "overflow",
    "width",
    "height",
    "font-family",
];

/// One element's CDP box model (`DOM.getBoxModel`): overall width/height plus
/// the four nested quads (each a flat `[x0,y0,x1,y1,x2,y2,x3,y3]` — the
/// corners of the content/padding/border/margin boxes, in that nesting
/// order).
#[derive(Debug, Clone, PartialEq)]
pub struct BoxModel {
    pub width: f64,
    pub height: f64,
    pub content: Vec<f64>,
    pub padding: Vec<f64>,
    pub border: Vec<f64>,
    pub margin: Vec<f64>,
}

/// [`inspect_element`]'s result: the selector queried, its box model, and the
/// [`CURATED_STYLE_PROPS`] subset of its computed style, in that fixed order.
#[derive(Debug, Clone, PartialEq)]
pub struct ElementInspection {
    pub selector: String,
    pub box_model: BoxModel,
    pub computed_style: Vec<(String, String)>,
}

fn parse_quad(model: &Value, key: &str) -> Vec<f64> {
    model
        .get(key)
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default()
}

/// Parse a `DOM.getBoxModel` result's nested `model` object. Pure so it's
/// unit-tested against canned CDP JSON, no live transport.
fn parse_box_model(result: &Value) -> Result<BoxModel, BrowserError> {
    let model = result
        .get("model")
        .ok_or_else(|| BrowserError::Driver("DOM.getBoxModel returned no model".into()))?;
    Ok(BoxModel {
        width: model.get("width").and_then(Value::as_f64).unwrap_or(0.0),
        height: model.get("height").and_then(Value::as_f64).unwrap_or(0.0),
        content: parse_quad(model, "content"),
        padding: parse_quad(model, "padding"),
        border: parse_quad(model, "border"),
        margin: parse_quad(model, "margin"),
    })
}

/// Filter a `CSS.getComputedStyleForNode` result's `computedStyle` array down
/// to [`CURATED_STYLE_PROPS`], in that fixed order. Pure so the curation is
/// unit-tested against canned CDP JSON — the acceptance property is that this
/// NEVER returns the full dump.
fn curate_computed_style(result: &Value) -> Vec<(String, String)> {
    let entries = result
        .get("computedStyle")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut by_name = std::collections::HashMap::new();
    for entry in &entries {
        if let (Some(name), Some(value)) = (
            entry.get("name").and_then(Value::as_str),
            entry.get("value").and_then(Value::as_str),
        ) {
            by_name.insert(name.to_string(), value.to_string());
        }
    }
    CURATED_STYLE_PROPS
        .iter()
        .filter_map(|prop| by_name.get(*prop).map(|v| ((*prop).to_string(), v.clone())))
        .collect()
}

/// Fetch `selector`'s box model + curated computed style over `transport`
/// (M20 D5's `ui_inspect`): `DOM.getDocument` → `DOM.querySelector` →
/// `DOM.getBoxModel` + `CSS.getComputedStyleForNode`. The selector rides as a
/// plain JSON command parameter here (not spliced into a JS expression
/// string), so there is no injection surface to guard the way [`js_string`]
/// guards the JS-evaluate action helpers below.
pub async fn inspect_element(
    transport: &dyn CdpTransport,
    selector: &str,
) -> Result<ElementInspection, BrowserError> {
    transport.send("DOM.enable", json!({})).await?;
    transport.send("CSS.enable", json!({})).await?;
    let doc = transport
        .send("DOM.getDocument", json!({ "depth": 0 }))
        .await?;
    let root_id = doc
        .get("root")
        .and_then(|r| r.get("nodeId"))
        .and_then(Value::as_u64)
        .ok_or_else(|| BrowserError::Driver("DOM.getDocument returned no root nodeId".into()))?;
    let found = transport
        .send(
            "DOM.querySelector",
            json!({ "nodeId": root_id, "selector": selector }),
        )
        .await?;
    let node_id = found
        .get("nodeId")
        .and_then(Value::as_u64)
        .filter(|id| *id != 0)
        .ok_or_else(|| BrowserError::Driver(format!("no element for selector `{selector}`")))?;
    let box_out = transport
        .send("DOM.getBoxModel", json!({ "nodeId": node_id }))
        .await?;
    let box_model = parse_box_model(&box_out)?;
    let style_out = transport
        .send("CSS.getComputedStyleForNode", json!({ "nodeId": node_id }))
        .await?;
    let computed_style = curate_computed_style(&style_out);
    Ok(ElementInspection {
        selector: selector.to_string(),
        box_model,
        computed_style,
    })
}

/// The CDP transport seam: issue a command, await its result; surface the
/// redirect hops + final status the navigation observed.
///
/// A CDP command is `{"id":N,"method":M,"params":P}`; the matching response is
/// `{"id":N,"result":R}` or `{"id":N,"error":{...}}`. [`send`](CdpTransport::send)
/// returns `R` (the `result` object) or maps the error. The redirect hops and
/// main-document status come from `Network.*` events the transport observes
/// out-of-band, so the orchestration can re-guard every hop (SSRF).
#[async_trait]
pub trait CdpTransport: Send + Sync {
    /// Issue one CDP command and await its `result` object.
    async fn send(&self, method: &str, params: Value) -> Result<Value, BrowserError>;

    /// Block until the page's load event fires or `timeout` elapses. A timeout
    /// is not fatal on its own — a slow page may still be renderable — so the
    /// live impl logs and returns `Ok(())`; a hard transport failure is `Err`.
    async fn wait_for_load(&self, timeout: Duration) -> Result<(), BrowserError>;

    /// Redirect-hop URLs the main-frame navigation followed, in order,
    /// EXCLUDING the initial target (which the async pre-flight already
    /// guarded). Each entry is re-checked by [`crate::driver::render`].
    fn redirect_hops(&self) -> Vec<String>;

    /// Final HTTP status of the main document, if the transport observed one.
    fn main_status(&self) -> Option<u16>;

    /// Buffered console entries observed on this transport since it connected
    /// (M20 D5): `console.*` calls, uncaught JS exceptions, and
    /// `Log.entryAdded` diagnostics, capped at [`CONSOLE_BUFFER_CAP`].
    /// Requires `Runtime.enable` + `Log.enable` to have been sent first (the
    /// in-container `ui_screenshot`/`ui_inspect` orchestration does this —
    /// see `crate::incontainer::capture` / `crate::incontainer::inspect`).
    /// Default empty so every existing mock transport in the test suite
    /// keeps compiling unchanged; only the live [`WsCdpTransport`] overrides
    /// it.
    fn console_entries(&self) -> Vec<ConsoleEntry> {
        Vec::new()
    }
}

/// The concrete CDP-backed [`BrowserDriver`]. Generic over the transport so
/// tests drive it with a mock and production drives it with [`WsCdpTransport`].
pub struct CdpBrowserDriver {
    transport: Box<dyn CdpTransport>,
    /// Host-side directory screenshots are written into.
    screenshot_dir: PathBuf,
    /// Navigation timeout handed to [`CdpTransport::wait_for_load`].
    nav_timeout: Duration,
    /// Capture fidelity (M20 D2): viewport preset, full-page vs. windowed,
    /// format/quality. Defaults to [`CaptureOptions::legacy_full_page`] — the
    /// pre-D2 hard-coded behavior — so [`Self::new`] stays byte-compatible
    /// with every caller that doesn't opt into [`Self::with_capture`].
    capture_opts: CaptureOptions,
}

/// Monotonic counter so concurrent screenshots never collide on a filename.
static SHOT_SEQ: AtomicU64 = AtomicU64::new(0);

impl CdpBrowserDriver {
    /// Build a driver over `transport`, writing screenshots into
    /// `screenshot_dir`, with `nav_timeout` for the load wait.
    #[must_use]
    pub fn new(
        transport: Box<dyn CdpTransport>,
        screenshot_dir: impl Into<PathBuf>,
        nav_timeout: Duration,
    ) -> Self {
        Self {
            transport,
            screenshot_dir: screenshot_dir.into(),
            nav_timeout,
            capture_opts: CaptureOptions::legacy_full_page(),
        }
    }

    /// Opt into non-default capture fidelity (M20 D2): a viewport preset,
    /// full-page vs. windowed, and/or image format/quality. Every caller that
    /// doesn't call this keeps [`Self::new`]'s byte-identical pre-D2 default.
    #[must_use]
    pub fn with_capture(mut self, opts: CaptureOptions) -> Self {
        self.capture_opts = opts;
        self
    }

    /// Evaluate a JS expression and return its value coerced to a string
    /// (`None` when the result had no string value).
    async fn eval_string(&self, expr: &str) -> Result<Option<String>, BrowserError> {
        let out = self
            .transport
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": expr,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await?;
        if let Some(details) = out.get("exceptionDetails") {
            return Err(BrowserError::Driver(format!(
                "Runtime.evaluate threw: {}",
                details
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown exception")
            )));
        }
        Ok(out
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    /// Capture a screenshot per `self.capture_opts`, write it under
    /// `screenshot_dir`, return the host-side path. `self.capture_opts`
    /// defaults to [`CaptureOptions::legacy_full_page`] (set by [`Self::new`]),
    /// which reproduces this method's pre-D2 hard-coded params byte-for-byte
    /// and skips the viewport override entirely (see
    /// [`capture::apply_viewport`]).
    async fn capture_screenshot(&self) -> Result<String, BrowserError> {
        capture::apply_viewport(self.transport.as_ref(), self.capture_opts.viewport).await?;
        let out = self
            .transport
            .send("Page.captureScreenshot", self.capture_opts.capture_params())
            .await?;
        let b64 = out.get("data").and_then(Value::as_str).ok_or_else(|| {
            BrowserError::Driver("Page.captureScreenshot returned no data".into())
        })?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| BrowserError::Driver(format!("screenshot base64 decode: {e}")))?;
        let seq = SHOT_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let ext = self.capture_opts.format.file_extension();
        let path = self
            .screenshot_dir
            .join(format!("render-{nanos}-{seq}.{ext}"));
        tokio::fs::create_dir_all(&self.screenshot_dir)
            .await
            .map_err(|e| BrowserError::Driver(format!("screenshot dir create: {e}")))?;
        tokio::fs::write(&path, &bytes)
            .await
            .map_err(|e| BrowserError::Driver(format!("screenshot write: {e}")))?;
        Ok(path.to_string_lossy().into_owned())
    }
}

/// Flatten a CDP `Accessibility.getFullAXTree` result into a compact,
/// human-/model-readable text snapshot (`role: name` per named node).
#[must_use]
pub fn serialize_ax_tree(tree: &Value) -> String {
    let mut lines = Vec::new();
    if let Some(nodes) = tree.get("nodes").and_then(Value::as_array) {
        for node in nodes {
            let role = node
                .get("role")
                .and_then(|r| r.get("value"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let name = node
                .get("name")
                .and_then(|n| n.get("value"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if role.is_empty() && name.is_empty() {
                continue;
            }
            if name.is_empty() {
                lines.push(role.to_string());
            } else {
                lines.push(format!("{role}: {name}"));
            }
        }
    }
    lines.join("\n")
}

/// Extract the ordered redirect-hop URLs from a slice of raw
/// `Network.requestWillBeSent` event params. A hop is a main-document request
/// (`type == "Document"`) that carries a `redirectResponse` — i.e. the browser
/// followed a 30x. The recorded URL is the request's own `url` (where the
/// redirect pointed). Pure so the live transport's event handling is testable.
#[must_use]
pub fn redirect_hops_from_events(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.get("redirectResponse").is_some())
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("Document"))
        .filter_map(|e| {
            e.get("request")
                .and_then(|r| r.get("url"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// Extract the main-document HTTP status from a slice of raw
/// `Network.responseReceived` event params (the first Document response wins).
#[must_use]
pub fn main_status_from_events(events: &[Value]) -> Option<u16> {
    events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("Document"))
        .find_map(|e| {
            e.get("response")
                .and_then(|r| r.get("status"))
                .and_then(Value::as_u64)
                .and_then(|s| u16::try_from(s).ok())
        })
}

/// Encode a Rust string as a JavaScript string literal via JSON (a JSON string
/// is a valid JS string). This is the injection guard for embedding a
/// caller-supplied CSS selector / typed text into a `Runtime.evaluate`
/// expression — the value can never break out of the string and inject code.
fn js_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

impl CdpBrowserDriver {
    /// Enable the CDP domains the interactive/render paths need (idempotent).
    async fn enable_domains(&self) -> Result<(), BrowserError> {
        self.transport.send("Page.enable", json!({})).await?;
        self.transport.send("Network.enable", json!({})).await?;
        Ok(())
    }

    /// Read the settled navigation state (final URL, redirect hops, status) so
    /// the interactive orchestration can re-guard it. `fallback_url` is used
    /// only if `document.location.href` cannot be read.
    async fn observe_navigation(&self, fallback_url: &str) -> Result<Navigation, BrowserError> {
        let final_url = self
            .eval_string("document.location.href")
            .await?
            .unwrap_or_else(|| fallback_url.to_string());
        Ok(Navigation {
            redirect_chain: self.transport.redirect_hops(),
            final_url,
            status: self.transport.main_status(),
        })
    }

    /// Evaluate an expression purely for effect, surfacing a thrown JS error
    /// (e.g. "no element for selector") as a [`BrowserError::Driver`].
    async fn eval_effect(&self, expr: &str) -> Result<(), BrowserError> {
        let out = self
            .transport
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": expr,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await?;
        if let Some(details) = out.get("exceptionDetails") {
            return Err(BrowserError::Driver(format!(
                "interactive action failed: {}",
                details
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown exception")
            )));
        }
        Ok(())
    }

    /// Click the first element matching `selector` (scrolled into view first).
    async fn do_click(&self, selector: &str) -> Result<(), BrowserError> {
        let expr = format!(
            "(function(){{const el=document.querySelector({sel});\
             if(!el){{throw new Error(\"no element for selector \"+{sel});}}\
             el.scrollIntoView({{block:'center'}});el.click();return true;}})()",
            sel = js_string(selector)
        );
        self.eval_effect(&expr).await
    }

    /// Focus the element matching `selector` and set its value/text to `text`,
    /// dispatching `input` + `change` so page handlers fire.
    async fn do_type(&self, selector: &str, text: &str) -> Result<(), BrowserError> {
        let expr = format!(
            "(function(){{const el=document.querySelector({sel});\
             if(!el){{throw new Error(\"no element for selector \"+{sel});}}\
             el.focus();if('value' in el){{el.value={txt};}}else{{el.textContent={txt};}}\
             el.dispatchEvent(new Event('input',{{bubbles:true}}));\
             el.dispatchEvent(new Event('change',{{bubbles:true}}));return true;}})()",
            sel = js_string(selector),
            txt = js_string(text)
        );
        self.eval_effect(&expr).await
    }

    /// Scroll: element into view if `selector` is set, else window by `(dx,dy)`.
    async fn do_scroll(
        &self,
        selector: Option<&str>,
        dx: f64,
        dy: f64,
    ) -> Result<(), BrowserError> {
        let expr = match selector {
            Some(sel) => format!(
                "(function(){{const el=document.querySelector({sel});\
                 if(!el){{throw new Error(\"no element for selector \"+{sel});}}\
                 el.scrollIntoView({{block:'center'}});return true;}})()",
                sel = js_string(sel)
            ),
            // dx/dy are validated finite upstream; format as plain JS numbers.
            None => format!("(function(){{window.scrollBy({dx},{dy});return true;}})()"),
        };
        self.eval_effect(&expr).await
    }

    /// Poll until an element matching `selector` exists or the (clamped) budget
    /// elapses.
    async fn do_wait_for_selector(
        &self,
        selector: &str,
        timeout_ms: Option<u64>,
    ) -> Result<(), BrowserError> {
        let budget = Duration::from_millis(
            timeout_ms
                .unwrap_or(crate::interactive::DEFAULT_WAIT_MS)
                .clamp(1, crate::interactive::MAX_WAIT_MS),
        );
        let expr = format!("!!document.querySelector({sel})", sel = js_string(selector));
        let deadline = std::time::Instant::now() + budget;
        loop {
            let out = self
                .transport
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
            if std::time::Instant::now() >= deadline {
                return Err(BrowserError::Driver(format!(
                    "wait_for_selector `{selector}` timed out after {budget:?}"
                )));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Read the final artifact for `mode` (no navigation, no mutation). Shared
    /// by the read-only render and the interactive read step.
    async fn read_artifact(&self, mode: RenderMode) -> Result<RenderedArtifact, BrowserError> {
        match mode {
            RenderMode::Screenshot => Ok(RenderedArtifact::ScreenshotPath(
                self.capture_screenshot().await?,
            )),
            RenderMode::DomText => {
                let text = self
                    .eval_string("document.body ? document.body.innerText : ''")
                    .await?
                    .unwrap_or_default();
                Ok(RenderedArtifact::Text(text))
            }
            RenderMode::AriaSnapshot => {
                let tree = self
                    .transport
                    .send("Accessibility.getFullAXTree", json!({}))
                    .await?;
                Ok(RenderedArtifact::Text(serialize_ax_tree(&tree)))
            }
        }
    }
}

#[async_trait]
impl crate::interactive::InteractiveDriver for CdpBrowserDriver {
    async fn navigate(&self, url: &str) -> Result<Navigation, BrowserError> {
        self.enable_domains().await?;
        let nav = self
            .transport
            .send("Page.navigate", json!({ "url": url }))
            .await?;
        if let Some(err) = nav.get("errorText").and_then(Value::as_str) {
            if !err.is_empty() {
                return Err(BrowserError::Driver(format!(
                    "navigation to `{url}` failed: {err}"
                )));
            }
        }
        self.transport.wait_for_load(self.nav_timeout).await?;
        self.observe_navigation(url).await
    }

    async fn act(
        &self,
        action: &crate::interactive::InteractiveAction,
    ) -> Result<Navigation, BrowserError> {
        use crate::interactive::InteractiveAction as A;
        match action {
            A::Click { selector } => self.do_click(selector).await?,
            A::Type { selector, text } => self.do_type(selector, text).await?,
            A::Scroll { selector, dx, dy } => {
                self.do_scroll(selector.as_deref(), *dx, *dy).await?;
            }
            A::WaitForSelector {
                selector,
                timeout_ms,
            } => self.do_wait_for_selector(selector, *timeout_ms).await?,
        }
        // An action may have navigated (click/submit/location change). Give the
        // load a best-effort chance to settle, then observe so the caller can
        // re-guard. (`wait_for_selector` is the caller's explicit settle knob
        // for a navigation that outlives this best-effort wait.)
        self.transport.wait_for_load(self.nav_timeout).await?;
        // No fallback URL: on a failed href read we keep an empty final_url,
        // which the guard treats as an invalid (non-public) target and blocks —
        // fail closed rather than trust an unknown post-action location.
        self.observe_navigation("").await
    }

    async fn read(&self, mode: RenderMode) -> Result<RenderedArtifact, BrowserError> {
        self.read_artifact(mode).await
    }
}

#[async_trait]
impl BrowserDriver for CdpBrowserDriver {
    async fn render(&self, url: &str, mode: RenderMode) -> Result<DriverRender, BrowserError> {
        // `navigate` enables the Page + Network domains (redirect + status
        // observation for the SSRF re-guard), drives the navigation, waits for
        // load, and observes the settled state (final URL falls back to `url`).
        // `read_artifact` then reads the requested surface — the same two steps
        // the interactive path composes, shared verbatim.
        let navigation = self.navigate(url).await?;
        let artifact = self.read_artifact(mode).await?;
        Ok(DriverRender {
            navigation,
            artifact,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Live transport: a real WebSocket CDP session against the child container.
// Compiled always; invoked only behind the opt-in gate + a live container.
// ─────────────────────────────────────────────────────────────────────────────

mod ws {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use futures::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio::sync::{Notify, oneshot};
    use tokio_tungstenite::tungstenite::Message;

    use super::{CdpTransport, ConsoleEntry, main_status_from_events, redirect_hops_from_events};
    use crate::error::BrowserError;

    type SinkHalf = futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >;

    /// Shared state the reader task fills and `send`/accessors drain.
    #[derive(Default)]
    struct Shared {
        pending: HashMap<u64, oneshot::Sender<Result<Value, BrowserError>>>,
        request_events: Vec<Value>,
        response_events: Vec<Value>,
        load_fired: bool,
        /// M20 D5: buffered console entries for this tab, capped at
        /// `super::CONSOLE_BUFFER_CAP`.
        console_events: Vec<ConsoleEntry>,
    }

    /// A live CDP transport over a `tokio-tungstenite` WebSocket to the child
    /// container's Chromium. Responses are matched to commands by `id`;
    /// `Network.*` events feed the redirect/status observation the SSRF
    /// re-guard depends on.
    pub struct WsCdpTransport {
        sink: tokio::sync::Mutex<SinkHalf>,
        shared: std::sync::Arc<Mutex<Shared>>,
        load_notify: std::sync::Arc<Notify>,
        next_id: AtomicU64,
        call_timeout: Duration,
        _reader: tokio::task::JoinHandle<()>,
    }

    impl WsCdpTransport {
        /// Connect to a page-level CDP WebSocket URL
        /// (`ws://<container-ip>:<port>/devtools/page/<id>`) and spawn the
        /// reader task. `call_timeout` bounds each command's response wait.
        pub async fn connect(ws_url: &str, call_timeout: Duration) -> Result<Self, BrowserError> {
            let (socket, _resp) = tokio_tungstenite::connect_async(ws_url)
                .await
                .map_err(|e| BrowserError::Container(format!("CDP websocket connect: {e}")))?;
            let (sink, mut stream) = socket.split();

            let shared = std::sync::Arc::new(Mutex::new(Shared::default()));
            let load_notify = std::sync::Arc::new(Notify::new());
            let reader_shared = std::sync::Arc::clone(&shared);
            let reader_notify = std::sync::Arc::clone(&load_notify);

            let reader = tokio::spawn(async move {
                while let Some(next) = stream.next().await {
                    let Ok(Message::Text(text)) = next else {
                        // Ignore ping/pong/binary; stop on a hard error or close.
                        if matches!(next, Err(_) | Ok(Message::Close(_))) {
                            break;
                        }
                        continue;
                    };
                    let Ok(msg) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    dispatch(&reader_shared, &reader_notify, &msg);
                }
                // On disconnect, fail every still-pending caller.
                let mut guard = reader_shared.lock().unwrap();
                for (_, tx) in guard.pending.drain() {
                    let _ = tx.send(Err(BrowserError::Driver(
                        "CDP websocket closed before response".into(),
                    )));
                }
            });

            Ok(Self {
                sink: tokio::sync::Mutex::new(sink),
                shared,
                load_notify,
                next_id: AtomicU64::new(1),
                call_timeout,
                _reader: reader,
            })
        }
    }

    /// Route one inbound CDP message: a response resolves its pending caller;
    /// an event updates the observation state.
    fn dispatch(shared: &Mutex<Shared>, load_notify: &Notify, msg: &Value) {
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            let mut guard = shared.lock().unwrap();
            if let Some(tx) = guard.pending.remove(&id) {
                if let Some(err) = msg.get("error") {
                    let m = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown CDP error");
                    let _ = tx.send(Err(BrowserError::Driver(format!("CDP error: {m}"))));
                } else {
                    let _ = tx.send(Ok(msg.get("result").cloned().unwrap_or_else(|| json!({}))));
                }
            }
            return;
        }
        // Event.
        match msg.get("method").and_then(Value::as_str) {
            Some("Network.requestWillBeSent") => {
                if let Some(p) = msg.get("params") {
                    shared.lock().unwrap().request_events.push(p.clone());
                }
            }
            Some("Network.responseReceived") => {
                if let Some(p) = msg.get("params") {
                    shared.lock().unwrap().response_events.push(p.clone());
                }
            }
            Some("Page.loadEventFired") => {
                shared.lock().unwrap().load_fired = true;
                load_notify.notify_waiters();
            }
            // M20 D5: console.* calls, uncaught exceptions, and browser-level
            // Log entries. Buffered (capped) so `ui_screenshot`/`ui_inspect`
            // can read them back after the navigation settles.
            Some(
                method
                @ ("Runtime.consoleAPICalled" | "Runtime.exceptionThrown" | "Log.entryAdded"),
            ) => {
                if let Some(params) = msg.get("params") {
                    if let Some(entry) = super::parse_console_event(method, params) {
                        let mut guard = shared.lock().unwrap();
                        super::push_capped(
                            &mut guard.console_events,
                            entry,
                            super::CONSOLE_BUFFER_CAP,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    #[async_trait]
    impl CdpTransport for WsCdpTransport {
        async fn send(&self, method: &str, params: Value) -> Result<Value, BrowserError> {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            self.shared.lock().unwrap().pending.insert(id, tx);

            let frame = json!({ "id": id, "method": method, "params": params }).to_string();
            {
                let mut sink = self.sink.lock().await;
                sink.send(Message::Text(frame))
                    .await
                    .map_err(|e| BrowserError::Driver(format!("CDP send `{method}`: {e}")))?;
            }

            match tokio::time::timeout(self.call_timeout, rx).await {
                Ok(Ok(res)) => res,
                Ok(Err(_)) => Err(BrowserError::Driver(format!(
                    "CDP `{method}`: response channel dropped"
                ))),
                Err(_) => {
                    self.shared.lock().unwrap().pending.remove(&id);
                    Err(BrowserError::Driver(format!(
                        "CDP `{method}`: timed out after {:?}",
                        self.call_timeout
                    )))
                }
            }
        }

        async fn wait_for_load(&self, timeout: Duration) -> Result<(), BrowserError> {
            if self.shared.lock().unwrap().load_fired {
                return Ok(());
            }
            // A load timeout is not fatal — render what settled.
            let _ = tokio::time::timeout(timeout, self.load_notify.notified()).await;
            Ok(())
        }

        fn redirect_hops(&self) -> Vec<String> {
            let guard = self.shared.lock().unwrap();
            redirect_hops_from_events(&guard.request_events)
        }

        fn main_status(&self) -> Option<u16> {
            let guard = self.shared.lock().unwrap();
            main_status_from_events(&guard.response_events)
        }

        fn console_entries(&self) -> Vec<ConsoleEntry> {
            self.shared.lock().unwrap().console_events.clone()
        }
    }
}

pub use ws::WsCdpTransport;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::test_guards::RecordingGuard;
    use crate::render::{Provenance, RenderRequest};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A scripted transport: canned `result` per method, plus configurable
    /// redirect hops + status. Exercises the whole driver command sequence
    /// without a live browser.
    #[derive(Default)]
    struct MockTransport {
        results: HashMap<String, Value>,
        calls: Mutex<Vec<String>>,
        hops: Vec<String>,
        status: Option<u16>,
        load_waited: Mutex<bool>,
    }

    impl MockTransport {
        fn with(mut self, method: &str, result: Value) -> Self {
            self.results.insert(method.to_string(), result);
            self
        }
        fn hops(mut self, hops: &[&str]) -> Self {
            self.hops = hops.iter().map(|s| (*s).to_string()).collect();
            self
        }
        fn status(mut self, status: u16) -> Self {
            self.status = Some(status);
            self
        }
    }

    #[async_trait]
    impl CdpTransport for MockTransport {
        async fn send(&self, method: &str, _params: Value) -> Result<Value, BrowserError> {
            self.calls.lock().unwrap().push(method.to_string());
            Ok(self
                .results
                .get(method)
                .cloned()
                .unwrap_or_else(|| json!({})))
        }
        async fn wait_for_load(&self, _timeout: Duration) -> Result<(), BrowserError> {
            *self.load_waited.lock().unwrap() = true;
            Ok(())
        }
        fn redirect_hops(&self) -> Vec<String> {
            self.hops.clone()
        }
        fn main_status(&self) -> Option<u16> {
            self.status
        }
    }

    fn tmp_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "copperclaw-browser-test-{}",
            SHOT_SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[tokio::test]
    async fn dom_text_render_runs_full_sequence() {
        let transport = MockTransport::default()
            .with(
                "Runtime.evaluate",
                json!({ "result": { "value": "hello body" } }),
            )
            .status(200);
        let driver = CdpBrowserDriver::new(Box::new(transport), tmp_dir(), DEFAULT_NAV_TIMEOUT);
        let out = driver
            .render("https://example.com", RenderMode::DomText)
            .await
            .unwrap();
        // eval_string is used for both final_url and body; both return the same
        // canned value here.
        assert_eq!(
            out.artifact,
            RenderedArtifact::Text("hello body".to_string())
        );
        assert_eq!(out.navigation.status, Some(200));
        assert!(out.navigation.redirect_chain.is_empty());
    }

    #[tokio::test]
    async fn screenshot_render_writes_png_file() {
        // 1x1 transparent PNG, base64.
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let dir = tmp_dir();
        let transport = MockTransport::default()
            .with(
                "Runtime.evaluate",
                json!({ "result": { "value": "https://example.com/" } }),
            )
            .with("Page.captureScreenshot", json!({ "data": png_b64 }));
        let driver = CdpBrowserDriver::new(Box::new(transport), &dir, DEFAULT_NAV_TIMEOUT);
        let out = driver
            .render("https://example.com", RenderMode::Screenshot)
            .await
            .unwrap();
        let path = match out.artifact {
            RenderedArtifact::ScreenshotPath(p) => p,
            RenderedArtifact::Text(t) => panic!("expected screenshot path, got text {t:?}"),
        };
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[1..4], b"PNG", "wrote real PNG bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn navigation_error_text_is_surfaced() {
        let transport = MockTransport::default().with(
            "Page.navigate",
            json!({ "errorText": "net::ERR_NAME_NOT_RESOLVED" }),
        );
        let driver = CdpBrowserDriver::new(Box::new(transport), tmp_dir(), DEFAULT_NAV_TIMEOUT);
        let err = driver
            .render("https://nope.invalid", RenderMode::DomText)
            .await
            .unwrap_err();
        assert!(matches!(err, BrowserError::Driver(_)));
    }

    #[tokio::test]
    async fn eval_exception_is_surfaced() {
        let transport = MockTransport::default().with(
            "Runtime.evaluate",
            json!({ "exceptionDetails": { "text": "boom" } }),
        );
        let driver = CdpBrowserDriver::new(Box::new(transport), tmp_dir(), DEFAULT_NAV_TIMEOUT);
        let err = driver
            .render("https://example.com", RenderMode::DomText)
            .await
            .unwrap_err();
        match err {
            BrowserError::Driver(m) => assert!(m.contains("boom"), "{m}"),
            other => panic!("expected driver error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn aria_snapshot_flattens_tree() {
        let transport = MockTransport::default()
            .with(
                "Runtime.evaluate",
                json!({ "result": { "value": "https://example.com/" } }),
            )
            .with(
                "Accessibility.getFullAXTree",
                json!({ "nodes": [
                    { "role": { "value": "button" }, "name": { "value": "Submit" } },
                    { "role": { "value": "heading" }, "name": { "value": "Title" } },
                    { "role": { "value": "generic" }, "name": { "value": "" } }
                ] }),
            );
        let driver = CdpBrowserDriver::new(Box::new(transport), tmp_dir(), DEFAULT_NAV_TIMEOUT);
        let out = driver
            .render("https://example.com", RenderMode::AriaSnapshot)
            .await
            .unwrap();
        assert_eq!(
            out.artifact,
            RenderedArtifact::Text("button: Submit\nheading: Title\ngeneric".to_string())
        );
    }

    #[tokio::test]
    async fn driver_reports_redirect_chain_for_orchestration_reguard() {
        // The driver reports hops; the orchestration re-guards them. Prove the
        // SSRF re-guard still fires against a CDP-reported chain.
        let transport = MockTransport::default()
            .with(
                "Runtime.evaluate",
                json!({ "result": { "value": "http://169.254.169.254/" } }),
            )
            .hops(&["http://169.254.169.254/latest/"]);
        let driver = CdpBrowserDriver::new(Box::new(transport), tmp_dir(), DEFAULT_NAV_TIMEOUT);
        let guard = RecordingGuard::blocking(&["169.254.169.254"]);
        let err = crate::driver::render(
            &RenderRequest {
                url: "https://public.example/redir".into(),
                mode: RenderMode::DomText,
                timeout_secs: None,
                capture: CaptureOptions::default(),
            },
            &guard,
            &driver,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, BrowserError::Blocked(_)));
        assert_eq!(guard.redirect_calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn driver_happy_path_through_orchestration_is_untrusted() {
        let transport = MockTransport::default()
            .with(
                "Runtime.evaluate",
                json!({ "result": { "value": "https://example.com/" } }),
            )
            .status(200)
            .hops(&["https://hop.example/"]);
        let driver = CdpBrowserDriver::new(Box::new(transport), tmp_dir(), DEFAULT_NAV_TIMEOUT);
        let guard = RecordingGuard::default();
        let out = crate::driver::render(
            &RenderRequest {
                url: "https://example.com".into(),
                mode: RenderMode::DomText,
                timeout_secs: None,
                capture: CaptureOptions::default(),
            },
            &guard,
            &driver,
        )
        .await
        .unwrap();
        assert_eq!(out.provenance, Provenance::Untrusted);
        assert_eq!(out.status, Some(200));
        assert_eq!(guard.redirect_calls.lock().unwrap().len(), 1);
    }

    // ── pure event-parsing helpers ───────────────────────────────────────

    #[test]
    fn redirect_hops_filters_to_main_document_redirects() {
        let events = vec![
            // A main-document redirect hop — kept.
            json!({ "type": "Document", "redirectResponse": { "status": 302 },
                    "request": { "url": "http://internal/" } }),
            // A subresource redirect — dropped (not the main frame nav).
            json!({ "type": "Image", "redirectResponse": { "status": 302 },
                    "request": { "url": "http://cdn/img" } }),
            // The initial document request (no redirectResponse) — dropped.
            json!({ "type": "Document", "request": { "url": "http://start/" } }),
        ];
        assert_eq!(
            redirect_hops_from_events(&events),
            vec!["http://internal/".to_string()]
        );
    }

    #[test]
    fn main_status_takes_first_document_response() {
        let events = vec![
            json!({ "type": "Image", "response": { "status": 200 } }),
            json!({ "type": "Document", "response": { "status": 404 } }),
            json!({ "type": "Document", "response": { "status": 500 } }),
        ];
        assert_eq!(main_status_from_events(&events), Some(404));
    }

    #[test]
    fn ax_tree_serialization_skips_empty_nodes() {
        let tree = json!({ "nodes": [
            { "role": { "value": "" }, "name": { "value": "" } },
            { "role": { "value": "link" }, "name": { "value": "Home" } }
        ] });
        assert_eq!(serialize_ax_tree(&tree), "link: Home");
    }

    // ── interactive driver (Phase 5b) — CDP command sequence ─────────────
    //
    // These exercise the concrete `InteractiveDriver` impl against a scripted
    // transport that inspects the `Runtime.evaluate` expression so click / type
    // / scroll / wait / read each get a plausible reply. They prove the CDP
    // command sequence WITHOUT a live Chromium (the acceptance's mock-CDP tier).

    use crate::interactive::{InteractRequest, InteractiveAction, InteractiveDriver, interact};

    /// A transport that replies to `Runtime.evaluate` by inspecting the
    /// expression: the location read yields `final_url`, a `!!querySelector`
    /// presence poll yields `true`, an `innerText` read yields `dom`, and any
    /// other effect expression yields an empty (no-exception) result. Records
    /// every method + evaluate-expression it saw.
    struct ScriptedTransport {
        final_url: String,
        dom: String,
        hops: Vec<String>,
        status: Option<u16>,
        calls: Mutex<Vec<String>>,
        exprs: Mutex<Vec<String>>,
    }

    impl ScriptedTransport {
        fn new(final_url: &str, dom: &str) -> Self {
            Self {
                final_url: final_url.to_string(),
                dom: dom.to_string(),
                hops: Vec::new(),
                status: Some(200),
                calls: Mutex::new(Vec::new()),
                exprs: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl CdpTransport for ScriptedTransport {
        async fn send(&self, method: &str, params: Value) -> Result<Value, BrowserError> {
            self.calls.lock().unwrap().push(method.to_string());
            if method == "Runtime.evaluate" {
                let expr = params
                    .get("expression")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.exprs.lock().unwrap().push(expr.clone());
                if expr.contains("location.href") {
                    return Ok(json!({ "result": { "value": self.final_url } }));
                }
                if expr.starts_with("!!document.querySelector") {
                    return Ok(json!({ "result": { "value": true } }));
                }
                if expr.contains("innerText") {
                    return Ok(json!({ "result": { "value": self.dom } }));
                }
                // A click/type/scroll effect expression: no exception → success.
                return Ok(json!({ "result": { "value": true } }));
            }
            Ok(json!({}))
        }
        async fn wait_for_load(&self, _timeout: Duration) -> Result<(), BrowserError> {
            Ok(())
        }
        fn redirect_hops(&self) -> Vec<String> {
            self.hops.clone()
        }
        fn main_status(&self) -> Option<u16> {
            self.status
        }
    }

    fn scripted_driver(final_url: &str, dom: &str) -> CdpBrowserDriver {
        CdpBrowserDriver::new(
            Box::new(ScriptedTransport::new(final_url, dom)),
            tmp_dir(),
            DEFAULT_NAV_TIMEOUT,
        )
    }

    /// A thin adapter forwarding to a shared [`ScriptedTransport`] Arc so a test
    /// can inspect the recorded `Runtime.evaluate` expressions after driving it.
    struct Forward(std::sync::Arc<ScriptedTransport>);

    #[async_trait]
    impl CdpTransport for Forward {
        async fn send(&self, method: &str, params: Value) -> Result<Value, BrowserError> {
            self.0.send(method, params).await
        }
        async fn wait_for_load(&self, t: Duration) -> Result<(), BrowserError> {
            self.0.wait_for_load(t).await
        }
        fn redirect_hops(&self) -> Vec<String> {
            self.0.redirect_hops()
        }
        fn main_status(&self) -> Option<u16> {
            self.0.main_status()
        }
    }

    /// A transport that answers `Page.captureScreenshot` with a 1x1 PNG and any
    /// `Runtime.evaluate` with a plausible reply, so the screenshot read path
    /// writes a real file.
    struct ShotTransport;

    #[async_trait]
    impl CdpTransport for ShotTransport {
        async fn send(&self, method: &str, params: Value) -> Result<Value, BrowserError> {
            match method {
                "Page.captureScreenshot" => Ok(json!({
                    "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="
                })),
                "Runtime.evaluate" => {
                    let expr = params
                        .get("expression")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if expr.starts_with("!!") {
                        Ok(json!({ "result": { "value": true } }))
                    } else {
                        Ok(json!({ "result": { "value": "https://example.com/" } }))
                    }
                }
                _ => Ok(json!({})),
            }
        }
        async fn wait_for_load(&self, _t: Duration) -> Result<(), BrowserError> {
            Ok(())
        }
        fn redirect_hops(&self) -> Vec<String> {
            Vec::new()
        }
        fn main_status(&self) -> Option<u16> {
            Some(200)
        }
    }

    /// A transport whose `.click()` evaluate throws — to prove a missing element
    /// surfaces as a driver error.
    struct ThrowTransport;

    #[async_trait]
    impl CdpTransport for ThrowTransport {
        async fn send(&self, method: &str, params: Value) -> Result<Value, BrowserError> {
            if method == "Runtime.evaluate" {
                let expr = params
                    .get("expression")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if expr.contains(".click()") {
                    return Ok(
                        json!({ "exceptionDetails": { "text": "no element for selector" } }),
                    );
                }
                return Ok(json!({ "result": { "value": "https://example.com/" } }));
            }
            Ok(json!({}))
        }
        async fn wait_for_load(&self, _t: Duration) -> Result<(), BrowserError> {
            Ok(())
        }
        fn redirect_hops(&self) -> Vec<String> {
            Vec::new()
        }
        fn main_status(&self) -> Option<u16> {
            Some(200)
        }
    }

    #[tokio::test]
    async fn interactive_click_then_read_runs_expected_cdp_sequence() {
        let driver = scripted_driver("https://example.com/next", "post-click DOM");
        let guard = RecordingGuard::default();
        let req = InteractRequest {
            url: "https://example.com".into(),
            actions: vec![InteractiveAction::Click {
                selector: "#go".into(),
            }],
            mode: RenderMode::DomText,
            timeout_secs: None,
        };
        let out = interact(&req, &guard, &driver).await.unwrap();
        assert_eq!(out.provenance, crate::render::Provenance::Untrusted);
        assert_eq!(out.text.as_deref(), Some("post-click DOM"));
        assert_eq!(out.final_url, "https://example.com/next");
    }

    #[tokio::test]
    async fn interactive_actions_emit_injection_safe_expressions() {
        // Drive the effect helpers directly against a scripted transport we keep
        // a handle to, so we can inspect the emitted expressions.
        let transport = std::sync::Arc::new(ScriptedTransport::new("https://example.com/", "dom"));
        let driver = CdpBrowserDriver::new(
            Box::new(Forward(std::sync::Arc::clone(&transport))),
            tmp_dir(),
            DEFAULT_NAV_TIMEOUT,
        );
        driver
            .act(&InteractiveAction::Click {
                selector: "#a\"b".into(), // a selector with a quote — must be escaped
            })
            .await
            .unwrap();
        driver
            .act(&InteractiveAction::Type {
                selector: "#in".into(),
                text: "hi\"there".into(),
            })
            .await
            .unwrap();
        driver
            .act(&InteractiveAction::Scroll {
                selector: None,
                dx: 0.0,
                dy: 250.0,
            })
            .await
            .unwrap();
        driver
            .act(&InteractiveAction::WaitForSelector {
                selector: ".done".into(),
                timeout_ms: Some(1000),
            })
            .await
            .unwrap();
        let exprs = transport.exprs.lock().unwrap().clone();
        // The click selector's quote is JSON-escaped, not raw — no injection.
        assert!(
            exprs
                .iter()
                .any(|e| e.contains("querySelector(\"#a\\\"b\")") && e.contains(".click()")),
            "click expr must query the escaped selector and click: {exprs:?}"
        );
        // Typed text is JSON-encoded into the expression.
        assert!(
            exprs.iter().any(|e| e.contains("\"hi\\\"there\"")),
            "type expr must embed the escaped text: {exprs:?}"
        );
        // Window scroll uses scrollBy with the numeric delta.
        assert!(
            exprs.iter().any(|e| e.contains("window.scrollBy(0,250)")),
            "scroll expr must scroll the window: {exprs:?}"
        );
        // Wait polls the selector presence.
        assert!(
            exprs
                .iter()
                .any(|e| e.starts_with("!!document.querySelector(\".done\")")),
            "wait expr must poll presence: {exprs:?}"
        );
        // A2 security follow-up F2: a SINGLE-quote in the selector must not break
        // out of the thrown Error string. The selector is JSON-encoded into a
        // double-quoted JS string literal, and the error message concatenates
        // that literal (`"..."+sel`) rather than splicing the raw selector into
        // a single-quoted `Error('...')`. So the hostile selector — including
        // its `');` — must appear only WITHIN one complete double-quoted literal,
        // never as bare code, and no single-quoted `Error('` literal may exist.
        driver
            .act(&InteractiveAction::Click {
                selector: "#x');globalThis.pwned=1//".into(),
            })
            .await
            .unwrap();
        let exprs = transport.exprs.lock().unwrap().clone();
        let click = exprs
            .iter()
            .find(|e| e.contains("globalThis.pwned=1"))
            .expect("the hostile-selector click expr should be recorded");
        // The whole selector rides inside one double-quoted JSON literal, so its
        // single-quote is inert data, not a string terminator.
        assert!(
            click.contains("\"#x');globalThis.pwned=1//\""),
            "hostile selector must be one escaped double-quoted literal: {click}"
        );
        // The Error message never uses a single-quoted literal the selector
        // could terminate.
        assert!(
            !click.contains("Error('"),
            "error message must not splice the selector into a single-quoted literal: {click}"
        );
    }

    #[tokio::test]
    async fn interactive_screenshot_read_writes_png() {
        // Read-back in screenshot mode writes a PNG through the shared
        // read_artifact path.
        let dir = tmp_dir();
        let driver = CdpBrowserDriver::new(Box::new(ShotTransport), &dir, DEFAULT_NAV_TIMEOUT);
        let guard = RecordingGuard::default();
        let req = InteractRequest {
            url: "https://example.com".into(),
            actions: vec![InteractiveAction::Scroll {
                selector: None,
                dx: 0.0,
                dy: 100.0,
            }],
            mode: RenderMode::Screenshot,
            timeout_secs: None,
        };
        let out = interact(&req, &guard, &driver).await.unwrap();
        let Some(path) = out.screenshot_path else {
            panic!("screenshot mode must yield a path");
        };
        assert!(std::path::Path::new(&path).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn interactive_ssrf_blocks_post_action_internal_navigation() {
        // The scripted transport reports an internal address as the settled
        // location after the click; the orchestration's post-nav re-guard blocks
        // before the DOM is read.
        let driver = scripted_driver("http://169.254.169.254/latest/meta-data/", "SECRET");
        let guard = RecordingGuard::blocking(&["169.254.169.254"]);
        let req = InteractRequest {
            url: "https://example.com".into(),
            actions: vec![InteractiveAction::Click {
                selector: "#go".into(),
            }],
            mode: RenderMode::DomText,
            timeout_secs: None,
        };
        let err = interact(&req, &guard, &driver).await.unwrap_err();
        assert!(matches!(err, BrowserError::Blocked(_)));
    }

    #[tokio::test]
    async fn interactive_click_missing_element_is_driver_error() {
        let driver =
            CdpBrowserDriver::new(Box::new(ThrowTransport), tmp_dir(), DEFAULT_NAV_TIMEOUT);
        let err = driver
            .act(&InteractiveAction::Click {
                selector: "#missing".into(),
            })
            .await
            .unwrap_err();
        match err {
            BrowserError::Driver(m) => assert!(m.contains("no element"), "{m}"),
            other => panic!("expected driver error, got {other:?}"),
        }
    }

    // ── M20 D2: capture fidelity (viewport / format / full-page) ─────────

    /// A transport recording every call (method + params) it received,
    /// through a `Mutex` shared via `Arc` so a test can keep reading it after
    /// the transport itself is boxed and moved into the driver. Replies to
    /// `Page.captureScreenshot` and `Runtime.evaluate` with plausible values
    /// so the render orchestration completes.
    struct CaptureRecordingTransport {
        calls: std::sync::Arc<Mutex<Vec<(String, Value)>>>,
    }

    #[async_trait]
    impl CdpTransport for CaptureRecordingTransport {
        async fn send(&self, method: &str, params: Value) -> Result<Value, BrowserError> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            match method {
                "Page.captureScreenshot" => Ok(json!({
                    "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="
                })),
                "Runtime.evaluate" => Ok(json!({ "result": { "value": "https://example.com/" } })),
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

    #[tokio::test]
    async fn default_driver_sends_byte_identical_legacy_capture_params() {
        // `CdpBrowserDriver::new` alone (no `.with_capture`) must reproduce
        // the EXACT pre-D2 hard-coded params: no Emulation.* call at all,
        // full-page PNG.
        let calls = std::sync::Arc::new(Mutex::new(Vec::new()));
        let transport = CaptureRecordingTransport {
            calls: std::sync::Arc::clone(&calls),
        };
        let driver = CdpBrowserDriver::new(Box::new(transport), tmp_dir(), DEFAULT_NAV_TIMEOUT);
        driver
            .render("https://example.com", RenderMode::Screenshot)
            .await
            .unwrap();
        let recorded = calls.lock().unwrap();
        assert!(
            recorded
                .iter()
                .all(|(m, _)| m != "Emulation.setDeviceMetricsOverride"),
            "legacy default must skip Emulation.* entirely: {recorded:?}"
        );
        let shot = recorded
            .iter()
            .find(|(m, _)| m == "Page.captureScreenshot")
            .expect("a capture call happened");
        assert_eq!(
            shot.1,
            json!({ "format": "png", "captureBeyondViewport": true })
        );
    }

    #[tokio::test]
    async fn with_capture_mobile_jpeg_wires_metrics_and_format_through() {
        let calls = std::sync::Arc::new(Mutex::new(Vec::new()));
        let transport = CaptureRecordingTransport {
            calls: std::sync::Arc::clone(&calls),
        };
        let opts = CaptureOptions {
            viewport: Some(crate::capture::ViewportPreset::Mobile),
            full_page: true,
            format: crate::capture::ImageFormat::Jpeg,
            quality: Some(80),
        };
        let driver = CdpBrowserDriver::new(Box::new(transport), tmp_dir(), DEFAULT_NAV_TIMEOUT)
            .with_capture(opts);
        driver
            .render("https://example.com", RenderMode::Screenshot)
            .await
            .unwrap();
        let recorded = calls.lock().unwrap();
        let metrics = recorded
            .iter()
            .find(|(m, _)| m == "Emulation.setDeviceMetricsOverride")
            .expect("mobile preset must set device metrics");
        assert_eq!(metrics.1["mobile"], json!(true));
        assert_eq!(metrics.1["width"], json!(390));
        let shot = recorded
            .iter()
            .find(|(m, _)| m == "Page.captureScreenshot")
            .expect("a capture call happened");
        assert_eq!(
            shot.1,
            json!({ "format": "jpeg", "captureBeyondViewport": true, "quality": 80 })
        );
    }

    #[tokio::test]
    async fn with_capture_jpeg_writes_jpg_extension() {
        let dir = tmp_dir();
        let calls = std::sync::Arc::new(Mutex::new(Vec::new()));
        let transport = CaptureRecordingTransport { calls };
        let opts = CaptureOptions {
            viewport: None,
            full_page: true,
            format: crate::capture::ImageFormat::Jpeg,
            quality: Some(70),
        };
        let driver = CdpBrowserDriver::new(Box::new(transport), &dir, DEFAULT_NAV_TIMEOUT)
            .with_capture(opts);
        let out = driver
            .render("https://example.com", RenderMode::Screenshot)
            .await
            .unwrap();
        let path = match out.artifact {
            RenderedArtifact::ScreenshotPath(p) => p,
            RenderedArtifact::Text(t) => panic!("expected screenshot path, got text {t:?}"),
        };
        let ext = std::path::Path::new(&path)
            .extension()
            .and_then(|e| e.to_str());
        assert_eq!(ext, Some("jpg"), "path was {path}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── M20 D5: console-event parsing + buffering + summarizing ─────────

    #[test]
    fn parses_console_api_called_error() {
        let params = json!({ "type": "error", "args": [{ "value": "boom" }] });
        let entry = parse_console_event("Runtime.consoleAPICalled", &params).unwrap();
        assert_eq!(entry.level, ConsoleLevel::Error);
        assert_eq!(entry.text, "boom");
    }

    #[test]
    fn parses_console_api_called_joins_multiple_args() {
        let params = json!({ "type": "log", "args": [{ "value": "a" }, { "value": "b" }] });
        let entry = parse_console_event("Runtime.consoleAPICalled", &params).unwrap();
        assert_eq!(entry.level, ConsoleLevel::Log);
        assert_eq!(entry.text, "a b");
    }

    #[test]
    fn parses_console_api_called_falls_back_to_description() {
        let params = json!({ "type": "warning", "args": [{ "description": "Object" }] });
        let entry = parse_console_event("Runtime.consoleAPICalled", &params).unwrap();
        assert_eq!(entry.level, ConsoleLevel::Warning);
        assert_eq!(entry.text, "Object");
    }

    #[test]
    fn parses_exception_thrown() {
        let params = json!({
            "exceptionDetails": {
                "text": "Uncaught",
                "exception": { "description": "TypeError: x is not a function" }
            }
        });
        let entry = parse_console_event("Runtime.exceptionThrown", &params).unwrap();
        assert_eq!(entry.level, ConsoleLevel::Error);
        assert_eq!(entry.text, "TypeError: x is not a function");
    }

    #[test]
    fn parses_log_entry_added() {
        let params = json!({ "entry": { "level": "error", "text": "Failed to load resource" } });
        let entry = parse_console_event("Log.entryAdded", &params).unwrap();
        assert_eq!(entry.level, ConsoleLevel::Error);
        assert_eq!(entry.text, "Failed to load resource");
    }

    #[test]
    fn unrelated_method_parses_to_none() {
        assert!(parse_console_event("Network.requestWillBeSent", &json!({})).is_none());
    }

    #[test]
    fn push_capped_evicts_oldest_once_at_cap() {
        let mut buf = Vec::new();
        for i in 0..5 {
            push_capped(&mut buf, i, 3);
        }
        assert_eq!(
            buf,
            vec![2, 3, 4],
            "keeps the 3 most recent, oldest evicted first"
        );
    }

    #[test]
    fn push_capped_zero_cap_never_buffers() {
        let mut buf: Vec<i32> = Vec::new();
        push_capped(&mut buf, 1, 0);
        assert!(buf.is_empty());
    }

    #[test]
    fn summarize_console_counts_by_level_and_finds_first_error() {
        let entries = vec![
            ConsoleEntry {
                level: ConsoleLevel::Log,
                text: "boot".into(),
            },
            ConsoleEntry {
                level: ConsoleLevel::Warning,
                text: "deprecated api".into(),
            },
            ConsoleEntry {
                level: ConsoleLevel::Error,
                text: "first crash".into(),
            },
            ConsoleEntry {
                level: ConsoleLevel::Error,
                text: "second crash".into(),
            },
        ];
        let summary = summarize_console(&entries);
        assert_eq!(summary.error_count, 2);
        assert_eq!(summary.warning_count, 1);
        assert_eq!(summary.first_error.as_deref(), Some("first crash"));
    }

    #[test]
    fn summarize_console_empty_has_no_first_error() {
        let summary = summarize_console(&[]);
        assert_eq!(summary, ConsoleSummary::default());
        assert!(summary.first_error.is_none());
    }

    // ── M20 D5: element inspection (box model + curated computed style) ──

    #[test]
    fn curate_computed_style_filters_to_whitelist_in_order() {
        // A stand-in for the real ~300-entry `CSS.getComputedStyleForNode`
        // dump: the whitelist props scattered among many others, plus one
        // whitelist prop absent entirely (`font-family`).
        let result = json!({
            "computedStyle": [
                { "name": "color", "value": "rgb(0, 0, 0)" },
                { "name": "overflow", "value": "hidden" },
                { "name": "z-index", "value": "auto" },
                { "name": "display", "value": "flex" },
                { "name": "width", "value": "200px" },
                { "name": "margin-top", "value": "0px" },
                { "name": "height", "value": "100px" },
                { "name": "position", "value": "absolute" },
            ]
        });
        let curated = curate_computed_style(&result);
        assert_eq!(
            curated,
            vec![
                ("display".to_string(), "flex".to_string()),
                ("position".to_string(), "absolute".to_string()),
                ("overflow".to_string(), "hidden".to_string()),
                ("width".to_string(), "200px".to_string()),
                ("height".to_string(), "100px".to_string()),
            ],
            "must be exactly the whitelist, in whitelist order, never the full dump"
        );
    }

    #[test]
    fn parse_box_model_reads_dimensions_and_quads() {
        let result = json!({
            "model": {
                "width": 200.0,
                "height": 100.0,
                "content": [0.0, 0.0, 200.0, 0.0, 200.0, 100.0, 0.0, 100.0],
                "padding": [0.0, 0.0, 200.0, 0.0, 200.0, 100.0, 0.0, 100.0],
                "border": [0.0, 0.0, 200.0, 0.0, 200.0, 100.0, 0.0, 100.0],
                "margin": [0.0, 0.0, 200.0, 0.0, 200.0, 100.0, 0.0, 100.0],
            }
        });
        let model = parse_box_model(&result).unwrap();
        assert!((model.width - 200.0).abs() < f64::EPSILON);
        assert!((model.height - 100.0).abs() < f64::EPSILON);
        assert_eq!(model.content.len(), 8);
    }

    #[test]
    fn parse_box_model_errors_without_a_model_field() {
        let err = parse_box_model(&json!({})).unwrap_err();
        assert!(matches!(err, BrowserError::Driver(_)));
    }

    /// A transport that answers the `inspect_element` CDP sequence with
    /// canned results, recording every call so the test can assert order.
    #[derive(Default)]
    struct InspectTransport {
        calls: Mutex<Vec<String>>,
        query_selector_node_id: u64,
    }

    #[async_trait]
    impl CdpTransport for InspectTransport {
        async fn send(&self, method: &str, _params: Value) -> Result<Value, BrowserError> {
            self.calls.lock().unwrap().push(method.to_string());
            match method {
                "DOM.getDocument" => Ok(json!({ "root": { "nodeId": 1 } })),
                "DOM.querySelector" => Ok(json!({ "nodeId": self.query_selector_node_id })),
                "DOM.getBoxModel" => Ok(json!({
                    "model": {
                        "width": 50.0, "height": 20.0,
                        "content": [], "padding": [], "border": [], "margin": []
                    }
                })),
                "CSS.getComputedStyleForNode" => Ok(json!({
                    "computedStyle": [
                        { "name": "display", "value": "block" },
                        { "name": "font-family", "value": "Inter" },
                    ]
                })),
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

    #[tokio::test]
    async fn inspect_element_runs_expected_cdp_sequence() {
        let transport = InspectTransport {
            calls: Mutex::new(Vec::new()),
            query_selector_node_id: 42,
        };
        let out = inspect_element(&transport, "#app").await.unwrap();
        assert_eq!(out.selector, "#app");
        assert!((out.box_model.width - 50.0).abs() < f64::EPSILON);
        assert_eq!(
            out.computed_style,
            vec![
                ("display".to_string(), "block".to_string()),
                ("font-family".to_string(), "Inter".to_string()),
            ]
        );
        let calls = transport.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                "DOM.enable",
                "CSS.enable",
                "DOM.getDocument",
                "DOM.querySelector",
                "DOM.getBoxModel",
                "CSS.getComputedStyleForNode",
            ]
        );
    }

    #[tokio::test]
    async fn inspect_element_missing_selector_is_driver_error() {
        let transport = InspectTransport {
            calls: Mutex::new(Vec::new()),
            query_selector_node_id: 0, // CDP's "no match" sentinel
        };
        let err = inspect_element(&transport, "#missing").await.unwrap_err();
        match err {
            BrowserError::Driver(m) => assert!(m.contains("no element"), "{m}"),
            other => panic!("expected driver error, got {other:?}"),
        }
    }
}
