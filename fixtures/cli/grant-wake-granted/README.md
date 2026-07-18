# fixtures/cli/grant-wake-granted (M22 A2 scaffold)

**Granted-act** wake transcript: a scheduled task fires an *autonomous* turn,
its firing task carries a live, human-approved capability grant, and the agent
**takes the pre-authorized credentialed-external action** instead of only
drafting it — the A2 marquee ("the agent does the thing it was told it could
do").

## What it exercises

- A `kind: task` wake inbound (no human Chat row) → the runner classifies the
  turn autonomous (`run/mod.rs`).
- The firing task's grant snapshot (`grant.json`, below) permits the action
  the turn takes (`web_fetch`; the same shape applies to an external-messaging
  MCP tool with a `mcp:<server>` grant — the "send the granted message"
  framing).
- `run/tool_dispatch.rs::invoke_tool` consults the grant, opens the autonomy
  gate **for that action only**, dispatches it, and charges **one fire**
  (`grant_consume` System row to `outbound.db`).

## Files

- `grant.json` — the host-written effective-grant snapshot the runner reads at
  turn start (`<data_root>/grant.json`). Shape = `run::TurnGrant`
  (`grant_id`, `task_id`, `capability_scope`, `tokens_remaining`,
  `fires_remaining`, `expires_at`). Matches the `task_grants` row the companion
  host writer would produce (see below).
- `inbound/001-wake.json` — the scheduled-fire wake row (`kind: task`,
  `content.task_id = t-standup`).
- `claude/001-turn.json` — the model script: take the granted action, then
  report success.
- `manifest.json` — scenario metadata.

## What the AX X-rider must finish

1. **Register** this fixture in `crates/copperclaw-host/tests/replay.rs` (a
   `#[tokio::test]` calling `run_fixture("cli", "grant-wake-granted")`).
2. **Task-fire injection**: a scheduled fire is synthesized by the sweep
   directly into `messages_in` (it bypasses the router), so the harness must
   seed the `tasks` row + fire it (reuse the M21 runner test-clock seam for
   croner timing) rather than route `inbound/001-wake.json` as a channel event.
3. **Grant plumbing**: copy `grant.json` into the session data root before the
   turn runs (this stands in for the companion host writer described in
   `docs/plans/m22-security-reviews.md` §A2 — the writer that snapshots
   `task_grants::effective_grant` to `<session>/grant.json` at fire/spawn time
   and applies the runner's `grant_consume` rows back to central).
4. Capture `expected/` (messages-out etc.).

## Expected outcome (assertion targets for AX)

- The granted `web_fetch` (or `mcp__<server>__*`) action **runs** — no
  `autonomous (heartbeat/scheduled) turn` deny.
- Exactly **one** `grant_consume` System row is emitted to `outbound.db`
  (`content.grant_consume.fires == 1`).
