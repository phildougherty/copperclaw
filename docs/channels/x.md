# x (Twitter/X) channel audit

## Implemented
- deliver: COMPLETE — text + files (one DM per file; first carries the
  text, subsequent use the filename as the required-non-empty body).
  `crates/ironclaw-channels/x/src/adapter.rs:201`
- subscribe: trait-default Ok (poll loop hits /2/dm_events).
- set_typing: trait-default Ok (no public typing API in v2).
- edit_message: any system action → Unsupported.
- add_reaction: any system action → Unsupported.
- plain_text_fallback: trait-default None.
- open_dm: synthesises a `user:<user_id>` DmHandle.

## Gaps
LOW:
- Media upload uses the v1.1 endpoint. v2 media upload is on the
  roadmap but not yet GA. Long-term migration needed.
- One DM per file means N HTTP calls for N attachments; X doesn't
  batch but the polling cost amplifies latency.
- Since-id file is per-adapter; if the file is removed the next poll
  re-emits the most-recent batch.

## Edge cases tested
- [x] user: prefix → /with/:id endpoint
- [x] conversation: prefix → conversation endpoint
- [x] malformed platform_id → BadRequest
- [x] empty user id / conversation id → BadRequest
- [x] no text + no files → BadRequest
- [x] single file upload + send
- [x] multi file: one DM per file, returns last id
- [x] edit action → Unsupported
- [x] reaction action → Unsupported
- [x] set_typing is noop
- [x] subscribe default returns Ok

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- Migrate to v2 media upload when GA.
- Backup since-id to the central DB so the file lost case still
  doesn't double-deliver.
