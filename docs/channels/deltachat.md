# deltachat channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | `send_msg` JSON-RPC into the Delta Chat account |
| Auto-split long messages | no | no `max_message_chars()` override; email transport has no practical cap |
| Honour `Retry-After` | yes | `AdapterError::Rate { retry_after }` from `rpc.rs`; delivery loop reads it |
| Typing indicator | no | trait default no-op — Delta Chat has no typing API (adapter.rs:411 comments this is intentional) |
| Native cards (buttons/sections) | no | falls back via trait-default text render |
| Native breadcrumbs (tool chips) | fallback | trait-default `[tool]` text line |
| Inbound reply_to context | no | parser does not set `reply_to` in the inbound shape |
| Inbound group vs DM distinction | yes | `chat.is_group()` (parse.rs:108) |
| Edit messages | no | trait-default Unsupported — Delta Chat protocol does not allow editing already-sent messages |
| Reactions | yes (via action) | `content.action = "reaction"` → `send_reaction`. Trait-level `add_reaction` NOT overridden |
| Files / attachments | yes | staged under `<data_dir>/outgoing/` then path-attached on `send_msg` |
| Threading | no | `supports_threads() = false` (adapter.rs:392) |
| Webhook secret verification | n/a (RPC) | inbound rides the Delta Chat JSON-RPC bridge; no HTTP webhook |

## Implemented
- deliver: COMPLETE — text + files (with caption on first), plus
  action-shape reaction / delete. Edit is explicitly Unsupported
  (Delta Chat protocol does not allow editing already-sent messages).
  `crates/copperclaw-channels/deltachat/src/adapter.rs:422`
- subscribe: validates the `account/<id>/chat/<id>` shape.
- set_typing: trait-default Ok (no typing API).
- edit_message: returns Unsupported via action shape.
- add_reaction: action="reaction" → send_reaction.
- plain_text_fallback: trait-default None.
- open_dm: trait-default None.

## Gaps
LOW:
- platform_id form is `account/<id>/chat/<id>` — distinctive and not
  documented in the operator-facing docs.
- Outgoing files are written to `<data_dir>/outgoing/<filename>`; the
  directory grows monotonically with no cleanup.

## Edge cases tested
- [x] text → send_msg
- [x] files with caption + filename
- [x] multiple files: caption-first then filename
- [x] reaction action
- [x] reaction with string target id
- [x] reaction missing target → BadRequest
- [x] reaction missing emoji → BadRequest
- [x] reaction with unparseable target → BadRequest
- [x] delete action
- [x] delete missing target → BadRequest
- [x] edit → Unsupported
- [x] unknown action → Unsupported
- [x] malformed platform_id → BadRequest
- [x] mismatched account → BadRequest
- [x] send_msg error propagates
- [x] subscribe with valid + bad platform_id
- [x] set_typing → Ok

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- Periodic cleanup of `outgoing/` directory.
- Document the `account/<id>/chat/<id>` platform_id format in the
  operator runbook.
