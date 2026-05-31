# Slice 3 — Native UI surfaces

Status: design, awaiting slice 2 land. Targets five concurrent implementer
agents (one per surface) once `MessageKind::Breadcrumb`, `deliver_breadcrumb`,
and the slice-2 native `deliver_card` overrides are on `main`.

## Goal

Extend slice 2's pattern — *canonical schema in `ironclaw-channels-core`,
dedicated `MessageKind`, trait method with text-fallback default impl,
`dispatch_X` in delivery service, per-channel native renderers, MCP tool
the runner routes into the right effect* — to five more UX surfaces:

| # | Surface         | One-liner                                                        |
|---|-----------------|------------------------------------------------------------------|
| 1 | Diff            | File edits emit a real diff card (gutters, before/after counts). |
| 2 | Todo list       | `todo_*` emits a checkbox list that edits in place; pinned.      |
| 3 | Error           | Agent errors emit a visually-distinct card (red where supported).|
| 4 | Long-output     | Decorator on `Chat`: summary + collapsible expander.             |
| 5 | Thinking        | Opt-in structured reasoning block, collapsed by default.         |

None of these redesign anything from slice 2; each lifts the Breadcrumb
pattern wholesale. The canonical templates to crib from are
`crates/ironclaw-channels/core/src/breadcrumb.rs` (schema +
`to_text_fallback` shape), `adapter.rs::deliver_breadcrumb` default impl
(adapter trait shape), `service.rs::dispatch_breadcrumb` ~L1063 (host
dispatch shape), `tools.rs::apply_send_card` L956 (runner apply-fn
shape), and `skills/send-card/SKILL.md` (skill prose shape).

## Shared conventions

- Schemas: tight caps in chars not bytes; `fn validate(&self) -> Result<(),
  <Name>Error>` + `to_text_fallback(&self) -> String`.
- New `MessageKind` variants: Diff, TodoList, Error, Thinking (surface 4 reuses Chat).
- New `ChannelAdapter::deliver_<X>` methods get the same default-impl
  shape as `deliver_breadcrumb`: convert via `to_text_fallback`, wrap in
  `OutboundMessage { kind: Chat, content: {"text": …}, files: vec![] }`,
  call `self.deliver(...)`. Native renderers land per-channel later.
- Dispatch arms in `service.rs::process_row` mirror `dispatch_card`:
  resolve target, deserialize from `row.content.<key>`, call adapter,
  fall back to text-deliver on `AdapterError::Unsupported`,
  `delivered::insert(...)`.
- Runner: one `OutboundToolEffect` variant per surface in
  `context.rs`, one `apply_<name>` in `tools.rs` that mirrors
  `apply_send_card`.
- Skills register via the SKILLS_DIR symlink — no install rebuild.

---

## Surface 1 — Diff

### Schema (`core/src/diff.rs`)

```rust
pub struct DiffCard {
    pub path: String,                  // ≤ MAX_PATH_CHARS (256)
    pub language: Option<String>,      // hint for syntax-highlight (rust, ts, …)
    pub hunks: Vec<DiffHunk>,          // ≤ MAX_HUNKS (8)
    pub added: u32,
    pub removed: u32,
    pub truncated: bool,               // true if hunks were dropped to fit caps
}
pub struct DiffHunk {
    pub old_start: u32, pub old_len: u32,
    pub new_start: u32, pub new_len: u32,
    pub lines: Vec<DiffLine>,          // ≤ MAX_LINES_PER_HUNK (60)
}
pub enum DiffLineKind { Context, Add, Remove }
pub struct DiffLine { pub kind: DiffLineKind, pub text: String }  // text ≤ 500 chars
```

Caps: payload ≤ ~25 KB JSON; Discord's 6 KB embed cap is enforced
per-channel and the renderer trims. Lines > 500 chars get truncated with
a `…` marker.

### Format on the wire — *structured, computed in the runner*

