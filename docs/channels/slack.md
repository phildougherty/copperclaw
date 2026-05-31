# slack channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | `chat.postMessage` / `chat.postEphemeral` |
| Auto-split long messages | yes | 40 000-char cap declared via `max_message_chars()` (adapter.rs:111) |
| Honour `Retry-After` | yes | `AdapterError::Rate { retry_after }` from `api.rs`; delivery loop reads it |
| Typing indicator | partial | `assistant.threads.setStatus` — only fires when `thread_id` is set; non-assistants threads are silently no-op (adapter.rs:115) |
| Native cards (buttons/sections) | yes | Block Kit blocks via `build_card_blocks`; text fallback rides on `text` field for notification surfaces. `deliver_card` override at adapter.rs:200 |
| Native breadcrumbs (tool chips) | landing this week (agent G) | Trait default today; planned override uses Block Kit `context` block + `chat.update` for in-place edits |
| Inbound reply_to context | yes | `thread_ts != ts` → `InboundEvent.reply_to` (events/router.rs:416); thread root is NOT treated as a reply |
| Inbound group vs DM distinction | yes | `C*` / `G*` channel id prefix → group; `D*` → DM (events/types.rs:86) |
| Edit messages | yes | `chat.update` (adapter.rs:175) |
| Reactions | yes | `reactions.add` (strips surrounding `:`) (adapter.rs:216) |
| Files / attachments | yes | multi-step `files.completeUploadExternal` |
| Threading | yes | `thread_ts`; `supports_threads() = true` |
| Webhook secret verification | yes | HMAC-SHA256 over `v0:timestamp:body`, constant-time compare in `signature.rs:115` |

## Implemented
- deliver: COMPLETE — chat.postMessage / chat.postEphemeral, then
  files.completeUploadExternal for attachments. `crates/ironclaw-channels/slack/src/adapter.rs:130`
- subscribe: trait-default Ok (Events API delivers everything for the
  bot's joined channels).
- set_typing: assistant.threads.setStatus when thread_id present;
  swallows BadRequest so non-assistants threads stay quiet.
- edit_message: chat.update.
- add_reaction: reactions.add (strips surrounding `:`).
- plain_text_fallback: strips `blocks`, keeps `text`.
- open_dm: returns None (Slack accepts user ids directly).

## Gaps
LOW:
- `chat.update` discards thread_id; correct for Slack but means an
  edit on a non-existent ts surfaces as a generic BadRequest.

## Edge cases tested
- [x] empty body — falls through to API which would reject; we
      pass `text` even if empty so postMessage returns ok.
- [x] thread_ts propagated
- [x] ephemeral_to → chat.postEphemeral
- [x] invalid_auth → AdapterError::Auth
- [x] 429 with Retry-After
- [x] ok:false body with `ratelimited` error
- [x] generic ok:false → BadRequest
- [x] file upload multipart
- [x] non-200 non-429 → Transport
- [x] plain_text_fallback strips blocks
- [ ] file > 1GB (Slack hard limit) — NOT tested

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- Surface a per-file upload error rather than aborting the whole
  deliver on the first failure.
