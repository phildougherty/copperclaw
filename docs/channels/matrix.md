# matrix channel audit

## Implemented
- deliver: COMPLETE — text, html, files (upload + media event), plus
  system-action shape for edit / reaction. `crates/ironclaw-channels/matrix/src/adapter.rs:174`
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
