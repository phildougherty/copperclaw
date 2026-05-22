# resend channel audit

## Implemented
- deliver: COMPLETE — POST /emails with subject/text/html/attachments/
  thread headers. `crates/ironclaw-channels/resend/src/adapter.rs:214`
- subscribe: trait-default Ok (email is outbound-only by design).
- set_typing: trait-default Ok (no platform concept).
- edit_message: trait-default Unsupported (email can't be edited
  after send).
- add_reaction: trait-default Unsupported.
- plain_text_fallback: trait-default None.
- open_dm: trait-default None.

## Gaps
None of HIGH severity. Outbound-only is the intentional design.

LOW:
- No webhook ingress for bounce / complaint / delivered events. Resend
  does emit these via webhooks; future inbound work could surface them
  as system events.

## Edge cases tested
- [x] text returns resend id
- [x] html variant
- [x] explicit + default subject
- [x] multi-recipient via comma split
- [x] empty / whitespace platform_id → BadRequest
- [x] trailing / double comma recipient → BadRequest
- [x] non-object content → BadRequest
- [x] missing body → BadRequest
- [x] non-string subject/text/html → BadRequest
- [x] null subject falls back to default
- [x] thread_id sets reply headers (In-Reply-To / References)
- [x] attachments encoded
- [x] attachment path-separator / backslash / .. / leading-dot rejection
- [x] empty / oversize attachment filename rejection
- [x] nul / control char in attachment name rejection
- [x] max-length attachment name accepted
- [x] system actions return Unsupported
- [x] auth / rate / bad-request / transport propagation

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- Inbound webhook handler for bounce / complaint / delivered events.
