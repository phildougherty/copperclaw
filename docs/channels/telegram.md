# telegram channel audit

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
