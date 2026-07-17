# cli/slow-spawn-notice

M21 F1 (Wave-2 X-rider): cold-start feedback pinned end to end — a
first message to a fresh session pulses typing while the container
spawn is still in flight (before the runner is up), crossing the 20s
`SLOW_SPAWN_NOTICE_AFTER` threshold posts exactly ONE "Setting things
up" notice through the real `DeliveryService` to the channel adapter,
and the released spawn's turn then answers the same pending inbound.
Registered as `cli_slow_spawn_typing_and_single_notice_end_to_end` in
`crates/copperclaw-host/tests/replay.rs`.

## What runs

1. The fixture's inbound is routed COLD via the harness seam
   `ReplayHarness::route_step_cold` — router-created session, routing
   seeded, but no runner, no delivery, and no `mark_container_running`.
   That is exactly the state the container manager's spawn classifier
   sees for a first message: a `Stopped` session with due inbound.
2. The registered test then builds F1's production wiring in miniature:
   one shared `SpawnActivity` registry handed to a real
   `ContainerManager` (over a runtime whose `spawn` blocks until
   released — the mock stand-in for a first image build/pull) and to
   the real `TypingTicker`, whose dispatcher is the harness delivery
   service's REAL dispatcher wrapped in a recording shim (the
   `MockAdapter` does not record `set_typing`, so the dispatcher seam
   — the same handle boot passes the production ticker — is the
   closest observable point that still runs the real adapter path).
3. Inside a mid-test `tokio::time::pause()` section (see "Why the
   clock is paused test-side" below): `maybe_spawn` is held
   mid-runtime-call; within one 4s tick the `run_loop` ticker pulses
   typing at the session's routed cli/stdin target while the session
   is still `Stopped`; below 20s there is no notice; crossing the
   threshold posts exactly one notice row; three more held minutes add
   none (episode dedup).
4. The spawn is released and completes; the clock resumes; the notice
   reaches the cli `MockAdapter` exactly once through the real
   delivery service; the deferred fixture turn
   (`ReplayHarness::run_turn_and_deliver`) answers the same inbound;
   a further delivery pass adds nothing. The four expected streams pin
   the whole run byte-for-byte — including the notice row (seq 1,
   written before the turn's rows) and the wire order (notice first,
   reply second).

## Why the clock is paused test-side, not manifest-driven

F1's timers — the 20s slow-spawn watchdog and the ticker's 4s cadence —
run on HOST tokio time. The S6 `TestClock` a fixture manifest can
advance reaches only the RUNNER's `RunnerDeps.clock`
(`fixtures/README-m21-wave1.md`, reachability item 2), so no
declarative fixture step can cross the threshold. The registered test
instead brackets the spawn-phase leg in `tokio::time::pause()` /
`resume()`: everything inside the bracket is pure timer + synchronous
DB work (no wiremock/provider I/O happens until after resume), so the
paused clock auto-advances deterministically with zero real waits.
Threshold-boundary precision and the fast-spawn / failing-spawn arms
stay pinned by F1's own paused-clock crate tests in
`crates/copperclaw-host/src/container_manager/cold_start.rs` and
`crates/copperclaw-host/src/typing_ticker.rs`.

Regenerate expected streams:
`COPPERCLAW_M21X2_GENERATE=1 cargo test -p copperclaw-host --test replay cli_slow_spawn -- --nocapture`
