# wechat (Work Weixin) channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | `message/send` text type |
| Auto-split long messages | yes | 600-char cap declared via `max_message_chars()` (adapter.rs:169) — under-approximation of the 2 KiB byte cap to stay safe with CJK content |
| Honour `Retry-After` | yes | `AdapterError::Rate { retry_after }` from `api.rs`; delivery loop reads it |
| Typing indicator | no | trait default; no Work Weixin typing API |
| Native cards (buttons/sections) | yes | `template_card` passthrough via `content.template_card` (adapter.rs:188) — opaque, no schema validation |
| Native breadcrumbs (tool chips) | fallback | trait-default `[tool]` text line |
| Inbound reply_to context | no | parser does not set `reply_to`; Work Weixin webhook payloads don't carry a parent-message reference |
| Inbound group vs DM distinction | no | `is_group: Some(false)` always — Work Weixin DMs have no group-vs-DM signal in the webhook payload (events/router.rs:273) |
| Edit messages | no | `content.action = "edit"` → Unsupported (platform limit) |
| Reactions | no | `content.action = "reaction"` → Unsupported (platform limit) |
| Files / attachments | yes | per-type upload then send by `media_id` (image / voice / video / file) |
| Threading | no | `supports_threads() = false` (adapter.rs:158) |
| Webhook secret verification | yes | SHA-1 over sorted-concat of `[token, timestamp, nonce, encrypted_text]` (NOT HMAC — Work Weixin spec). Constant-time compare in `signature.rs` |

## Implemented
- deliver: COMPLETE — text + files (image / voice / video / file with
  per-type upload + send-by-id) + template_card passthrough.
  `crates/ironclaw-channels/wechat/src/adapter.rs:163`
- subscribe: trait-default Ok (webhook events route via events/router.rs).
- set_typing: trait-default Ok.
- edit_message: action="edit" → Unsupported (platform doesn't allow).
- add_reaction: action="reaction" → Unsupported.
- plain_text_fallback: trait-default None.
- open_dm: trait-default None.

## Gaps
LOW:
- Edit / reaction are platform limits, not implementation gaps —
  Work Weixin's API does not expose either. Documented inline.
- template_card content passes through opaquely — malformed cards
  surface as remote 40xx rather than local BadRequest.
- Access token cache lifetime is fixed (2h per platform default); no
  proactive refresh ahead of expiry.

## Edge cases tested
- [x] text to user
- [x] text to party
- [x] text to tag
- [x] unknown prefix → BadRequest
- [x] bare id without prefix → BadRequest
- [x] empty user/party/tag id → BadRequest
- [x] file-only upload then send
- [x] image-only upload as image
- [x] text + files both sent
- [x] edit / reaction / unknown → Unsupported
- [x] template_card passthrough
- [x] auth / rate / bad-request propagation

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- template_card schema validation.
- Proactive access-token refresh.
