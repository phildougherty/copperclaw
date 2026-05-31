# github channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | `POST /repos/:o/:r/issues/:n/comments` |
| Auto-split long messages | no | no `max_message_chars()` override; GitHub Markdown comments have no practical cap |
| Honour `Retry-After` | yes | `AdapterError::Rate { retry_after }` from `api.rs`; delivery loop reads it |
| Typing indicator | no | trait default; GitHub has no typing concept (adapter.rs:106) |
| Native cards (buttons/sections) | no | falls back via trait-default text render (GitHub Markdown only) |
| Native breadcrumbs (tool chips) | fallback | trait-default `[tool]` text line |
| Inbound reply_to context | no | router does not set `reply_to`; PR review threads use comment trees that aren't surfaced as replies in v1 |
| Inbound group vs DM distinction | yes (always group) | `is_group: Some(true)` — every comment lives on a repo, no DM concept |
| Edit messages | yes (via action) | `content.action = "edit"` → `PATCH comment`. Trait-level `edit_message` NOT overridden |
| Reactions | yes (via action) | `content.action = "reaction"` with shortcode → GitHub slug (`+1`, `heart`, ...). Trait-level `add_reaction` NOT overridden |
| Files / attachments | no | explicit `Unsupported` — GitHub API rejects binary uploads on issue comments (adapter.rs:181) |
| Threading | no | `supports_threads() = false` (adapter.rs:102) |
| Webhook secret verification | yes | HMAC-SHA256 over body, `X-Hub-Signature-256: sha256=<hex>` (signature.rs:69) |

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