Ship `DiffHunk[]` (structured), not unified-diff text. The runner computes
the diff. Rationale: (a) the model produces *new content*; making it emit
a correct unified diff with line numbers is failure-prone; (b) Slack
Block Kit, Discord embeds, and Telegram MarkdownV2 want different
renderings — re-parsing unified-diff text in every renderer is wasted
work; (c) `to_text_fallback()` reconstitutes unified-diff trivially from
structured input.

Runner computes diff inside `apply_edit_file` / `apply_multi_edit` /
`apply_apply_patch` *after* the file write succeeds, using the `similar`
crate (workspace add if not present; pure-Rust MIT). Diff is derived
from the pre-edit snapshot held in the tool handler vs the post-edit
content. The `DiffCard` row is emitted *alongside* (not instead of) the
breadcrumb — breadcrumb = "what tool", DiffCard = "what changed".

### Trait method

```rust
async fn deliver_diff(
    &self,
    platform_id: &str,
    thread_id: Option<&str>,
    diff: &DiffCard,
) -> Result<Option<String>, AdapterError> { default → text fallback via deliver }
```

No `existing_message_id`; diffs are immutable post-emit.

### Text fallback

Standard unified diff: `--- a/<path>` / `+++ b/<path>` header,
`@@ -old,+new @@` hunk markers, `+`/`-`/` ` line prefixes, footer
`+<added> / -<removed>`. Bare text — renderers wrap in code fences where
useful.

### Dispatch path

`MessageKind::Diff` arm → `dispatch_diff`. Diff vs `dispatch_card` (L953):
deserialize from `row.content.diff`, no `to` hint, no typing indicator
(diffs follow breadcrumbs which already signalled).

### Per-channel native renderers

- **Telegram**: `sendMessage` MarkdownV2, body wrapped in ` ```diff … ``` `
  fenced block — mobile client colorises `diff` syntax.
- **Slack**: Block Kit — `section` mrkdwn header `*<path>* (+N / -M)`,
  then one `rich_text_preformatted` block per hunk (honors `+`/`-` gutters
  visually, dodges the 3000-char truncation surprise).
- **Discord**: one embed per `DiffCard`, `description` is the ` ```diff ``` `
  fenced block. `color = 0x57F287` (added > removed), `0xED4245`
  (removed > added), `0xFEE75C` (balanced). Hunks beyond the 4096-char
  description cap spill into `fields` (cap 25).
- **Google Chat**: Cards v2 `decoratedText` widget per hunk, `textParagraph`
  for the path header. Multi-hunk diffs span multiple `cardsV2` entries.
- **Matrix**: `m.notice` with `formatted_body = <pre><code
  class="language-diff">…</code></pre>`. Element highlights natively.
- **Teams**: Adaptive Card `TextBlock` `fontType=Monospace` per hunk;
  Teams markdown ignores ` ```diff ``` `, so colorise via `color: Good` /
  `color: Attention` on the `+` / `-` runs.

### MCP tool surface

No new tool. `edit_file`, `multi_edit`, `apply_patch`, and `write_file`
(when overwriting) gain runner-side diff emission *after the write
succeeds*. The model is unchanged — having the model emit a diff is
redundant (it just produced the new content) and failure-prone.

### Skill prose

Update `skills/edit-file/SKILL.md` (and the related write/patch skills)
with a single new sentence: "The host renders a diff card to the user
automatically — you don't need to summarise the change in prose."

### Disjoint scope per future agent

- `channels/core/src/diff.rs` (new) + `lib.rs` re-export
- `types/src/message.rs` — `MessageKind::Diff`
- `channels/core/src/adapter.rs` — `deliver_diff`
- `host-delivery/src/service.rs` — `dispatch_diff` arm + method
- `runner/src/tools.rs` — diff compute in `apply_edit_file` /
  `apply_multi_edit` / `apply_apply_patch` / `apply_write_file`;
  `OutboundToolEffect::EmitDiff` in `mcp/src/context.rs`
- Native renderers: one `adapter.rs` per priority channel
- `skills/edit-file/SKILL.md` — one-sentence amendment

---

