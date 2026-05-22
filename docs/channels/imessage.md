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
MED:
- Empty body (no text + no files) returns `Ok(None)` silently. Should
  be `BadRequest`. The current behavior is enshrined in
  `deliver_empty_text_is_a_noop_when_no_files` so changing it requires
  also updating that test (which is outside this audit's scope per
  the "don't modify existing tests" rule).
  `crates/ironclaw-channels/imessage/src/adapter.rs:145`

LOW:
- AppleScript escape failures map to BadRequest, which is correct, but
  the escape function rejects control chars rather than encoding them.

## Edge cases tested
- [x] handle: target sends buddy_send_script
- [x] chat: target sends chat_id_script
- [x] quotes / newlines / backslashes escaped
- [x] null / control bytes → BadRequest
- [x] bad platform_id → BadRequest
- [x] system edit / reaction / unknown → Unsupported
- [x] auth / transport bridge errors
- [x] empty text + no files → Ok(None) (NOTED: should be BadRequest)
- [x] file writes to outgoing dir + sends POSIX file
- [x] file with dirty filename → BadRequest
- [x] file with empty basename → BadRequest
- [x] two files invokes bridge twice

## Fixes in this PR
None — the empty-body bug is documented but unfixed (would break the
existing test).

## Deferred for follow-up
- Convert empty-body to BadRequest (requires updating the existing
  test in a follow-up PR; that's the only test that would break).
