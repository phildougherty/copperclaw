# signal channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | `signal-cli` JSON-RPC `send` |
| Auto-split long messages | no | no `max_message_chars()` override; Signal cap is large enough to skip splitting |
| Honour `Retry-After` | yes | `AdapterError::Rate { retry_after }` from `api.rs`; delivery loop reads it |
| Typing indicator | yes | `send_typing(stop=false)` (adapter.rs:287) |
| Native cards (buttons/sections) | no | Signal protocol has no card primitive; falls back via trait-default text render |
| Native breadcrumbs (tool chips) | fallback | trait-default `[tool]` text line |
| Inbound reply_to context | yes | `dataMessage.quote.id` → `InboundEvent.reply_to` (parse.rs:124); quote `id` is the ms timestamp, which matches our `message.id` format |
| Inbound group vs DM distinction | yes | `group_id.is_some()` (parse.rs:73) |
| Edit messages | yes (via action) | `content.action = "edit"` → `send_edit`. Trait-level `edit_message` NOT overridden |
| Reactions | yes (via action) | `content.action = "reaction"` → `send_reaction` with `remove` flag. Trait-level `add_reaction` NOT overridden |
| Files / attachments | yes | staged on disk then passed by path to `send_with_attachments` |
| Threading | no | `supports_threads() = false` (adapter.rs:272) |
| Webhook secret verification | n/a (RPC) | inbound rides the `signal-cli` JSON-RPC stdio; no HTTP webhook |

## Implemented
- deliver: COMPLETE — text + files, plus action-shape edit / reaction /
  delete. `crates/copperclaw-channels/signal/src/adapter.rs:296`
- subscribe: trait-default Ok (signal-cli daemon streams everything).
- set_typing: send_typing(stop=false).
- edit_message: action="edit" — send_edit.
- add_reaction: action="reaction" — send_reaction(remove=false by default).
- plain_text_fallback: trait-default None.
- open_dm: synthesises a `user:<e164>` DmHandle.

## Gaps
LOW:
- `target_author` field is required for reactions (Signal needs the
  message author's identity to address the reaction); the test for
  missing-author exists but the field name is undocumented.

## Edge cases tested
- [x] text to user (recipient form)
- [x] text to group (group_id form)
- [x] files staged + sent
- [x] edit with int timestamp
- [x] edit accepts string target id
- [x] reaction sets default remove=false
- [x] reaction with remove=true
- [x] delete via remote_delete
- [x] unknown action → Unsupported
- [x] edit missing target / text → BadRequest
- [x] reaction missing author → BadRequest
- [x] bad platform_id → BadRequest
- [x] attachment with unsafe filename → BadRequest
- [x] rate / auth propagation
- [x] set_typing
- [x] set_typing bad platform_id → BadRequest

## Fixes in this PR
- `SignalSupervisor` (in `rpc.rs`) wraps the `JsonRpcClient` and
  polls liveness; when the underlying signal-cli daemon's stdio
  closes (child exit), it respawns the process with exponential
  backoff (500 ms → 30 s) and forwards notifications through a
  shared mpsc so the adapter sees the respawn as transparent.
  Factory wires the supervisor in place of a bare `JsonRpcClient`.

## Deferred for follow-up
- Document the `target_author` field name on the action shape.
