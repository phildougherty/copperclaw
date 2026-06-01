//! Channel adapter and factory traits.

use crate::breadcrumb::Breadcrumb;
use crate::card::Card;
use crate::container::ContainerContribution;
use crate::diff::DiffCard;
use crate::dm::DmHandle;
use crate::error::AdapterError;
use crate::error_card::ErrorCard;
use crate::setup::ChannelSetup;
use crate::thinking::ThinkingBlock;
use crate::todo_list::TodoList;
use async_trait::async_trait;
use copperclaw_types::{ChannelType, MessageKind, OutboundMessage};
use std::sync::Arc;

/// A channel adapter speaks to one external platform (Telegram, Slack, …).
///
/// The host calls into the adapter to subscribe to events, signal typing,
/// deliver outbound messages, and (where supported) open DMs.
///
/// All methods except `deliver` have sensible defaults so channel
/// implementations only override what the platform actually supports.
#[async_trait]
pub trait ChannelAdapter: Send + Sync {
    /// The `ChannelType` this adapter handles (e.g. `ChannelType::new("telegram")`).
    fn channel_type(&self) -> &ChannelType;

    /// Whether the platform has a distinct concept of threads.
    /// Defaults to `false`.
    fn supports_threads(&self) -> bool {
        false
    }

    /// Maximum length (in `char`s, not bytes) of a single chat message the
    /// platform will accept. Returning `Some(n)` opts the channel into the
    /// host's outbound chat-splitter: chat rows whose `content.text` exceeds
    /// `n` chars are split into a sequence of messages (paragraph → sentence
    /// → hard cut) before [`Self::deliver`] is called.
    ///
    /// Returning `None` (the default) disables the splitter — the adapter
    /// will receive the original message regardless of length, and is
    /// responsible for its own length handling. Use `None` for channels
    /// with no documented hard cap (Matrix), or where the cap is large
    /// enough that splitting would be surprising (email).
    fn max_message_chars(&self) -> Option<usize> {
        None
    }

    /// Begin observing the given conversation for inbound events. For
    /// channels that already stream everything (e.g. long-polling bots)
    /// this is a no-op. Defaults to `Ok(())`.
    async fn subscribe(
        &self,
        _platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        Ok(())
    }

    /// Send a typing indicator to the platform. Defaults to `Ok(())` so
    /// channels without typing support are silent no-ops.
    async fn set_typing(
        &self,
        _platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        Ok(())
    }

