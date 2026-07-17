# cli/stuck-tool-restart

M21 S2 (Wave-1 X-rider): the hung-tool recovery sequence, pinned end to
end — detection past the sweep's unconditional 30-minute ceiling,
`ReconcileAction::StuckRestart` through the real `ContainerManager`
wired as the sweep's `StuckActuator`, and the single crash-restart
apology actually reaching the channel adapter through the real
`DeliveryService`. Registered as
`cli_stuck_tool_restart_delivers_single_apology_end_to_end` in
`crates/copperclaw-host/tests/replay.rs`.

## What runs

1. The fixture drives one normal turn ("kick off the long build" ->
   "Starting the long build now."), so the session, its routing, and the
   first delivery are produced by the REAL inbound -> router -> runner ->
   outbound -> delivery pipeline. The four expected streams pin that
   baseline byte-for-byte.
2. The registered test then reproduces exactly the mid-hang state
   production would be in — this is state, not behavior, so seeding it
   directly is honest (see "Why the hang itself is not fixture-driven"
   below): session `Running`, heartbeat file fresh (the runner process
   IS alive during a hung tool — the shape the heartbeat/crash path can
   never catch, M21 decision (a)), one in-flight inbound with a
   `Processing` ack, and a `container_state` tool row started 31 minutes
   ago inside a declared 1-hour timeout.
3. One `SweepService::run_once_actuated` pass with the manager as
   actuator (mock runtime): the session is detected past
   `ABSOLUTE_CEILING_MS`, the restart lands (container `Stopped`, tool
   state cleared), and exactly one apology row is written in reply to
   the wedged inbound.
4. A delivery pass hands the apology to the cli `MockAdapter` — the leg
   S2's own mock-runtime integration test
   (`container_manager/stuck_actuator.rs`) stops short of.
5. A second sweep + delivery pass is byte-quiet: no re-detection (the
   tool state was cleared), no second apology row, no second apology on
   the wire.

## What the test asserts

- `report.stuck_past_ceiling == [session]` on the first pass only.
- Container `Stopped`, `container_state.current_tool` cleared.
- Exactly ONE delivered chat containing the "snag ... restart" recovery
  copy; exactly one such row in `messages_out`, `in_reply_to` the wedged
  inbound.
- The apology does NOT claim "the operator has been notified" — the
  honest-copy fix (decision (d)) holds until O4 wires real alerts.

## Why the hang itself is not fixture-driven

A fixture cannot make the in-process runner genuinely hang mid-tool:
the harness runs each turn to completion before its delivery pass, and
the S6 `TestClock` advances the RUNNER's timed surfaces only — the
sweep's stuck check compares the persisted `tool_started_at` against
wall-clock `Utc::now()`, which no harness hook rewinds. Seeding the
persisted tool state (the exact rows a wedged runner leaves behind) and
driving the real detection/actuation/delivery machinery over it is the
deterministic equivalent. The runner-side "keeps its heartbeat fresh
during tool dispatch" half is pinned by the runner's own unit tests.
