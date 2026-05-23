# webex channel audit

## Implemented
- deliver: COMPLETE — text + markdown + card + files (multipart, one
  POST per file), plus system actions (edit / delete / reaction).
  `crates/ironclaw-channels/webex/src/adapter.rs:297`
- subscribe: trait-default Ok (webhook ingress; firehose model).
- set_typing: trait-default Ok (no public typing API).
- edit_message: action="edit" → PUT.
- add_reaction: action="reaction" — POSTs to `<api_base>/reactions`.
  A 404 or 501 from the server is mapped to `AdapterError::Unsupported`
  so a Webex deployment that hasn't enabled the reactions API
  degrades cleanly.
- plain_text_fallback: trait-default None.
- open_dm: synthesises a `person:<user_id>` DmHandle.

## Gaps
LOW:
- Reaction endpoint discovery is via HTTP-status fallback (404/501 from
  POST `/reactions` → `AdapterError::Unsupported`). No config knob —
  Webex deployments without the reactions API degrade cleanly but
  operators only learn about the limit at first reaction attempt.
- Multipart file post is one HTTP call per file — not batched.

## Edge cases tested
- [x] text to room
- [x] thread_id sets parentId
- [x] markdown-only message
- [x] card attachment
- [x] file via multipart
- [x] card + file
- [x] two files → two posts
- [x] empty content still calls API
- [x] BadRequest / 401 / 429 / 5xx propagation
- [x] person platform_id → DM endpoint
- [x] person + file → Unsupported (Webex DMs don't take files via this
      adapter shape)
- [x] edit / delete / reaction
- [x] reaction 404 / 501 from `/reactions` → Unsupported
- [x] system unknown action → BadRequest
- [x] system to person → Unsupported
- [x] chat / task / webhook / agent message kinds all route to chat path

## Fixes in this PR
- `SignatureAlgo::Auto` config value: when set, the verifier inspects
  the incoming X-Spark-Signature length (40 → sha1, 64 → sha256) and
  validates against the corresponding HMAC. Operators on the Webex
  sha256 rollout no longer need to flip a config when the upstream
  switches; `webhook_algo: "auto"` survives the transition. Sha1 and
  sha256 explicit modes still work as before; `compute_signature`
  with `Auto` panics (it's verifier-only).

## Deferred for follow-up
- Allow file delivery to person targets.
- Batch multipart for multiple files.
