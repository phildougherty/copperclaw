# fixtures/cli/grant-wake-ungranted (M22 A2 scaffold)

**Ungranted-propose** wake transcript: a scheduled task fires an *autonomous*
turn with **no matching grant**, so a credentialed-external action stays
**blocked** and the agent falls back to read-then-propose — the safe default
half of the A2 gate.

## What it exercises

- A `kind: task` wake inbound (no human Chat row) → autonomous turn.
- **No `grant.json`** present (the firing task has no live grant), so
  `run/tool_dispatch.rs::invoke_tool` leaves the autonomy block in place: the
  credentialed-external action is denied with the
  `autonomous (heartbeat/scheduled) turn` reason, and the F2 blocker surfaces
  the "Blocked: this needs a person / pre-authorize it" wall card.
- **No** `grant_consume` row is emitted (a blocked action is never charged).
- The agent can still `send_message` to propose the action for a human to
  approve.

## Files

- `inbound/001-wake.json` — the scheduled-fire wake row (`kind: task`).
- `claude/001-turn.json` — the model script: attempt the action (blocked),
  then propose it to the user.
- `manifest.json` — scenario metadata.
- (Deliberately **no** `grant.json`.)

## What the AX X-rider must finish

Same as `grant-wake-granted/README.md` steps 1, 2, 4 (register in
`replay.rs`; seed + fire the task via the M21 test-clock seam; capture
`expected/`). This scenario needs **no** grant plumbing — its whole point is
the absence of a grant.

## Expected outcome (assertion targets for AX)

- The credentialed-external action is **blocked** (`is_error`, reason contains
  `autonomous (heartbeat/scheduled) turn`).
- **Zero** `grant_consume` rows in `outbound.db`.
- A proposal `send_message` (and/or the Autonomous blocker wall card) reaches
  the user — the turn is not a silent no-op.
