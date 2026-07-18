# fixtures/cli/grant-wake-granted (M22 A2 — granted-act)

**Granted-act** wake transcript: a scheduled task fires an *autonomous* turn,
its firing task carries a live, human-approved capability grant, and the runner
**takes the pre-authorized credentialed-external action** instead of only
drafting it — the A2 marquee ("the agent does the thing it was told it could
do"). Driven end-to-end by the AX X-rider.

## What it exercises

- A `kind: task` wake row (`inbound.sql`, no human Chat row) → the runner
  classifies the turn autonomous (`run/mod.rs`).
- The firing task (`t-standup`) carries an **approved `task_grants` row**
  (`central.sql`) whose scope permits the action the turn takes (`web_fetch`).
- The **A2H `grant.json` writer**, folded into
  `container_manager::tasks_snapshot::write_tasks_snapshot`, renders the live
  effective grant to `<session>/grant.json` at spawn time. The AX harness calls
  that same production writer before the turn (`run_one_turn`), so the runner
  reads a **real** grant snapshot — not a hand-copied file.
- `run/mod.rs::load_turn_grant` loads it, and `run/tool_dispatch.rs::invoke_tool`
  consults it: the autonomy gate **opens for that action only**, admits the
  call (not the `autonomous (heartbeat/scheduled) turn` deny), and charges
  **one fire** (`grant_consume` System row → `outbound.db`), which the delivery
  loop applies back to central via `task_grants::consume_fire`.

## Files

- `central.sql` — agent group + cli wiring, an idle session, the `t-standup`
  `tasks` row, and the **approved** `task_grants` row (`grant-standup-001`,
  scope `web_fetch`).
- `inbound.sql` — the pending `kind:task` wake row (`content.task_id =
  t-standup`, `series_id = t-standup`) + `session_routing` for the reply.
- `claude/001-turn.json` — round 1: the granted `web_fetch` action.
- `claude/002-turn.json` — round 2: the end-of-turn report.
- `grant.json` — reference copy of the effective-grant snapshot the A2H writer
  must reproduce from the `task_grants` row (shape = `run::TurnGrant`). It is
  documentation only — the *live* `grant.json` the runner reads is produced by
  the writer into the session data root at turn time.
- `manifest.json` — `trigger_sweep: true`; scenario metadata.

## The `web_fetch` / SSRF boundary (honest note)

The granted action is `web_fetch` to a **loopback** URL (`http://127.0.0.1/…`).
The autonomy gate is the thing under test, and it fully opens: the call is
admitted by every policy layer and **one fire is charged BEFORE dispatch**
(`charge_grant_fire_once` runs after the policy allow, before the tool body).
The `web_fetch` tool's own SSRF net-guard then rejects the loopback target —
that guard is orthogonal to the autonomy gate and keeps the replay offline and
instant. The deterministic proof of "the gate opened and the action fired" is
therefore the emitted+applied `grant_consume`, not the fetch response body.

## Assertions (in `tests/replay.rs`)

- Exactly **one** `grant_consume` System row in `outbound.db`
  (`content.grant_consume.grant_id == grant-standup-001`, `.fires == 1`).
- The central `task_grants` row's `fires_consumed` is **1** after delivery
  applied the consume (proves the runner→delivery→central writeback closed).
- **No** outbound row carries the `autonomous (heartbeat/scheduled) turn` deny
  (the gate opened rather than blocking).
- The scheduled wake inbound is marked `completed` and the round-2 report is
  delivered through the cli adapter (the turn is not a silent no-op).