## Surface 2 — Todo list

### Schema (`core/src/todo.rs`)

```rust
pub struct TodoList {
    pub items: Vec<TodoListItem>,   // ≤ MAX_ITEMS (50)
    pub title: Option<String>,      // ≤ 64 chars; default "Plan"
}
pub struct TodoListItem {
    pub id: u32,
    pub text: String,               // ≤ 200 chars
    pub status: TodoItemStatus,
}
pub enum TodoItemStatus { Pending, InProgress, Completed }
```

Validation: monotonic non-duplicate ids, non-empty text, list non-empty.
`to_text_fallback` emits one line per item: `- [ ]` / `- [~]` / `- [x]`
prefix, then text. Pending count footer.

### Trait method

```rust
async fn deliver_todo_list(
    &self,
    platform_id: &str,
    thread_id: Option<&str>,
    list: &TodoList,
    existing_message_id: Option<&str>,
    pin_hint: bool,
) -> Result<Option<String>, AdapterError>
```

`existing_message_id` is the crux: TodoList must edit-in-place, not spam
fresh messages on every `todo_update`. Runner emits a fresh row per
mutation; delivery service looks up the most recent TodoList row in the
session (factor `lookup_prior_breadcrumb_external_id` at service.rs:1518
into a generic `lookup_prior_kind_external_id(kind, session)`) and threads
the platform message id through. `pin_hint = true` on first emit asks the
adapter to pin (and unpin any previous TodoList pin); ignored where
unsupported.

### Dispatch path

`MessageKind::TodoList` arm → `dispatch_todo_list`. Diff vs `dispatch_card`:
runs the prior-id lookup, passes resolved `existing_message_id` and a
`pin_hint` derived from whether this is the first emit. Unsupported
fallback identical.

### Per-channel native renderers

- **Telegram**: `sendMessage` (first emit) → `editMessageText` MarkdownV2,
  `[x]` / `[~]` / `[ ]` glyphs per item. `pinChatMessage` on first emit;
  `unpinChatMessage` when every item is `Completed`.
- **Slack**: Block Kit `section` per item with `accessory = checkbox`.
  `chat.update` to mutate, `pins.add` on first emit.
- **Discord**: embed with one `field` per item, name prefixed by status
  glyph. `PATCH /channels/.../messages/...` to edit. Pin best-effort
  (swallow 403 — bots often lack pin permission).
- **Google Chat**: Cards v2 `decoratedText` per item, `startIcon` bound to
  status. `spaces.messages.patch` to edit. No public pin API — emit the
  card sticky-styled and accept the limitation.
- **Matrix**: `m.text` + `m.replace` relations on update. No native pin;
  emit `m.room.pinned_events` state event when bot has permission.
- **Teams**: Adaptive Card `CheckboxColumn` per item; replace via
  `PUT /messages/{id}`. No bot-pinnable API.

### MCP tool surface

No new model-facing tool. Existing `todo_add` / `todo_update` /
`todo_delete` handlers (`mcp/src/tools/todo.rs`) emit a
`MessageKind::TodoList` row carrying the *full* post-mutation list at
the end of each call. Full-list-on-each-mutation beats a delta protocol
— list is capped at 50 items, wire cost is trivial.

### Skill prose

Update `skills/todo-tracker/SKILL.md`: "Your todos are *also* rendered to
the user as a live, pinned checklist on channels that support it — pick
item text the user will appreciate seeing (imperative, specific). Avoid
verbose internal-jargon items."

### Disjoint scope per future agent

- `channels/core/src/todo.rs` (new) + `lib.rs` re-export
- `types/src/message.rs` — `MessageKind::TodoList`
- `channels/core/src/adapter.rs` — `deliver_todo_list`
- `host-delivery/src/service.rs` — `dispatch_todo_list` + extract
  `lookup_prior_kind_external_id` helper
- `mcp/src/tools/todo.rs` — emit at end of each mutating handler;
  `OutboundToolEffect::EmitTodoList` in `mcp/src/context.rs`;
  `apply_emit_todo_list` in `runner/src/tools.rs`
