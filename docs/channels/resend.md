# resend channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | outbound `POST /emails` with subject/text/html |
| Auto-split long messages | no | no `max_message_chars()` override; email bodies have no practical wire cap |
| Honour `Retry-After` | yes | `AdapterError::Rate { retry_after }` from `api.rs`; delivery loop reads it |
| Typing indicator | no | trait default; email has no typing concept |
| Native cards (buttons/sections) | no | falls back via trait-default text render. Card → HTML email is a future override |
| Native breadcrumbs (tool chips) | no | not surfaced — email is too high-latency for live tool chips |
| Inbound reply_to context | n/a | outbound-only by design |
| Inbound group vs DM distinction | n/a | outbound-only |
| Edit messages | no | trait-default Unsupported — email can't be edited after send |
| Reactions | no | trait-default Unsupported |
| Files / attachments | yes | base64-encoded `attachments[]` on the email |
| Threading | yes | RFC 2822 `In-Reply-To` / `References` headers set when `thread_id` present; `supports_threads() = true` (adapter.rs:208) |
| Webhook secret verification | n/a (outbound) | no inbound endpoint; Resend's bounce/complaint webhooks are deferred for follow-up |

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
