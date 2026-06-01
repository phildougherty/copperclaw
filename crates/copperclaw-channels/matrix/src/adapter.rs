//! [`MatrixAdapter`] — the [`ChannelAdapter`] implementation.
//!
//! Owns a shared [`MatrixApi`] client, an `RwLock<HashSet>` of subscribed
//! rooms (used by the `/sync` filter), the per-instance access token, and
//! the cancellation token used to stop the background `/sync` task.

use crate::api::MatrixApi;
use crate::config::MatrixConfig;
use crate::factory::CHANNEL_TYPE_STR;
use crate::sync::{NEXT_BATCH_FILENAME, run_sync_loop};
use async_trait::async_trait;
use copperclaw_channels_core::{
    AdapterError, Breadcrumb, BreadcrumbStatus, ChannelAdapter, DiffCard, DmHandle, ErrorCard,
    ErrorCardKind, ThinkingBlock, TodoItemStatus, TodoList,
};
use copperclaw_types::{ChannelType, InboundEvent, OutboundMessage};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Matrix channel adapter.
pub struct MatrixAdapter {
    channel_type: ChannelType,
    api: Arc<MatrixApi>,
    bot_user_id: String,
    config: MatrixConfig,
    rooms: Arc<RwLock<HashSet<String>>>,
    /// In-memory cache of alias → room id resolutions.
    alias_cache: Mutex<HashMap<String, String>>,
    sync_handle: Mutex<Option<JoinHandle<()>>>,
    cancel: CancellationToken,
}

impl MatrixAdapter {
    /// Build an adapter and start its `/sync` background task.
    ///
    /// `data_dir` is the per-channel directory the host provides; the
    /// adapter persists its `next_batch` token in `data_dir/next_batch.txt`.
    #[allow(clippy::needless_pass_by_value)]
    pub fn start(
        config: MatrixConfig,
        inbound_tx: Sender<InboundEvent>,
        data_dir: PathBuf,
    ) -> Result<Arc<Self>, AdapterError> {
        let api = Arc::new(MatrixApi::with_client(
            reqwest::Client::new(),
            config.homeserver_url.clone(),
            config.access_token.clone(),
            config.txn_prefix.clone(),
        ));
        Self::start_with_api(api, config, inbound_tx, data_dir)
    }

    /// As [`start`] but with a caller-supplied [`MatrixApi`] (for tests).
    #[allow(clippy::needless_pass_by_value)]
    pub fn start_with_api(
        api: Arc<MatrixApi>,
        config: MatrixConfig,
        inbound_tx: Sender<InboundEvent>,
        data_dir: PathBuf,
    ) -> Result<Arc<Self>, AdapterError> {
        let rooms: HashSet<String> = config.rooms.iter().cloned().collect();
        let rooms = Arc::new(RwLock::new(rooms));
        let cancel = CancellationToken::new();
        let state_path = data_dir.join(NEXT_BATCH_FILENAME);
        let handle = tokio::spawn(run_sync_loop(
            api.clone(),
            state_path,
            inbound_tx,
            config.user_id.clone(),
            rooms.clone(),
            config.sync_timeout_ms,
            cancel.clone(),
        ));
        Ok(Arc::new(Self {
            channel_type: ChannelType::new(CHANNEL_TYPE_STR),
            bot_user_id: config.user_id.clone(),
            api,
            config,
            rooms,
            alias_cache: Mutex::new(HashMap::new()),
            sync_handle: Mutex::new(Some(handle)),
            cancel,
        }))
    }

    /// Configured Matrix config.
    pub fn config(&self) -> &MatrixConfig {
        &self.config
    }

    /// Shared HTTP client.
    pub fn api(&self) -> &Arc<MatrixApi> {
        &self.api
    }

    /// Bot's Matrix user id.
    pub fn bot_user_id(&self) -> &str {
        &self.bot_user_id
    }

    /// Set of rooms the `/sync` filter is currently configured for. Clones
    /// the inner set; primarily useful for tests.
    pub async fn subscribed_rooms(&self) -> HashSet<String> {
        self.rooms.read().await.clone()
    }

    /// Stop the `/sync` task and wait for it to finish. Idempotent.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        if let Some(handle) = self.sync_handle.lock().await.take() {
            let _ = handle.await;
        }
    }

    /// Resolve an alias to a canonical room id, caching the result.
    async fn resolve_room(&self, identifier: &str) -> Result<String, AdapterError> {
        if identifier.starts_with('!') {
            return Ok(identifier.to_owned());
        }
        if let Some(cached) = self.alias_cache.lock().await.get(identifier).cloned() {
            return Ok(cached);
        }
        let resolved = self.api.resolve_alias(identifier).await?;
        self.alias_cache
            .lock()
            .await
            .insert(identifier.to_owned(), resolved.room_id.clone());
        Ok(resolved.room_id)
    }
}