- Native renderers per channel
- `skills/todo-tracker/SKILL.md` — amend

---

## Surface 3 — Error

### Schema (`core/src/error_card.rs`)

```rust
pub struct ErrorCard {
    pub title: String,              // ≤ 120 chars, default "Something went wrong"
    pub summary: String,            // ≤ 500 chars, plain language for the user
    pub kind: ErrorCardKind,        // visual severity
    pub details: Option<String>,    // ≤ 2000 chars, monospace; tool stderr / trace
    pub retryable: bool,            // hint for "will retry" vs "needs you"
}
pub enum ErrorCardKind { Error, Warning, RateLimit, Timeout }
```

Validation: title + summary non-empty; details optional.

### Trait method

```rust
async fn deliver_error(
    &self,
    platform_id: &str,
    thread_id: Option<&str>,
    err: &ErrorCard,
) -> Result<Option<String>, AdapterError>
```

No edit-in-place: error messages are immutable receipts.

### Text fallback

```
[ERROR] <title>
<summary>
---
<details>
(will retry automatically)   <-- when retryable
```

Severity prefix (`[ERROR]` / `[WARN]` / `[RATE LIMIT]` / `[TIMEOUT]`) is
the plain-text signal for channels without color.

### Dispatch path

`MessageKind::Error` arm → `dispatch_error`. Diff vs `dispatch_card`: no
typing indicator, no `to` hint, no editability.

### Per-channel native renderers

- **Telegram**: `sendMessage` HTML, body wrapped `<b>Error</b>`, `<i>summary</i>`,
  `<code>` for details. No color — icon + bold carries it.
- **Slack**: `attachments[].color` per severity (`#E01E5A` error, `#ECB22E`
  warning, `#1264A3` rate-limit/timeout); `section` blocks for title/summary,
  `rich_text_preformatted` for details.
- **Discord**: embed `color = 0xED4245` error, `0xFEE75C` warning,
  `0x5865F2` rate-limit/timeout. Title, description, optional `fields[0]`
  for details (truncated to embed cap).
- **Google Chat**: Cards v2 `header.imageUrl` set to a hosted severity
  glyph. No color theming — rely on icon + bold copy.
- **Matrix**: `m.notice` (Element renders muted by default — fits warnings);
  for `ErrorCardKind::Error` use `m.text` with `<font color="#cc3333">` in
  `formatted_body` so it stands out.
- **Teams**: Adaptive Card `style: "attention"` (Error), `"warning"`,
  `"emphasis"` (RateLimit/Timeout) on the container.

### MCP tool surface

No new model-facing tool. The runner / host emits `MessageKind::Error`
rows when (a) a tool handler returns `ToolError::Internal`, (b) provider
returns terminal failure (the failure-reason path plumbed by commit
0105626), (c) slice-1 `RetryAfter` backoff exhausts retries. Errors
are emitted *by the host* on behalf of the agent — same model as the
existing `emit_terminal_failure_apologies` path in `tools.rs`. Already-
wired failure modes become user-visible UI without a model code change.

### Skill prose

No agent-facing skill needed — surface is host-side automation.

### Disjoint scope per future agent

- `channels/core/src/error_card.rs` (new) + `lib.rs` re-export
- `types/src/message.rs` — `MessageKind::Error`
- `channels/core/src/adapter.rs` — `deliver_error`
- `host-delivery/src/service.rs` — `dispatch_error` arm
- `host-delivery/src/loops.rs` + the terminal-failure path —
  switch from plain chat row to `MessageKind::Error`
- Native renderers per channel

---

## Surface 4 — Long-output expander

### Decorator on existing kind — *not* a new MessageKind

This is a **decorator on `MessageKind::Chat`**, not its own kind.
"Summary + collapsible" is a visual treatment, not a semantic category;
the agent thinks "long shell output", not "collapsible". Reusing `Chat`
keeps every existing inbound/outbound path, splitter, replay fixture, and
audit query unchanged, and lets tool outputs (`shell` stdout, `grep` hits,
`web_fetch` body) get uniform treatment without each apply-fn rewriting.

