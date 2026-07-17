# M21 Wave 1 — recovery coverage map (X-rider)

The Wave-1 X-rider's honest inventory: every Wave-1 acceptance behavior
mapped to the test that pins it (existing card tests or the two new
replay-registered ones), plus an explicit list of what is NOT
fixture-reachable and precisely why. "Fixture" below means the replay
suite (`crates/copperclaw-host/tests/replay.rs` over `fixtures/**`);
"crate test" means a unit/integration test inside the implementing
card's crate.

New in this rider:

- `fixtures/cli/stuck-tool-restart/` + test
  `cli_stuck_tool_restart_delivers_single_apology_end_to_end`
- `fixtures/cli/delivery-retry-restart/` + test
  `cli_delivery_retry_restart_resumes_and_dead_letters_once`
- Harness extension `ReplayHarness::restart_delivery`
  (`crates/copperclaw-host/tests/replay/harness.rs`) — kill/recreate the
  `DeliveryService` (+ fresh `MockAdapter`s) over the same central DB and
  per-session files, the seam S3's report asked this rider to land.

## Behavior -> pinning test

### S2: hung tool -> `StuckRestart` -> single apology

| Behavior | Pinned by |
|---|---|
| Detection: past the 30-min `ABSOLUTE_CEILING_MS` (unconditionally) and past a declared timeout at the 60s claim floor | crate tests in `crates/copperclaw-host-sweep/src/checks/stuck.rs` + `service.rs` |
| Actuator fires only past the ceiling; claim-threshold detections stay observe-only | S2 crate tests (sweep + `crates/copperclaw-host/src/container_manager/stuck_actuator.rs`) |
| End to end on the mock runtime: detection -> `StuckRestart` within one sweep pass -> apology row once (deduped) -> next inbound spawns after backoff | `stuck_actuator.rs::hung_tool_past_ceiling_is_restarted_within_one_sweep_pass` |
| Stale detections are quiet no-ops (session stopped / deleted) | `stuck_actuator.rs::restart_stuck_is_noop_when_container_not_running`, `::restart_stuck_is_noop_when_session_missing` |
| Repeated stuck restarts walk the S4 backoff (no hot-loop on sweep cadence) | `stuck_actuator.rs::repeated_stuck_restarts_escalate_the_backoff` |
| **The apology reaches the WIRE through the real `DeliveryService`, exactly once; second sweep+delivery pass byte-quiet; honest (pre-O4) apology copy** | **NEW** `replay.rs::cli_stuck_tool_restart_delivers_single_apology_end_to_end` over `fixtures/cli/stuck-tool-restart/` |

### S3: delivery retry state survives restart, exactly-once dead-letter

| Behavior | Pinned by |
|---|---|
| `bump_retry` writes through to the row; priming honors persisted `not_before`; counters round-trip | crate tests in `crates/copperclaw-host-delivery/src/service.rs` (`bump_retry_writes_through_to_the_row`, `priming_honors_persisted_not_before`) |
| Restart mid-retry resumes at the persisted count; exhaustion -> one `delivered{status=failed}` + one ErrorCard | `service.rs::restart_resumes_persisted_attempt_count`, `::persisted_exhaustion_dead_letters_without_a_fresh_attempt` |
| Fresh-install and migrated-install schemas agree (migration 029) | `crates/copperclaw-db/src/migrate.rs::migration_029_adds_retry_columns_with_defaults`, `::session_outbound_fresh_and_migrated_schemas_agree` |
| Live-service backoff deferral honoring `Rate { retry_after }` | fixture `telegram/rate-limited-retry` |
| **Pipeline-level: runner-produced row, a service restart before EVERY attempt, budget exhausted across three service lifetimes, failure card delivered to the adapter exactly once, terminal state stable across a further restart** | **NEW** `replay.rs::cli_delivery_retry_restart_resumes_and_dead_letters_once` over `fixtures/cli/delivery-retry-restart/` (uses `ReplayHarness::restart_delivery`) |

### S4: OOM classification + backoff (unit-harness level, per the card)

All landed with S4; nothing duplicated here — verified present and
mapped:

| Behavior | Pinned by |
|---|---|
| Exit-137 / `State.OOMKilled` classifies as `OomKill`, distinct from generic | `crates/copperclaw-host/src/container_manager/crash_loop.rs::crash_cause_classification_matches_decision_e` (+ `crash_cause_tokens_are_stable`) |
| Backoff curve 5s -> 15s -> 60s -> 300s cap; healthy 10-min reset | `crash_loop.rs::backoff_walks_the_curve_and_caps`, `::spawn_delay_remaining_tracks_the_window`, `::healthy_window_resets_the_streak_and_episode` (paused clock) |
| 3rd OOM in an episode -> exactly one ErrorCard; generic crashes never card; per-session independence | `crash_loop.rs::oom_threshold_emits_exactly_one_card_per_episode`, `::generic_crashes_never_emit_the_oom_card`, `::mixed_causes_count_only_ooms_toward_the_card`, `::sessions_are_independent` |
| Mock-runtime integration: crash-looping container respawned at increasing intervals; OOM card once per episode; card skipped without chat routing | `crates/copperclaw-host/src/container_manager/classify.rs::oom_crash_loop_emits_error_card_exactly_once_per_episode`, `::generic_crash_loop_never_emits_oom_card`, `::oom_card_skipped_without_chat_routing` |

