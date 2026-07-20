# telegram/hud-transcript — M22 Wave 0 multi-batch HUD frame baseline

**Card:** M22 Wave 0 ("Fixtures first") — the byte-anchor for the
transcript-UI program's Waves 1-3.

**What it pins.** Nothing before this fixture pinned Task HUD frame TEXT
end-to-end across a multi-batch run. Wave 0 froze the pre-M22 frames;
Wave 2's B-card regenerated it and it now pins the TRANSCRIPT frames:
`Breadcrumb.steps` populated with each completed tool call (B5 — tool
name, arg detail, done/failed status, result-line summary, capped at 40
newest), the summary carrying the rounded token count and cost from the
scripted usage events (B4 — `"N tool call(s) | M:SS | 5k tokens |
$0.02"`, tokens rounded to the nearest 100 and cost to the nearest cent
so frame fingerprints stay stable between rounding boundaries), and the
enriched `"done in M:SS, 5 tool calls, 6k tokens, $0.03"` collapse that
keeps the full transcript attached. Later waves (D2 telegram restyle)
land as further diffs over `expected/messages-out.jsonl`.

**Shape.** One telegram group message drives a six-turn scripted
provider: five sequential `shell` tool calls (`echo step one` …
`echo step five`), then the final text. Every scripted turn carries
usage events (900 input + 100 output tokens) so the B4 spend fields are
exercised end-to-end against the claude-sonnet-4-6 list price. Telegram is edit-capable, so the
HUD resolves to `Behavior::Live`:

- batch 1 start posts the chip (a `MessageKind::Breadcrumb` row);
- every later batch boundary (start + end) emits an `update_breadcrumb`
  System row — nine running edits across the five batches;
- `finalize` collapses to the `done in …` one-liner (the tenth edit).

`max_tool_turns: 8` keeps the five scripted tool rounds under the soft
cap so the run ends on the model's own final text, not a budget stop.

**Harness modelling.** `model_rich_breadcrumbs: true` — same as
`matrix/hud-live-edit`: the first frame is a `Breadcrumb`-kind post
returning a stable anchor; every later frame routes through the inner
mock's `edit_message` addressed to that one anchor.

**Assertions** (in `tests/replay.rs`, on top of the byte-stable JSONL
diff): exactly one breadcrumb post, at least three in-place edits all on
a single anchor, at least one frame carrying the full `5 tool calls`
count, the last edit being the done-collapse, and the final chat answer
delivered on its own. Plus the B5/B4 pins: >= 3 running frames with
non-empty `steps`, one frame carrying all five steps with the first
step's fields pinned exactly, rounded token/cost fields on a running
frame, and the collapse keeping the transcript and total spend.

**Regeneration.** `COPPERCLAW_M22W0_GENERATE=1 cargo test -p
copperclaw-host --test replay telegram_hud_transcript -- --nocapture`
prints the dump blocks to slice into `expected/*.jsonl`.
