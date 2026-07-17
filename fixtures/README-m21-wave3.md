# M21 Wave 3 — operator-surface coverage map (X-rider)

The Wave-3 X-rider's honest inventory, mirroring
`fixtures/README-m21-wave2.md`: every Wave-3 acceptance behavior mapped to
the test that pins it (the implementing cards' own tests, the one new
replay-registered fixture, or the new cross-card integration test), plus an
explicit list of what is NOT fixture-reachable and precisely why.
"Fixture" below means the replay suite
(`crates/copperclaw-host/tests/replay.rs` over `fixtures/**`); "crate test"
means a unit/integration test inside the implementing card's crate.

Wave 3 is an **operator-surface** wave: three of its four cards land on
surfaces the inbound → router → runner → outbound → delivery replay
pipeline does not traverse — `cclaw doctor` is an admin-socket surface (O1);
quarantine is a sweep + filesystem concern (O2); provider selection happens
inside the container's runner against providers the harness does not wire
(O3). Only O4 rides the real delivery pipeline, so it is the one behavior
pinned by a new end-to-end replay fixture. The rest are pinned at the right
layer and mapped here — honesty over coverage theater.

New in this rider:

- `fixtures/cli/operator-alert-delivery/` + test
  `cli_operator_alert_delivered_and_silent_when_unconfigured`
  (`crates/copperclaw-host/tests/replay.rs`) — O4 enqueue → real
  `DeliveryService` → wire, for a loop-death event, plus the
  secure-by-default silence when unconfigured.
- `crates/copperclaw-host/tests/wave3_quarantine_doctor.rs` —
  `o2_quarantine_artifact_is_read_by_the_o1_doctor_contract_and_excluded_from_sweeps`:
  the O2 → O1 cross-card seam. A real `SweepService` detect → quarantine →
  exclude pass, plus an assertion that the artifact O2 wrote satisfies O1's
  documented `cclaw doctor` reader contract exactly (path convention +
  `detail` key). A neutral crate that sees both lanes; nothing else crosses
  the seam.

## Behavior → pinning test

### O1: `cclaw doctor` learns the Wave-1 failure modes (six new rows, healthy + failing)

| Behavior | Pinned by |
|---|---|
| `container-runtime` row: reachable → OK; unreachable → FAIL naming `docker info` | crate test `crates/copperclaw-cclaw/src/lib.rs::tests::container_runtime_check_ok_and_fail` |
| `host-loops` row (S1 supervisor status): all alive → OK; a dead loop OR the supervisor-wide degraded flag → FAIL naming `copperclaw stop && copperclaw start` | `lib.rs::tests::host_loops_check_ok_dead_and_degraded` |
| `stuck-sessions` row: fresh heartbeat → OK; stale past the ceiling → FAIL naming a real recovery command; a running session with no heartbeat is not cried wolf on | `lib.rs::tests::stuck_sessions_check_fresh_and_stale`, `::stuck_sessions_missing_heartbeat_is_not_counted` |
| `provider-chain` row: healthy chain → OK; empty/no chain → WARN with the `set-chain` config pointer (not FAIL); every entry cooling → FAIL; a lapsed cooldown re-probe-eligible → healthy again; empty state set → row skipped | `lib.rs::tests::provider_chain_states_and_row`, `::provider_chain_check_empty_states_is_skipped` |
| `dead-letter` row: empty backlog → OK; backlog → FAIL naming `cclaw dropped-messages replay` and counting `no_adapter` rows | `lib.rs::tests::dead_letter_check_empty_and_backlog` |
| `db-integrity` row: clean → OK; a quarantine sidecar → FAIL surfacing the detail + `cclaw sessions delete`; a corrupt central DB takes precedence and names `cclaw db restore` | `lib.rs::tests::db_integrity_check_clean_quarantine_and_corrupt_central` |
| Full flow: every new FAIL/WARN row appears with its `fix:` line at once; existing rows byte-identical | `lib.rs::tests::doctor_full_flow_surfaces_all_new_failing_rows` (+ the existing-output regression tests around it) |

### O2: find corruption instead of skipping it (quarantine sidecar → doctor FAIL → sweep exclusion)

