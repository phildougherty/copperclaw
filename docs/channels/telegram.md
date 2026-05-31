# telegram channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | `sendMessage` with MarkdownV2 by default |
| Auto-split long messages | yes | 4096-char cap declared via `max_message_chars()` (adapter.rs:174) |
| Honour `Retry-After` | yes | `AdapterError::Rate { retry_after }` from `api.rs`; delivery loop reads it |
| Typing indicator | yes | `sendChatAction(typing)` (adapter.rs:191) |
| Native cards (buttons/sections) | yes | MarkdownV2 body + `reply_markup.inline_keyboard`; image variant via `sendPhoto`. `deliver_card` override at adapter.rs:277 |
| Native breadcrumbs (tool chips) | yes | `deliver_breadcrumb` override at adapter.rs:398 — HTML `<code>` chip; in-place edit via `editMessageText` when `existing_message_id` present |
| Inbound reply_to context | yes | `reply_to_message.message_id` → `InboundEvent.reply_to` (ingress/mod.rs:245) |
| Inbound group vs DM distinction | yes | `chat.kind` "group" / "supergroup" vs "private" (ingress/mod.rs:178) |
| Edit messages | yes | `editMessageText` (adapter.rs:257) |
| Reactions | yes | `setMessageReaction` — single emoji (adapter.rs:380) |
| Files / attachments | yes | `sendDocument`, caption rides on first file |
| Threading | yes | forum topics; `supports_threads() = true`, `message_thread_id` propagated |
| Webhook secret verification | yes | constant-time compare via `subtle::ConstantTimeEq` on `X-Telegram-Bot-Api-Secret-Token` (ingress/webhook.rs:67) |

## Implemented
- deliver: COMPLETE — text via sendMessage, files via sendDocument
  (first file picks up the caption). `crates/ironclaw-channels/telegram/src/adapter.rs:190`
- subscribe: validates bot token via getMe.
- set_typing: sendChatAction(typing).
- edit_message: editMessageText.
- add_reaction: setMessageReaction.
- plain_text_fallback: strips `parse_mode` and prepends marker.
- open_dm: synthesises a DmHandle (Telegram DMs are addressed by user id).

## Gaps
None of HIGH severity.

LOW:
- The `parse_platform_id` path implicitly accepts any string; malformed
  ids surface as a Telegram-side 400 rather than a local BadRequest.
- `set_message_reaction` only sets a single emoji; agents that want to
  remove a reaction must pass an empty emoji (undocumented).

## Edge cases tested
- [x] empty body without files (`deliver_with_no_text_and_no_files_errors`)
- [x] files with caption-on-first
- [x] explicit parse_mode override
- [x] no parse_mode field at all
- [x] set_typing
- [x] plain_text_fallback strip
- [ ] message-too-long (Telegram limits to 4096 chars) — NOT tested
- [ ] rate-limit retry — NOT tested

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- Add a `BadRequest` arm for messages over 4096 chars so the host can
  split before sending.
- Surface `setMessageReaction` remove path as a dedicated `remove` flag
  in the content payload (currently passes empty string).
