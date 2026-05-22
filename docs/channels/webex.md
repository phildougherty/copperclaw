# webex channel audit

## Implemented
- deliver: COMPLETE — text + markdown + card + files (multipart, one
  POST per file), plus system actions (edit / delete / reaction).
  `crates/ironclaw-channels/webex/src/adapter.rs:297`
- subscribe: trait-default Ok (webhook ingress; firehose model).
- set_typing: trait-default Ok (no public typing API).
- edit_message: action="edit" → PUT.
- add_reaction: action="reaction" — only works when reaction endpoint
  is present; otherwise Unsupported.
- plain_text_fallback: trait-default None.
- open_dm: synthesises a `person:<user_id>` DmHandle.

## Gaps
LOW:
- Reaction endpoint detection is config-driven (`reactions_endpoint`
  field). When unset, reactions silently return Unsupported. Setup
  should warn.
- Webhook signature uses sha1 hard-coded; Webex has hinted sha256.
- Multipart file post is one HTTP call per file — not batched.

## Edge cases tested
- [x] text to room
- [x] thread_id sets parentId
- [x] markdown-only message
- [x] card attachment
- [x] file via multipart
- [x] card + file
- [x] two files → two posts
- [x] empty content still calls API
- [x] BadRequest / 401 / 429 / 5xx propagation
- [x] person platform_id → DM endpoint
- [x] person + file → Unsupported (Webex DMs don't take files via this
      adapter shape)
- [x] edit / delete / reaction
- [x] reaction without endpoint → Unsupported
- [x] system unknown action → BadRequest
- [x] system to person → Unsupported
- [x] chat / task / webhook / agent message kinds all route to chat path

## Fixes in this PR
None.

## Deferred for follow-up
- Verify sha256 webhook signature when Webex rolls it out.
- Allow file delivery to person targets.
- Batch multipart for multiple files.
