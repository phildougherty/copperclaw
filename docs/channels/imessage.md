# imessage channel audit

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
