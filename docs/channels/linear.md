# linear channel audit

## Implemented
- deliver: COMPLETE — GraphQL `commentCreate`, plus system-action for
  edit / reaction. `crates/ironclaw-channels/linear/src/adapter.rs:86`
- subscribe: trait-default Ok (webhook ingress).
- set_typing: trait-default Ok (no platform concept).
- edit_message: routed via `content.action="edit"` (commentUpdate).
- add_reaction: routed via `content.action="reaction"` (reactionCreate).
- plain_text_fallback: trait-default None.
- open_dm: None (Linear has no DMs).

## Gaps
LOW:
- Files explicitly Unsupported — Linear's `commentCreate` only accepts
  markdown text + attachments-by-url; the host doesn't yet model
  attachment URLs.
- Trait-level edit/reaction not overridden.

## Edge cases tested
- [x] text creates comment
- [x] thread_id → parent_id
- [x] empty text → BadRequest
- [x] whitespace-only text → BadRequest
- [x] files → Unsupported
- [x] auth error propagates
- [x] edit action
- [x] edit missing target → BadRequest
- [x] edit empty text → BadRequest
- [x] reaction action
- [x] reaction missing target → BadRequest
- [x] reaction empty emoji → BadRequest
- [x] reaction invalid chars → BadRequest
- [x] reaction accepts underscore/plus/minus/digits
- [x] unknown action → Unsupported
- [x] rate / transport / bad-request propagation

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- Support attachments-by-url in commentCreate input.
- Override trait-level edit_message / add_reaction.