Optional `expander` field on `OutboundMessage.content` when `kind == Chat`:

```json
{
  "text": "<full output>",
  "expander": {
    "summary": "shell command produced 312 lines (12 KB)",
    "preview_lines": ["…first 6 lines…"]
  }
}
```

`dispatch_chat` (service.rs:872) checks for `content.expander`. Present →
call new trait method `deliver_collapsible(&self, …, text, summary,
preview)` instead of `deliver`. Default impl ignores `expander` and falls
through to today's chat splitter behaviour.

Runner attaches `expander` in `tools/shell.rs` / `tools/grep.rs` /
`tools/web_fetch.rs` etc. when output exceeds a threshold (default 30
lines or 4 KB). Summary is generated host-side from byte/line count +
tool name; no model involvement.

### Trait method

```rust
async fn deliver_collapsible(
    &self,
    platform_id: &str,
    thread_id: Option<&str>,
    text: &str,
    summary: &str,
    preview_lines: &[String],
) -> Result<Option<String>, AdapterError>
```

Default: `deliver` with `summary + "\n" + preview + "\n…(N more lines)"`
truncated to `max_message_chars()`.

### Per-channel native renderers

- **Telegram**: HTML `<blockquote expandable>` (Bot API 7.6+) wrapping
  full text; `summary` outside as first line. Clients without `expandable`
  see the blockquote fully rendered — graceful.
- **Slack**: `section` mrkdwn with `summary`, `actions` block with
  "Show full output" button (`action_id = expand:<row_id>`). Button click
  routes through the slice-2 card-callback inbound path; runner re-emits
  full text as a follow-up. (Thread-reply alternative rejected: confusing
  in DMs without threads.)
- **Discord**: embed `description = summary + ``` + preview + ```` plus a
  "Show full" button component (existing component-interaction path).
- **Google Chat**: Cards v2 `collapsibleSection` (native primitive) —
  `summary` as header, full text in body.
- **Matrix**: `m.text` with `<details><summary>…</summary>…</details>` in
  `formatted_body`. Element renders the disclosure widget natively.
- **Teams**: Adaptive Card `Container` with `isVisible: false` on the
  full-text block + `Action.ToggleVisibility` button.

### MCP tool surface

No new model-facing tool. Long outputs are decorated automatically when
tool handlers emit them; model continues to call `shell` / `grep` etc.
unchanged.

### Skill prose

No agent-facing skill. The decoration is invisible to the model.

### Disjoint scope per future agent

- `channels/core/src/adapter.rs` — `deliver_collapsible` default impl
- `host-delivery/src/service.rs::dispatch_chat` — branch on `content.expander`
- `runner/src/tools.rs` — `attach_expander_if_long(text, tool_name)`
  helper invoked from each long-output apply-fn
- Native renderers per channel

---

## Surface 5 — Thinking block

### Schema (`core/src/thinking.rs`)

```rust
pub struct ThinkingBlock {
    pub text: String,                  // ≤ MAX_THINKING_CHARS (8000)
    pub redacted: bool,                // true for redacted_thinking blocks
    pub model: Option<String>,         // optional provenance ("claude-opus-4-7", "kimi-k2.6")
}
```

Validation: text non-empty unless `redacted == true`. `to_text_fallback`:
` <details>like</details> ` block in markdown; on plaintext channels, a
`> ` quoted block prefixed with `[reasoning]`.

### Opt-in mechanism

`apply_send_message` / `apply_send_file` continue to strip inline
`<thinking>…</thinking>` via `strip_reasoning_blocks` (`tools.rs:766`) —
unchanged default. Inline markup in *chat output* is prose contamination,
not structured reasoning; the strip is orthogonal to this surface.

