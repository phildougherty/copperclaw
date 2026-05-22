# emacs channel audit

## Implemented
- deliver: COMPLETE for text via Elisp template eval. Files +
  system-actions are explicitly Unsupported (genuinely out of scope —
  file handling in Emacs is too platform/buffer-specific).
  `crates/ironclaw-channels/emacs/src/adapter.rs:97`
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
