# cli / status-row-heartbeat

Origin M21 S6, reshaped by M22 D5: pins how a long silent cli run keeps
the user reassured, via the replay harness's test-clock seam — the
timed legs that were unfixturable since M18 (the X2 known gap) because
they needed a real 60-second wall-clock wait.

One CLI inbound drives three scripted Claude turns: `tool_use` (shell
`echo step one`), `tool_use` (shell `echo step two`), then final text.
The manifest's `provider_responses` entries carry `advance_clock_ms`:
serving turn 1 advances the harness's shared runner `TestClock` by 61s,
serving turn 2 by another 90s. Since the runner's HUD reads elapsed time
through `RunnerDeps.clock` (`copperclaw-runner/src/clock.rs`), the two
tool-batch boundaries land at exactly 61s and 151s of "wall" time with
zero real waiting.

## What it pins since M22 D5

cli takes `Behavior::Transcript`
(`capabilities::renders_client_side_transcript` — the append-only
`chat.log` has no `edit_message`, but its reader `cclaw chat` collapses
repeated frames into an in-place repaint). The HUD therefore emits its
frames as fresh `Breadcrumb` EVENTS, never `update_breadcrumb` edits,
and the expected streams pin the cli transcript cadence byte-for-byte:

- batch 1 boundary: `running: shell (echo step one)` then
  `1 tool call | 1:01` with the first step attached,
- batch 2 boundary: `running: shell (echo step two) · 1 tool call |
  2:31` then `2 tool calls | 2:31` with both steps,
- the final collapse: `done in 2:31, 2 tool calls`, steps attached.

The clock only moves on explicit advancement, so the `1:01` / `2:31`
renderings are exact and byte-stable.

## Where the old StatusRows pin went

Before D5 this fixture pinned the bare-channel StatusRows sentences
(`Still working on this — 61s in ... I'll keep going.` and the 151s
`taking longer than usual` softening). The heartbeat *purpose* —
periodic reassurance on a long silent run — is now covered on cli by
the transcript frames themselves: every batch boundary emits a frame
carrying the elapsed clock and the cumulative tool log (the same
boundaries the old status rows fired on), and the client repaints the
live elapsed clock between frames itself — the HUD's wall-clock ticker
deliberately stays edit-only, since on an append-only log every tick
would be a permanent line. This fixture stays registered as the pin of
that cli transcript cadence.

`Behavior::StatusRows` still serves genuinely bare channels (webhooks,
email, ...) unchanged; its 60s/150s sentence legs are pinned by the
hud.rs unit tests
(`status_rows_60s_first_fire_and_150s_softening_pinned_by_test_clock`,
now driven over the webhooks channel).

## Regeneration

`COPPERCLAW_M22W0_GENERATE=1 cargo test -p copperclaw-host --test
replay -- --exact cli_status_row_heartbeat_pins_60s_and_150s_legs
--nocapture` prints the dump to slice into `expected/*.jsonl`.