impl std::fmt::Debug for MatrixAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatrixAdapter")
            .field("channel_type", &self.channel_type)
            .field("bot_user_id", &self.bot_user_id)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ChannelAdapter for MatrixAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        true
    }

    async fn subscribe(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        let room_id = self.resolve_room(platform_id).await?;
        self.rooms.write().await.insert(room_id);
        Ok(())
    }

    async fn set_typing(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        let room_id = self.resolve_room(platform_id).await?;
        self.api.typing(&room_id, &self.bot_user_id, true).await
    }

    async fn deliver(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let room_id = self.resolve_room(platform_id).await?;

        // System-action shape: { "action": "edit"|"reaction", ... }
        if let Some(action) = extract_action(&message.content) {
            return self
                .deliver_action(&room_id, action, &message.content)
                .await;
        }

        let text = extract_text(&message.content);
        let html = extract_html(&message.content);

        // Files first; the first file picks up `text` as its body (if any),
        // and subsequent files use their filename.
        if !message.files.is_empty() {
            let mut last_event_id: Option<String> = None;
            for (i, file) in message.files.iter().enumerate() {
                let mime = guess_mime(&file.filename);
                let upload = self
                    .api
                    .upload_media(&file.filename, &mime, file.data.clone())
                    .await?;
                let msgtype = msgtype_for_mime(&mime);
                let body = if i == 0 && !text.is_empty() {
                    text.as_str()
                } else {
                    file.filename.as_str()
                };
                let sent = self
                    .api
                    .send_media_message(
                        &room_id,
                        msgtype,
                        body,
                        &upload.content_uri,
                        &mime,
                        file.data.len(),
                        thread_id,
                    )
                    .await?;
                last_event_id = Some(sent.event_id);
            }
            return Ok(last_event_id);
        }

        if text.is_empty() && html.is_none() {
            return Err(AdapterError::BadRequest(
                "matrix deliver requires `text`, `html`, or files".into(),
            ));
        }

        let sent = if let Some(html) = html {
            // If the user supplied HTML the plain-text fallback is either
            // explicit `text` or the html stripped down. Use `text` if
            // present, otherwise the same html.
            let plain = if text.is_empty() { html.as_str() } else { text.as_str() };
            self.api.send_html(&room_id, plain, &html).await?
        } else if let Some(thread) = thread_id {
            self.api.send_threaded(&room_id, thread, &text).await?
        } else {
            self.api.send_text(&room_id, &text).await?
        };
        Ok(Some(sent.event_id))
    }

    /// Native breadcrumb chip — rendered as an `m.notice` HTML event
    /// with the tool name in `<code>…</code>`. Matrix clients style
    /// `m.notice` more subtly than `m.text`, matching the chip
    /// aesthetic. When `existing_message_id` is provided we send an
    /// `m.replace` relation so clients show "edited" in place
    /// instead of a new line.
    ///
    /// When `breadcrumb.steps` is non-empty this is the rolling
    /// "activity" aggregate: the HTML carries a `<details>` disclosure
    /// (collapsed `<summary>` line + one styled line per step inside the
    /// body) and the plain-text `body` falls back to one step per line.
    /// Both the first-emit and the `m.replace` edit path render the
    /// aggregate.
    async fn deliver_breadcrumb(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        breadcrumb: &Breadcrumb,
        existing_message_id: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        let room_id = self.resolve_room(platform_id).await?;
        let html = render_breadcrumb_html_matrix(breadcrumb);
        let plain = if breadcrumb.steps.is_empty() {
            breadcrumb.to_text_fallback()
        } else {
            render_activity_text_matrix(breadcrumb)
        };
        if let Some(target) = existing_message_id {
            let resp = self
                .api
                .edit_message_notice_html(&room_id, target, &plain, &html)
                .await?;
            return Ok(Some(resp.event_id));
        }
        let resp = self.api.send_notice_html(&room_id, &plain, &html).await?;
        Ok(Some(resp.event_id))
    }

    /// Native diff card — rendered as an `m.notice` HTML event with
    /// `formatted_body = <pre><code class="language-diff">…</code></pre>`.
    /// Element (the reference Matrix client) honours the
    /// `language-diff` class natively and colourises `+` / `-` lines.
    /// The plain-text `body` field carries the unified-diff fallback
    /// so non-HTML clients (CLI bots, IRC bridges) still get a
    /// readable rendering.
    async fn deliver_diff(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        diff: &DiffCard,
    ) -> Result<Option<String>, AdapterError> {
        let room_id = self.resolve_room(platform_id).await?;
        let plain = diff.to_text_fallback();
        let html = render_diff_html_matrix(diff);
        let resp = self.api.send_notice_html(&room_id, &plain, &html).await?;
        Ok(Some(resp.event_id))
    }

    /// Native long-output expander (slice 3.4) — rendered as an
    /// `m.text` HTML event whose `formatted_body` wraps the full
    /// text in a `<details><summary>…</summary>…</details>`
    /// disclosure widget. Element (the reference Matrix client)
    /// renders the disclosure natively as a clickable expander, and
    /// other Matrix clients fall back to "summary text + visible
    /// body" (graceful).
    ///
    /// The plain-text body powers the `body` field so non-HTML
    /// clients (CLI bots, scrapers, IRC bridges) still get a
    /// readable rendering — we concatenate the summary, the
    /// preview, a `…(N more lines)` marker, and the full body.
    async fn deliver_collapsible(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        text: &str,
        summary: &str,
        preview_lines: &[String],
    ) -> Result<Option<String>, AdapterError> {
        let room_id = self.resolve_room(platform_id).await?;
        let html = render_collapsible_html_matrix(text, summary, preview_lines);
        // Plain-text fallback for clients without HTML support uses
        // the same shape as the trait-level default impl.
        let plain = copperclaw_channels_core::render_collapsible_text_fallback(
            text,
            summary,
            preview_lines,
        );
        let resp = self.api.send_html(&room_id, &plain, &html).await?;
        Ok(Some(resp.event_id))
    }

    /// Native todo-list checklist — rendered as an `m.text` HTML
    /// event with a `<h4>` title carrying `done/total`, then a `<ul>`
    /// list with one `<li>` per item. Each `<li>` is prefixed by a
    /// status glyph (✅/▶/☐) and wraps completed items in `<s>` so
    /// the user can scan unfinished work at a glance. On mutation we
    /// send an `m.replace` relation so Element shows the same chip
    /// ticking through.
    ///
    /// `pin_hint` is best-effort: when set on first emit, we send an
    /// `m.room.pinned_events` state event appending the rendered
    /// list's event id to the pinned set; when set on the
    /// fully-completed transition, we remove it. The bot needs the
    /// `m.room.pinned_events` state-event permission for this to
    /// succeed; without it, the state-event call returns 403 and we
    /// swallow the failure (chip is the load-bearing UX, pinning is
    /// decoration).
    async fn deliver_todo_list(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        list: &TodoList,
        existing_message_id: Option<&str>,
        _pin_hint: bool,
    ) -> Result<Option<String>, AdapterError> {
        // Matrix pinning is a state-event API we don't currently
        // model — see the doc comment above. We render the chip
        // (and edit-in-place via m.replace), and accept the pin
        // limitation. The `_pin_hint` is silently honoured as a
        // no-op for now.
        let room_id = self.resolve_room(platform_id).await?;
        let html = render_todo_list_html_matrix(list);
        let plain = list.to_text_fallback();
        if let Some(target) = existing_message_id {
            let resp = self
                .api
                .edit_message_html(&room_id, target, &plain, &html)
                .await?;
            return Ok(Some(resp.event_id));
        }
        let resp = self.api.send_html(&room_id, &plain, &html).await?;
        Ok(Some(resp.event_id))
    }

    /// Native error card — rendered as `m.text` (NOT `m.notice`) so
    /// Element raises an actual notification rather than muting the
    /// row, with a `<font color="#cc3333">` wrapper carrying the red
    /// affordance. (`m.notice` is Matrix's "muted" message type and
    /// suppresses notification badges — wrong call for errors that
    /// genuinely need user attention.) Details ride inside a
    /// `<pre><code>` block for monospace.
    async fn deliver_error(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        err: &ErrorCard,
    ) -> Result<Option<String>, AdapterError> {
        let room_id = self.resolve_room(platform_id).await?;
        let html = render_error_html_matrix(err);
        let plain = err.to_text_fallback();
        let resp = self.api.send_html(&room_id, &plain, &html).await?;
        Ok(Some(resp.event_id))
    }

    /// Native thinking block (slice 3.5) — rendered as an `m.notice`
    /// event (Matrix's idiomatic "muted by default" message type, same
    /// as the breadcrumb chip) with an HTML `<details>` disclosure
    /// widget in `formatted_body`. Element / SchildiChat / Cinny all
    /// render `<details>` as a native expander; legacy clients fall
    /// back to the plain-text `to_text_fallback` body. Redacted
    /// blocks render the placeholder body — the raw blob never
    /// reaches the wire.
    async fn deliver_thinking(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        thinking: &ThinkingBlock,
    ) -> Result<Option<String>, AdapterError> {
        let room_id = self.resolve_room(platform_id).await?;
        let html = render_thinking_html_matrix(thinking);
        let plain = thinking.to_text_fallback();
        let resp = self
            .api
            .send_notice_html(&room_id, &plain, &html)
            .await?;
        Ok(Some(resp.event_id))
    }

    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        // Matrix has no separate DM concept at the protocol level; callers
        // must configure the room and pass it as `platform_id`.
        Ok(None)
    }
}

