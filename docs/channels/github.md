# github channel audit

## Implemented
- deliver: COMPLETE — POST /repos/:o/:r/issues/:n/comments; edit /
  reaction via system-action shape. `crates/ironclaw-channels/github/src/adapter.rs:125`
- subscribe: trait-default Ok (webhook ingress watches all repos).
- set_typing: noop (no platform concept).
- edit_message: routed via `content.action="edit"` (PATCH comment).
- add_reaction: routed via `content.action="reaction"` (POST reactions
  with reaction slug like `+1`, `heart`, ...).
- plain_text_fallback: trait-default None (Markdown is just rendered).
- open_dm: None (GitHub has no DM concept).

## Gaps
LOW:
- Files explicitly Unsupported — GitHub API does not accept binary
  uploads on issue comments. Correct behavior.
- Trait-level `edit_message` / `add_reaction` not overridden; only the
  action shape works.

## Edge cases tested
- [x] post comment returns id as string
- [x] edit via PATCH
- [x] edit accepts string target id
- [x] edit accepts numeric target id
- [x] reaction action posts slug
- [x] reaction with unknown emoji → BadRequest
- [x] reaction missing emoji → BadRequest
- [x] edit missing target → BadRequest
- [x] edit non-numeric target → BadRequest
- [x] unknown action → BadRequest
- [x] files → Unsupported
- [x] malformed platform_id → BadRequest (multiple variants)
- [x] auth error
- [x] 429 → Rate

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- Override trait-level `edit_message` / `add_reaction` for parity with
  other adapters.
