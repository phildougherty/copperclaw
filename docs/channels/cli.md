# cli channel audit

Reference adapter. Drives the `cclaw chat` REPL via stdio + FIFO.

## Native UI capabilities

| Capability | Native | Notes |
|---|---|---|
| Chat (text) | yes | always supported via stdio / FIFO write |
| Auto-split long messages | no | no `max_message_chars()` override; terminal has no hard cap |
| Honour `Retry-After` | yes | shared via delivery loop (no platform to rate-limit but the path is the same) |
| Typing indicator | no | trait default no-op; no terminal concept |
| Native cards (buttons/sections) | no | trait-default text fallback writes the rendered card |
| Native breadcrumbs (tool chips) | fallback | trait-default writes `[tool] detail` lines |
| Inbound reply_to context | no | `is_group: None`, `reply_to: None` on every event (lib.rs:381) |
| Inbound group vs DM distinction | no | reference adapter has no concept of either |
| Edit messages | no | trait-default Unsupported |
| Reactions | no | trait-default Unsupported |
| Files / attachments | yes | rendered as `[files: ...]` listing in chat output |
| Threading | no | `supports_threads() = false` (trait default) |
| Webhook secret verification | n/a | local process, no network surface |

## Implemented
- deliver: COMPLETE (`crates/copperclaw-channels/cli/src/lib.rs:438`)
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