/// Render a [`Breadcrumb`] as Matrix HTML — `<code>tool</code> · detail`
/// with a status glyph prefix. Plain text fallback lives on the same
/// event so legacy clients still see something sensible.
/// Render a [`DiffCard`] as Matrix HTML wrapping the unified-diff
/// body in `<pre><code class="language-diff">…</code></pre>`. Element
/// honours the `language-diff` class and applies `+` / `-` gutter
/// colouring natively; clients without language-aware highlighting
/// see the same monospaced block.
pub(crate) fn render_diff_html_matrix(diff: &DiffCard) -> String {
    let mut out = String::with_capacity(128 + diff.hunks.len() * 64);
    out.push_str("<b>");
    out.push_str(&escape_html_matrix(&diff.path));
    out.push_str("</b> <i>(+");
    out.push_str(&diff.added.to_string());
    out.push_str(" / -");
    out.push_str(&diff.removed.to_string());
    if diff.truncated {
        out.push_str(", truncated");
    }
    out.push_str(")</i>\n");
    out.push_str("<pre><code class=\"language-diff\">");
    for h in &diff.hunks {
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            h.old_start, h.old_lines, h.new_start, h.new_lines
        ));
        for line in &h.lines {
            out.push(line.kind.unified_prefix());
            // HTML-escape so source code can't smuggle in tags.
            for c in line.text.chars() {
                match c {
                    '<' => out.push_str("&lt;"),
                    '>' => out.push_str("&gt;"),
                    '&' => out.push_str("&amp;"),
                    _ => out.push(c),
                }
            }
            out.push('\n');
        }
    }
    out.push_str("</code></pre>");
    out
}

/// ASCII-only status marker per the project's no-emoji rule. Element
/// renders the symbol forms (U+23F3 / U+2713 / U+2717) as colourful
/// emoji on iOS, so we stay strictly ASCII.
fn breadcrumb_glyph_matrix(status: BreadcrumbStatus) -> &'static str {
    match status {
        BreadcrumbStatus::Running => "[~]",
        BreadcrumbStatus::Done => "[ok]",
        BreadcrumbStatus::Failed => "[x]",
    }
}

/// Cap on steps rendered inside the `<details>` body — keeps a long
/// turn's aggregate chip from ballooning the formatted body. Older steps
/// beyond this are summarised as a `+N earlier` line.
const ACTIVITY_MAX_STEPS_MATRIX: usize = 40;

pub(crate) fn render_breadcrumb_html_matrix(b: &Breadcrumb) -> String {
    // Rolling "activity" aggregate: a `<details>` disclosure whose
    // `<summary>` is the collapsed one-line status and whose body lists
    // each tool step, individually styled. The legacy single-tool path
    // below is left byte-for-byte unchanged.
    if !b.steps.is_empty() {
        return render_activity_html_matrix(b);
    }
    // ASCII-only markers. Element renders U+23F3 / U+2713 / U+2717
    // as colourful emoji on iOS, violating the project's no-emoji rule.
    let glyph = match b.status {
        BreadcrumbStatus::Running => "[~]",
        BreadcrumbStatus::Done => "[ok]",
        BreadcrumbStatus::Failed => "[x]",
    };
    let mut out = String::with_capacity(64);
    out.push_str(glyph);
    out.push(' ');
    out.push_str("<code>");
    out.push_str(&escape_html_matrix(&b.tool_name));
    out.push_str("</code>");
    if let Some(d) = b.detail.as_deref() {
        let d = d.trim();
        if !d.is_empty() {
            out.push_str(" · ");
            out.push_str(&escape_html_matrix(d));
        }
    }
    if let Some(s) = b.summary.as_deref() {
        let s = s.trim();
        if !s.is_empty() {
            if b.status == BreadcrumbStatus::Failed {
                out.push_str(" — failed: ");
            } else {
                out.push_str(" — ");
            }
            out.push_str(&escape_html_matrix(s));
        }
    }
    out
}

/// Render the rolling aggregate "activity" chip as Matrix HTML using the
/// native `<details><summary>…</summary>…</details>` disclosure element
/// (same primitive the long-output / thinking renderers use). The
/// `<summary>` carries the collapsed one-line status (current activity +
/// completed/total count); the body is one styled line per tool step,
/// each on its own `<div>` so Element lays them out as discrete rows.
///
/// Shape:
///
/// ```html
/// <details>
///   <summary>[~] shell npm run build · 1/2 steps</summary>
///   <div>[ok] <b>read_file</b> <code>src/App.tsx</code> <i>— 120 lines</i></div>
///   <div>[~] <b>shell</b> <code>npm run build</code></div>
/// </details>
/// ```
///
/// Element / `SchildiChat` / Cinny render `<details>` as a clickable
/// expander natively; legacy clients fall back to the plain-text `body`
/// (one step per line — see [`render_activity_text_matrix`]). Every
/// dynamic field is HTML-escaped individually so a path like `src/<x>`
/// or a bare `&` round-trips without leaking raw markup.
fn render_activity_html_matrix(b: &Breadcrumb) -> String {
    let mut out = String::with_capacity(128 + b.steps.len() * 64);
    out.push_str("<details><summary>");
    // Collapsed summary line from the top-level fields.
    out.push_str(breadcrumb_glyph_matrix(b.status));
    out.push(' ');
    match b.detail.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => out.push_str(&escape_html_matrix(d)),
        None => out.push_str("working"),
    }
    if let Some(s) = b.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(" · ");
        out.push_str(&escape_html_matrix(s));
    }
    out.push_str("</summary>");
    // Body — one styled line per step, newest-biased when the turn ran
    // more than ACTIVITY_MAX_STEPS_MATRIX tools.
    let total = b.steps.len();
    let hidden = total.saturating_sub(ACTIVITY_MAX_STEPS_MATRIX);
    if hidden > 0 {
        out.push_str(&format!("<div><i>+{hidden} earlier step(s)</i></div>"));
    }
    for step in b.steps.iter().skip(hidden) {
        out.push_str("<div>");
        out.push_str(&render_step_line_html_matrix(step));
        out.push_str("</div>");
    }
    out.push_str("</details>");
    out
}

/// One styled step line for the `<details>` body: status marker, bold
/// tool name, `<code>` detail, italic summary. Every dynamic field is
/// HTML-escaped individually so source paths / commands round-trip
/// without leaking raw markup.
fn render_step_line_html_matrix(s: &Breadcrumb) -> String {
    let mut out = String::with_capacity(48);
    out.push_str(breadcrumb_glyph_matrix(s.status));
    out.push_str(" <b>");
    out.push_str(&escape_html_matrix(&s.tool_name));
    out.push_str("</b>");
    if let Some(d) = s.detail.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        out.push_str(" <code>");
        out.push_str(&escape_html_matrix(d));
        out.push_str("</code>");
    }
    if let Some(sum) = s.summary.as_deref().map(str::trim).filter(|x| !x.is_empty()) {
        if s.status == BreadcrumbStatus::Failed {
            out.push_str(" <i>— failed: ");
        } else {
            out.push_str(" <i>— ");
        }
        out.push_str(&escape_html_matrix(sum));
        out.push_str("</i>");
    }
    out
}

