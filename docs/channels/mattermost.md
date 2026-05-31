# mattermost channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | `POST /api/v4/posts` |
| Auto-split long messages | no | no `max_message_chars()` override; Mattermost accepts long posts |
| Honour `Retry-After` | yes | `AdapterError::Rate { retry_after }` from `api.rs`; delivery loop reads it |
| Typing indicator | no | trait default; outgoing webhooks adapter cannot push typing back |
| Native cards (buttons/sections) | no | falls back via trait-default text render. Interactive Message Buttons are a future override |
| Native breadcrumbs (tool chips) | fallback | trait-default `[tool]` text line |
| Inbound reply_to context | no | router does not set `reply_to`; threading rides `thread_id = post_id` (router.rs:150) |
| Inbound group vs DM distinction | no (not in payload) | `is_group: None` — outgoing-webhook payload (token / channel_id / channel_name / user_id / text / trigger_word / file_ids) carries no channel-type field. Deriving `D`/`G`/`O`/`P` requires a follow-up `GET /api/v4/channels/{channel_id}` — tracked by `TODO(channel-ux)` in router.rs around the `InboundEvent` construction site (router.rs:147) |
| Edit messages | yes (via action) | `content.action = "edit"` → `update_post`. Trait-level `edit_message` NOT overridden |
| Reactions | yes (via action) | `content.action = "reaction"` → `add_reaction`. Requires `bot_user_id` in config; absent → Unsupported |
| Files / attachments | yes | two-step: `POST /api/v4/files` upload then `posts.file_ids` |
| Threading | yes | `root_id`; `supports_threads() = true` |
| Webhook secret verification | yes | shared `token` field in body, constant-time compare against `webhook_token` (router.rs:96) |

## Implemented
- deliver: COMPLETE for text + files + edit + reaction.
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
