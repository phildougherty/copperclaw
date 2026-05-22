# deltachat channel audit

## Implemented
- deliver: COMPLETE — text + files (with caption on first), plus
  action-shape reaction / delete. Edit is explicitly Unsupported
  (Delta Chat protocol does not allow editing already-sent messages).
  `crates/ironclaw-channels/deltachat/src/adapter.rs:422`
- subscribe: validates the `account/<id>/chat/<id>` shape.
- set_typing: trait-default Ok (no typing API).
- edit_message: returns Unsupported via action shape.
- add_reaction: action="reaction" → send_reaction.
- plain_text_fallback: trait-default None.
- open_dm: trait-default None.

## Gaps
LOW:
- platform_id form is `account/<id>/chat/<id>` — distinctive and not
  documented in the operator-facing docs.
- Outgoing files are written to `<data_dir>/outgoing/<filename>`; the
  directory grows monotonically with no cleanup.

## Edge cases tested
- [x] text → send_msg
- [x] files with caption + filename
- [x] multiple files: caption-first then filename
- [x] reaction action
- [x] reaction with string target id
- [x] reaction missing target → BadRequest
- [x] reaction missing emoji → BadRequest
- [x] reaction with unparseable target → BadRequest
- [x] delete action
- [x] delete missing target → BadRequest
- [x] edit → Unsupported
- [x] unknown action → Unsupported
- [x] malformed platform_id → BadRequest
- [x] mismatched account → BadRequest
- [x] send_msg error propagates
- [x] subscribe with valid + bad platform_id
- [x] set_typing → Ok

## Fixes in this PR
None — adapter healthy.

## Deferred for follow-up
- Periodic cleanup of `outgoing/` directory.
- Document the `account/<id>/chat/<id>` platform_id format in the
  operator runbook.
