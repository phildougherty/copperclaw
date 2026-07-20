# cli/transcript-render — M22 Wave 0 flattened-rich-payload baseline

**Card:** M22 Wave 0 ("Fixtures first"), baseline for Wave 1's A5
structured cli output and Wave 3's D5 cli transcript promotion.

**What it pins.** How rich outbound payloads render on the cli surface
TODAY. The delivery service calls the rich hooks (`dispatch_todo_list`,
`dispatch_diff`, …) on every adapter; `CliAdapter` (like the harness's
bare mock) overrides none of them, so the `ChannelAdapter` trait
defaults flatten each structured payload to plain text one call before
the write. `expected/delivered.jsonl` freezes that flattening
byte-for-byte:

- the todo checklist as `Plan\n[ ] …\n(0/2 done, 0 in progress,
  2 pending)` text,
- the diff as `--- a/…\n+++ b/…\n@@ …` unified-diff prose (including
  today's `a//tmp/...` double-slash quirk),
- while `expected/messages-out.jsonl` proves the structured
  `todo_list` / `diff` rows existed upstream of the flattening.

Since Wave 3's D5 the fixture ALSO pins the cli transcript frames.
cli takes `Behavior::Transcript`
(`capabilities::renders_client_side_transcript` — no `edit_message`,
but `cclaw chat` collapses the append-only frame log client-side), so
the Task HUD emits every frame as a fresh `Breadcrumb` EVENT: a
batch-start frame ("running: <tool>") and a batch-end frame
("N tool calls | M:SS" with the cumulative step transcript) per tool
batch, then the final done-collapse ("done in M:SS, 4 tool calls",
steps attached). No `update_breadcrumb` System row appears anywhere —
the append-only log has no edit anchor, so frames never ride the edit
rail (`existing_message_id` stays `None` through delivery). This
flipped the Wave 0 baseline, which pinned the ABSENCE of Breadcrumb
rows on cli (`Behavior::StatusRows` never called `emit_task_hud`).

One absence remains deliberately part of the pin:

- **No Diff row from `write_file` creating a new file** — there is no
  before-content to diff against; only the `edit_file` mutation emits
  one.

**Shape.** One cli inbound drives five scripted turns: `todo_add` x2
(each emits the post-mutation `TodoList`), `write_file` creating
`notes.txt` under the fixture's data root, `edit_file` rewriting its
summary line (emits the `DiffCard`), then the final text.

**Re-exec seam.** The `todo_*` tools resolve their store through
`COPPERCLAW_DATA_ROOT` (compiled-in `/data` otherwise), and the
workspace `forbid(unsafe_code)` blocks `std::env::set_var` — so the
registered test re-execs itself as a child with the var set to
`/tmp/copperclaw-m22-transcript-render` via the safe `Command::env`,
exactly like the X2 verify-gate fixture. The scripted turns hard-code
file paths under that root.

**Regeneration.** `COPPERCLAW_M22W0_GENERATE=1 cargo test -p
copperclaw-host --test replay cli_transcript_render -- --nocapture`
prints the dump (relayed from the re-exec'd child) to slice into
`expected/*.jsonl`.
