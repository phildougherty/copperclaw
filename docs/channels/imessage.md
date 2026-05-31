# imessage channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | one AppleScript `send` per outbound text body |
| Auto-split long messages | no | no `max_message_chars()` override; Messages.app has no documented hard cap |
| Honour `Retry-After` | yes | shared via delivery loop (AppleScript bridge surfaces transport errors but rarely emits `Rate`) |
| Typing indicator | no | trait default; AppleScript cannot reach the typing indicator reliably |
| Native cards (buttons/sections) | no | falls back via trait-default text render — Messages.app has no card primitive over AppleScript |
| Native breadcrumbs (tool chips) | fallback | trait-default `[tool]` text line |
| Inbound reply_to context | no | bridge does not surface `associated_message_guid`; out of scope for the slice-2 reply_to batch |
| Inbound group vs DM distinction | yes | poll loop sets `is_group` from the Messages SQLite `chat` row (parse.rs:132) |
| Edit messages | no | `content.action = "edit"` → Unsupported (AppleScript can't reliably edit) |
| Reactions | no | `content.action = "reaction"` → Unsupported (AppleScript tapbacks are fragile) |
| Files / attachments | yes | one AppleScript invocation per file; staged under `<data_dir>` first |
| Threading | no | iMessage has no thread concept; `supports_threads()` defaults to false |
| Webhook secret verification | n/a (local) | inbound rides a polling loop over the Messages SQLite DB; no network surface |

## Implemented
- deliver: COMPLETE for text + files (one AppleScript invocation per
  file). `crates/ironclaw-channels/imessage/src/adapter.rs:105`
- subscribe: trait-default Ok (poll loop over the Messages SQLite DB).
- set_typing: trait-default Ok (AppleScript can't reach the typing
  indicator reliably).
- edit_message: system action → Unsupported with explanatory message.
- add_reaction: system action → Unsupported with explanatory message
  (AppleScript tapbacks are fragile).
- plain_text_fallback: trait-default None.
- open_dm: trait-default None.

## Gaps
LOW:
- AppleScript escape failures map to BadRequest, which is correct, but
  the escape function rejects control chars rather than encoding them.

## Fixed (was MED)
- Empty body (no text + no files) no longer silently drops to
  `Ok(None)`; it returns `BadRequest("imessage deliver: empty body
  (no text, no files)")` so the host records a visible
  `dropped_messages` row instead of marking delivered=ok. Replaced
  test: `deliver_empty_body_is_bad_request_not_silent_drop`.

## Edge cases tested
- [x] handle: target sends buddy_send_script
- [x] chat: target sends chat_id_script
- [x] quotes / newlines / backslashes escaped
- [x] null / control bytes → BadRequest
- [x] bad platform_id → BadRequest
- [x] system edit / reaction / unknown → Unsupported
- [x] auth / transport bridge errors
- [x] empty text + no files → BadRequest (regression-pinned)
- [x] file writes to outgoing dir + sends POSIX file
- [x] file with dirty filename → BadRequest
- [x] file with empty basename → BadRequest
- [x] two files invokes bridge twice

## Fixes in this PR
- Empty body returns `BadRequest` instead of `Ok(None)`. The previous
  test was renamed to `deliver_empty_body_is_bad_request_not_silent_drop`
  and now asserts the failure path.

## Deferred for follow-up
- None.
