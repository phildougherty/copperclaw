# cli/restart-recovery-notice

M21 F3 (Wave-2 X-rider): the host-restart recovery notice pinned end
to end — a turn interrupted by a host death gets exactly one recovery
notice (the crash-restart copy) through the REAL boot step and the
real `DeliveryService`, and the interrupted inbound then processes
normally. Registered as
`cli_restart_recovery_notice_delivered_exactly_once` in
`crates/copperclaw-host/tests/replay.rs`.

## What runs

1. The fixture drives one normal baseline turn ("start compiling the
   weekly report" -> "On it — compiling the weekly report now."), so
   the session, its routing, and the first delivery come off the REAL
   inbound -> router -> runner -> outbound -> delivery pipeline.
2. The registered test then reproduces exactly the state a host death
   mid-turn leaves behind — this is state, not behavior (the in-process
   harness cannot kill and re-exec a host): session `running`, one
   pending inbound OLD enough (10 minutes) that the sweep's
   `pending_too_long` apology WOULD fire were the boot path's dedupe
   stamp absent, and a `Processing` claim (a runner had picked the turn
   up when the host went down).
3. The REAL boot step — `boot::reset_stale_running_sessions`, the same
   function `run_host` calls at step 9b — resets the session to
   `stopped` and emits exactly one recovery notice row, routed at the
   interrupted inbound, flipping the claim to `Failed` and stamping
   the row `tries = 99` while leaving it due.
4. Exactly-once, three ways: a second boot pass (host restarted twice)
   adds nothing; a real `SweepService::run_once` pass adds nothing
   (the stamps keep both sweep apology paths out — meaningful because
   the row is past the 5-minute `pending_too_long` threshold); and
   after delivery hands the notice to the cli `MockAdapter`, a second
   delivery pass re-delivers nothing.
5. The re-queued inbound processes: the deferred turn
   (`ReplayHarness::run_turn_and_deliver`, scripted `claude/002`)
   answers it and the reply is delivered. The four expected streams
   pin the whole run byte-for-byte, including the notice row's exact
   copy (which must NOT claim operator notification pre-O4) and the
   `tries = 99` stamp on the interrupted row.

## Why the restart itself is not fixture-driven

Boot-time recovery runs inside `run_host`'s boot sequence, which the
replay harness deliberately does not execute (it drives `Router` /
runner / `DeliveryService` directly — see the harness module docs).
Killing and re-running a host process is imperative test-side
orchestration, not a replayable inbound stream, so the registered test
calls the extracted boot step over the harness's real DBs instead —
the same function production boot calls, over the same state shape.
The liveness-gate arms (idle restart -> no notice; unclaimed inbound
-> no notice) and the Spawn-classification of the reset session stay
pinned by F3's own crate tests in
`crates/copperclaw-host/src/boot.rs` (`tests::boot_recovery`).

Regenerate expected streams:
`COPPERCLAW_M21X2_GENERATE=1 cargo test -p copperclaw-host --test replay cli_restart_recovery -- --nocapture`
