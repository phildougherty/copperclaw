# x (Twitter/X) channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | DM event create via v2 endpoint (user-with or conversation form) |
| Auto-split long messages | no | no `max_message_chars()` override; X DM caps at ~10 000 chars, splitter not yet wired |
| Honour `Retry-After` | yes | `AdapterError::Rate { retry_after }` from `api.rs` (prefers `x-rate-limit-reset` then `Retry-After`); delivery loop reads it |
| Typing indicator | no | trait default — X v2 has no public DM typing API (adapter.rs:207) |
| Native cards (buttons/sections) | no | falls back via trait-default text render |
| Native breadcrumbs (tool chips) | fallback | trait-default `[tool]` text line |
| Inbound reply_to context | no | parser does not set `reply_to`; DM events are flat |
| Inbound group vs DM distinction | no | `is_group: None` (parse.rs:89) — DM-only adapter |
| Edit messages | no | any system action → Unsupported |
| Reactions | no | any system action → Unsupported |
| Files / attachments | yes | one DM per file; first carries the text, subsequent use filename as required-non-empty body |
| Threading | no | `supports_threads() = false` (adapter.rs:203) |
| Webhook secret verification | n/a (poll) | inbound rides a since-id poll loop over `/2/dm_events`; no webhook surface |

## Implemented
- deliver: COMPLETE — text + files (one DM per file; first carries the
  text, subsequent use the filename as the required-non-empty body).
  `crates/ironclaw-channels/x/src/adapter.rs:216`
- subscribe: trait-default Ok (poll loop hits /2/dm_events).
- set_typing: trait-default Ok (no public typing API in v2).
- edit_message: any system action → Unsupported.
- add_reaction: any system action → Unsupported.
- plain_text_fallback: trait-default None.
- open_dm: synthesises a `user:<user_id>` DmHandle.

## Gaps
LOW:
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
- v2 media upload path: `XApi::upload_media_v2` posts to
  `{api_base}/2/media/upload` as multipart and returns the media id
  from `data.id` (with a tolerant fallback for early v2 responses
  that surfaced `media_id_string` at the top level). Selected via
  the new `media_api_version: "v2"` config field (default `"v1"`
  keeps legacy behaviour). The adapter's `upload_files` dispatches
  on the configured version.

## Deferred for follow-up
- Backup since-id to the central DB so the file lost case still
  doesn't double-deliver.
