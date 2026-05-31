# line channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | text via reply-or-push (uses inbound reply-token cache when fresh) |
| Auto-split long messages | yes | 5000-char cap declared via `max_message_chars()` (adapter.rs:130) |
| Honour `Retry-After` | yes | `AdapterError::Rate { retry_after }` from `api.rs`; delivery loop reads it |
| Typing indicator | no | trait default; no platform concept |
| Native cards (buttons/sections) | no | LINE has Flex Messages but the adapter does not yet emit them; falls back via trait-default text render |
| Native breadcrumbs (tool chips) | fallback | trait-default `[tool]` text line |
| Inbound reply_to context | no | router does not set `reply_to`; the LINE webhook payload's `quotedMessageId` is available but not wired in v1 |
| Inbound group vs DM distinction | yes | `source.type` group / room / user (router.rs:159, 288) |
| Edit messages | no | LINE Messaging API does not expose edit; trait-default Unsupported (any non-`post` action returns BadRequest) |
| Reactions | no | LINE Messaging API does not expose reactions |
| Files / attachments | no | explicit `Unsupported` — multipart upload step not built (adapter.rs:135) |
| Threading | no | LINE has no thread concept; `supports_threads()` defaults to false |
| Webhook secret verification | yes | HMAC-SHA256 over body, base64-encoded, `X-Line-Signature` header (signature.rs:63) |

## Implemented
- deliver: PARTIAL — text only. Reply-or-push fallback uses the
  inbound reply token cache. `crates/ironclaw-channels/line/src/adapter.rs:129`
- subscribe: trait-default Ok (router handles webhook).
- set_typing: trait-default Ok (no platform concept).
- edit_message: NOT implemented. Any non-"post" action returns BadRequest.
- add_reaction: NOT implemented.
- plain_text_fallback: trait-default None.
- open_dm: trait-default None.

## Gaps
HIGH:
- The README claim "no half-finished adapters" is the closest to false
  here. line supports only the `post` action; edit and reaction are
  not implemented. LINE Messaging API does NOT expose either, so this
  is a platform limit rather than missing work — but the README should
  acknowledge it.

MED:
- Files unsupported. LINE supports image / video / audio / file
  messages, but encoding them requires a multipart upload step we
  haven't built. `crates/ironclaw-channels/line/src/adapter.rs:135`

LOW:
- Reply tokens are single-use and time-limited; if the cache slot is
  stale, the API call surfaces a generic BadRequest. We could attempt
  to fall through to push automatically.

## Edge cases tested
- [x] reply when token present
- [x] falls back to push without token
- [x] without text → BadRequest
- [x] unknown action → BadRequest
- [x] files → Unsupported

## Fixes in this PR
- This audit doc — to explicitly document the partial scope.

## Deferred for follow-up
- Image / video / audio / file delivery via multipart upload.
- Automatic reply→push fallback on expired token.
