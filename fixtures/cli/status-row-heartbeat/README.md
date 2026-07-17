# cli / status-row-heartbeat

M21 S6: pins the bare-channel Task HUD status-row timed legs via the
replay harness's test-clock seam — the legs that were unfixturable since
M18 (the X2 known gap) because they needed a real 60-second wall-clock
wait.

One CLI inbound drives three scripted Claude turns: `tool_use` (shell
`echo step one`), `tool_use` (shell `echo step two`), then final text.
The manifest's `provider_responses` entries carry `advance_clock_ms`:
serving turn 1 advances the harness's shared runner `TestClock` by 61s,
serving turn 2 by another 90s. Since the runner's HUD reads elapsed time
through `RunnerDeps.clock` (`copperclaw-runner/src/clock.rs`), the two
tool-batch boundaries land at exactly 61s and 151s of "wall" time with
zero real waiting, so the expected streams pin:

- the 60s first-fire status row: `Still working on this — 61s in, 1 tool
  call so far (latest: shell). I'll keep going.`
- the 150s softened row: `Still working on this — 151s in, 2 tool calls
  so far (latest: shell). This is taking longer than usual, but I'm
  still going.`

The clock only moves on explicit advancement, so the `61s in` / `151s
in` renderings are exact and byte-stable. The cli channel has no
message-edit API, which is what routes the HUD onto the status-row path
in the first place.