    /// Deliver an outbound message. Returns the platform-side message id
    /// when known (`None` if the platform doesn't expose one).
    async fn deliver(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError>;

    /// Render and deliver a portable [`Card`].
    ///
    /// The default implementation converts the card to the plain-text
    /// rendering from [`Card::to_text_fallback`] and routes it through
    /// [`Self::deliver`] as a `MessageKind::Chat` outbound message — so
    /// every adapter gets a usable card rendering for free, even before
    /// it provides a native implementation.
    ///
    /// Adapters with rich card support (Telegram inline keyboards, Slack
    /// Block Kit, Discord embeds, Google Chat cards v2, etc.) should
    /// override this to render the card structurally. Wave 2 of the cards
    /// rollout will implement those overrides — see the doc comment at
    /// the top of `card.rs` for the schema contract.
    ///
    /// `to` is a routing hint the host pulls from `SendCardSpec::to`. It
    /// is currently unused by the default impl (which uses `platform_id`)
    /// but is part of the signature so wave 2's native renderers can pass
    /// it through to platform DM-open flows.
    async fn deliver_card(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        card: &Card,
        to: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        let _ = to;
        let text = card.to_text_fallback();
        let outbound = OutboundMessage {
            kind: MessageKind::Chat,
            content: serde_json::json!({ "text": text }),
            files: vec![],
        };
        self.deliver(platform_id, thread_id, &outbound).await
    }

    /// Render and deliver a [`Breadcrumb`] chip.
    ///
    /// Breadcrumbs are emitted around tool invocations to surface
    /// "agent is working on X" UX without polluting the conversation
    /// with chat rows. The default impl converts the breadcrumb to
    /// [`Breadcrumb::to_text_fallback`] and routes through
    /// [`Self::deliver`] as `MessageKind::Chat` — so every adapter
    /// has a usable rendering for free, mirroring today's `[tool]
    /// detail` chat-breadcrumb behaviour.
    ///
    /// Adapters with rich native rendering override this to draw a
    /// compact chip (Telegram HTML `<code>`, Slack Block Kit
    /// `context` block, Discord embed footer, Google Chat cards v2,
    /// Matrix `m.notice` with `<code>`, …).
    ///
    /// `existing_message_id` is the platform-side message id of a
    /// previously delivered breadcrumb for the same logical event,
    /// when known. Adapters with an edit API (Telegram
    /// `editMessageText`, Slack `chat.update`, Discord `PATCH`,
    /// Google Chat `spaces.messages.patch`, Matrix `m.replace`)
    /// MUST edit the original chip in place when this is `Some` so
    /// the user sees `Running…` → `Done` rather than a new line on
    /// every tool boundary. Adapters without an edit API SHOULD
    /// ignore the argument and emit a fresh chip (visible but
    /// harmless).
    ///
    /// Return value: the platform-side message id of the rendered
    /// chip. For an in-place edit, returning the original id is
    /// recommended so the host can keep editing the same chip on
    /// subsequent updates.
    async fn deliver_breadcrumb(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        breadcrumb: &Breadcrumb,
        existing_message_id: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        let _ = existing_message_id;
        let text = breadcrumb.to_text_fallback();
        let outbound = OutboundMessage {
            kind: MessageKind::Chat,
            content: serde_json::json!({ "text": text }),
            files: vec![],
        };
        self.deliver(platform_id, thread_id, &outbound).await
    }

    /// Render and deliver a [`DiffCard`] — the slice-3.1 "file edit
    /// diff" surface.
    ///
    /// The runner emits one of these via `MessageKind::Diff` after a
    /// successful `edit_file` / `multi_edit` / `apply_patch` /
    /// `write_file` write, carrying the structured per-hunk diff
    /// computed from the pre-edit snapshot vs the post-edit content.
    /// The diff card is delivered *alongside* the existing tool
    /// breadcrumb — the breadcrumb says "what tool ran", the diff card
    /// shows "what changed".
    ///
    /// Adapters with native code-block / fenced-diff rendering
    /// (Telegram MarkdownV2 ` ```diff ``` `, Slack Block Kit
    /// `rich_text_preformatted`, Discord embed with `description`
    /// fenced block + color, Google Chat Cards v2 `decoratedText`,
    /// Matrix `<pre><code class="language-diff">…</code></pre>`)
    /// override this to draw a real diff with `+` / `-` gutters.
    ///
    /// The default impl converts the card to its unified-diff text
    /// rendering via [`DiffCard::to_text_fallback`] and routes through
    /// [`Self::deliver`] as a `MessageKind::Chat` row — so every
    /// adapter has a usable rendering for free.
    ///
    /// Diff cards are *immutable* post-emit — there is no
    /// `existing_message_id` argument, and the host's dispatch path
    /// never edits an emitted diff. If the same file gets edited again
    /// the runner emits a fresh diff card; we never compose two edits
    /// into one card on the wire.
    async fn deliver_diff(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        diff: &DiffCard,
    ) -> Result<Option<String>, AdapterError> {
        let text = diff.to_text_fallback();
        let outbound = OutboundMessage {
            kind: MessageKind::Chat,
            content: serde_json::json!({ "text": text }),
            files: vec![],
        };
        self.deliver(platform_id, thread_id, &outbound).await
    }

    /// Render and deliver a "summary + collapsible expander" treatment
    /// of an outbound chat message whose body is long enough to warrant
    /// hiding behind a disclosure widget.
    ///
    /// This is the slice-3.4 "long-output expander" decorator surface.
    /// It is intentionally NOT its own `MessageKind` — the agent is
    /// just emitting chat; the runner attaches the decorator metadata
    /// in long-output tool handlers (`shell`, `web_fetch`, `read_file`,
    /// `grep`, …) when output exceeds a threshold (default 30 lines OR
    /// 4 KB, whichever hits first). `dispatch_chat` routes here when
    /// `content.expander` is present on the row.
    ///
    /// Arguments:
    /// - `text` — the full body (what the user sees when they expand).
    /// - `summary` — host-generated one-liner (e.g. `"shell command
    ///   produced 312 lines (12 KB)"`) shown collapsed.
    /// - `preview_lines` — first ~6 lines from the body; renderers may
    ///   show these in the collapsed view as a teaser. Empty if the
    ///   runner couldn't extract a useful preview.
    ///
    /// Adapters with rich native primitives override this:
    /// - Telegram: `<blockquote expandable>` (Bot API 7.6+).
    /// - Slack: `section` + button → click triggers full-text follow-up.
    /// - Discord: embed `description` with preview + "Show full" button.
    /// - Google Chat: Cards v2 `collapsibleSection`.
    /// - Matrix: HTML `<details><summary>…</summary>…</details>`.
    /// - Teams: `Container` `isVisible:false` + `Action.ToggleVisibility`.
    ///
    /// The default impl gracefully degrades: emit a Chat row that
    /// concatenates the summary, the preview, and a `…(N more lines)`
    /// truncation marker — so adapters without a native primitive still
    /// give the user a readable, scoped rendering. The full body is
    /// kept on disk in the outbound row regardless; this method's job
    /// is the on-the-wire shape only.
    async fn deliver_collapsible(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        text: &str,
        summary: &str,
        preview_lines: &[String],
    ) -> Result<Option<String>, AdapterError> {
        let body = render_collapsible_text_fallback(text, summary, preview_lines);
        let outbound = OutboundMessage {
            kind: MessageKind::Chat,
            content: serde_json::json!({ "text": body }),
            files: vec![],
        };
        self.deliver(platform_id, thread_id, &outbound).await
    }

    /// Render and deliver a portable [`TodoList`] — the slice-3.2
    /// "live, pinned checklist" surface.
    ///
    /// The runner emits one of these via `MessageKind::TodoList` after
    /// each `todo_add` / `todo_update` / `todo_delete` MCP-tool
    /// mutation, carrying the *full* post-mutation list. The host's
    /// delivery service looks up the prior list's platform message id
    /// (when one exists) and threads it through as
    /// `existing_message_id` so adapters with an edit API can REPLACE
    /// the existing chip rather than emit a new message every time the
    /// agent moves an item to `in_progress`.
    ///
    /// `pin_hint`:
    /// - `true` on the first emit per session: ask the adapter to pin
    ///   the rendered chip (Telegram `pinChatMessage`, Slack
    ///   `pins.add`, Matrix `m.room.pinned_events`).
    /// - `true` again once every item is `Completed` so the adapter
    ///   may unpin (the adapter inspects [`TodoList::is_fully_completed`]
    ///   to choose the verb).
    ///
    /// Adapters that lack a pin API (Discord — bots typically can't
    /// pin; Google Chat — no public pin API) SHOULD silently treat
    /// `pin_hint` as a no-op rather than fail the delivery; the chip
    /// is the load-bearing UX, pinning is best-effort decoration.
    ///
    /// The default impl converts the list to
    /// [`TodoList::to_text_fallback`] and routes through
    /// [`Self::deliver`] as `MessageKind::Chat` — so every adapter
    /// has a usable rendering for free even before it provides a
    /// native renderer. Returns the platform-side message id of the
    /// rendered chip so the host can keep editing it on subsequent
    /// mutations.
    async fn deliver_todo_list(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        list: &TodoList,
        existing_message_id: Option<&str>,
        pin_hint: bool,
    ) -> Result<Option<String>, AdapterError> {
        let _ = (existing_message_id, pin_hint);
        let text = list.to_text_fallback();
        let outbound = OutboundMessage {
            kind: MessageKind::Chat,
            content: serde_json::json!({ "text": text }),
            files: vec![],
        };
        self.deliver(platform_id, thread_id, &outbound).await
    }

    /// Render and deliver a portable [`ErrorCard`] — the slice-3.3
    /// "visually-distinct error" surface.
    ///
    /// `ErrorCard` rows are emitted by the **host**, not the model:
    ///
    /// - Internal tool errors (`ToolError::Internal`).
    /// - Provider terminal failures (after retry exhaustion).
    /// - Delivery retry exhaustion (3 failed adapter sends in a row).
    ///
    /// Channels with color affordances render in red (Slack
    /// `attachments.color: "danger"`, Discord embed `color = 0xE74C3C`,
    /// Matrix `<font color="red">`, Google Chat decorated icon);
    /// channels without color (Telegram) use bold HTML + monospace
    /// details and rely on the `[ERROR]` text prefix the default
    /// fallback emits.
    ///
    /// No edit-in-place: errors are immutable receipts — there is no
    /// `existing_message_id` argument and no `update_error` system
    /// action to mirror `update_breadcrumb`.
    ///
    /// The default impl converts via [`ErrorCard::to_text_fallback`]
    /// and routes through [`Self::deliver`] as `MessageKind::Chat` so
    /// every adapter has a usable rendering for free — the `[ERROR:
    /// <kind>]` prefix carries the severity signal on plain-text
    /// channels even before a native renderer ships.
    async fn deliver_error(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        err: &ErrorCard,
    ) -> Result<Option<String>, AdapterError> {
        let text = err.to_text_fallback();
        let outbound = OutboundMessage {
            kind: MessageKind::Chat,
            content: serde_json::json!({ "text": text }),
            files: vec![],
        };
        self.deliver(platform_id, thread_id, &outbound).await
    }

    /// Render and deliver a portable [`ThinkingBlock`] — the slice-3.5
    /// opt-in "surface model reasoning" surface.
    ///
    /// `ThinkingBlock` rows are emitted by the runner when the provider
    /// streams a `thinking` (or `redacted_thinking`) content block AND
    /// the per-group `surface_thinking` config knob is on. The default
    /// is off — surfacing model chain-of-thought has privacy
    /// implications (mid-thought speculation about the user, debugging
    /// notes the model didn't intend the user to see, etc.).
    ///
    /// Channels with a native collapsed-section primitive
    /// (Telegram `<blockquote expandable>`, Slack `context` block,
    /// Discord muted-grey embed, Google Chat `collapsibleSection`,
    /// Matrix `<details>`) render the block collapsed by default so the
    /// chat stays uncluttered and the user opens the disclosure widget
    /// only when they want to see the reasoning. Channels without one
    /// fall through to the default text fallback (a `[reasoning]`-
    /// headered quoted block) via [`Self::deliver`].
    ///
    /// No edit-in-place: thinking blocks are point-in-time receipts of
    /// what the model thought before that turn's reply — there is no
    /// `existing_message_id` argument and no `update_thinking` system
    /// action to mirror `update_breadcrumb`.
    ///
    /// The default impl converts via [`ThinkingBlock::to_text_fallback`]
    /// and routes through [`Self::deliver`] as `MessageKind::Chat` so
    /// every adapter has a usable rendering for free — even before a
    /// native renderer ships, the user sees the `[reasoning]` header +
    /// quoted text and can tell the block apart from the agent's reply.
    async fn deliver_thinking(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        thinking: &ThinkingBlock,
    ) -> Result<Option<String>, AdapterError> {
        let text = thinking.to_text_fallback();
        let outbound = OutboundMessage {
            kind: MessageKind::Chat,
            content: serde_json::json!({ "text": text }),
            files: vec![],
        };
        self.deliver(platform_id, thread_id, &outbound).await
    }

    /// Open a direct-message thread with the given user. Defaults to
    /// `Ok(None)` — channels that don't support DMs leave it alone.
    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        Ok(None)
    }