New container-config knob `surface_thinking: bool` (default `false`)
alongside `breadcrumbs_enabled`. When `true`: Anthropic provider's
`ThinkingAccumulator` (`anthropic.rs:334`) additionally emits
`ProviderEvent::Thinking { text, redacted, signature }` (other providers
analogously). Runner observes in `provider_call.rs`, emits via new
`ToolContext::emit_thinking` mirroring `emit_breadcrumb` (best-effort,
direct `insert_outbound_row`, swallowed errors). Per-group override via
`iclaw groups config edit <id>`.

### Trait method

```rust
async fn deliver_thinking(
    &self,
    platform_id: &str,
    thread_id: Option<&str>,
    thinking: &ThinkingBlock,
) -> Result<Option<String>, AdapterError>
```

### Per-channel native renderers

- **Telegram**: HTML `<blockquote expandable>` with an `<i>thinking</i>`
  prefix. Same primitive as surface 4.
- **Slack**: `context` block (small muted text) with a "reasoning" label.
  Truncate to 3000 chars per element; spill across multiple blocks.
- **Discord**: embed `color = 0x99AAB5` (muted grey), `author.name =
  "reasoning"`, description = thinking text.
- **Google Chat**: Cards v2 `collapsibleSection` titled "reasoning",
  `collapsed: true` by default. Same primitive as surface 4.
- **Matrix**: `m.notice` (muted by spec) with `<details>` in `formatted_body`.
- **Teams**: Adaptive Card `Container` hidden behind an
  `Action.ToggleVisibility` "Show reasoning" button.

### MCP tool surface

No model-facing tool. The model already produces thinking via the
provider; this surface just unhides what we currently drop. Crucially,
the model is not asked to call `<emit_thinking>` — that path is
provider-driven and lives entirely in `provider_call.rs`.

### Skill prose

No skill needed — surface is invisible to the model.

### Disjoint scope per future agent

- `channels/core/src/thinking.rs` (new) + `lib.rs` re-export
- `types/src/message.rs` — `MessageKind::Thinking`
- `channels/core/src/adapter.rs` — `deliver_thinking`
- `host-delivery/src/service.rs` — `dispatch_thinking`
- `providers/src/anthropic.rs` (+ `subprocess.rs`) — emit `ProviderEvent::Thinking`
- `runner/src/run/provider_call.rs` — observe event, gate on
  `surface_thinking`, call new `ToolContext::emit_thinking`
- `mcp/src/context.rs` — `surface_thinking` flag wiring +
  `emit_thinking` method (mirrors `emit_breadcrumb` L237)
- Container-config struct — `surface_thinking: bool` field, default `false`
- Native renderers per channel

---

## Cross-cutting questions needing operator input

1. **Diff on `write_file` overwrites.** Today `write_file` doesn't read
   prior content. Diff would add an `O(file size)` pre-read per overwrite
   — free for typical source files (< 50 KB), wasteful for blobs > 1 MB.
   Proposed cutoff: skip diff when target > 256 KB, emit
   `BlobReplaced(path, before_bytes, after_bytes)` instead. Confirm cutoff
   or push for "always diff" / "never diff for write_file".

2. **Long-output thresholds.** Default ≥ 30 lines OR ≥ 4 KB. Per-tool
   overrides? (`web_fetch` is often legitimately huge; `shell echo` never
   needs collapsing.) Simplest is one global threshold; alternative is a
   small table in `expander.rs`.

3. **Thinking default.** Defaulted off because surfacing model reasoning
   has privacy implications. Confirm "off by default, on per-group" vs
   "on for new groups, off for existing".

4. **In-place edit retention horizon.** TodoList edit-in-place reuses
   the `lookup_prior_*` SQL. Telegram refuses `editMessageText` after
   48 h. Proposed: filter to rows ≤ 24 h; older mutations fall back to
   fresh emission. Confirm or pick a ceiling.

5. **Per-surface metrics naming.** Each new dispatch path should add
   `inc_delivery_<kind>_native` / `_fallback` / `_unsupported`
   (mirroring `inc_delivery_chat_split` from slice 2). Confirm naming
   + per-channel-type label cardinality acceptable.
