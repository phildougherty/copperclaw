# fixtures/cli/grant-wake-ungranted (M22 A2 — ungranted-propose)

**Ungranted-propose** wake transcript: a scheduled task fires an *autonomous*
turn with **no matching grant**, so a credentialed-external action stays
**blocked** and the agent falls back to read-then-propose — the safe-default
half of the A2 gate. Driven end-to-end by the AX X-rider.

## What it exercises

- A `kind: task` wake row (`inbound.sql`, no human Chat row) → autonomous turn.
- **No `task_grants` row** for the firing task (`t-report`), so
  `effective_grant` reads `None`, `write_tasks_snapshot` (the A2H writer)
  produces **no `grant.json`**, and `load_turn_grant` returns `None`.
- `run/tool_dispatch.rs::invoke_tool` leaves the autonomy block in place:
  `autonomy_verdict` is `NotGated`, so `policy.rs` layer 4 denies the
  credentialed-external action with the stable
  `autonomous (heartbeat/scheduled) turn` reason — **before** dispatch, so no
  fire is charged.
- **No** `grant_consume` row is emitted (a blocked action is never charged).
- The agent still emits a plain chat reply to **propose** the action for a human
  to approve — the turn is not a silent no-op.

## Files

- `central.sql` — agent group + cli wiring + an idle session. Deliberately **no**
  `task_grants` row (and no `tasks` row is needed — its whole point is the
  absence of a grant).
- `inbound.sql` — the pending `kind:task` wake row (`content.task_id =
  t-report`) + `session_routing` for the proposal reply.
- `claude/001-turn.json` — round 1: attempt the action (blocked).
- `claude/002-turn.json` — round 2: propose it to the user.
- (Deliberately **no** `grant.json`.)

## Assertions (in `tests/replay.rs`)

- **Zero** `grant_consume` rows in `outbound.db` (nothing was charged).
- **Zero** `task_grants` rows in central (the fixture seeds none) — the gate had
  nothing to open against.
- The round-2 proposal reply is delivered through the cli adapter and the wake
  inbound is marked `completed` (blocked ≠ silent no-op).