    /// Edit an existing message. `external_id` is the platform's id for
    /// the original message (Telegram's `message_id`, Slack's `ts`,
    /// Discord's message id, etc.).
    ///
    /// Default impl returns `Err(AdapterError::Unsupported(_))` so adapters
    /// that don't expose an edit API fall through cleanly to a
    /// "fallback: send a new message" path in the host's delivery service.
    async fn edit_message(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        external_id: &str,
        new_text: &str,
    ) -> Result<(), AdapterError> {
        let _ = (platform_id, thread_id, external_id, new_text);
        Err(AdapterError::Unsupported("edit_message".into()))
    }

    /// React to a message with an emoji. `external_id` is the platform's
    /// id for the target message (same shape as [`Self::edit_message`]).
    ///
    /// Default impl returns `Err(AdapterError::Unsupported(_))` so adapters
    /// that don't expose a reaction API fall through cleanly to a
    /// "fallback: send a new message" path in the host's delivery service.
    async fn add_reaction(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        external_id: &str,
        emoji: &str,
    ) -> Result<(), AdapterError> {
        let _ = (platform_id, thread_id, external_id, emoji);
        Err(AdapterError::Unsupported("add_reaction".into()))
    }

    /// Strip channel-specific formatting from an outbound message body,
    /// returning a plain-text fallback the channel will accept even when
    /// the original failed a formatting validation.
    ///
    /// The delivery loop calls this when a `deliver` call returned
    /// `AdapterError::BadRequest(_)` with a known formatting-error signature
    /// (e.g. Telegram's "can't parse entities", Slack's block-kit errors,
    /// Discord's embed errors). The returned message must be safe to feed
    /// directly back through `deliver`.
    ///
    /// Default impl returns `None` — adapters without a known plain-text
    /// recovery should fail fast instead of degrading silently. Channels that
    /// know how to strip formatting metadata (parse_mode for Telegram,
    /// `blocks` for Slack, `embeds` for Discord) override this.
    ///
    /// The text body itself is preserved (emoji, unicode included) — only
    /// formatting metadata is removed. Implementations are expected to
    /// prepend a "[reduced formatting] " marker to the text so the user
    /// knows the message arrived in a downgraded shape.
    fn plain_text_fallback(&self, _msg: &OutboundMessage) -> Option<OutboundMessage> {
        None
    }
}

