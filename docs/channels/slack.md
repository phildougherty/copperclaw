# slack channel audit

## Implemented
- deliver: COMPLETE — chat.postMessage / chat.postEphemeral, then
  files.completeUploadExternal for attachments. `crates/ironclaw-channels/slack/src/adapter.rs:130`
- subscribe: trait-default Ok (Events API delivers everything for the
  bot's joined channels).
- set_typing: assistant.threads.setStatus when thread_id present;
  swallows BadRequest so non-assistants threads stay quiet.
- edit_message: chat.update.
- add_reaction: reactions.add (strips surrounding `:`).
- plain_text_fallback: strips `blocks`, keeps `text`.
- open_dm: returns None (Slack accepts user ids directly).

## Gaps
LOW:
- `chat.update` discards thread_id; correct for Slack but means an
  edit on a non-existent ts surfaces as a generic BadRequest.

## Edge cases tested
- [x] empty body — falls through to API which would reject; we
      pass `text` even if empty so postMessage returns ok.
- [x] thread_ts propagated
- [x] ephemeral_to → chat.postEphemeral
- [x] invalid_auth → AdapterError::Auth
- [x] 429 with Retry-After
- [x] ok:false body with `ratelimited` error
- [x] generic ok:false → BadRequest
- [x] file upload multipart
- [x] non-200 non-429 → Transport
- [x] plain_text_fallback strips blocks
- [ ] file > 1GB (Slack hard limit) — NOT tested

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- Surface a per-file upload error rather than aborting the whole
  deliver on the first failure.