### S1: loop panic -> supervisor restart

| Behavior | Pinned by |
|---|---|
| Integration: panic-injected loop resumes within one backoff step; `host.status` over the REAL admin socket reports the restart; shutdown still drains | `crates/copperclaw-host/tests/supervisor.rs::panic_injected_loop_resumes_and_status_handler_reports_the_restart` |
| Backoff curve / degraded flag at cap / healthy reset / unexpected-return restart / shutdown semantics | unit tests in `crates/copperclaw-host/src/supervisor.rs` (`backoff_walks_the_curve_and_flips_degraded_past_the_cap`, `crash_looping_task_degrades_and_keeps_retrying_at_the_cap`, `panicking_loop_restarts_on_the_backoff_curve`, `unexpected_return_is_also_restarted`, `shutdown_drains_all_loops_without_counting_restarts`, ...) |

### S5: no-adapter outbound expiry (adjacent Wave-1 surface, mapped for completeness)

| Behavior | Pinned by |
|---|---|
| 24h ceiling math; below-ceiling rows untouched; expiry -> central `outbound_dropped_messages` with reason `no_adapter` -> replay once the adapter exists; backlog gauge; UI-chrome kinds skip the dead-letter | crate tests in `crates/copperclaw-host-delivery/src/service.rs` (`no_adapter_ceiling_math`, `no_adapter_rows_below_ceiling_stay_pending`, `no_adapter_rows_expire_into_dead_letters_and_replay`, `no_adapter_backlog_counts_and_clears`, `no_adapter_expiry_of_ui_chrome_kind_skips_dead_letter`) |

### S6: runner test-clock seam (the rider's enabling dependency)

| Behavior | Pinned by |
|---|---|
| The formerly-unfixturable HUD StatusRows 60s first-fire + 150s softening | fixture `cli/status-row-heartbeat` (`replay.rs::cli_status_row_heartbeat_pins_60s_and_150s_legs`) |
| Clock semantics (frozen unless advanced; shared across clones; real clock at default) | `crates/copperclaw-runner/src/clock.rs` unit tests |

## NOT fixture-reachable — and why

Documented, not silently skipped (per the card):

1. **A genuinely hanging tool inside a replay fixture.** The harness
   runs each per-step runner to completion before its delivery pass; a
   scripted turn cannot leave a tool mid-flight. And the sweep's stuck
   detection compares persisted `tool_started_at` against wall-clock
   `Utc::now()` on the HOST side — the S6 `TestClock` is injected into
   the RUNNER's `RunnerDeps.clock` only, so neither `advance_clock_ms`
   nor `ReplayHarness::advance_clock` moves the sweep's notion of time.
   The new stuck-tool test therefore seeds the persisted mid-hang state
   (exactly the rows a wedged runner leaves behind) and drives the real
   detection/actuation/delivery machinery over it.
2. **Host-side timers in general (sweep cadence, S4 crash-loop backoff,
   S1 supervisor backoff).** These run on `tokio::time` / wall clock in
   the HOST process, not the runner `TestClock`. The replay harness has
   no paused-clock mode (`run()` performs real `sleep`s, and wiremock /
   MockServer do real I/O that a globally paused clock would wedge).
   They are pinned instead by `#[tokio::test(start_paused = true)]`
   crate tests: `crash_loop.rs` + `stuck_actuator.rs` (S4/S2 backoff),
   `supervisor.rs` (S1), where the paused clock is native.
3. **Loop-panic -> supervisor restart as a fixture.** The replay harness
   deliberately does not boot the supervised host loops at all — it
   drives `Router` / runner / `DeliveryService` directly (see
   `harness.rs` module docs), so there is no supervised task to
   panic-inject. The S1 integration test builds the real `Supervisor` +
   real admin socket instead, which is the honest equivalent
   (`tests/supervisor.rs`).
4. **OOM classification through a fixture.** Classification consumes a
   container runtime `exit_status` inspection (`State.OOMKilled` / exit
   137) at `CrashRestart` capture; replay fixtures never spawn real
   containers, and the harness's in-process runner has no exit status
   to inspect. The mock-runtime classify/crash-loop tests inject the
   exit status directly — the same seam production reads.
5. **A delivery-service restart driven purely by fixture manifest.** The
   restart is imperative test-side orchestration (fail, restart, rewind
   window, fail again), not a replayable inbound stream, so it lives in
   the registered test over the new `restart_delivery` harness seam
   rather than in `manifest.json`. The fixture still owns the pipeline
   half: the runner-produced row, the scripted first failure, and the
   byte-stable "nothing delivered" streams.
