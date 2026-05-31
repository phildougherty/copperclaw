# whatsapp-cloud channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | `messages` endpoint, text or text-with-context (reply) |
| Auto-split long messages | yes | 4096-char cap declared via `max_message_chars()` (adapter.rs:158) |
| Honour `Retry-After` | yes | `AdapterError::Rate { retry_after }` from `api.rs`; delivery loop reads it |
| Typing indicator | partial | no real typing API. `set_typing` is approximated by `mark_read` when `thread_id` (i.e. the inbound message id) is supplied (adapter.rs:162); otherwise no-op |
| Native cards (buttons/sections) | no | trait-default text render. Interactive messages / list templates are a future override |
| Native breadcrumbs (tool chips) | fallback | trait-default `[tool]` text line |
| Inbound reply_to context | yes | `messages[].context.message_id` → `InboundEvent.reply_to` (events/router.rs:282) |
| Inbound group vs DM distinction | no | `is_group: Some(false)` always — WhatsApp Cloud personal/business numbers have no group concept here (events/router.rs:302) |
| Edit messages | no | `content.action = "edit"` → Unsupported (platform limit) |
| Reactions | yes (via action) | `content.action = "reaction"` → `send_reaction` (adapter.rs:250) |
| Files / attachments | yes | upload via `/media` then send by id; reply context propagated |
| Threading | no | `supports_threads() = false` (adapter.rs:150) — WhatsApp uses flat replies addressed by message id, not a separate thread object |
| Webhook secret verification | yes | HMAC-SHA256 over body, `X-Hub-Signature-256: sha256=<hex>`. Constant-time via `subtle::ConstantTimeEq` (signature.rs) |

## Implemented
- deliver: COMPLETE — text + reply (text-with-context) + files (upload
  + send-by-id, plus reply context) + action-reaction. Edit is
  Unsupported (WhatsApp Cloud API does not allow editing sent messages).
  `crates/copperclaw-channels/whatsapp-cloud/src/adapter.rs:174`
- subscribe: trait-default Ok (webhook ingress).
- set_typing: approximates typing by marking the user's last message
  as read when thread_id (containing the message_id) is supplied.
- edit_message: action="edit" → Unsupported.
- add_reaction: action="reaction" → send_reaction.
- plain_text_fallback: trait-default None.
- open_dm: trait-default None.

## Gaps
LOW:
- The `set_typing` mark-read approximation is best-effort and only
  fires when the caller passes a real message id in thread_id. A
  cleaner contract would be a dedicated `mark_read` method.
- Reaction action accepts both `target_message_id` and `message_id`
  field names — the legacy name should be deprecated.
- MIME inference is best-effort by extension; binary uploads without
  an extension default to `application/octet-stream`.

## Edge cases tested
- [x] text uses parsed phone_number_id
- [x] text reply includes context
- [x] default pnid when no prefix
- [x] no prefix and no default → BadRequest
- [x] malformed prefix → BadRequest
- [x] files upload + send
- [x] text + files
- [x] edit → Unsupported
- [x] reaction action
- [x] reaction with legacy message_id field
- [x] reaction without target → error
- [x] unknown action → Unsupported
- [x] set_typing without thread_id → noop
- [x] set_typing with thread_id → mark_read
- [x] set_typing auth propagation
- [x] set_typing malformed platform_id → error
- [x] text auth / rate propagation

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- Deprecate `message_id` in favor of `target_message_id`.
- Consider a dedicated `mark_read` adapter method instead of
  overloading `set_typing`.