/// Plain-text `body` fallback for the rolling aggregate chip — the
/// collapsed summary line followed by one step per line (reusing each
/// step's canonical [`Breadcrumb::to_text_fallback`] shape). Non-HTML
/// Matrix clients (CLI bots, IRC bridges) get a readable rendering. The
/// step list is capped to match the HTML body, with a `+N earlier` note.
fn render_activity_text_matrix(b: &Breadcrumb) -> String {
    let mut out = String::with_capacity(64 + b.steps.len() * 48);
    out.push_str(breadcrumb_glyph_matrix(b.status));
    out.push(' ');
    match b.detail.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => out.push_str(d),
        None => out.push_str("working"),
    }
    if let Some(s) = b.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(" · ");
        out.push_str(s);
    }
    let total = b.steps.len();
    let hidden = total.saturating_sub(ACTIVITY_MAX_STEPS_MATRIX);
    if hidden > 0 {
        out.push_str(&format!("\n+{hidden} earlier step(s)"));
    }
    for step in b.steps.iter().skip(hidden) {
        out.push('\n');
        out.push_str(&step.to_text_fallback());
    }
    out
}

/// Render an [`ErrorCard`] as Matrix HTML `formatted_body`. Shape:
///
/// ```html
/// <font color="#cc3333"><b>Error: Title</b></font><br>
/// summary line
/// <pre><code>details monospace block</code></pre>
/// <em>will retry automatically</em>
/// ```
///
/// Element renders `<font color>` natively in the message body; the
/// `<pre><code>` block survives Matrix's HTML allowlist and lands as
/// monospace. The `<em>` retryable footer styles distinctly without
/// adding noise on minimal clients (where it falls back to plain
/// italics).
///
/// The severity label varies by [`ErrorCardKind`] so the user can
/// see which pipeline stage gave up.
pub(crate) fn render_error_html_matrix(err: &ErrorCard) -> String {
    const RED: &str = "#cc3333";
    let label = match err.kind {
        ErrorCardKind::Internal => "Tool error",
        ErrorCardKind::Provider => "Provider error",
        ErrorCardKind::Delivery => "Delivery error",
    };
    let mut out = String::with_capacity(160 + err.summary.len());
    out.push_str("<font color=\"");
    out.push_str(RED);
    out.push_str("\"><b>");
    out.push_str(label);
    out.push_str(": ");
    out.push_str(&escape_html_matrix(err.title.trim()));
    out.push_str("</b></font><br>");
    out.push_str(&escape_html_matrix(err.summary.trim()));
    if let Some(d) = err.details.as_deref() {
        let d = d.trim();
        if !d.is_empty() {
            out.push_str("<pre><code>");
            out.push_str(&escape_html_matrix(d));
            out.push_str("</code></pre>");
        }
    }
    if err.retryable {
        out.push_str("<br><em>will retry automatically</em>");
    }
    out
}

/// Render the slice-3.4 long-output expander as Matrix HTML using
/// the native `<details><summary>…</summary>…</details>` disclosure
/// element. Element (the reference Matrix client) renders this as a
/// clickable expander natively; other clients fall back to "summary
/// visible + body visible inline".
///
/// Shape:
///
/// ```html
/// <details>
///   <summary><em>{summary}</em></summary>
///   <pre><code>{full body}</code></pre>
/// </details>
/// ```
///
/// Preview lines aren't rendered separately because the `<details>`
/// element itself handles the "what's visible when collapsed" UX
/// (the summary line). Pulling preview lines outside the details
/// would just duplicate content.
pub(crate) fn render_collapsible_html_matrix(
    text: &str,
    summary: &str,
    _preview_lines: &[String],
) -> String {
    let mut out = String::with_capacity(text.len() + summary.len() + 64);
    out.push_str("<details>");
    out.push_str("<summary><em>");
    out.push_str(&escape_html_matrix(summary.trim()));
    out.push_str("</em></summary>");
    out.push_str("<pre><code>");
    out.push_str(&escape_html_matrix(text));
    out.push_str("</code></pre>");
    out.push_str("</details>");
    out
}

/// Render a [`ThinkingBlock`] as Matrix HTML using the native
/// `<details><summary>…</summary>…</details>` disclosure element —
/// same primitive surface 4 (long-output) uses. Element /
/// `SchildiChat` / Cinny render `<details>` as a clickable expander
/// natively; legacy clients render the body inline.
///
/// Shape:
///
/// ```html
/// <details>
///   <summary><em>reasoning (claude-opus-4-7)</em></summary>
///   <blockquote>thinking text…</blockquote>
/// </details>
/// ```
///
/// `<blockquote>` (rather than `<pre><code>`, which the long-output
/// renderer uses) reflects that reasoning is prose, not code; clients
/// render it with a subtle left-bar indent so the user reads it as
/// quoted aside. Redacted blocks emit the placeholder body — the raw
/// blob never reaches the wire.
pub(crate) fn render_thinking_html_matrix(t: &ThinkingBlock) -> String {
    let mut out = String::with_capacity(t.text.len() + 64);
    let label_suffix = match t.model.as_deref().map(str::trim) {
        Some(m) if !m.is_empty() => format!(" ({})", escape_html_matrix(m)),
        _ => String::new(),
    };
    out.push_str("<details>");
    out.push_str("<summary><em>reasoning");
    out.push_str(&label_suffix);
    out.push_str("</em></summary>");
    out.push_str("<blockquote>");
    if t.redacted {
        out.push_str("(redacted reasoning)");
    } else {
        out.push_str(&escape_html_matrix(&t.text));
    }
    out.push_str("</blockquote>");
    out.push_str("</details>");
    out
}

/// Render a [`TodoList`] as Matrix HTML — `<h4>` title with
/// `done/total` counter, `<ul>` with one `<li>` per item, status
/// glyph prefix, `<s>` strikethrough on completed lines. Element
/// renders this with proper list indentation.
///
/// Glyphs are unicode `✅` / `▶` / `☐` so they survive every Matrix
/// client (Element web, Element mobile, `FluffyChat`, etc.). Item text
/// goes through [`escape_html_matrix`].
pub(crate) fn render_todo_list_html_matrix(list: &TodoList) -> String {
    let done = list.completed_count();
    let total = list.items.len();
    let mut out = String::with_capacity(64 + list.items.len() * 48);
    out.push_str("<h4>");
    out.push_str(&escape_html_matrix(list.title_or_default()));
    out.push_str(&format!(" ({done}/{total})"));
    out.push_str("</h4>");
    out.push_str("<ul>");
    for item in &list.items {
        // ASCII-only glyphs per the project's no-emoji rule. Element on
        // iOS renders the symbol forms (✅ / ▶ / ☐) as colourful emoji.
        let glyph = match item.status {
            TodoItemStatus::Completed => "[x]",
            TodoItemStatus::InProgress => "[~]",
            TodoItemStatus::Pending => "[ ]",
        };
        out.push_str("<li>");
        out.push_str(glyph);
        out.push(' ');
        if item.status == TodoItemStatus::Completed {
            out.push_str("<s>");
            out.push_str(&escape_html_matrix(item.text.trim()));
            out.push_str("</s>");
        } else {
            out.push_str(&escape_html_matrix(item.text.trim()));
        }
        out.push_str("</li>");
    }
    out.push_str("</ul>");
    out
}

