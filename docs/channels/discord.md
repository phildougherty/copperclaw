# discord channel audit

## Implemented
- deliver: COMPLETE — POST /channels/:id/messages, multipart when files
  are present. `crates/ironclaw-channels/discord/src/adapter.rs:337`
- subscribe: trait-default Ok (gateway streams everything).
- set_typing: POST /channels/:id/typing.
- edit_message: PATCH /channels/:cid/messages/:mid.
- add_reaction: PUT /channels/:cid/messages/:mid/reactions/:emoji/@me.
- plain_text_fallback: strips `embeds`.
- open_dm: POST /users/@me/channels resolves to a dm channel id.

## Gaps
LOW:
- thread_id is ignored in deliver (Discord threads use channel id of
  the thread directly; the host should map the thread to a channel id
  before calling deliver). Surface a doc-comment note.
- Reactions accept any emoji string — Discord requires URL-encoding for
  custom emoji `name:id`. The rest layer encodes; not user-facing.

## Edge cases tested
- [x] returns message id
- [x] files multipart
- [x] auth error propagates
- [x] plain_text_fallback strips embeds
- [ ] message > 2000 chars — NOT tested
- [ ] file > 25MB / >10MB depending on guild boost — NOT tested
- [ ] gateway reconnect with resume — covered separately in gateway tests

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- Validate text length (Discord caps at 2000 chars per message) before
  POST.
- Document the thread_id-is-channel-id convention in the adapter
  doc-comment.