| Behavior | Pinned by |
|---|---|
| Rotation covers every session within N passes; a healthy DB pays one `quick_check` per rotation, not per pass; slot stable | crate tests `crates/copperclaw-host-sweep/src/checks/integrity.rs::rotation_slot_is_stable_and_in_range`, `::rotation_covers_every_session_within_n_passes`; `service.rs::integrity_rotation_checks_each_healthy_session_once_per_rotation` |
| A corrupt DB is detected, quarantined (sibling `.quarantined` sidecar of the documented shape), and excluded from sweeps; healthy/missing DBs are not | `integrity.rs::corrupt_db_is_detected_and_quarantined`, `::healthy_session_is_not_quarantined`, `::missing_dbs_are_not_corruption`; `service.rs::corrupt_session_is_detected_quarantined_and_excluded` |
| Quarantine survives a host restart (it is a file) | `service.rs::quarantine_survives_a_restart` |
| **Security:** the marker lives OUTSIDE the container-writable session dir (a sibling in the host-only agent-group dir); a marker the agent forges inside its own `/data` is NOT honored | `integrity.rs::quarantine_marker_lives_outside_the_container_writable_session_dir` |
| Central-DB integrity at boot + daily; corruption is surfaced (not quarantined — unrecoverable) | `service.rs::central_integrity_runs_at_boot_then_daily` |
| A quarantine sidecar → `cclaw doctor` `db-integrity` FAIL (reader side, against the documented on-disk contract) | O1's `lib.rs::tests::db_integrity_check_clean_quarantine_and_corrupt_central` + `::doctor_full_flow_surfaces_all_new_failing_rows` |
| **Cross-card end-to-end (NEW):** a REAL `SweepService` pass detects → quarantines → excludes a genuinely-corrupted `outbound.db`, and the artifact it produces satisfies O1's documented reader contract exactly — sibling path `sessions/<ag>/<session_uuid>.quarantined`, the `detail` key O1 surfaces — reproducing O1's scan over O2's real bytes | **NEW** `crates/copperclaw-host/tests/wave3_quarantine_doctor.rs::o2_quarantine_artifact_is_read_by_the_o1_doctor_contract_and_excluded_from_sweeps` |

### O3: live provider failover, mid-session (mock provider)

| Behavior | Pinned by |
|---|---|
| Live health selection: single-candidate always starts at 0 (byte-identical no-failover path); healthy chain starts on the primary; a dead primary moves the start to the fallback; a recovered primary is restored after the re-probe window OR an explicit success; transitions noted only on real switches; same `(provider, model)` candidates track health independently | crate tests `crates/copperclaw-runner/src/run/failover_health.rs` (`single_candidate_always_starts_at_zero`, `healthy_chain_starts_on_primary`, `dead_primary_moves_start_to_fallback`, `recovered_primary_is_restored_after_reprobe_window`, `record_success_restores_primary_immediately`, `enter_candidate_notes_only_real_transitions`, `same_provider_model_candidates_do_not_collide`) |
| Wired into real provider calls (mock providers): a mid-turn failover serves off the fallback and emits the "switched to <provider>" HUD note + per-entry usage reports; whole-chain exhaustion still yields the terminal failure; an empty chain is byte-stable single-provider; a dead primary is SKIPPED on the next call WITHOUT a respawn (mid-session); a recovered primary is restored with the note | crate tests `crates/copperclaw-runner/src/run/provider_call.rs` (`failover_switches_to_next_entry_and_completes`, `whole_chain_exhaustion_still_fails`, `empty_chain_is_single_provider_behaviour`, `dead_primary_is_skipped_on_next_call`, `recovered_primary_is_restored_after_reprobe_window`) |

### O4: opt-in operator alert destination (enqueue → delivery; silence when unconfigured)

