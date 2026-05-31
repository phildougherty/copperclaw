# emacs channel audit

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | Elisp template `eval` of the configured outbound s-expression |
| Auto-split long messages | no | no `max_message_chars()` override; Emacs has no wire cap |
| Honour `Retry-After` | yes | shared via delivery loop (client surfaces transport errors; emacs has no rate-limit semantic) |
| Typing indicator | no | trait default; no buffer concept of typing |
| Native cards (buttons/sections) | no | falls back via trait-default text render — cards land in the buffer as formatted text |
| Native breadcrumbs (tool chips) | fallback | trait-default `[tool]` text line — appears in the buffer |
| Inbound reply_to context | no | poll-loop alist parser sets `reply_to: None` (adapter.rs:248) |
| Inbound group vs DM distinction | no | `is_group: Some(false)` always (adapter.rs:246) — single buffer model |
| Edit messages | no | system action → Unsupported (out of scope; buffer edits are too implementation-specific) |
| Reactions | no | system action → Unsupported |
| Files / attachments | no | explicit `Unsupported` — file handling in Emacs is too buffer-specific |
| Threading | no | `supports_threads()` defaults to false |
| Webhook secret verification | n/a (local) | inbound polls the Emacs inbound-queue s-expression via the client; no network surface |

## Implemented
- deliver: COMPLETE for text via Elisp template eval. Files +
  system-actions are explicitly Unsupported (genuinely out of scope —
  file handling in Emacs is too platform/buffer-specific).
  `crates/copperclaw-channels/emacs/src/adapter.rs:97`
- subscribe: trait-default Ok (poll loop hits the inbound queue sexp).
- set_typing: trait-default Ok.
- edit_message: system action → Unsupported.
- add_reaction: system action → Unsupported.
- plain_text_fallback: trait-default None.
- open_dm: trait-default None.

## Gaps
LOW:
- Template rendering uses naive `${BUFFER_JSON}` / `${TEXT_JSON}` token
  replacement. If a user-supplied template contains those literal
  strings outside the intended placement, behavior is undefined.
- Poll loop has no jitter; multiple emacs adapters all polling at the
  same configured cadence could create thundering herd.

## Edge cases tested
- [x] text invokes template with buffer + text
- [x] escapes quotes in text
- [x] escapes newline in text
- [x] uses default buffer when platform_id empty
- [x] uses non-default buffer
- [x] files → Unsupported
- [x] system edit → Unsupported
- [x] system reaction → Unsupported
- [x] transport / auth error propagation
- [x] missing text sends empty string
- [x] subscribe / set_typing defaults

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- Add poll-loop jitter.
- Tighten template token replacement (use a real templating engine).
