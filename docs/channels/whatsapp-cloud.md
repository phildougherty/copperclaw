# whatsapp-cloud channel audit

## Implemented
- deliver: COMPLETE — text + reply (text-with-context) + files (upload
  + send-by-id, plus reply context) + action-reaction. Edit is
  Unsupported (WhatsApp Cloud API does not allow editing sent messages).
  `crates/ironclaw-channels/whatsapp-cloud/src/adapter.rs:174`
- subscribe: trait-default Ok (webhook ingress).
- set_typing: approximates typing by marking the user's last message
  as read when thread_id (containing the message_id) is supplied.
- edit_message: action="edit" → Unsupported.
- add_reaction: action="reaction" → send_reaction.
- plain_text_fallback: trait-default None.
- open_dm: trait-default None.

## Gaps
LOW:
- The `set_typing` mark-read approximation is best-effort and only
  fires when the caller passes a real message id in thread_id. A
  cleaner contract would be a dedicated `mark_read` method.
- Reaction action accepts both `target_message_id` and `message_id`
  field names — the legacy name should be deprecated.
- MIME inference is best-effort by extension; binary uploads without
  an extension default to `application/octet-stream`.

## Edge cases tested
- [x] text uses parsed phone_number_id
- [x] text reply includes context
- [x] default pnid when no prefix
- [x] no prefix and no default → BadRequest
- [x] malformed prefix → BadRequest
- [x] files upload + send
- [x] text + files
- [x] edit → Unsupported
- [x] reaction action
- [x] reaction with legacy message_id field
- [x] reaction without target → error
- [x] unknown action → Unsupported
- [x] set_typing without thread_id → noop
- [x] set_typing with thread_id → mark_read
- [x] set_typing auth propagation
- [x] set_typing malformed platform_id → error
- [x] text auth / rate propagation

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- Deprecate `message_id` in favor of `target_message_id`.
- Consider a dedicated `mark_read` adapter method instead of
  overloading `set_typing`.
