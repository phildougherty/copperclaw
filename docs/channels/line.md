# line channel audit

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
