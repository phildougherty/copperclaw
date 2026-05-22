# cli channel audit

Reference adapter. Drives the `iclaw chat` REPL via stdio + FIFO.

## Implemented
- deliver: COMPLETE (`crates/ironclaw-channels/cli/src/lib.rs:438`)
- subscribe: noop (no platform concept)
- set_typing: noop (no platform concept)
- edit_message: trait-default Unsupported (no platform concept)
- add_reaction: trait-default Unsupported (no platform concept)
- plain_text_fallback: trait-default None (no formatting validation possible)
- open_dm: trait-default None
- supports_threads: false

## Gaps
None. The cli channel is intentionally the simplest channel — it
exists to drive the REPL and to be the in-process reference for other
adapters. All defaults are correct for its semantics.

## Edge cases tested
- [x] empty input (`empty_input_produces_no_events`)
- [x] blank line (`blank_lines_are_skipped`)
- [x] file attachments rendered (`deliver_appends_attachment_list`,
      `deliver_attachment_only_when_text_empty`)
- [x] FIFO mode + writer drain (`fifo_survives_writer_drain`)
- [x] log mode append + flush (`log_mode_appends_and_flushes`)

## Fixes in this PR
None — reference adapter is healthy.

## Deferred for follow-up
None.
