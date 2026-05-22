# wechat (Work Weixin) channel audit

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
