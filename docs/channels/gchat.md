# gchat (Google Chat) channel audit

## Implemented
- deliver: COMPLETE for text + threaded text + card + edit / delete /
  reaction. `crates/ironclaw-channels/gchat/src/adapter.rs:91`
- subscribe: trait-default Ok (webhook ingress).
- set_typing: trait-default Ok (no platform concept).
- edit_message: action="edit" → PUT with updateMask.
- add_reaction: action="reaction" with shortcode → codepoint map.
- plain_text_fallback: trait-default None.
- open_dm: returns None (DMs require spaces.create which is out of
  scope in v1).

## Gaps
MED:
- Files Unsupported — Google Chat attachments require the Drive upload
  flow (uploadAttachment → Drive file → attachment reference). Documented.
- Card content passes through opaquely — malformed cards surface as a
  remote 400 rather than local BadRequest.

LOW:
- Emoji map covers ~30 shortcodes; broader coverage would be useful.

## Edge cases tested
- [x] text → send_text
- [x] threaded text
- [x] card via send_card with default card_id
- [x] edit action via PUT + updateMask
- [x] edit missing message_name → BadRequest
- [x] edit missing text → BadRequest
- [x] delete action
- [x] delete missing message_name → BadRequest
- [x] reaction with mapped codepoint
- [x] reaction with unknown emoji → Unsupported
- [x] reaction missing message_name / emoji → BadRequest
- [x] unknown action → Unsupported
- [x] files → Unsupported
- [x] bad platform_id → BadRequest
- [x] auth / rate / transport / 404 propagation

## Fixes in this PR
None.

## Deferred for follow-up
- Implement Drive-based attachment upload.
- Validate card schema at deliver time.
- Expand emoji shortcode map.
