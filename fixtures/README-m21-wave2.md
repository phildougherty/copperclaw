# M21 Wave 2 — feedback coverage map (X-rider)

The Wave-2 X-rider's honest inventory, mirroring
`fixtures/README-m21-wave1.md`: every Wave-2 acceptance behavior mapped
to the test that pins it (existing card tests, the F2 fixture that
already shipped with its card, or the two new replay-registered ones),
plus an explicit list of what is NOT fixture-reachable and precisely
why. "Fixture" below means the replay suite
(`crates/copperclaw-host/tests/replay.rs` over `fixtures/**`); "crate
test" means a unit/integration test inside the implementing card's
crate.

New in this rider:

- `fixtures/cli/slow-spawn-notice/` + test
  `cli_slow_spawn_typing_and_single_notice_end_to_end`
- `fixtures/cli/restart-recovery-notice/` + test
  `cli_restart_recovery_notice_delivered_exactly_once`
- Harness seams (`crates/copperclaw-host/tests/replay/harness.rs`):
  `ReplayHarness::route_step_cold` — route a fixture step WITHOUT the
  per-step runner, delivery pass, or `mark_container_running`, leaving
  exactly the Stopped-with-due-inbound state a cold container spawn
  sees; and `ReplayHarness::run_turn_and_deliver` — the per-step
  pipeline tail (one runner turn + delivery drain), exposed so a
  registered test can defer it past imperative host-side work (a held
  spawn, a boot-recovery pass).
- Technique: a mid-test `tokio::time::pause()` / `resume()` bracket in
  the slow-spawn test. The Wave-1 finding that the harness "has no
  paused-clock mode" still holds for `run()` as a whole (wiremock I/O
  + real sleeps); what IS safe is pausing around a section that does
  only timer + synchronous-DB work, which the spawn-phase leg is.

F2's fixture (`fixtures/cli/question-expiry/`) shipped with the F2 card
itself and already covers this rider's third behavior end to end; it is
folded into the map below, not duplicated or extended (no gap found
against the card's intent: ask -> expiry -> terminal note -> unblocked
agent -> reply resumes are all pinned).

## Behavior -> pinning test

### F1: cold-start feedback — typing from message one, one slow-spawn notice

| Behavior | Pinned by |
|---|---|
| Registry semantics: begin/finish generations, stale-guard no-op, notice claim once per episode until a successful spawn, no claim for a finished (fast) attempt | crate tests in `crates/copperclaw-host/src/container_manager/cold_start.rs` (`activity_begin_finish_roundtrip_with_generations`, `activity_notice_claim_once_per_episode_until_success`, `activity_claim_requires_in_flight_attempt`) |
| Typing pulses for a mid-spawn session before the runner is up; stops when the attempt ends; quiet without pending inbound; a session both Running and mid-spawn fires once | `cold_start.rs::typing_fires_for_mid_spawn_session_before_runner_is_up` + `typing_ticker.rs` crate tests (`tick_fires_for_mid_spawn_session_and_stops_when_attempt_ends`, `mid_spawn_session_without_pending_inbound_stays_quiet`, `running_session_also_in_spawn_registry_fires_once`) |
| Threshold precision: slow spawn -> exactly one notice; fast spawn -> zero; failing spawn -> apology path untouched; consecutive slow failing attempts share one notice per episode | `cold_start.rs::slow_spawn_emits_exactly_one_notice`, `::fast_spawn_produces_no_notice`, `::failing_spawn_still_feeds_the_apology_path`, `::slow_failing_attempts_notice_once_per_episode` (paused clock) |
| **End to end on the pipeline: router-created session routed COLD, real `maybe_spawn` held mid-runtime-call, real `TypingTicker::run_loop` over the shared registry pulsing typing through the harness delivery service's real dispatcher BEFORE the runner is up, exactly one notice row past 20s (none below, none after 3 more held minutes), the notice reaching the WIRE through the real `DeliveryService` exactly once, and the same pending inbound answered by the released spawn's turn — all four JSONL streams byte-stable** | **NEW** `replay.rs::cli_slow_spawn_typing_and_single_notice_end_to_end` over `fixtures/cli/slow-spawn-notice/` (uses `route_step_cold`, `run_turn_and_deliver`, and the paused-clock bracket) |

### F2: question expiry -> terminal note -> unblocked agent (folded in — shipped with the F2 card)

| Behavior | Pinned by |
|---|---|
| Expiry selection honors the TTL; answered/inside-TTL questions untouched; exactly-once surfacing; no-origin and no-routing degradations; expiry copy house style | crate tests in `crates/copperclaw-host-sweep/src/checks/questions.rs` (`unanswered_question_past_ttl_is_surfaced_out_loud`, `expiry_is_surfaced_exactly_once`, `question_inside_ttl_is_untouched`, `question_answered_by_later_reply_is_resolved_silently`, `module_answered_question_is_never_selected`, `expired_question_without_origin_records_lapse_only`, `expired_question_without_routing_skips_note_but_unblocks_agent`, `expiry_copy_matches_house_style`) + `copperclaw-modules/src/interactive.rs` TTL tests |
| End to end on the pipeline: ask -> question card delivered -> real sweep pass surfaces the lapse once (terminal typed card edit, no live buttons; synthetic `trigger = 0` no-answer result in the inbox) -> second pass byte-quiet -> the user's late reply resumes a normal turn whose provider request carries the expired `ask_user_question_result` | `replay.rs::cli_question_expiry_surfaces_lapse_and_resumes_on_reply` over `fixtures/cli/question-expiry/` (uses `run_steps` + `install_interactive_module`, shipped with F2) |