/// Compose the default text-fallback rendering for
/// [`ChannelAdapter::deliver_collapsible`].
///
/// Shape:
///
/// ```text
/// <summary>
/// <preview line 1>
/// <preview line 2>
/// …
/// …(N more lines, M chars total)
/// ```
///
/// When `preview_lines` is empty the body is just `<summary>` + the
/// truncation marker. The full text is intentionally NOT embedded
/// here — adapters relying on the default impl can only signal the
/// shape; the body lives in the outbound row regardless. Native
/// renderers that DO have a disclosure primitive will use `text` in
/// full.
pub fn render_collapsible_text_fallback(
    text: &str,
    summary: &str,
    preview_lines: &[String],
) -> String {
    let total_lines = text.lines().count();
    let total_chars = text.chars().count();
    let preview_used = preview_lines.len();
    let remaining_lines = total_lines.saturating_sub(preview_used);
    let mut body = String::with_capacity(summary.len() + 64);
    body.push_str(summary);
    if !preview_lines.is_empty() {
        body.push('\n');
        for (i, line) in preview_lines.iter().enumerate() {
            body.push_str(line);
            if i + 1 < preview_lines.len() {
                body.push('\n');
            }
        }
    }
    if remaining_lines > 0 {
        body.push('\n');
        body.push_str(&format!(
            "…({remaining_lines} more lines, {total_chars} chars total)"
        ));
    }
    body
}