| Behavior | Pinned by |
|---|---|
| Disabled by default → zero new outbound; env parse yields a disabled instance when unset | crate tests `crates/copperclaw-host/src/operator_alerts.rs::disabled_by_default_produces_zero_outbound`, `::unconfigured_from_env_is_disabled` |
| Configured → exactly one alert row to the destination with the severity-prefixed body | `operator_alerts.rs::configured_enqueues_exactly_one_row_to_the_destination` |
| Dedup (one alert per episode, not per sweep pass); re-alert after the window; distinct keys each alert once | `operator_alerts.rs::dedup_collapses_repeat_fires_within_the_window`, `::dedup_re_alerts_after_the_window_elapses`, `::distinct_keys_each_alert_once` |
| Global rate-limit token cap bounds a burst; tokens recover after the window | `operator_alerts.rs::rate_limit_caps_a_burst_of_distinct_keys`, `::rate_limit_tokens_recover_after_the_window` |
| Fail-closed: no carrier session never panics; severity tokens stable | `operator_alerts.rs::no_active_session_fails_closed_without_panicking`, `::severity_tokens_and_parse_are_stable` |
| Loop permanent-failure (S1 seam) → the degraded-watch fires exactly one alert per episode on the flip | `operator_alerts.rs::degraded_watch_fires_exactly_one_alert_on_the_flip` |
| Quarantine (O2) → one operator alert per finding; no sink wired → no-op | `crates/copperclaw-host-sweep/src/service.rs::quarantine_fires_one_operator_alert_per_finding`, `::quarantine_without_sink_is_a_noop` |
| Apology copy cross-wire: honest "tell your operator" when no destination; the true "the operator has been notified" restored only when a destination is enabled | `crates/copperclaw-host-sweep/src/checks/apology.rs::apology_copy_does_not_claim_operator_notification`, `::emitted_error_card_claims_notification_when_operator_alerts_enabled` |
| **Enqueue → delivery end-to-end (NEW):** a loop-death event's alert row, enqueued by the REAL `OperatorAlerts` via the S1 degraded-watch, reaches the cli `MockAdapter` through the REAL `DeliveryService` at the operator's OWN configured target (not the chat's), exactly once (a second pass is quiet); a DISABLED instance firing the same event produces zero new outbound and nothing on the wire | **NEW** `replay.rs::cli_operator_alert_delivered_and_silent_when_unconfigured` over `fixtures/cli/operator-alert-delivery/` |

## NOT fixture-reachable — and why

Documented, not silently skipped (per the card):

1. **The six `cclaw doctor` rows through a replay fixture.** `doctor` is a
   `cclaw` admin-socket / filesystem surface — it queries `host.status`,
   the central DB, the dropped-messages tables, and the session tree; none
   of its inputs is produced by driving an inbound message through the
   pipeline the replay harness runs. Each row is pinned instead by O1's
   own healthy + failing crate cases (table above) and the full-flow test,
   which is the honest layer for a pull-only diagnostic surface.
2. **O2 quarantine driven by a fixture manifest.** Quarantine needs a
   genuinely corrupt on-disk `quick_check` failure plus a rotating
   `SweepService` pass — neither expressible as `inbound/*.json` steps, and
   the harness runs each per-step runner to completion (it never leaves a
   corrupt DB behind). It is pinned by O2's service/unit tests and, for the
   cross-card link, by the new `wave3_quarantine_doctor.rs` integration
   test driving the REAL `SweepService`.
3. **The `cclaw doctor` COMMAND run over a real O2 sidecar, cross-crate.**
   O1's doctor data-root + runtime overrides (`set_data_root_override` /
   `set_runtime_override`) are `#[cfg(test)]` — internal to
   `copperclaw-cclaw`, not reachable from another crate's integration test;
   `run_cli(["cclaw","doctor"])` without them resolves the real install
   root and probes real docker. So the reader → FAIL/`fix:` mapping stays
   pinned by O1's own unit test (`db_integrity_check_clean_quarantine_and_
   corrupt_central`). The new cross-card test reproduces O1's DOCUMENTED
   scan (`scan_quarantined_sessions`: sibling `<uuid>.quarantined`, the
   `detail` key) over O2's real artifact — closing the writer↔reader
   contract gap the two hand-written literals would otherwise leave open.
4. **O3 mid-session failover through a replay fixture.** Provider selection
   runs inside the container's runner. The replay harness wires a SINGLE
   `AnthropicProvider` against one wiremock endpoint with an empty
   `failover_chain`, so there is no second provider to fall over to and no
   way to script a mid-session provider death the `FailoverHealth` would
   route around — and the point of the card (live health re-consulted at
   call construction) is a runner-internal property, not a pipeline
   ordering one. Pinned by the mock-provider unit tests in
   `failover_health.rs` + `provider_call.rs`, which drive real
   `run_llm_turn` calls over scripted-failure providers — the honest
   equivalent.
5. **O4's host-side fire semantics by fixture clock.** The dedup
   ([`RE_ALERT_WINDOW`]) and rate-limit ([`RATE_LIMIT_WINDOW`]) windows run
   on `tokio::time::Instant` — HOST time the S6 runner `TestClock` cannot
   reach (`fixtures/README-m21-wave1.md`, reachability item 2). The new
   fixture therefore covers only the single-fire enqueue → delivery wire
   leg; the window semantics are pinned by O4's paused-clock unit tests
   (`#[tokio::test(start_paused = true)]`).
6. **The real DB-corruption cause and central-DB restore.** What actually
   corrupts a per-session DB (a torn bind-mount write under container load)
   is outside the replay suite's scope — it never spawns real containers
   (`docs/replay-fixtures.md`, "what the suite does not cover"). The tests
   inject the corrupt bytes directly at the same file the runtime writes;
   the central-DB `quick_check` + "restore from backup" path is pinned by
   host-sweep (`central_integrity_runs_at_boot_then_daily`) and O1 (the
   corrupt-central row).
