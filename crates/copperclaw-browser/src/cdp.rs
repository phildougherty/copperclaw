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

use crate::driver::{BrowserDriver, DriverRender, Navigation, RenderedArtifact};
use crate::error::BrowserError;
use crate::interactive::InteractiveDriver;
use crate::render::RenderMode;

/// Default navigation/idle timeout when the caller does not specify one.
pub const DEFAULT_NAV_TIMEOUT: Duration = Duration::from_secs(30);

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
}

/// The concrete CDP-backed [`BrowserDriver`]. Generic over the transport so
/// tests drive it with a mock and production drives it with [`WsCdpTransport`].
pub struct CdpBrowserDriver {
    transport: Box<dyn CdpTransport>,
    /// Host-side directory screenshots are written into.
    screenshot_dir: PathBuf,
    /// Navigation timeout handed to [`CdpTransport::wait_for_load`].
    nav_timeout: Duration,
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
        }
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

    /// Capture a PNG screenshot, write it under `screenshot_dir`, return the
    /// host-side path.
    async fn capture_screenshot(&self) -> Result<String, BrowserError> {
        let out = self
            .transport
            .send(
                "Page.captureScreenshot",
                json!({ "format": "png", "captureBeyondViewport": true }),
            )
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
        let path = self
            .screenshot_dir
            .join(format!("render-{nanos}-{seq}.png"));
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

    use super::{CdpTransport, main_status_from_events, redirect_hops_from_events};
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
}
