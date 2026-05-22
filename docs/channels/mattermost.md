# mattermost channel audit

## Implemented
- deliver: COMPLETE for text + edit + reaction; files Unsupported.
  `crates/ironclaw-channels/mattermost/src/adapter.rs:106`
- subscribe: trait-default Ok (router handles all incoming webhooks).
- set_typing: trait-default Ok.
- edit_message: action="edit" → update_post.
- add_reaction: action="reaction" → add_reaction (requires bot_user_id config).
- plain_text_fallback: trait-default None.
- open_dm: trait-default None.

## Gaps
LOW:
- Reaction fails Unsupported when bot_user_id isn't in the config —
  the user discovers this at first reaction attempt. Setup wizard
  should require it when reactions are intended.

## Edge cases tested
- [x] post succeeds + returns id
- [x] thread_id propagated as root_id
- [x] edit via PATCH
- [x] edit without target → BadRequest
- [x] reaction with bot id
- [x] reaction without bot id → Unsupported
- [x] post without text → BadRequest
- [x] files → uploaded via `/api/v4/files`, attached via `posts.file_ids`
- [x] files on edit/reaction → BadRequest
- [x] unknown action → BadRequest

## Fixes in this PR
- Two-step file upload: `MattermostApi::upload_file` does
  `POST /api/v4/files` multipart, `create_post_with_files` attaches
  the returned ids on the post. The adapter wires both together for
  `action: "post"` outbound messages with `files` set. Edit and
  reaction actions reject files with BadRequest.

## Deferred for follow-up
- Surface a setup warning when bot_user_id is missing.
