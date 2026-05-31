# matrix channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | `send_text` / `send_html` / `send_threaded` |
| Auto-split long messages | no | `max_message_chars()` returns `None`; Matrix has no documented hard cap |
| Honour `Retry-After` | yes | `AdapterError::Rate { retry_after }` from `api.rs`; delivery loop reads it |
| Typing indicator | yes | `PUT /typing` (adapter.rs:165) |
| Native cards (buttons/sections) | no | Matrix has no card primitive at the protocol level; falls back via trait-default text render |
| Native breadcrumbs (tool chips) | landing this week (agent G) | Trait default today; planned override uses `m.notice` with `<code>` HTML body and `m.replace` for in-place edits |
| Inbound reply_to context | yes | `content."m.relates_to"."m.in_reply_to".event_id` → `InboundEvent.reply_to` (parse.rs:124) |
| Inbound group vs DM distinction | no | `is_group` hardcoded to `Some(true)`; Matrix has no protocol-level DM concept (parse.rs:147) |
| Edit messages | yes (via action) | `content.action = "edit"` → `m.replace`. Trait-level `edit_message` NOT overridden |
| Reactions | yes (via action) | `content.action = "reaction"` → `m.annotation`. Trait-level `add_reaction` NOT overridden |
| Files / attachments | yes | media upload + `m.image` / `m.file` / etc. event |
| Threading | yes | `m.thread`; `supports_threads() = true` |
| Webhook secret verification | n/a (sync) | inbound rides `/sync` long-poll authenticated by access token; no webhook |

## Implemented
- deliver: COMPLETE — text, html, files (upload + media event), plus
  system-action shape for edit / reaction. `crates/copperclaw-channels/matrix/src/adapter.rs:174`
- subscribe: adds the room to the live /sync set; resolves room alias if needed.
- set_typing: PUT /typing endpoint.
- edit_message: routed via `content.action="edit"` in deliver (uses m.replace).
- add_reaction: routed via `content.action="reaction"` (uses m.annotation).
- plain_text_fallback: trait-default None (HTML fallback is handled at
  the send-html call site by passing the plaintext alongside).
- open_dm: returns None (Matrix has no DM concept at the protocol level).

## Gaps
LOW:
- Trait-level `edit_message` / `add_reaction` not overridden; action
  shape is the only path. Adequate but means Team ER's host-side edit
  call would fall through to the trait default `Unsupported` and have
  to fallback to a new message. Recommend overriding.
- Alias resolution cache is in-process and never invalidated.

## Edge cases tested
- [x] text → send_text
- [x] thread_id → send_threaded
- [x] html → send_html
- [x] html without text uses html as plain
- [x] edit action via m.replace
- [x] reaction action via m.annotation
- [x] action with missing target → BadRequest
- [x] unknown action → Unsupported
- [x] files upload + send media
- [x] image MIME → m.image
- [x] no text no files → BadRequest
- [x] subscribe resolves alias

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- Override `edit_message` / `add_reaction` on the trait so the host's
  generic edit-message dispatch works without action wrapping.
- Add a periodic alias-cache invalidation (or watch for m.room.aliases
  events).
