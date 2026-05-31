# discord channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | `POST /channels/:id/messages` |
| Auto-split long messages | yes | 2000-char cap declared via `max_message_chars()` (adapter.rs:359) |
| Honour `Retry-After` | yes | `AdapterError::Rate { retry_after }` from `rest.rs`; delivery loop reads it |
| Typing indicator | yes | `POST /channels/:id/typing` (adapter.rs:373) |
| Native cards (buttons/sections) | yes | Embed + `components` (ActionRow of Button). `deliver_card` override at adapter.rs:416 |
| Native breadcrumbs (tool chips) | landing this week (agent G) | Trait default today; planned override uses embed footer + `PATCH` for in-place edits |
| Inbound reply_to context | yes | `message_reference.message_id` → `InboundEvent.reply_to` (events.rs:71); legacy `thread_id` mirror kept |
| Inbound group vs DM distinction | yes | `guild_id.is_some()` (events.rs:56) |
| Edit messages | yes | `PATCH /channels/:cid/messages/:mid` (adapter.rs:395) |
| Reactions | yes | `PUT /channels/:cid/messages/:mid/reactions/:emoji/@me` (adapter.rs:428) |
| Files / attachments | yes | multipart POST |
| Threading | no | Discord models threads as separate channels; `supports_threads() = false` (adapter.rs:349) |
| Webhook secret verification | n/a (gateway) | inbound rides the gateway WebSocket authenticated by bot token; no HTTP webhook signature |

## Implemented
- deliver: COMPLETE — POST /channels/:id/messages, multipart when files
  are present. `crates/ironclaw-channels/discord/src/adapter.rs:337`
- subscribe: trait-default Ok (gateway streams everything).
- set_typing: POST /channels/:id/typing.
- edit_message: PATCH /channels/:cid/messages/:mid.
- add_reaction: PUT /channels/:cid/messages/:mid/reactions/:emoji/@me.
- plain_text_fallback: strips `embeds`.
- open_dm: POST /users/@me/channels resolves to a dm channel id.

## Gaps
LOW:
- thread_id is ignored in deliver (Discord threads use channel id of
  the thread directly; the host should map the thread to a channel id
  before calling deliver). Surface a doc-comment note.
- Reactions accept any emoji string — Discord requires URL-encoding for
  custom emoji `name:id`. The rest layer encodes; not user-facing.

## Edge cases tested
- [x] returns message id
- [x] files multipart
- [x] auth error propagates
- [x] plain_text_fallback strips embeds
- [ ] message > 2000 chars — NOT tested
- [ ] file > 25MB / >10MB depending on guild boost — NOT tested
- [ ] gateway reconnect with resume — covered separately in gateway tests

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- Validate text length (Discord caps at 2000 chars per message) before
  POST.
- Document the thread_id-is-channel-id convention in the adapter
  doc-comment.
