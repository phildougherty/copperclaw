# webhooks channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | no | inbound-only by design; `deliver` returns `Unsupported("webhooks channel is inbound-only ...")` |
| Auto-split long messages | no | no outbound path |
| Honour `Retry-After` | yes | shared via delivery loop (path unused — outbound is Unsupported) |
| Typing indicator | no | no platform concept (axum receive endpoint) |
| Native cards (buttons/sections) | no | inbound-only |
| Native breadcrumbs (tool chips) | no | inbound-only |
| Inbound reply_to context | passthrough | router copies `reply_to` from the payload if the sender includes it (router.rs) |
| Inbound group vs DM distinction | no | `is_group: None`; the generic shape doesn't pin platform semantics |
| Edit messages | n/a | trait-default Unsupported (outbound is Unsupported) |
| Reactions | n/a | trait-default Unsupported |
| Files / attachments | n/a | inbound-only |
| Threading | no | `supports_threads() = false` (adapter.rs:84) |
| Webhook secret verification | yes | generic HMAC-SHA256 over body with operator-supplied prefix (e.g. `sha256=`, GitHub style). Constant-time via `hmac::Mac::verify_slice` (signature.rs:73) |

## Implemented
- deliver: returns `Unsupported("webhooks channel is inbound-only ...")`
  by design. `crates/ironclaw-channels/webhooks/src/adapter.rs:88`
- subscribe: trait-default Ok (the axum server is already bound).
- set_typing: trait-default Ok (no platform concept).
- edit_message / add_reaction: trait-default Unsupported.
- plain_text_fallback: trait-default None.
- open_dm: trait-default None.

The adapter owns the spawned axum task handle so it can abort on
shutdown. HMAC signature verification lives in `signature.rs` and is
exercised by the router tests.

## Gaps
None of HIGH severity. Inbound-only is the intentional design.

LOW:
- README claim "no half-finished adapters" technically holds because
  this is documented as inbound-only — but a user wiring webhooks
  expecting to deliver via it would only find out at first deliver.
  The host's wiring-validation step should warn at wiring-create time.

## Edge cases tested
- [x] deliver always returns Unsupported with `inbound-only` marker
- [x] subscribe / set_typing / open_dm defaults
- [x] server abort on Drop
- [x] abort_server idempotent

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- Add a wiring-time check that warns when an outbound wiring targets
  the webhooks channel (the user definitely meant to wire another
  channel for replies).
