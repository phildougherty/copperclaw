# cli/operator-alert-delivery

M21 O4 (Wave-3 X-rider): the opt-in operator-alert destination, pinned
end to end — a loop-death event's alert row enqueued by the REAL
`OperatorAlerts` and reaching the channel adapter through the REAL
`DeliveryService`, exactly once, routed to the operator's own configured
target; AND the secure-by-default silence when no destination is
configured. Registered as
`cli_operator_alert_delivered_and_silent_when_unconfigured` in
`crates/copperclaw-host/tests/replay.rs`.

## What runs

1. The fixture drives one normal turn ("how are things looking?" ->
   "All systems nominal."), so the session, its routing, and the first
   delivery are produced by the REAL inbound -> router -> runner ->
   outbound -> delivery pipeline. The four expected streams pin that
   baseline byte-for-byte. Its only structural job is to leave exactly
   one `Active` session — the carrier `OperatorAlerts::pick_carrier`
   selects for the alert row.
2. **Silence when unconfigured (secure-by-default).** The registered
   test builds a DISABLED `OperatorAlerts` (no destination) and fires a
   loop-death alert. A delivery pass follows: zero new outbound rows,
   zero deliveries to the operator target. This is the pre-O4 world —
   log + metric only, no new outward message.
3. **Enqueue -> delivery for a loop-death event.** The test then builds
   a CONFIGURED `OperatorAlerts` whose destination is the cli channel at
   a distinct operator target (`operator-cli`, not the chat's `stdin`),
   and drives the real S1 permanent-failure seam
   (`OperatorAlerts::run_degraded_watch`) by flipping the supervisor
   `degraded` watch to `true` — the loop-death signal. Exactly one alert
   row is enqueued into the carrier session's `outbound.db`, carrying its
   OWN routing (`channel_type=cli`, `platform_id=operator-cli`).
4. A real `DeliveryService::process_session_once` pass hands that row to
   the cli `MockAdapter` — the leg O4's own unit tests
   (`operator_alerts.rs`) stop short of: they assert the row is
   ENQUEUED, not that it reaches the wire. `DeliveryService::resolve_target`
   routes on the row's own fields, so the alert lands at `operator-cli`
   regardless of which session physically carried it.
5. A second delivery pass is byte-quiet: the alert is delivered exactly
   once.

## What the test asserts

- Baseline: all four fixture streams clean.
- Disabled: zero alert rows in the carrier `outbound.db`, zero
  `operator-cli` deliveries.
- Configured: exactly ONE alert row (channel `cli`, platform
  `operator-cli`, kind chat, body prefixed `[copperclaw critical]` and
  carrying the loop-death copy); exactly ONE such delivery on the cli
  `MockAdapter`; a second delivery pass adds nothing.
- The baseline chat reply stayed on `stdin` — the alert's own routing,
  not the session's, chose the operator target.

## Why the alert path is driven by the test, not the fixture manifest

`OperatorAlerts` is a HOST component fired from host-side call sites
(the S1 supervisor degraded-watch, S4 crash/OOM, O2 quarantine,
spawn-failure streaks) — none of which is an inbound message the replay
manifest can script. The dedup/rate-limit windows also run on
`tokio::time::Instant`, host time the fixture clock cannot reach. So the
alert is fired imperatively over the harness's REAL `DeliveryService`
and central DB, on top of the fixture's byte-stable baseline. The
enqueue-side semantics (dedup, rate-limit, disabled-default, the
degraded-watch fire-once) are pinned exhaustively by O4's own unit tests
in `crates/copperclaw-host/src/operator_alerts.rs`; this fixture adds
the one leg they omit — the row actually reaching the wire.