/// A factory builds a `ChannelAdapter` for a particular channel kind.
///
/// Factories are registered with the `ChannelRegistry` at startup. The
/// host looks up factories by `channel_type` and calls `init` once per
/// configured channel instance.
#[async_trait]
pub trait ChannelFactory: Send + Sync {
    /// Channel type produced by this factory.
    fn channel_type(&self) -> ChannelType;

    /// Build the adapter from the host-provided `ChannelSetup`.
    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError>;

    /// Gracefully tear down any global resources. Defaults to `Ok(())`.
    async fn shutdown(&self) -> Result<(), AdapterError> {
        Ok(())
    }

    /// What this channel contributes to an agent container. Defaults to
    /// `ContainerContribution::default()` (nothing).
    fn container_contribution(&self) -> ContainerContribution {
        ContainerContribution::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{MockAdapter, MockFactory};
    use copperclaw_types::MessageKind;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn outbound() -> OutboundMessage {
        OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "hi"}),
            files: vec![],
        }
    }

    #[tokio::test]
    async fn default_subscribe_returns_ok() {
        // MockAdapter does not override subscribe, so we exercise the default
        // body declared on the trait.
        let a = MockAdapter::new("x");
        a.subscribe("p", None).await.unwrap();
        a.subscribe("p", Some("t")).await.unwrap();
    }

    #[tokio::test]
    async fn default_set_typing_returns_ok() {
        let a = MockAdapter::new("x");
        a.set_typing("p", None).await.unwrap();
    }

    #[tokio::test]
    async fn default_open_dm_returns_none() {
        let a = MockAdapter::new("x");
        let res = a.open_dm("u").await.unwrap();
        assert!(res.is_none());
    }

    /// Adapter that only implements the mandatory `channel_type` /
    /// `deliver` methods — used to verify the trait-level defaults for
    /// `edit_message` and `add_reaction` return `Unsupported`.
    struct MinimalAdapter {
        channel_type: ChannelType,
    }

    #[async_trait]
    impl ChannelAdapter for MinimalAdapter {
        fn channel_type(&self) -> &ChannelType {
            &self.channel_type
        }
        async fn deliver(
            &self,
            _platform_id: &str,
            _thread_id: Option<&str>,
            _message: &OutboundMessage,
        ) -> Result<Option<String>, AdapterError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn default_edit_message_returns_unsupported() {
        let a = MinimalAdapter {
            channel_type: ChannelType::new("minimal"),
        };
        let err = a
            .edit_message("p", None, "ext-1", "new text")
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported(ref s) if s == "edit_message"));
    }

    #[tokio::test]
    async fn default_add_reaction_returns_unsupported() {
        let a = MinimalAdapter {
            channel_type: ChannelType::new("minimal"),
        };
        let err = a
            .add_reaction("p", None, "ext-1", "thumbsup")
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported(ref s) if s == "add_reaction"));
    }

    #[tokio::test]
    async fn default_supports_threads_is_false() {
        let a = MockAdapter::new("x");
        assert!(!a.supports_threads());
    }

    #[tokio::test]
    async fn deliver_records_and_returns_id() {
        let a = MockAdapter::new("x");
        let id = a.deliver("plat-1", None, &outbound()).await.unwrap();
        assert!(id.is_some());
        assert_eq!(a.deliveries().len(), 1);
    }

    #[tokio::test]
    async fn factory_init_returns_adapter() {
        let f = MockFactory::new("mock");
        let (tx, _rx) = mpsc::channel(1);
        let setup = ChannelSetup::new(json!({}), tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "mock");
    }

    #[tokio::test]
    async fn factory_default_shutdown_is_ok() {
        let f = MockFactory::new("mock");
        f.shutdown().await.unwrap();
    }

    #[test]
    fn factory_default_container_contribution_is_empty() {
        let f = MockFactory::new("mock");
        assert!(f.container_contribution().is_empty());
    }

    #[tokio::test]
    async fn default_deliver_card_falls_back_to_text_deliver() {
        // Cards with no native renderer get the trait-level fallback:
        // convert to text via `Card::to_text_fallback` and call `deliver`.
        let a = MockAdapter::new("ch");
        let card = crate::card::Card {
            title: Some("Hello".into()),
            body: Some("World".into()),
            ..crate::card::Card::default()
        };
        let id = a.deliver_card("plat-1", None, &card, None).await.unwrap();
        assert!(id.is_some());
        let calls = a.deliveries();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].message.kind, MessageKind::Chat);
        let text = calls[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(text.contains("**Hello**"));
        assert!(text.contains("World"));
    }

    #[tokio::test]
    async fn default_deliver_card_propagates_routing_args() {
        let a = MockAdapter::new("ch");
        let card = crate::card::Card {
            title: Some("Hi".into()),
            ..crate::card::Card::default()
        };
        a.deliver_card("plat-9", Some("thread-7"), &card, Some("user-1"))
            .await
            .unwrap();
        let calls = a.deliveries();
        assert_eq!(calls[0].platform_id, "plat-9");
        assert_eq!(calls[0].thread_id.as_deref(), Some("thread-7"));
    }

    #[tokio::test]
    async fn default_deliver_breadcrumb_falls_back_to_text_deliver() {
        // The trait-level default converts a Breadcrumb to its text
        // fallback and routes it through `deliver` — mirrors today's
        // legacy `[tool] detail` chat-breadcrumb behaviour for adapters
        // without a native override.
        let a = MockAdapter::new("ch");
        let bc = crate::breadcrumb::Breadcrumb::running("shell").with_detail("cargo check");
        let id = a
            .deliver_breadcrumb("plat-1", None, &bc, None)
            .await
            .unwrap();
        assert!(id.is_some());
        let calls = a.deliveries();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].message.kind, MessageKind::Chat);
        let text = calls[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(text, "[shell] cargo check");
    }

    #[tokio::test]
    async fn default_deliver_diff_falls_back_to_unified_diff_text() {
        // The trait-level default converts a DiffCard to a unified-diff
        // text body via `to_text_fallback` and routes it through
        // `deliver` as `MessageKind::Chat` — so every adapter has a
        // usable diff rendering even before it implements a native
        // `deliver_diff` override.
        let a = MockAdapter::new("ch");
        let card = crate::diff::DiffCard {
            path: "src/main.rs".into(),
            language: Some("rust".into()),
            hunks: vec![crate::diff::DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                lines: vec![
                    crate::diff::DiffLine {
                        kind: crate::diff::DiffLineKind::Remove,
                        text: "fn old() {}".into(),
                    },
                    crate::diff::DiffLine {
                        kind: crate::diff::DiffLineKind::Add,
                        text: "fn new() {}".into(),
                    },
                ],
            }],
            added: 1,
            removed: 1,
            truncated: false,
        };
        let id = a.deliver_diff("plat-1", None, &card).await.unwrap();
        assert!(id.is_some());
        let calls = a.deliveries();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].message.kind, MessageKind::Chat);
        let text = calls[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(text.contains("--- a/src/main.rs"));
        assert!(text.contains("+++ b/src/main.rs"));
        assert!(text.contains("@@ -1,1 +1,1 @@"));
        assert!(text.contains("-fn old() {}"));
        assert!(text.contains("+fn new() {}"));
        assert!(text.contains("(+1 / -1)"));
    }

    #[tokio::test]
    async fn default_deliver_collapsible_falls_back_to_text_deliver() {
        // Adapters without a native disclosure primitive get the
        // trait-level default: the summary + preview + truncation
        // marker get rendered into a single chat row via `deliver`.
        let a = MockAdapter::new("ch");
        let body = (1..=30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview: Vec<String> = (1..=4).map(|i| format!("line {i}")).collect();
        let id = a
            .deliver_collapsible(
                "plat-1",
                None,
                &body,
                "shell produced 30 lines (190 B)",
                &preview,
            )
            .await
            .unwrap();
        assert!(id.is_some());
        let calls = a.deliveries();
        assert_eq!(calls.len(), 1);
        let text = calls[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(text.starts_with("shell produced 30 lines"));
        assert!(text.contains("line 1"));
        assert!(text.contains("line 4"));
        // Truncation marker accounts for the remaining 26 lines.
        assert!(text.contains("…(26 more lines"));
    }

    #[tokio::test]
    async fn default_deliver_collapsible_no_preview_lines() {
        // Empty preview is legal — the runner can decide to not extract
        // one for binary-ish or single-line-but-huge bodies. The body
        // should be just the summary + the truncation marker.
        let a = MockAdapter::new("ch");
        let body = "x".repeat(8000);
        let id = a
            .deliver_collapsible("p", None, &body, "shell 8000 chars", &[])
            .await
            .unwrap();
        assert!(id.is_some());
        let text = a.deliveries()[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();
        assert!(text.starts_with("shell 8000 chars"));
        // Single huge line → total_lines is 1, remaining is 1.
        assert!(text.contains("more lines"));
    }

    #[test]
    fn render_collapsible_text_fallback_shape() {
        let text = "alpha\nbeta\ngamma\ndelta\nepsilon";
        let preview = vec!["alpha".to_owned(), "beta".to_owned()];
        let body = render_collapsible_text_fallback(text, "5 lines (29 B)", &preview);
        // The summary comes first, the preview block follows, the
        // truncation marker closes — with line counts derived from
        // text.lines() vs preview_lines.len().
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines[0], "5 lines (29 B)");
        assert_eq!(lines[1], "alpha");
        assert_eq!(lines[2], "beta");
        assert!(lines[3].starts_with("…(3 more lines"));
    }

    #[test]
    fn render_collapsible_text_fallback_empty_preview() {
        let body = render_collapsible_text_fallback("one\ntwo\nthree", "summary", &[]);
        assert!(body.starts_with("summary\n"));
        assert!(body.contains("…(3 more lines"));
    }

    #[tokio::test]
    async fn default_deliver_breadcrumb_ignores_existing_message_id() {
        // The default impl can't edit in place — it must ignore the
        // `existing_message_id` and just send a fresh text line.
        // (Native adapters override this to drive their edit APIs.)
        let a = MockAdapter::new("ch");
        let bc = crate::breadcrumb::Breadcrumb::running("shell")
            .with_detail("cargo check")
            .finished(true, Some("passed (0.4s)".into()));
        a.deliver_breadcrumb("plat-1", None, &bc, Some("prev-id-9"))
            .await
            .unwrap();
        let calls = a.deliveries();
        assert_eq!(calls.len(), 1);
        let text = calls[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(text.contains("passed (0.4s)"));
    }

    #[tokio::test]
    async fn default_deliver_todo_list_falls_back_to_text_deliver() {
        // The trait-level default converts a TodoList to its text
        // fallback and routes it through `deliver` as Chat — every
        // adapter gets a usable rendering for free.
        let a = MockAdapter::new("ch");
        let list = crate::todo_list::TodoList {
            items: vec![
                crate::todo_list::TodoListItem {
                    id: 1,
                    text: "Wash dishes".into(),
                    status: crate::todo_list::TodoItemStatus::Completed,
                },
                crate::todo_list::TodoListItem {
                    id: 2,
                    text: "Dry dishes".into(),
                    status: crate::todo_list::TodoItemStatus::Pending,
                },
            ],
            title: Some("Kitchen".into()),
        };
        let id = a
            .deliver_todo_list("plat-1", None, &list, None, false)
            .await
            .unwrap();
        assert!(id.is_some());
        let calls = a.deliveries();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].message.kind, MessageKind::Chat);
        let text = calls[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(text.starts_with("Kitchen\n"));
        assert!(text.contains("[x] Wash dishes"));
        assert!(text.contains("[ ] Dry dishes"));
    }

    #[tokio::test]
    async fn default_deliver_todo_list_ignores_existing_id_and_pin_hint() {
        // The default impl can't edit in place and can't pin — it must
        // ignore both signals and just emit a fresh text rendering.
        // (Native adapters override this to drive their edit + pin APIs.)
        let a = MockAdapter::new("ch");
        let list = crate::todo_list::TodoList {
            items: vec![crate::todo_list::TodoListItem {
                id: 1,
                text: "single".into(),
                status: crate::todo_list::TodoItemStatus::Pending,
            }],
            title: None,
        };
        let id = a
            .deliver_todo_list("plat-1", Some("thread-9"), &list, Some("prev-99"), true)
            .await
            .unwrap();
        assert!(id.is_some());
        let calls = a.deliveries();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].platform_id, "plat-1");
        assert_eq!(calls[0].thread_id.as_deref(), Some("thread-9"));
    }

    #[tokio::test]
    async fn default_deliver_error_falls_back_to_text_deliver() {
        // Trait-level default converts an ErrorCard via
        // `to_text_fallback` and routes through `deliver` — every
        // adapter therefore has a usable rendering even before it
        // ships a native red-styled override.
        let a = MockAdapter::new("ch");
        let card = crate::error_card::ErrorCard::new(
            crate::error_card::ErrorCardKind::Internal,
            "the shell tool timed out",
        );
        let id = a.deliver_error("plat-1", None, &card).await.unwrap();
        assert!(id.is_some());
        let calls = a.deliveries();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].message.kind, MessageKind::Chat);
        let text = calls[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(text.starts_with("[ERROR: tool]"));
        assert!(text.contains("the shell tool timed out"));
    }

    #[tokio::test]
    async fn default_deliver_error_propagates_routing_and_retry_footer() {
        // Routing (platform_id, thread_id) must arrive at the adapter
        // verbatim, and the retryable footer must reach the wire for
        // adapters relying on the default impl.
        let a = MockAdapter::new("ch");
        let card = crate::error_card::ErrorCard::new(
            crate::error_card::ErrorCardKind::Delivery,
            "telegram returned 502",
        )
        .retryable();
        a.deliver_error("plat-9", Some("thread-7"), &card)
            .await
            .unwrap();
        let calls = a.deliveries();
        assert_eq!(calls[0].platform_id, "plat-9");
        assert_eq!(calls[0].thread_id.as_deref(), Some("thread-7"));
        let text = calls[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(text.contains("(will retry automatically)"));
    }

    #[tokio::test]
    async fn default_deliver_thinking_falls_back_to_text_deliver() {
        // The trait-level default converts a ThinkingBlock to its text
        // fallback (a `[reasoning]`-headered quoted block) and routes it
        // through `deliver` as Chat — every adapter gets a usable
        // rendering for free even before a native collapsed primitive
        // ships.
        let a = MockAdapter::new("ch");
        let t = crate::thinking::ThinkingBlock::visible("Let me think about this question.");
        let id = a.deliver_thinking("plat-1", None, &t).await.unwrap();
        assert!(id.is_some());
        let calls = a.deliveries();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].message.kind, MessageKind::Chat);
        let text = calls[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(text.starts_with("[reasoning]"));
        assert!(text.contains("> Let me think about this question."));
    }

    #[tokio::test]
    async fn default_deliver_thinking_redacted_emits_placeholder_only() {
        // Redacted blocks MUST NOT put the raw opaque blob on the wire
        // — the fallback substitutes a placeholder. This is the privacy
        // contract every renderer (default + native) must honour.
        let a = MockAdapter::new("ch");
        let t = crate::thinking::ThinkingBlock::redacted("opaque-blob-secret");
        a.deliver_thinking("plat-1", None, &t).await.unwrap();
        let text = a.deliveries()[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();
        assert!(text.contains("(redacted reasoning)"));
        assert!(
            !text.contains("opaque-blob-secret"),
            "raw redacted blob must never leak to the wire"
        );
    }
}