fn escape_html_matrix(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

impl MatrixAdapter {
    async fn deliver_action(
        &self,
        room_id: &str,
        action: &str,
        content: &Value,
    ) -> Result<Option<String>, AdapterError> {
        match action {
            "edit" => {
                let target = required_str(content, "target_event_id").or_else(|_| {
                    required_str(content, "target_platform_id")
                })?;
                let new_text = required_str(content, "text")?;
                let resp = self.api.edit_message(room_id, target, new_text).await?;
                Ok(Some(resp.event_id))
            }
            "reaction" => {
                let target = required_str(content, "target_event_id").or_else(|_| {
                    required_str(content, "target_platform_id")
                })?;
                let key = required_str(content, "emoji")
                    .or_else(|_| required_str(content, "key"))?;
                let resp = self.api.send_reaction(room_id, target, key).await?;
                Ok(Some(resp.event_id))
            }
            other => Err(AdapterError::Unsupported(format!(
                "matrix action `{other}` is not supported"
            ))),
        }
    }
}

fn extract_action(content: &Value) -> Option<&str> {
    content.get("action").and_then(Value::as_str)
}

fn extract_text(value: &Value) -> String {
    value
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn extract_html(value: &Value) -> Option<String> {
    value
        .get("html")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, AdapterError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::BadRequest(format!("missing `{key}` in matrix action")))
}

fn guess_mime(filename: &str) -> String {
    let lower = filename.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or_default();
    match ext {
        "png" => "image/png".into(),
        "jpg" | "jpeg" => "image/jpeg".into(),
        "gif" => "image/gif".into(),
        "webp" => "image/webp".into(),
        "mp4" => "video/mp4".into(),
        "webm" => "video/webm".into(),
        "mp3" => "audio/mpeg".into(),
        "ogg" => "audio/ogg".into(),
        "wav" => "audio/wav".into(),
        "txt" => "text/plain".into(),
        "json" => "application/json".into(),
        "pdf" => "application/pdf".into(),
        _ => "application/octet-stream".into(),
    }
}

fn msgtype_for_mime(mime: &str) -> &'static str {
    if mime.starts_with("image/") {
        "m.image"
    } else if mime.starts_with("audio/") {
        "m.audio"
    } else if mime.starts_with("video/") {
        "m.video"
    } else {
        "m.file"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_TXN_PREFIX;
    use copperclaw_types::{MessageKind, OutboundFile};
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn build_adapter(
        server_url: &str,
    ) -> (Arc<MatrixAdapter>, TempDir, mpsc::Receiver<InboundEvent>) {
        let dir = TempDir::new().unwrap();
        let api = Arc::new(MatrixApi::with_client(
            reqwest::Client::new(),
            server_url.to_owned(),
            "tok".to_owned(),
            DEFAULT_TXN_PREFIX.to_owned(),
        ));
        let config = MatrixConfig {
            homeserver_url: server_url.to_owned(),
            access_token: "tok".to_owned(),
            user_id: "@bot:m.org".to_owned(),
            rooms: vec![],
            sync_timeout_ms: 0,
            txn_prefix: DEFAULT_TXN_PREFIX.to_owned(),
        };
        let (tx, rx) = mpsc::channel::<InboundEvent>(8);
        let adapter = MatrixAdapter::start_with_api(api, config, tx, dir.path().to_path_buf())
            .unwrap();
        (adapter, dir, rx)
    }

    async fn mount_empty_sync(s: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next_batch": "n", "rooms": { "join": {} }
            })))
            .mount(s)
            .await;
    }

    #[tokio::test]
    async fn channel_type_and_supports_threads() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        assert_eq!(adapter.channel_type().as_str(), "matrix");
        assert!(adapter.supports_threads());
        assert_eq!(adapter.bot_user_id(), "@bot:m.org");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_text_routes_to_send_text() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$x:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({ "text": "hi" }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$x:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_with_thread_id_routes_to_send_threaded() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .and(wiremock::matchers::body_string_contains("\"m.thread\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$t:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "!a:m.org",
                Some("$root:m.org"),
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({ "text": "in-thread" }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$t:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_html_routes_to_send_html() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .and(wiremock::matchers::body_string_contains("\"formatted_body\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$h:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({ "text": "hi", "html": "<b>hi</b>" }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$h:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_html_without_text_uses_html_as_plain() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .and(wiremock::matchers::body_string_contains("\"format\":\"org.matrix.custom.html\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$h2:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({ "html": "<b>hi</b>" }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$h2:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_edit_action_uses_m_replace() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .and(wiremock::matchers::body_string_contains("\"m.replace\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$e:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({
                        "action": "edit",
                        "target_event_id": "$tgt:m.org",
                        "text": "updated"
                    }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$e:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_reaction_action_uses_m_annotation() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.reaction/.+"))
            .and(wiremock::matchers::body_string_contains("\"m.annotation\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$r:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({
                        "action": "reaction",
                        "target_event_id": "$tgt:m.org",
                        "emoji": "\u{1F44D}"
                    }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$r:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_action_missing_target_errors() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let err = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({"action": "edit", "text": "x"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_unknown_action_is_unsupported() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let err = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({"action": "shrug"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_with_files_uploads_then_sends_media() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("POST"))
            .and(path("/_matrix/media/v3/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content_uri": "mxc://m.org/abc"
            })))
            .mount(&s)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$file:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "caption"}),
                    files: vec![OutboundFile {
                        filename: "doc.txt".into(),
                        data: vec![1, 2, 3],
                    }],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$file:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_with_image_uses_m_image() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("POST"))
            .and(path("/_matrix/media/v3/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content_uri": "mxc://m.org/p"
            })))
            .mount(&s)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .and(wiremock::matchers::body_string_contains("\"m.image\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$img:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({}),
                    files: vec![OutboundFile {
                        filename: "pic.png".into(),
                        data: vec![0; 4],
                    }],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$img:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_with_no_text_no_files_errors() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let err = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn set_typing_calls_typing_endpoint() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/typing/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        adapter.set_typing("!a:m.org", None).await.unwrap();
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn subscribe_adds_room_to_live_set() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        assert!(adapter.subscribed_rooms().await.is_empty());
        adapter.subscribe("!a:m.org", None).await.unwrap();
        let set = adapter.subscribed_rooms().await;
        assert!(set.contains("!a:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn subscribe_resolves_alias_via_api() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/v3/directory/room/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "room_id": "!real:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        adapter.subscribe("#alias:m.org", None).await.unwrap();
        let set = adapter.subscribed_rooms().await;
        assert!(set.contains("!real:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn alias_cache_avoids_second_lookup() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cc = counter.clone();
        Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/v3/directory/room/.+"))
            .respond_with(move |_req: &wiremock::Request| {
                cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(json!({
                    "room_id": "!real:m.org"
                }))
            })
            .mount(&s)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/typing/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        adapter.set_typing("#alias:m.org", None).await.unwrap();
        adapter.set_typing("#alias:m.org", None).await.unwrap();
        // First call resolves; second call should hit the cache.
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn open_dm_returns_none() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        assert!(adapter.open_dm("@u:m.org").await.unwrap().is_none());
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        adapter.shutdown().await;
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn debug_format_renders() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let s = format!("{adapter:?}");
        assert!(s.contains("MatrixAdapter"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn config_and_api_accessors() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        assert_eq!(adapter.config().user_id, "@bot:m.org");
        assert_eq!(adapter.api().access_token(), "tok");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn start_initialises_configured_rooms() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let dir = TempDir::new().unwrap();
        let cfg = MatrixConfig {
            homeserver_url: s.uri(),
            access_token: "tok".into(),
            user_id: "@bot:m.org".into(),
            rooms: vec!["!a:m.org".into(), "!b:m.org".into()],
            sync_timeout_ms: 0,
            txn_prefix: DEFAULT_TXN_PREFIX.into(),
        };
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let adapter = MatrixAdapter::start(cfg, tx, dir.path().to_path_buf()).unwrap();
        let set = adapter.subscribed_rooms().await;
        assert!(set.contains("!a:m.org"));
        assert!(set.contains("!b:m.org"));
        adapter.shutdown().await;
    }

    #[test]
    fn guess_mime_for_common_extensions() {
        assert_eq!(guess_mime("a.png"), "image/png");
        assert_eq!(guess_mime("a.jpg"), "image/jpeg");
        assert_eq!(guess_mime("a.JPEG"), "image/jpeg");
        assert_eq!(guess_mime("a.gif"), "image/gif");
        assert_eq!(guess_mime("a.webp"), "image/webp");
        assert_eq!(guess_mime("a.mp4"), "video/mp4");
        assert_eq!(guess_mime("a.webm"), "video/webm");
        assert_eq!(guess_mime("a.mp3"), "audio/mpeg");
        assert_eq!(guess_mime("a.ogg"), "audio/ogg");
        assert_eq!(guess_mime("a.wav"), "audio/wav");
        assert_eq!(guess_mime("a.txt"), "text/plain");
        assert_eq!(guess_mime("a.json"), "application/json");
        assert_eq!(guess_mime("a.pdf"), "application/pdf");
        assert_eq!(guess_mime("blob"), "application/octet-stream");
        assert_eq!(guess_mime("blob.weird"), "application/octet-stream");
    }

    #[test]
    fn msgtype_for_mime_branches() {
        assert_eq!(msgtype_for_mime("image/png"), "m.image");
        assert_eq!(msgtype_for_mime("audio/mpeg"), "m.audio");
        assert_eq!(msgtype_for_mime("video/mp4"), "m.video");
        assert_eq!(msgtype_for_mime("application/pdf"), "m.file");
    }

    #[test]
    fn extract_text_and_html_and_action() {
        assert_eq!(extract_text(&json!({})), "");
        assert_eq!(extract_text(&json!({"text": "x"})), "x");
        assert_eq!(extract_html(&json!({})), None);
        assert_eq!(extract_html(&json!({"html":"<b>x</b>"})).as_deref(), Some("<b>x</b>"));
        assert_eq!(extract_action(&json!({"action": "edit"})), Some("edit"));
        assert_eq!(extract_action(&json!({})), None);
    }

    #[test]
    fn required_str_errors_when_missing() {
        let err = required_str(&json!({}), "x").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    // ── Breadcrumb chip rendering ──────────────────────────────────

    #[test]
    fn render_breadcrumb_html_matrix_running_uses_code_and_ascii_marker() {
        let bc = copperclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check");
        let html = super::render_breadcrumb_html_matrix(&bc);
        assert!(html.starts_with("[~]"), "got: {html}");
        assert!(html.contains("<code>shell</code>"), "got: {html}");
        assert!(html.contains("cargo check"));
    }

    #[test]
    fn render_breadcrumb_html_matrix_done_with_summary() {
        let bc = copperclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check")
            .finished(true, Some("passed (0.4s)".into()));
        let html = super::render_breadcrumb_html_matrix(&bc);
        assert!(html.starts_with("[ok]"), "got: {html}");
        assert!(html.contains("passed (0.4s)"));
    }

    #[test]
    fn render_breadcrumb_html_matrix_escapes_special_chars() {
        let bc = copperclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("echo <a> & <b>");
        let html = super::render_breadcrumb_html_matrix(&bc);
        assert!(html.contains("echo &lt;a&gt; &amp; &lt;b&gt;"));
    }

    #[test]
    fn render_breadcrumb_aggregate_renders_styled_details_steps() {
        use copperclaw_channels_core::{Breadcrumb, BreadcrumbStatus};
        let steps = vec![
            Breadcrumb::running("read_file")
                .with_detail("src/App.tsx")
                .finished(true, Some("120 lines".into())),
            Breadcrumb::running("shell").with_detail("npm run build"),
        ];
        let agg = Breadcrumb {
            tool_name: "activity".into(),
            detail: Some("shell npm run build".into()),
            status: BreadcrumbStatus::Running,
            summary: Some("1/2 steps".into()),
            steps,
        };
        let html = super::render_breadcrumb_html_matrix(&agg);
        // Native Matrix collapse primitive: <details>/<summary> wrapper.
        assert!(html.starts_with("<details><summary>"), "details wrapper: {html}");
        assert!(html.ends_with("</details>"), "details closed: {html}");
        // Collapsed summary line with ASCII marker + count inside <summary>.
        assert!(
            html.contains("<summary>[~] shell npm run build · 1/2 steps</summary>"),
            "summary line: {html}"
        );
        // Each step individually styled (bold tool, <code> detail, italic
        // summary) — not raw text / markdown.
        assert!(
            html.contains("[ok] <b>read_file</b> <code>src/App.tsx</code> <i>— 120 lines</i>"),
            "step 1: {html}"
        );
        assert!(
            html.contains("[~] <b>shell</b> <code>npm run build</code>"),
            "step 2: {html}"
        );
        assert!(!html.contains("**"), "no raw markdown: {html}");
    }

    #[test]
    fn render_breadcrumb_aggregate_escapes_and_caps_steps() {
        use copperclaw_channels_core::{Breadcrumb, BreadcrumbStatus};
        let mut steps: Vec<Breadcrumb> = (0..50)
            .map(|i| {
                Breadcrumb::running("shell")
                    .with_detail(format!("step {i}"))
                    .finished(true, None)
            })
            .collect();
        steps.push(Breadcrumb::running("write_file").with_detail("a <b> & c"));
        let agg = Breadcrumb {
            tool_name: "activity".into(),
            detail: Some("write_file a <b> & c".into()),
            status: BreadcrumbStatus::Running,
            summary: Some("50/51 steps".into()),
            steps,
        };
        let html = super::render_breadcrumb_html_matrix(&agg);
        // Over the cap (40) → "+N earlier step(s)" note inside <details>.
        assert!(html.contains("earlier step(s)"), "cap note: {html}");
        // 51 steps, cap 40 → 11 hidden.
        assert!(html.contains("+11 earlier step(s)"), "cap count: {html}");
        // HTML in a detail is escaped individually (no raw < > & on the wire)
        // in BOTH the collapsed summary and the per-step line.
        assert!(html.contains("a &lt;b&gt; &amp; c"), "escaped: {html}");
        assert!(!html.contains("<b> &"), "no raw markup leak: {html}");
    }

    #[tokio::test]
    async fn deliver_breadcrumb_aggregate_first_emit_sends_details() {
        use copperclaw_channels_core::{Breadcrumb, BreadcrumbStatus};
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .and(wiremock::matchers::body_string_contains("\"m.notice\""))
            .and(wiremock::matchers::body_string_contains("<details><summary>"))
            .and(wiremock::matchers::body_string_contains("<b>read_file</b>"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$agg:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let agg = Breadcrumb {
            tool_name: "activity".into(),
            detail: Some("shell npm run build".into()),
            status: BreadcrumbStatus::Running,
            summary: Some("1/2 steps".into()),
            steps: vec![
                Breadcrumb::running("read_file")
                    .with_detail("src/App.tsx")
                    .finished(true, Some("120 lines".into())),
                Breadcrumb::running("shell").with_detail("npm run build"),
            ],
        };
        let id = adapter
            .deliver_breadcrumb("!room:m.org", None, &agg, None)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$agg:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_breadcrumb_aggregate_edit_path_renders_details() {
        use copperclaw_channels_core::{Breadcrumb, BreadcrumbStatus};
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        // The m.replace edit path must ALSO carry the <details> aggregate.
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .and(wiremock::matchers::body_string_contains("\"m.replace\""))
            .and(wiremock::matchers::body_string_contains("\"m.notice\""))
            .and(wiremock::matchers::body_string_contains("<details><summary>"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$aggedit:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let agg = Breadcrumb {
            tool_name: "activity".into(),
            detail: Some("shell npm run build".into()),
            status: BreadcrumbStatus::Done,
            summary: Some("2/2 steps".into()),
            steps: vec![
                Breadcrumb::running("read_file")
                    .with_detail("src/App.tsx")
                    .finished(true, Some("120 lines".into())),
                Breadcrumb::running("shell")
                    .with_detail("npm run build")
                    .finished(true, Some("ok".into())),
            ],
        };
        let id = adapter
            .deliver_breadcrumb("!room:m.org", None, &agg, Some("$prev:m.org"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$aggedit:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_breadcrumb_running_sends_notice_html() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .and(wiremock::matchers::body_string_contains("\"m.notice\""))
            .and(wiremock::matchers::body_string_contains("<code>shell</code>"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$chip:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let bc = copperclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check");
        let id = adapter
            .deliver_breadcrumb("!room:m.org", None, &bc, None)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$chip:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_breadcrumb_with_existing_id_emits_m_replace() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .and(wiremock::matchers::body_string_contains("\"m.replace\""))
            .and(wiremock::matchers::body_string_contains("\"m.notice\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$edit:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let bc = copperclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check")
            .finished(true, Some("passed (0.4s)".into()));
        let id = adapter
            .deliver_breadcrumb("!room:m.org", None, &bc, Some("$prev:m.org"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$edit:m.org"));
        adapter.shutdown().await;
    }

    // ── Diff card rendering ────────────────────────────────────────

    #[test]
    fn render_diff_html_matrix_wraps_body_in_language_diff_pre_code() {
        let card = copperclaw_channels_core::DiffCard {
            path: "src/lib.rs".into(),
            language: Some("rust".into()),
            hunks: vec![copperclaw_channels_core::DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                lines: vec![
                    copperclaw_channels_core::DiffLine {
                        kind: copperclaw_channels_core::DiffLineKind::Remove,
                        text: "let x = 1;".into(),
                    },
                    copperclaw_channels_core::DiffLine {
                        kind: copperclaw_channels_core::DiffLineKind::Add,
                        text: "let x = 2;".into(),
                    },
                ],
            }],
            added: 1,
            removed: 1,
            truncated: false,
        };
        let html = super::render_diff_html_matrix(&card);
        assert!(html.contains("<b>src/lib.rs</b>"));
        assert!(html.contains("(+1 / -1)"));
        assert!(html.contains("<pre><code class=\"language-diff\">"));
        assert!(html.contains("@@ -1,1 +1,1 @@"));
        assert!(html.contains("-let x = 1;"));
        assert!(html.contains("+let x = 2;"));
        assert!(html.trim_end().ends_with("</code></pre>"));
    }

    #[test]
    fn render_diff_html_matrix_escapes_hostile_source_chars() {
        let card = copperclaw_channels_core::DiffCard {
            path: "x.rs".into(),
            language: None,
            hunks: vec![copperclaw_channels_core::DiffHunk {
                old_start: 1,
                old_lines: 0,
                new_start: 1,
                new_lines: 1,
                lines: vec![copperclaw_channels_core::DiffLine {
                    kind: copperclaw_channels_core::DiffLineKind::Add,
                    text: "fn f<T>() -> &str { x & y }".into(),
                }],
            }],
            added: 1,
            removed: 0,
            truncated: false,
        };
        let html = super::render_diff_html_matrix(&card);
        assert!(html.contains("&lt;T&gt;"));
        assert!(html.contains("&amp;"));
    }

    #[tokio::test]
    async fn deliver_diff_sends_notice_html_with_language_diff_pre() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .and(wiremock::matchers::body_string_contains("\"m.notice\""))
            .and(wiremock::matchers::body_string_contains("language-diff"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$diff:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let card = copperclaw_channels_core::DiffCard {
            path: "src/lib.rs".into(),
            language: Some("rust".into()),
            hunks: vec![copperclaw_channels_core::DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                lines: vec![
                    copperclaw_channels_core::DiffLine {
                        kind: copperclaw_channels_core::DiffLineKind::Add,
                        text: "fn main() {}".into(),
                    },
                ],
            }],
            added: 1,
            removed: 0,
            truncated: false,
        };
        let id = adapter
            .deliver_diff("!room:m.org", None, &card)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$diff:m.org"));
        adapter.shutdown().await;
    }

    // ── Error card rendering ────────────────────────────────────────

    #[test]
    fn render_error_html_matrix_wraps_red_label() {
        let card = copperclaw_channels_core::ErrorCard::new(
            copperclaw_channels_core::ErrorCardKind::Internal,
            "the shell tool timed out",
        )
        .with_title("Tool failed");
        let html = super::render_error_html_matrix(&card);
        // Note: raw string uses double `#` delimiter because the
        // colour value already contains a `#` — single `#` would
        // close the raw string early.
        assert!(html.contains(r##"<font color="#cc3333">"##));
        assert!(html.contains("Tool error: Tool failed"));
        assert!(html.contains("the shell tool timed out"));
    }

    #[test]
    fn render_error_html_matrix_includes_pre_code_for_details() {
        let card = copperclaw_channels_core::ErrorCard::new(
            copperclaw_channels_core::ErrorCardKind::Provider,
            "summary line",
        )
        .with_details("stderr line 1\nstderr line 2");
        let html = super::render_error_html_matrix(&card);
        assert!(html.contains("<pre><code>"));
        assert!(html.contains("stderr line 1"));
        assert!(html.contains("</code></pre>"));
    }

    #[test]
    fn render_error_html_matrix_appends_retry_footer() {
        let card = copperclaw_channels_core::ErrorCard::new(
            copperclaw_channels_core::ErrorCardKind::Delivery,
            "telegram 502",
        )
        .retryable();
        let html = super::render_error_html_matrix(&card);
        assert!(html.contains("<em>will retry automatically</em>"));
    }

    #[test]
    fn render_error_html_matrix_escapes_user_content() {
        // User-supplied stderr containing HTML must NOT survive verbatim
        // — escape every angle bracket and ampersand.
        let card = copperclaw_channels_core::ErrorCard::new(
            copperclaw_channels_core::ErrorCardKind::Internal,
            "got: <script>alert(1)</script>",
        )
        .with_title("attack? & escape");
        let html = super::render_error_html_matrix(&card);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("attack? &amp; escape"));
    }

    #[tokio::test]
    async fn deliver_error_sends_html_via_m_text() {
        // Crucially `m.text` (not `m.notice`) — errors warrant
        // notification badges; muting them in Element would defeat
        // the surface's purpose.
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .and(wiremock::matchers::body_string_contains("\"m.text\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$err:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let card = copperclaw_channels_core::ErrorCard::new(
            copperclaw_channels_core::ErrorCardKind::Provider,
            "model 502 after retry exhaustion",
        );
        let id = adapter
            .deliver_error("!room:m.org", None, &card)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$err:m.org"));
        adapter.shutdown().await;
    }

    // ── Long-output expander (slice 3.4) rendering ────────────────

    #[test]
    fn render_collapsible_html_matrix_wraps_in_details_summary() {
        // Native Matrix primitive: `<details><summary>` is the
        // disclosure widget Element renders natively.
        let body = (0..10).map(|i| format!("L{i}")).collect::<Vec<_>>().join("\n");
        let html = super::render_collapsible_html_matrix(&body, "10 lines (40 B)", &[]);
        assert!(html.starts_with("<details>"), "got: {html}");
        assert!(html.ends_with("</details>"), "got: {html}");
        assert!(html.contains("<summary><em>10 lines (40 B)</em></summary>"));
        assert!(html.contains("<pre><code>"));
        assert!(html.contains("L0"));
        assert!(html.contains("L9"));
    }

    #[test]
    fn render_collapsible_html_matrix_escapes_html_in_body() {
        // Tool output with literal `<` or `&` must be escaped; bare
        // `<script>` would otherwise hand the rendering to the
        // client's parser uncontrolled.
        let html = super::render_collapsible_html_matrix(
            "<script>alert(1)</script>",
            "fetched 1 page",
            &[],
        );
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[tokio::test]
    async fn deliver_collapsible_sends_html_details_body() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(wiremock::matchers::path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+$",
            ))
            .and(wiremock::matchers::body_string_contains("<details>"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$long:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let body = (0..40).map(|i| format!("L{i}")).collect::<Vec<_>>().join("\n");
        let preview: Vec<String> = (0..3).map(|i| format!("L{i}")).collect();
        let id = adapter
            .deliver_collapsible("!room:m.org", None, &body, "shell 40 lines", &preview)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$long:m.org"));
        adapter.shutdown().await;
    }

    // ── Thinking block (slice 3.5) rendering ────────────────────────

    #[test]
    fn render_thinking_html_matrix_wraps_body_in_details() {
        // Native Matrix primitive for this surface is `<details>` —
        // same disclosure element surface 4 uses for long-output. The
        // summary line carries the `reasoning` italic label.
        let t = ThinkingBlock::visible("Let me work through the question.");
        let html = super::render_thinking_html_matrix(&t);
        assert!(html.starts_with("<details>"), "got: {html}");
        assert!(html.contains("<summary><em>reasoning</em></summary>"), "got: {html}");
        assert!(html.contains("<blockquote>"), "got: {html}");
        assert!(html.contains("Let me work through the question."));
        assert!(html.ends_with("</details>"));
    }

    #[test]
    fn render_thinking_html_matrix_includes_model_provenance() {
        let t = ThinkingBlock::visible("ok").with_model("claude-opus-4-7");
        let html = super::render_thinking_html_matrix(&t);
        assert!(
            html.contains("<summary><em>reasoning (claude-opus-4-7)</em></summary>"),
            "got: {html}"
        );
    }

    #[test]
    fn render_thinking_html_matrix_escapes_html_in_body() {
        let t = ThinkingBlock::visible("<script>alert(1)</script>");
        let html = super::render_thinking_html_matrix(&t);
        assert!(!html.contains("<script>"), "got: {html}");
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn render_thinking_html_matrix_redacted_emits_placeholder_only() {
        // Privacy contract: redacted blocks must never put the raw
        // blob on the wire.
        let t = ThinkingBlock::redacted("opaque-secret-blob");
        let html = super::render_thinking_html_matrix(&t);
        assert!(html.contains("(redacted reasoning)"));
        assert!(!html.contains("opaque-secret-blob"), "leak: {html}");
    }

    // ── TodoList chip rendering ────────────────────────────────────

    fn matrix_todo_list_sample() -> copperclaw_channels_core::TodoList {
        copperclaw_channels_core::TodoList {
            items: vec![
                copperclaw_channels_core::TodoListItem {
                    id: 1,
                    text: "Wash dishes".into(),
                    status: copperclaw_channels_core::TodoItemStatus::Completed,
                },
                copperclaw_channels_core::TodoListItem {
                    id: 2,
                    text: "Dry dishes".into(),
                    status: copperclaw_channels_core::TodoItemStatus::Pending,
                },
            ],
            title: Some("Kitchen".into()),
        }
    }

    #[test]
    fn render_todo_list_html_matrix_emits_h4_and_ul() {
        let html = super::render_todo_list_html_matrix(&matrix_todo_list_sample());
        assert!(html.contains("<h4>Kitchen (1/2)</h4>"), "header: {html}");
        assert!(html.contains("<ul>"));
        assert!(html.contains("[x] <s>Wash dishes</s>"), "done strike: {html}");
        assert!(html.contains("[ ] Dry dishes"), "pending: {html}");
    }
}
