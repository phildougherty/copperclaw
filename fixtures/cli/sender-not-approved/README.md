# cli/sender-not-approved

Exercises the **sender-approval gate**.

A single inbound event from `cli:stranger` (not in `users`) reaches the
router. With the `approvals` gate wired:

1. The router's sender-scope gate consults the central `users` table
   via the persistent lookup, misses, and returns `Pending`.
2. The approvals module's new-pending notifier dispatches an in-channel
   "approve this sender?" message through the delivery dispatcher.
3. The router returns `RouteOutcome::Pending` — no `messages_in` row is
   written, no session is created, the runner is never invoked.

The fixture asserts:

- `inbound-events.jsonl` — the raw inbound event replayed by the
  harness.
- `messages-in.jsonl` — empty (no inbound row was written).
- `messages-out.jsonl` — empty (no outbound row was written;
  notification is dispatched directly through the adapter, not via
  `messages_out`).
- `delivered.jsonl` — one chat-kind message routed back to `cli/stdin`
  asking the operator to approve the unknown sender.

See also `crates/copperclaw-host-router/src/route.rs` (`PendingReason::
SenderUnregistered`) and `crates/copperclaw-modules/src/approvals.rs`.
