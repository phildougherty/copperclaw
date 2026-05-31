# linear channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | GraphQL `commentCreate` |
| Auto-split long messages | no | no `max_message_chars()` override; Linear comments accept long markdown |
| Honour `Retry-After` | yes | `AdapterError::Rate { retry_after }` from `api.rs`; delivery loop reads it |
| Typing indicator | no | trait default; Linear has no typing concept |
| Native cards (buttons/sections) | no | falls back via trait-default text render |
| Native breadcrumbs (tool chips) | fallback | trait-default `[tool]` text line |
| Inbound reply_to context | no | router does not set `reply_to`; thread context rides on `thread_id` via `parent_id` |
| Inbound group vs DM distinction | yes (always group) | `is_group: Some(true)` — every comment lives on a workspace issue, no DM concept |
| Edit messages | yes (via action) | `content.action = "edit"` → `commentUpdate`. Trait-level `edit_message` NOT overridden |
| Reactions | yes (via action) | `content.action = "reaction"` → `reactionCreate`. Trait-level `add_reaction` NOT overridden |
| Files / attachments | no | explicit `Unsupported` — Linear's `commentCreate` only accepts attachments-by-URL, which the host doesn't model in v1 (adapter.rs:106) |
| Threading | yes | comment parent_id; `supports_threads() = true` |
| Webhook secret verification | yes | HMAC-SHA256 over body, `Linear-Signature: <hex>` (signature.rs:39) |

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