### F3: host-restart recovery notice, exactly once

| Behavior | Pinned by |
|---|---|
| Liveness gates: turn in flight -> one notice + row stays due + `Spawn` classification (no crash backoff); idle restart -> zero; unclaimed pending inbound -> zero (row unstamped) | crate tests in `crates/copperclaw-host/src/boot.rs` (`tests::boot_recovery::restart_with_turn_in_flight_emits_one_notice_and_requeues`, `::restart_with_no_in_flight_work_emits_no_notice`, `::restart_with_unclaimed_pending_inbound_emits_no_notice`) |
| Dedup: once per session per boot, not per row / pass / sweep | `boot.rs::tests::boot_recovery::notice_fires_once_per_session_not_per_row_pass_or_sweep` |
| **End to end on the pipeline: baseline turn off the real pipeline, host-death-mid-turn state, the REAL boot step (`reset_stale_running_sessions`) emitting one notice (crash-restart copy, pre-O4 honest), a second boot pass AND a real sweep pass byte-quiet (the row is past the `pending_too_long` threshold, so the stamp assertion is load-bearing), the notice reaching the WIRE exactly once, and the re-queued inbound processed normally by the deferred turn — all four JSONL streams byte-stable including the `tries = 99` stamp** | **NEW** `replay.rs::cli_restart_recovery_notice_delivered_exactly_once` over `fixtures/cli/restart-recovery-notice/` (uses `run_turn_and_deliver`) |

### F4: external-MCP connection caching (adjacent Wave-2 surface, mapped for completeness)

| Behavior | Pinned by |
|---|---|
| Cache keying, idle reap, broken-pipe retry-once | crate tests in `crates/copperclaw-mcp/src/external_cache.rs` |
| Stub-server integration: N sequential calls open one connection; server restart mid-session reconnects instead of erroring; first-call behavior byte-identical (success and connect-failure) | `crates/copperclaw-mcp/tests/external_cache_stub.rs` (`n_sequential_calls_open_exactly_one_connection`, `server_restart_mid_session_reconnects_and_retries`, `first_call_success_is_byte_identical_to_uncached`, `first_call_connect_failure_is_byte_identical_to_uncached`) |

## NOT fixture-reachable — and why

Documented, not silently skipped (per the card):

1. **The 20s slow-spawn threshold (or the 4s typing cadence) driven by
   a fixture manifest.** F1's timers run on HOST tokio time; the S6
   `TestClock` that `advance_clock_ms` / `ReplayHarness::advance_clock`
   move is injected into the RUNNER's `RunnerDeps.clock` only
   (`fixtures/README-m21-wave1.md`, reachability item 2). The
   registered test crosses them instead with a mid-test
   `tokio::time::pause()` bracket around the spawn-phase leg — safe
   there because that leg is pure timer + synchronous-DB work, unlike
   `run()` as a whole (wiremock I/O + real sleeps), which still has no
   paused-clock mode.
2. **A real image build/pull as the slow-spawn cause.** Replay
   fixtures never spawn real containers; the held `ContainerRuntime`
   (`HoldSpawnRuntime`, blocking in `spawn`) stands in at the same
   seam production's Docker runtime occupies. What 20 slow seconds are
   SPENT on is a container-rt concern outside the replay suite's
   scope (`docs/replay-fixtures.md`, "what the suite does not cover").
3. **Killing and re-running the host process for F3.** The replay
   harness deliberately does not run `run_host`'s boot sequence (it
   drives `Router` / runner / `DeliveryService` directly — harness
   module docs), so "restart the host" cannot be expressed as fixture
   steps. The registered test calls the extracted boot step
   (`boot::reset_stale_running_sessions` — the same public function
   `run_host` step 9b calls) over the harness's real DBs, and pins the
   double-boot case by calling it twice. Whether `run_host` actually
   invokes that step at boot is production wiring pinned by F3's own
   boot tests.
4. **The 24h question TTL by fixture clock.** Same host-time
   reachability limit; the F2 registered test builds the
   `InteractiveModule` handle with a ZERO TTL so the ask is already
   lapsed when the real sweep pass runs, and TTL-boundary precision
   stays with the paused-clock crate tests (carried over from the F2
   card; see `fixtures/cli/question-expiry/README.md`).
5. **`set_typing` observed at the `MockAdapter`.** `MockAdapter`
   records `deliver` / `edit` / `reaction` calls but not `set_typing`,
   and extending it is a production-crate change outside this rider's
   scope. The slow-spawn test observes typing at the dispatcher seam
   instead — a recording wrapper around the harness delivery service's
   REAL dispatcher (the same `Arc<dyn DeliveryDispatcher>` handle boot
   passes the production ticker), which still forwards into the real
   adapter path, so nothing is stubbed out of the traversal.
6. **F4 through a replay fixture.** External-MCP connections are
   opened by the RUNNER inside the container against servers named in
   the group's MCP config; the replay harness's in-process runner
   wires no external servers (and the point of the card — connection
   reuse across calls — is a transport property, not a pipeline
   ordering property). The stub-server integration tests in
   `copperclaw-mcp` exercise the real client end to end over a real
   socket, which is the honest equivalent.
