# signal channel audit

## Implemented
- deliver: COMPLETE — text + files, plus action-shape edit / reaction /
  delete. `crates/ironclaw-channels/signal/src/adapter.rs:296`
- subscribe: trait-default Ok (signal-cli daemon streams everything).
- set_typing: send_typing(stop=false).
- edit_message: action="edit" — send_edit.
- add_reaction: action="reaction" — send_reaction(remove=false by default).
- plain_text_fallback: trait-default None.
- open_dm: synthesises a `user:<e164>` DmHandle.

## Gaps
LOW:
- Adapter relies on a long-lived signal-cli daemon; if the daemon
  dies it only logs and waits for restart. No automatic respawn.
- `target_author` field is required for reactions (Signal needs the
  message author's identity to address the reaction); the test for
  missing-author exists but the field name is undocumented.

## Edge cases tested
- [x] text to user (recipient form)
- [x] text to group (group_id form)
- [x] files staged + sent
- [x] edit with int timestamp
- [x] edit accepts string target id
- [x] reaction sets default remove=false
- [x] reaction with remove=true
- [x] delete via remote_delete
- [x] unknown action → Unsupported
- [x] edit missing target / text → BadRequest
- [x] reaction missing author → BadRequest
- [x] bad platform_id → BadRequest
- [x] attachment with unsafe filename → BadRequest
- [x] rate / auth propagation
- [x] set_typing
- [x] set_typing bad platform_id → BadRequest

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- Daemon respawn watchdog.
- Document the `target_author` field name on the action shape.
