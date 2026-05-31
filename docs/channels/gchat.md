# gchat (Google Chat) channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | `spaces.messages.create` text / threaded text |
| Auto-split long messages | yes | 4096-char cap declared via `max_message_chars()` (adapter.rs:92) |
| Honour `Retry-After` | yes | `AdapterError::Rate { retry_after }` from `api.rs`; delivery loop reads it |
| Typing indicator | no | trait default; Chat REST has no typing API |
| Native cards (buttons/sections) | yes | Cards v2 via `content.card` (adapter.rs:123) — passes through to `send_card`. No trait-level `deliver_card` override needed; the deliver path branches on `card` |
| Native breadcrumbs (tool chips) | yes | Cards v2 `decoratedText` single-section card; in-place edits via `spaces.messages.patch` with `updateMask=cardsV2` (adapter.rs:191) |
| Inbound reply_to context | no | wire payload carries no per-message reply id; `thread` is the only stitching signal and already lands on `thread_id` |
| Inbound group vs DM distinction | yes | `space.spaceType == "ROOM"` (events/router.rs:195) |
| Edit messages | yes (via action) | `content.action = "edit"` → `PUT spaces/.../messages/...?updateMask=text` |
| Reactions | yes (via action) | `content.action = "reaction"` with shortcode → codepoint (≈30 mapped). Unknown shortcode → Unsupported |
| Files / attachments | yes | two-step: `attachments:upload` then include resource name in `attachment[]` |
| Threading | yes | space threads; `supports_threads() = true` |
| Webhook secret verification | yes | `?token=` query param compared via `subtle::ConstantTimeEq` against the configured client token (events/router.rs:109) |

## Implemented
- deliver: COMPLETE for text + threaded text + card + edit / delete /
  reaction. `crates/copperclaw-channels/gchat/src/adapter.rs:91`
- subscribe: trait-default Ok (webhook ingress).
- set_typing: trait-default Ok (no platform concept).
- edit_message: action="edit" → PUT with updateMask.
- add_reaction: action="reaction" with shortcode → codepoint map.
- plain_text_fallback: trait-default None.
- open_dm: returns None (DMs require spaces.create which is out of
  scope in v1).

## Gaps
MED:
- Card content passes through opaquely — malformed cards surface as a
  remote 400 rather than local BadRequest.

LOW:
- Emoji map covers ~30 shortcodes; broader coverage would be useful.
- Threaded replies + attachments fall back to top-level posting with a
  warning, because the Chat `attachment[]` array isn't supported alongside
  `messageReplyOption=REPLY_MESSAGE_OR_FAIL`. Track if the API gains
  that combination.

## Edge cases tested
- [x] text → send_text
- [x] threaded text
- [x] card via send_card with default card_id
- [x] edit action via PUT + updateMask
- [x] edit missing message_name → BadRequest
- [x] edit missing text → BadRequest
- [x] delete action
- [x] delete missing message_name → BadRequest
- [x] reaction with mapped codepoint
- [x] reaction with unknown emoji → Unsupported
- [x] reaction missing message_name / emoji → BadRequest
- [x] unknown action → Unsupported
- [x] files → uploaded via attachments:upload, attached via attachment[]
- [x] files + card → BadRequest
- [x] files + edit/reaction → BadRequest
- [x] bad platform_id → BadRequest
- [x] auth / rate / transport / 404 propagation

## Fixes in this PR
- Two-step attachment flow: `GchatApi::upload_attachment` posts the
  bytes to `POST /upload/v1/spaces/{space}/attachments:upload`
  (multipart media upload), and `send_text_with_attachments`
  includes the returned `resourceName` in the message's
  `attachment[]` array. The adapter wires both for text outbounds
  with `files`. Cards / edits / reactions reject files with
  BadRequest.

## Deferred for follow-up
- Validate card schema at deliver time.
- Expand emoji shortcode map.
