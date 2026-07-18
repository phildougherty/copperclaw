# fixtures/cli/goal-progress (M22 A3 — goal check-in progress)

A first-class **long-running goal** fires check-in wakes and the agent reports
**progress across ≥2 wakes**. Proves the A3 sweep fan-out end-to-end: goal due →
`kind:task` check-in wake synthesised into the session → woken agent records
progress via `update_goal` → the goal accrues check-in count + progress log
across successive, croner-timed wakes.

## What it exercises

- `central.sql` seeds an active `goals` row (`g-standup`) that is due a check-in
  (`next_checkin` in the past) and re-arms every 5 minutes (`checkin_recurrence
  = */5 * * * *`). No grant / token bound, so it never pauses.
- The AX X-rider test drives **two** `SweepService::run_once` passes at
  controlled instants via the sweep's `MockClock` seam
  (`ReplayHarness::run_goal_sweep_at`) — the M21/M22 sweep test-clock seam for
  croner timing:
  - Pass 1 at `2026-06-01T00:00:30Z` (> the seeded `next_checkin`) → fires
    check-in #1; `checks::goals` re-arms `next_checkin` to `00:05:00`.
  - Pass 2 at `2026-06-01T00:06:00Z` (> the re-armed `00:05:00`) → fires
    check-in #2; re-arms to `00:10:00`.
- Each pass's woken turn calls `update_goal` (progress + 50 tokens); the
  `UpdateGoal` effect becomes a `goal` System row the delivery loop applies via
  `goals::record_progress`.

Because a goal wake can fire into a session a prior turn left `running`, the
harness drives each pass's turn off `report.goal_checkins_fired` (resolved to
the goal's session) rather than the wake check's `woken_sessions`.

## Files

- `central.sql` — agent group + cli wiring, idle session, the `goals` row.
- `inbound.sql` — `session_routing` only (the sweep synthesises the wake rows).
- `claude/001..004-turn.json` — two check-in turns, each an `update_goal` tool
  call + an end-of-turn progress report.
- `manifest.json` — scenario metadata (no `trigger_sweep`; the test drives the
  passes explicitly).

## Assertions (in `tests/replay.rs`)

- Each pass's `SweepReport.goal_checkins_fired` has length 1.
- After both passes the goal's `checkin_count == 2` and `status == active`.
- `goals::list_progress` returns **2** rows (one per wake) and
  `tokens_consumed == 100` (50 accrued per wake) — cumulative progress across
  the wakes.
