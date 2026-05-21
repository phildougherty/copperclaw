# cli/scheduled-wake

Exercises the **due-message wake check** in `SweepService::run_once`.

`central.sql` seeds:

- Agent group + cli/stdin wiring (standard).
- A pre-existing `sessions` row in `container_status='idle'`.

`inbound.sql` seeds the session's per-session `inbound.db`:

- One `messages_in` row with `kind='task'`, `process_after =
  2026-01-01T00:00:00Z` (long in the past), and `trigger = 1`.
- `session_routing` so the runner-emitted reply has a destination.

`manifest.json` sets `trigger_sweep: true`. The harness then:

1. Calls `SweepService::run_once()`. The wake check sees a pending,
   due-now inbound row in an idle session and transitions
   `container_status='running'`. The sweep report's
   `woken_sessions` lists the session id.
2. For each woken session the harness runs one in-process runner
   turn against the mocked Anthropic endpoint, which serves the
   recorded "Scheduled task ran." reply.
3. The reply lands in `messages_out`, the delivery loop fans it
   through the cli adapter, and the original inbound is marked
   `status='completed'`.

The fixture deliberately avoids any inbound JSON file under `inbound/`:
the wake check is the only trigger.
