# Changelog

All notable changes to Copperclaw are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added (M21 O2: DB integrity — rotating quick_check + quarantine sidecar, 2026-07-17)

- **The sweep now finds DB corruption instead of silently skipping it**
  (M21 O2, decision (f)). New read-only `PRAGMA quick_check` helpers in
  `crates/copperclaw-db/src/integrity.rs` (`quick_check(path)` /
  `quick_check_conn(&conn)` returning `QuickCheckOutcome::{Healthy,
  Missing,Corrupt(detail)}`, additive, no migration) and a new sweep
  check `crates/copperclaw-host-sweep/src/checks/integrity.rs`. The
  central DB is `quick_check`ed at boot and once per day; per-session
  DBs are `quick_check`ed on a rotating subset each pass
  (`INTEGRITY_ROTATION_SLOTS = 60`, keyed on the session UUID's low
  bits), so the whole fleet is covered every 60 passes and a healthy DB
  pays one probe per rotation rather than one per pass.
- **Corrupt per-session DBs are quarantined via an on-disk sidecar that
  survives restarts.** On corruption the check writes
  `<data_root>/sessions/<agent_group_uuid>/<session_uuid>/.quarantined`
  (`QUARANTINE_SIDECAR_NAME`) — a single line of JSON
  (`{"reason":"quick_check","db":"inbound.db|outbound.db","detail":...,
  "detected_at":<rfc3339>}`) — logs ONE escalating ERROR line, and
  excludes the session from all sweep work thereafter (replacing today's
  silent per-pass log-and-swallow in every downstream check). Because the
  marker is a plain file, quarantine survives a host restart with no
  in-memory state to rebuild, and `cclaw doctor` (O1) reads the same
  sidecar to surface it — no new `cclaw` verb. Central-DB corruption is
  not quarantinable (the host depends on it); it is logged at ERROR and
  surfaced in the `SweepReport` for doctor. New `SweepReport` fields:
  `integrity_quarantined`, `integrity_checked`, `integrity_excluded`,
  `central_integrity_checked`, `central_integrity_corrupt`. A new
  `SessionRoot::session_paths` trait method exposes the per-session
  filesystem layout to the check.

### Added (M21 Wave-2 X-rider: feedback fixtures, 2026-07-17)

- **Two new replay fixtures pin the Wave-2 "user is never in the dark"
  behaviors end to end.** `fixtures/cli/slow-spawn-notice/` +
  `cli_slow_spawn_typing_and_single_notice_end_to_end` drives the real
  `ContainerManager::maybe_spawn` over a held-spawn runtime and the real
  `TypingTicker::run_loop` reading the shared `SpawnActivity` registry:
  typing pulses through the production dispatcher before the runner is
  up, exactly one slow-spawn notice crosses the 20s threshold (none
  below it, none on a further three minutes held), and the notice
  reaches the wire once before the deferred turn answers the same
  pending inbound. `fixtures/cli/restart-recovery-notice/` +
  `cli_restart_recovery_notice_delivered_exactly_once` runs the real
  `boot::reset_stale_running_sessions` step and asserts the recovery
  notice is delivered exactly once. New test-only harness seams in
  `crates/copperclaw-host/tests/replay/harness.rs`
  (`ReplayHarness::route_step_cold`, `run_turn_and_deliver`) expose the
  cold-spawn state and the deferred pipeline tail; the slow-spawn test
  brackets its timer-only spawn-phase leg in `tokio::time::pause()`
  since F1's watchdog runs on host tokio time the fixture manifest
  cannot advance. `fixtures/README-m21-wave2.md` is the coverage map
  (behavior to pinning test, plus the not-fixture-reachable list with
  reasons); F2's `fixtures/cli/question-expiry/` is folded in, not
  duplicated.

### Added (M21 F4: external-MCP connection caching, 2026-07-17)

- **External MCP connections are now reused across a session's tool
  calls.** New per-session connection cache
  `crates/copperclaw-mcp/src/external_cache.rs`
  (`McpConnectionCache` / `call_external_tool_cached`): keyed by the
  session scope crossed with a canonical (recursively key-sorted — the
  workspace's `serde_json/preserve_order` makes raw serialization
  key-order-sensitive) fingerprint of the `mcp_servers` entry, so every
  host-proxied external tool call after the first skips the ~1-2s
  connection setup (child-process spawn + MCP handshake for stdio, TCP +
  SSE handshake for HTTP) that users felt as unexplained per-step latency
  in long multi-tool tasks. This is the twice-deferred M17 B1b wish.
  Idle connections are reaped after 5 minutes (`DEFAULT_IDLE_TTL`),
  lazily on each access plus a background sweep; eviction is drop-based
  (rmcp's `DropGuard` cancels the service and kills a stdio child), so
  nothing lingers. Failure behavior is unchanged: a cache-miss first
  call is byte-identical to the old fresh-connect path (same connect,
  same errors, no retry), and only a transport-dead *cached* connection
  (broken pipe / transport closed — e.g. the server restarted) is
  retried exactly once on a fresh connection before erroring; `Timeout`
  and remote/protocol/filter errors are never retried, since the remote
  may already have executed a side-effecting tool. Proven by
  paused-clock unit tests (zero real waits) and a real stdio stub-server
  integration test (`crates/copperclaw-mcp/tests/external_cache_stub.rs`,
  `harness = false` so the MCP stream owns stdout): N sequential calls
  open exactly one connection, and a SIGKILLed server degrades to
  reconnect-and-retry, not an error.

### Changed (M21 F4: external-MCP connection caching, 2026-07-17)

- **The host-proxied external tool-call executor uses the cache.**
  `crates/copperclaw-host-delivery/src/service.rs::execute_mcp_call` now
  calls `copperclaw_mcp::call_external_tool_cached` with the session id
  as the cache scope (declared minimal out-of-lane touch: the per-call
  client construction lived here, not in the runner), so connections are
  reused within a session and never shared across sessions. The uncached
  `copperclaw_mcp::call_external_tool` primitive remains for the
  spawn-time manifest seam and one-shot callers.

### Fixed (M21 F3: host-restart recovery notice, 2026-07-17)

- **A host restart mid-turn is no longer silent.** Boot's reset of stale
  `container_status=running` rows (`crates/copperclaw-host/src/boot.rs`,
  now extracted as `reset_stale_running_sessions`) used to drop an
  in-flight turn with no explanation, ever — unlike the live
  `CrashRestart` path, which apologizes immediately. Each reset session
  now runs through
  `crates/copperclaw-host/src/container_manager/classify.rs::emit_boot_recovery_notice`,
  which reuses the crash-restart apology machinery (same
  `processing_ack` scan, same `CRASH_RESTART_APOLOGY_TEXT` copy — no
  duplicated string, same dedupe stamps): liveness-gated so a clean idle
  restart never fires it (both pending unprocessed inbound AND an
  in-flight `Processing` claim are required), and deduped to exactly one
  notice per affected session per boot — not per inbound row, not per
  repeat pass, and the claim flip + `tries` stamp keep the sweep's
  `pending_too_long` apology and stale-claim reset out afterwards. The
  interrupted inbound stays `pending`, so the respawned runner picks the
  turn back up (no crash-loop backoff applies — a host restart is not a
  container crash).

### Added (M21 F1: cold-start feedback — typing from message one, one slow-spawn notice, 2026-07-17)

- **One slow-spawn notice.** A container spawn attempt that runs past
  ~20s (`SLOW_SPAWN_NOTICE_AFTER`,
  `crates/copperclaw-host/src/container_manager/cold_start.rs`) —
  first-image-build/pull territory — now enqueues exactly one "Setting
  things up — this can take a minute or two on the first message."
  reply through the session's normal outbound path. Deduped per
  cold-start episode: consecutive slow/failing attempts share one
  notice and the flag re-arms only after a successful spawn — never
  periodic, never repeated (the standing "no periodic status messages"
  rejection holds). A failing spawn still feeds the existing
  spawn-attempt-tracker -> sweep-apology path unchanged. This is a
  default-behavior change (a new default-on notice on the slow-spawn
  path); the default-change argument is recorded in the F1 commit/PR.
- **`SpawnActivity` registry**
  (`crates/copperclaw-host/src/container_manager/cold_start.rs`): the
  container manager registers every real spawn attempt (only after the
  pending-inbound, budget, and rate-limit gates, so deferred spawns
  never register) for exactly as long as it runs, generation-tagged so
  stale guards and late watchdogs can never clobber a newer attempt.
  Boot (`crates/copperclaw-host/src/boot.rs`) shares one handle between
  the manager and the typing ticker.

### Changed (M21 F1, 2026-07-17)

- **Typing from message one.** The typing ticker
  (`crates/copperclaw-host/src/typing_ticker.rs`) widens its
  `Running`-only gate to also cover sessions whose container spawn is
  currently in flight (per the M21 decision-(c) architecture): a first
  message to a fresh session now pulses the channel typing indicator
  within one tick — through the whole image-build/boot/handshake window
  — instead of dead air until the runner is up (previously the first
  signal on a slow or failing spawn was the 300-second sweep apology).
  The mid-spawn arm passes the identical pending-inbound / rate-limit
  cooldown / routing gates as the `Running` arm; typing behavior for
  `Running` sessions is byte-identical, and without the registry wired
  (tests, embedders) the ticker behaves exactly as before.
### Added (M21 F2: expire `ask_user_question` out loud, 2026-07-17)

- **Unanswered questions now expire out loud instead of silently
  evaporating.** `InteractiveModule::sweep_expired`
  (`crates/copperclaw-modules/src/interactive.rs`) existed since the
  module landed but no sweep loop ever called it: a question past its
  24h TTL vanished from module state while the card sat in chat with
  live option buttons and the asking agent never learned no answer was
  coming. A new sweep check
  (`crates/copperclaw-host-sweep/src/checks/questions.rs`) wires the
  module's expiry into every sweep pass, mirroring the polished
  approval-expiry pattern (`handlers/approvals.rs::expire_and_edit_cards`):
  each lapsed question gets ONE terminal user-facing note — an `edit`
  System row keyed at the original ask row's seq, so edit-capable
  channels replace the card in place ("This question expired before
  anyone answered — just reply and I'll pick it up from there.", no
  live buttons left behind; edit-less channels fall back to the
  standard `"(edit) ..."` chat line) — plus a `trigger = 0` synthetic
  `ask_user_question_result` (`status = "expired"`) inbound row, so the
  agent's next turn sees the no-answer result instead of waiting
  forever. Questions the user de-facto answered (any chat inbound after
  the ask — replies land as ordinary inbounds the runner already
  handled) are resolved silently; questions inside their TTL are
  untouched.
- **Ask-time provenance on pending questions.** `PendingQuestion` gains
  a `QuestionOrigin` (session, agent group, ask-row id, channel
  routing), captured by the `ask_user_question` delivery action from
  the delivery service's threaded context — this is what lets the
  sweep route the expiry surfacing without new DB state.
  `InteractiveModule` is now `Clone` (clones share pending state);
  `boot::install_modules` (`crates/copperclaw-host/src/boot.rs`)
  returns the shared handle and `run_host` injects it via the new
  set-once `SweepService::set_question_store` seam (unset — every
  pre-F2 caller — the check is a strict no-op).
- **Replay fixture for the full lapse cycle.** New fixture
  `fixtures/cli/question-expiry/` + registered test
  `cli_question_expiry_surfaces_lapse_and_resumes_on_reply`
  (`crates/copperclaw-host/tests/replay.rs`): ask -> question card on
  the wire -> real `SweepService` pass surfaces the lapse exactly once
  (typed adapter edit stamps the card terminal; second pass byte-quiet)
  -> the user's late reply resumes normally with the synthetic
  no-answer result visible in the turn's provider request. Host-side
  sweep timing is wall-clock (not the runner `TestClock`), so the test
  drives the TTL with a zero-TTL module handle and the TTL-selection
  boundary is pinned by paused-time crate tests instead — documented in
  the fixture README per the Wave-1 reachability map. Harness seams
  added for it: `ReplayHarness::run_steps` (drive a subrange of inbound
  steps so a test can interleave a sweep pass between them),
  `ReplayHarness::install_interactive_module`, and module
  delivery-action registrations now forward to the harness
  `DeliveryService` (mirroring the host's `HostContext`).

### Added (M21 Wave-1 X-rider: recovery fixtures, 2026-07-17)

- **Replay-level pin for the hung-tool recovery sequence.** New fixture
  `fixtures/cli/stuck-tool-restart/` + registered test
  `cli_stuck_tool_restart_delivers_single_apology_end_to_end`
  (`crates/copperclaw-host/tests/replay.rs`): a real pipeline-created
  session is put into the exact mid-hang state a wedged runner leaves
  behind (running container, fresh heartbeat, in-flight `Processing`
  ack, `container_state` tool row 31 minutes old), then one actuated
  sweep pass (`SweepService::run_once_actuated` with the real
  `ContainerManager` as `StuckActuator`, mock runtime) must fire the
  `StuckRestart` and the single "snag ... restarted" apology must reach
  the channel adapter through the real `DeliveryService` — the wire leg
  S2's own crate tests stop short of. A second sweep + delivery pass is
  asserted byte-quiet, and the apology copy is asserted honest (no
  "operator has been notified" until O4 wires real alerts).
- **Replay-level pin for delivery-retry persistence (migration 029).**
  New fixture `fixtures/cli/delivery-retry-restart/` + registered test
  `cli_delivery_retry_restart_resumes_and_dead_letters_once`: a
  runner-produced chat row fails its first delivery via a scripted
  transport error, then the delivery service is killed and recreated
  before EVERY subsequent attempt — three failures across three service
  lifetimes must exhaust `MAX_DELIVERY_ATTEMPTS` (a restart must never
  reset the budget), dead-letter exactly once (one
  `delivered{status=failed}` record + one ErrorCard), deliver the
  "Could not deliver message" card to the adapter exactly once, and
  stay terminal across yet another restart.
- **Replay-harness restart seam.** `ReplayHarness::restart_delivery()`
  (`crates/copperclaw-host/tests/replay/harness.rs`) rebuilds the
  `DeliveryService` — fresh in-memory retry cache, primed-session set,
  in-flight guards — plus a fresh `MockAdapter` set over the same
  central DB and per-session files, mirroring what a real host restart
  preserves. This is the harness extension the S3 card deferred to the
  X-rider; adapter-set construction is factored into a shared
  `build_adapter_set` so a restarted service registers a byte-identical
  topology.
- **Wave-1 coverage map.** `fixtures/README-m21-wave1.md` maps every
  Wave-1 acceptance behavior (S1 loop supervision, S2 stuck restart, S3
  retry persistence, S4 OOM/backoff, S5 no-adapter expiry, S6 clock
  seam) to the test that pins it, and documents — per the plan's
  "documented, not skipped silently" rule — the five things a replay
  fixture genuinely cannot reach (host-side tokio/wall-clock timers, a
  truly hanging in-process tool, supervised-loop panic injection, OOM
  exit-status inspection, manifest-driven service restarts) and where
  each is pinned instead. `docs/replay-fixtures.md` gains the
  runner-clock-reach caveat and the restart seam.

### Added (M21 S2: stuck-tool actuator — detected stuck sessions get restarted, 2026-07-17)

- **Stuck tools are finally recovered, not just logged.** The sweep has
  always detected a tool running past its timeout
  (`crates/copperclaw-host-sweep/src/checks/stuck.rs`), but the
  detection went nowhere — `SweepReport.stuck_sessions` was only a log
  field, and the crash path could never fire because the runner keeps
  its heartbeat fresh during tool dispatch (deliberately: the process IS
  alive). A hung tool therefore wedged the session forever. Now a new
  `StuckActuator` trait (`crates/copperclaw-host-sweep/src/actuator.rs`)
  is injected into `SweepService` at boot
  (`crates/copperclaw-host/src/boot.rs`) and implemented by the
  container manager as a new `ReconcileAction::StuckRestart`
  (`crates/copperclaw-host/src/container_manager/{classify.rs,
  stuck_actuator.rs}`): the container is torn down, the user gets the
  existing crash-restart apology ("Hit a snag mid-task..."), the stale
  tool-state row is cleared so the sweep does not re-fire against the
  fresh container, and the session respawns on the next inbound.
  **Default-behavior change:** sessions that previously wedged forever
  are now restarted automatically — but only past the unconditional
  30-minute `ABSOLUTE_CEILING_MS`; detections past the 60s claim
  threshold (or a tool's declared timeout) remain observe-only, so
  long-but-legitimate tools are never killed. `stuck::check` now
  returns a `StuckSeverity` (`ClaimThreshold` observe-only vs
  `AbsoluteCeiling` actuated) and `SweepReport` gains a
  `stuck_past_ceiling` subset field.
- **Stuck restarts participate in the S4 crash-loop backoff.** A
  session whose tool wedges immediately after every respawn is
  respawned at increasing intervals (5s -> 15s -> 60s -> 300s cap, same
  per-session tracker as crash restarts) instead of hot-looping on the
  60s sweep cadence. Stuck restarts do NOT increment
  `copperclaw_containers_crashed_total` (a deliberate recovery is not a
  crash); a dedicated stuck-restarts-by-reason series is an M1 metrics
  wish.

### Fixed (M21 S2: apology copy no longer claims the operator was notified, 2026-07-17)

- **The stuck-inbound apology and the degraded-mode apology no longer
  claim "The operator has been notified"** — nothing of the sort
  happened (the only signals were a log line and a metric). Per M21
  decision (d), the honest copy in
  `crates/copperclaw-host-sweep/src/checks/apology.rs` and
  `crates/copperclaw-host/src/image_health.rs` now tells the user to
  tell their operator if the problem persists, and that `cclaw doctor`
  on the host will show what's wrong. O4 (opt-in operator alerts) will
  conditionally restore the notified claim once it is actually true;
  tests pin that no apology copy contains "notified" until then.

### Added (M21 S5: forever-pending outbound rows are bounded, 2026-07-17)

- **A 24h age ceiling for outbound rows whose channel has no live
  adapter.** `crates/copperclaw-host-delivery/src/service.rs` previously
  left such rows pending forever ("deferred" every pass), so a
  permanently-unconfigured or removed channel accumulated unbounded
  pending outbound that nothing drained and nothing reported. Now a
  pending row older than `NO_ADAPTER_MAX_AGE_HOURS` (24h) is
  dead-lettered into the central `outbound_dropped_messages` table with
  reason prefix `no_adapter` (visible in `cclaw dropped-messages
  outbound-list`, recoverable with `cclaw dropped-messages replay` once
  the channel is configured) and terminally marked
  `delivered{status="failed"}`. Deliberately no user-facing ErrorCard —
  these rows by definition have no deliverable channel. Ephemeral UI
  kinds (breadcrumb / todo_list / diff / error / thinking) are failed
  without a dead-letter row: they are not replayable and stale UI chrome
  is meaningless to re-send.
- **`DeliveryService::no_adapter_backlog()` — a queryable count of rows
  currently waiting on a missing adapter**, refreshed per processing
  pass (per-session slices, cleared when the rows deliver or
  dead-letter). O1/S1 handoff: the M21 S1 admin-socket status handler
  (lane H, built in parallel) is the intended surface for this count so
  `cclaw doctor` (O1) can flag a no-adapter backlog; until then this
  method is the only read side.
### Added (M21 S4: OOM and crash-loop classification with backoff, 2026-07-17)

- **OOM kills are now classified distinctly from generic crashes.** New
  `ContainerRuntime::exit_status` method in
  `crates/copperclaw-container-rt/src/lib.rs` (Docker override inspects
  `State.OOMKilled` + `State.ExitCode`; other backends default to
  "unknown"). The crash-restart path
  (`crates/copperclaw-host/src/container_manager/classify.rs`) inspects
  the container before removal and classifies exit 137 /
  `State.OOMKilled` as an OOM kill — previously indistinguishable from
  any other crash.
- **One user-facing `ErrorCard` per OOM episode.** After 3 OOM kills
  within a crash-loop episode the user gets a single "Task keeps running
  out of memory" card naming the operator fix (`memory_mb` in the
  group's container config) instead of an endless string of generic
  restart apologies. Deduped per episode (new
  `crates/copperclaw-host/src/container_manager/crash_loop.rs`); the
  dedup re-arms only after the session stays healthy for 10 minutes.
- **Image build/pull failure is a modeled spawn-failure class.** New
  `SpawnFailureReason` (`image_build` / `image_missing` / `runtime`) in
  `crates/copperclaw-host/src/container_manager/spawn.rs`; an image
  rebuild failure with no fallback tag now records into the shared
  `SpawnAttemptTracker`, so the sweep's "container never came up"
  apology can finally fire for a group whose image cannot be built —
  previously that path returned an error without ever feeding the
  tracker, leaving the group silently dark.

### Fixed (M21 S4: crash restarts back off instead of hot-looping, 2026-07-17)

- **A crash-looping session no longer respawns once per reconcile tick
  forever.** `ContainerManager` now keeps a per-session in-memory
  crash-loop tracker
  (`crates/copperclaw-host/src/container_manager/crash_loop.rs`) on the
  same decision-(e) curve as the S1 loop supervisor (5s -> 15s -> 60s ->
  300s cap, streak reset after 10 minutes healthy). The crash-restart
  teardown (log capture, container removal, immediate apology) is
  unchanged and immediate; only the respawn is deferred, via a gate in
  `classify`'s Stopped-arm. Deliberately not persisted across host
  restarts — boot's recovery re-baselines every session. Non-OOM crash
  behavior is otherwise byte-identical.

### Added (M21 S3: delivery retry state persists across host restarts, 2026-07-17)

- **Migration 029 (`029_messages_out_retry_state.sql`, session-outbound
  set): `tries` + `not_before` columns on per-session
  `outbound.db::messages_out`.** Delivery retry state previously lived
  only in an in-memory `DashMap` on the host's `DeliveryService`, so a
  host restart wiped attempt counters and backoff windows —
  `MAX_DELIVERY_ATTEMPTS=3` was really "3 per host lifetime". New
  accessors `messages_out::set_retry_state` / `list_retry_state` in
  `crates/copperclaw-db/src/tables/messages_out.rs`; a fresh-vs-migrated
  `PRAGMA table_info` diff test guards the session-outbound schema in
  `crates/copperclaw-db/src/migrate.rs`.

### Fixed (M21 S3: retry budgets and backoff windows survive restarts, 2026-07-17)

- **A poisoned outbound row can no longer retry unboundedly across host
  restarts, and a row mid-backoff no longer fires immediately on boot.**
  `crates/copperclaw-host-delivery/src/service.rs`: the `retries` map is
  now a write-through cache — every `bump_retry` mirrors the counter and
  the wall-clock window onto the row (best-effort; in-memory state stays
  authoritative within a lifetime), and the cache is primed lazily from
  the persisted columns on the first poll of each session after boot
  (persisted windows are honoured with their remaining span, capped at
  the 30-minute ceiling).
- **Exhaustion dead-letters exactly once across restarts.** The final
  bump persists the exhausted count before the `failed` record lands; a
  new persisted-exhaustion guard in `process_session_once` dead-letters
  a row that already spent its budget in a prior host lifetime without
  burning another adapter attempt — one `delivered{status="failed"}` row
  (the `cclaw dropped-messages` artefact) and one delivery-failure
  ErrorCard, never more. No new user surface.

### Changed (README + observability doc refreshed to the current surface, 2026-07-16)
### Added (M21 S1: host background loops are supervised, 2026-07-17)

- **Every host background loop is now supervised — a panic no longer
  silently kills the subsystem.** New
  `crates/copperclaw-host/src/supervisor.rs`: a `JoinSet`-based supervisor
  with named tasks. A loop that panics (or returns while the host is
  running) is logged at ERROR and restarted on the decision-(e) backoff
  curve (5s -> 15s -> 60s -> 300s cap, streak reset after 10 minutes
  healthy); a loop that exceeds the curve flips a supervisor-wide degraded
  flag (exposed as a `watch` channel — the seam M21 O4 hooks for operator
  alerts) but keeps retrying at the cap. `boot.rs` registers the inbound
  consumer, both delivery loops (active + sweep), the sweep loop, the
  typing ticker, and the todo watcher through it; previously each was a
  bare `tokio::spawn` awaited only at shutdown, so a panic left the
  subsystem dead — indistinguishable from idle — until the process exited.
  Loops keep their own internal error handling; shutdown drain semantics
  are unchanged (same token, same 30s deadline).
- **New `host.status` admin-socket command** —
  `crates/copperclaw-host/src/handlers/host_status.rs` reports per-loop
  liveness, lifetime restart counts, current restart streak, last exit
  reason, uptime, and the degraded flag. M21 O1 teaches `cclaw doctor` to
  read it; until then it is a complete, callable surface (listed in
  `socket.rs`'s new `HOST_LOCAL_COMMANDS` so the dispatch-table parity
  test still catches drift). `HandlerCtx` gained an optional supervisor
  handle (`with_supervisor`) threaded through `serve_listener`.

- **`README.md` caught up with M16-M20.** The stale numbers are fixed
  (~7,700 tests, 51 tools + opt-in browser + 3 preview verbs, 11 of 21
  channels fixture-covered) and the features the last five programs
  shipped are now described: delegation (`delegate` / `delegate_batch`),
  vision + browser tools, web preview + public tunnel, code-quality
  gates (`diagnostics`, `self_review`), memory tools, `save_skill`, the
  task HUD / progressive reveal, provider failover with prompt caching,
  the OpenCode provider, and the M16 hardening surface. Two fixed
  "What's rough" items removed (generic `cclaw approvals approve-id` /
  `deny` now exist; the sparse `--help` text is descriptive now); the
  config table gained the notable post-M16 env keys.
- **`docs/observability.md` now documents all ~140 metric families**,
  grouped by subsystem with labels and meaning extracted from the
  `copperclaw-metrics` helper docs (the doc previously covered 13). New
  recommended alerts: `degraded_state`, failover-chain exhaustion,
  stuck-inbound apologies, SSRF blocks, and the outbound-rendering
  quality counters.

### Fixed (task-HUD step label no longer pinned by a stranded todo, 2026-07-16)

- **The task HUD's `step X/N: <text>` label now tracks the first active item
  AFTER the last completed one.** Found reviewing a live Telegram build
  (2026-07-16): the agent's completion of todos 1-2 was refused by the
  evidence gate (correctly — no verification yet) and never retried, so both
  sat `in_progress` while items 3-9 completed — and the HUD label read
  "step 1/11: Scaffold vite..." for the entire 13-minute run because
  `current_todo_step()` picked the FIRST `in_progress` item. It now scans
  from just past the last `completed` item, falling back to the old
  whole-list preference (first `in_progress`, else first `pending`) when
  nothing active follows it. `crates/copperclaw-runner/src/run/hud.rs`, with
  a regression test mirroring the live shape.

- **Tool-call failures are now visible in the runner log.** The same session
  showed `last: todo_update failed` / `last: ui_screenshot failed` HUD
  breadcrumbs with no recoverable reason anywhere: the error text lives only
  in the model-facing `tool_result` (gone after compaction) and the runner
  logged nothing. `drive_turn` now emits one `WARN` per failed tool call
  (`tool`, first 240 chars of the error as `reason`, `tool_turn`) so
  operators can answer "WHY did that tool fail" from `docker logs` /
  `copperclaw logs` after the fact.
  `crates/copperclaw-runner/src/run/drive_turn.rs`.

### Changed (web_search exempt from the taint half of the provenance gate, 2026-07-16)

- **`web_search` no longer self-locks on tainted turns.** Found live in a
  telegram smoke test: the FIRST `web_search` succeeded, its results (untrusted
  web content) tainted the turn, and the M16 provenance gate then denied every
  follow-up `web_search` in the same turn as "a credentialed external action on
  a tainted turn" — iterative research (search, read, refine, search again) was
  structurally impossible. `web_search` is now EXEMPT from the **taint** half of
  the gate, by the same local-vs-attacker-chosen boundary as the M19 A7 LAN
  preview exemption: a search query egresses only to the operator-configured
  provider endpoint (`TAVILY_API_KEY` et al), never to an attacker-chosen one,
  so a poisoned turn cannot use it to route the agent's credentials at an
  attacker target — the worst it can do is choose a query string the attacker
  never sees. New `PROVIDER_PINNED_SEARCH_TOOLS` tight allow-list +
  `is_provider_pinned_search()` in `crates/copperclaw-runner/src/policy.rs`;
  the rule for future entries is documented there: never add a tool whose call
  arguments can name an endpoint. Unchanged: the **autonomy** half still blocks
  `web_search` on autonomous/heartbeat turns; search results still taint the
  turn, so `web_fetch`, external MCP tools, and `make_preview_public` remain
  gated behind fresh approval; all profile/skill/role layers untouched. New
  unit tests pin the exemption and the still-blocked neighbours.

### Fixed (cclaw doctor: env-var checks now consult the install's .env, 2026-07-16)

- **`cclaw doctor` no longer reports a configured web-search provider as
  missing.** The `web-search` and `anthropic-key` checks only read the cclaw
  shell's env, so a `TAVILY_API_KEY` living in the install's `.env` (the normal
  setup-written case — the host reads it at boot and forwards it into session
  containers) reported "no web_search providers configured", sending the
  operator down the wrong debugging path. Both checks now also parse
  `<install_root>/.env` for non-empty keys (presence only — values are never
  read out; new `install_env_nonempty_keys()` in
  `crates/copperclaw-cclaw/src/lib.rs`) and report the source
  (`tavily (.env)` vs `tavily (shell env)`).
### Added (M21 S6 — test-clock seam for the runner's timed surfaces)

- **`Clock` seam in the runner** (`crates/copperclaw-runner/src/clock.rs`,
  new): a minimal injectable monotonic-time trait — `SystemClock` (real
  time, the production default) and `TestClock` (frozen; advanced by hand,
  shared across clones) — carried as `RunnerDeps.clock`
  (`crates/copperclaw-runner/src/run/mod.rs`). The Task HUD
  (`crates/copperclaw-runner/src/run/hud.rs`) now reads every elapsed-time
  decision through it: the elapsed clock on live frames and finalize, the
  R6 progressive-final gate (`TaskHud::elapsed`), the bare-channel 60s
  status-row cadence, and the 150s softening threshold. At the default
  real clock all timing behavior is byte-identical; the seam exists so
  timed legs are testable without wall-clock waits, and later M21 timed
  surfaces (backoffs, TTLs, spawn thresholds) should read the same clock
  instead of growing their own.
- **Replay-harness clock advancement**
  (`crates/copperclaw-host/tests/replay/harness.rs`,
  `crates/copperclaw-host/tests/replay/fixture.rs`): the harness now owns
  a shared `TestClock` injected into every per-step runner, advanceable
  programmatically (`ReplayHarness::advance_clock`, for between-step
  time) or declaratively via a new `advance_clock_ms` key on
  `provider_responses` manifest entries (the wiremock responder advances
  the clock as the scripted LLM call is served — the only hook that lands
  *inside* a single turn's tool loop). Built for the M21 X-riders to pin
  timed legs deterministically.
- **The M18 X2 known gap is closed**: the HUD StatusRows 60s first-fire
  and 150s softening — unfixturable since M18 because they needed a real
  60s wait — are now pinned twice: unit-level in
  `crates/copperclaw-runner/src/run/hud.rs`
  (`status_rows_60s_first_fire_and_150s_softening_pinned_by_test_clock`)
  and end-to-end by the new `fixtures/cli/status-row-heartbeat/` replay
  fixture, whose expected streams assert the exact "61s in ... I'll keep
  going." and "151s in ... taking longer than usual" rows byte-for-byte
  with zero real waiting.

### Security (M20 D1/D5 — recorded security-review pass for the in-container vision tools, 2026-07-16)

- **Reviewed and PASSED** the two M20 cards that change the default posture:
  D1 (`ui_screenshot`, registered by default in Coding/Full — the one
  default change in M20) and D5 (`ui_inspect`, which adds page-originated
  console text to the transcript). The plan (decision (a)) required a
  recorded `security-review` pass for both; this is that record. Verified
  against the code on `main`:
  - **Loopback boundary is sound.** `validate_loopback_url`
    (`crates/copperclaw-mcp/src/tools/ui_screenshot.rs`, reused verbatim by
    `ui_inspect`) parses with `reqwest::Url`, enforces an http/https scheme,
    and validates the *host* via `IpAddr::is_loopback()` — not a string
    match — so `10.x`, `169.254.169.254`, `0.0.0.0`, and `file://` are all
    rejected, before chromium is ever touched.
  - **No new privilege — confirmed at the network layer.** The in-container
    chromium shares the session container's network namespace, so it is
    bound by the *same* deny-default DNS/egress filter
    (`crates/copperclaw-container-rt/src/dns.rs`) as `shell`/`curl` — it can
    reach nothing the agent's existing shell could not. The CDP
    remote-debugging port is bound to `--remote-debugging-address=127.0.0.1`
    (`incontainer.rs`), so no other host or container can drive the browser.
    `--no-sandbox` is safe because the container is the sandbox boundary and
    the agent already has arbitrary `shell` inside it.
  - **Untrusted-content marking is correct.** `ui_inspect` calls
    `mark_untrusted_context` unconditionally, before navigating;
    `ui_screenshot` calls it exactly when it folds a page-originated console
    error into its text response (`error_count > 0`). Both mark the turn
    before the content lands in history.
  - **DoS bounds present.** Console buffer capped (`CONSOLE_BUFFER_CAP = 50`),
    `wait_ms` clamped, screenshot capped at 5 MB with an automatic jpeg
    downgrade, jpeg quality clamped, and the chromium singleton idle-reaped
    (`IDLE_TIMEOUT = 5m`). Both tools are Coding/Full-only; Guest and Minimal
    are denied (`copperclaw-runner/src/policy.rs`, with tests).
  - **Accepted residual risks (documented, not defects).** (1) The
    loopback check validates only the initial URL; chromium *will* follow an
    HTTP redirect from a loopback page — this is contained by the container
    netns egress (identical to `shell`), so it is not a new privilege, but
    the "loopback-only" label is an intent guard, not an egress control; the
    netns posture is. (2) Screenshot pixels are page-originated visual
    content the vision model reads and are deliberately NOT tainted (decision
    (a), `view_image` parity) — tainting every UI screenshot would defeat the
    see→fix loop; the mitigating context is that it is the agent's own app.
    Neither warrants a code change; both are recorded here so operators do
    not over-trust the loopback label.

### Added (M20 M1 — metrics rider: sweep the M20 metric wishes into `copperclaw-metrics`, 2026-07-16)

- One card, absolute last in the program, sweeps every metric "wish" the merged
  M20 cards (Q1–Q8, D1–D5) recorded into `crates/copperclaw-metrics/src/lib.rs`
  (the workspace hotspot — no other M20 card touches it). Each new
  counter/gauge/histogram follows the crate's established convention
  (`copperclaw_` prefix, `_total`/`_seconds`/`_bytes`/etc. suffix, snake_case
  labels) and is wired to a real emit site; new tests extend the
  prefix/no-double-underscore/`_total`-suffix invariants, a helpers-compile
  smoke test, and a labeled-counter-renders test to the additions.
  - **Q1** `copperclaw_image_bundle_version{profile, pinned_binary, version}`
    gauge — exactly which pinned-binary version a profile bakes, set at spawn
    alongside the existing `copperclaw_group_image_profile`
    (`copperclaw-host/src/container_manager/spawn.rs`). Plus
    `copperclaw_pinned_binary_fetch_total{binary, outcome}`
    (`cache_hit|fetch_ok|checksum_fail|arch_unsupported|fetch_failed`) at every
    exit path of `fetch_pinned_binary` (`copperclaw-setup/src/steps/image.rs`).
  - **Q2** `copperclaw_verify_run_stage_total{stage, result}` — a stage-labeled
    sibling of the pre-Q2 `inc_verify_run` (kept, both fire together so
    existing dashboards don't break), from `apply_verify_gate`
    (`copperclaw-mcp/src/tools/computer_use.rs`).
    `copperclaw_verify_gate_pending_stages_total{pending}` (`"1"|"2+"`) at the
    todo-completion refusal (`copperclaw-mcp/src/tools/todo.rs`).
    `copperclaw_verify_stages_declared` histogram (declared stage count per
    project) from the same `apply_verify_gate` call site.
  - **Q3** `copperclaw_diagnostics_run_total{tool, outcome}`
    (`eslint|tsc|ruff` × `ran|not_available|error`) from the per-tool loop in
    `diagnose_project` (`copperclaw-mcp/src/tools/diagnostics.rs`).
  - **Q6** `copperclaw_review_gate_completion_total{outcome}`
    (`refused_never_reviewed|refused_dirty|passed|blocked_cycle_cap`),
    mirroring `inc_verify_gate_completion` one layer up the delivery pipeline
    (`copperclaw-mcp/src/tools/todo.rs`, distinguishing `NeverReviewed` from
    `Dirty` via `self_review::review_state`).
    `copperclaw_self_review_findings` histogram +
    `copperclaw_self_review_submission_total{kind}` (`no_findings|findings`)
    from `self_review::handle` (`copperclaw-mcp/src/tools/self_review.rs`).
  - **Q7** `copperclaw_delegate_batch_contract_total{present}` — one increment
    per `delegate_batch` call (not per worker), and
    `copperclaw_delegate_batch_post_join_dirty_total{outcome}`
    (`marked|skipped_no_project|skipped_no_verify|skipped_gate_off|skipped_all_spawn_failed`)
    covering every branch of the post-join integration-verify decision
    (`copperclaw-mcp/src/tools/agents.rs`).
  - **Q8** `copperclaw_compaction_file_inventory_count` /
    `..._file_inventory_bytes` / `..._verify_stages_pinned` /
    `..._decisions_tail_lines` histograms — one observation per project per
    compaction round from `push_file_inventory` / `push_verify_stages` /
    `push_decisions_tail` (`copperclaw-runner/src/compaction.rs`).
  - **D1/D2** `copperclaw_ui_screenshot_total{outcome, viewport}`
    (`ok|blocked_non_loopback|chromium_missing|driver_error|oversize|downgraded`
    × `desktop|mobile`) + `copperclaw_ui_screenshot_refused_url_total` (shared
    with `ui_inspect`) + `copperclaw_ui_screenshot_capture_seconds` histogram,
    all from `ui_screenshot::handle`
    (`copperclaw-mcp/src/tools/ui_screenshot.rs`).
    `copperclaw_chromium_singleton_spawn_total{result}` +
    `copperclaw_chromium_singleton_idle_reap_total` from the lazy singleton
    launcher/reaper (`copperclaw-browser/src/incontainer.rs`).
    `copperclaw_browser_output_format_total{tool, format}`
    (`ui_screenshot|browser_render|browser_interact` × `png|jpeg`) — the last
    two wired at their existing outcome-emission points
    (`copperclaw-mcp/src/tools/browser_render.rs`,
    `.../browser_interact.rs`; the latter is always `png` since interactive
    screenshot mode has no format arg).
  - **D4** `copperclaw_see_fix_screenshots_per_build` histogram (count of
    `ui_screenshot` calls per inbound, threaded out of `drive_turn_inner` via
    a new `&mut u32` out-param alongside the existing `BlockerRun`,
    `copperclaw-runner/src/run/drive_turn.rs`) and
    `copperclaw_ritual_screenshot_delivery_total{outcome}` — **only
    `"delivered"` is emitted** (a `send_file` call whose `path` is under
    `.copperclaw/screenshots/`, `copperclaw-mcp/src/tools/core.rs`). The
    card's other wished outcome, `"omitted"` (a ready-card sent with no
    screenshot on a UI project), is a **documented gap**: `send_card` and
    `send_file` carry no "this is the ready card" / provenance tag today, so
    detecting an omission needs new cross-tool-call turn state and a new
    marker on both specs — out of scope for a metrics-only rider. See the
    doc comment on `inc_ritual_screenshot_delivery` in
    `crates/copperclaw-metrics/src/lib.rs` for the full reasoning. Likewise,
    the histogram is an honest proxy (call *count*), not proof a
    critique/edit cycle actually happened between calls — that step is
    prompt-level with no runtime marker.
  - **D5** `copperclaw_ui_inspect_total{outcome}`
    (`success|refused_url|chromium_missing`) + `copperclaw_ui_inspect_console_errors`
    histogram, from `ui_inspect::handle`
    (`copperclaw-mcp/src/tools/ui_inspect.rs`).
  - `copperclaw-setup` gained a new workspace dependency on `copperclaw-metrics`
    (previously the only crate compiling a first-party binary without it).

### Added (M20 X-rider Wave 3 — vision-loop fixtures, testing)

- Assessed all three Wave-3 X-rider fixture wishes (see→fix transcript
  shape / D4, viewport-preset screenshot metadata / D2, console-error
  surfacing / D5) against the CLI replay harness. **No new fixture added** —
  all three route through `ui_screenshot`/`ui_inspect`, which the Wave-1
  X-rider already established are not replay-expressible: the harness
  (`crates/copperclaw-host/tests/replay/harness.rs`) dispatches real
  production tools with no injection point for a mock `CdpTransport`, so an
  `ui_screenshot`/`ui_inspect` call in a fixture would spawn real chromium
  rather than replay a canned capture. All three are unit-covered in their
  own cards instead: the see→fix floor wiring by the `prompt.rs` snapshot +
  byte-stability tests and `skills/frontend-design` validation (D4/D3); the
  viewport/format arg surface + oversize→jpeg auto-downgrade by the mocked
  `CdpTransport` unit tests in `copperclaw-browser` (D2); the curated
  computed-style whitelist, console buffer cap, and untrusted-marking path
  by the `ui_inspect` unit tests + `ui_screenshot` console fold-in tests
  (D5). The generic image→provider-block read path is pinned by
  `ui_screenshot_tool_result_image_converts_to_provider_image_block`
  (`copperclaw-providers/src/anthropic.rs`, from D1). Closing this gap for
  real would require a `ToolContext`-level `CdpTransport` seam in the
  harness (a product change) — recorded as a follow-up, deliberately not
  forced here per the X-rider honesty rule.

### Added (M20 D3 — `frontend-design` skill)

- New `skills/frontend-design/SKILL.md` (6,659-byte body, well under the
  8 KiB cap): a genuinely opinionated visual-design skill teaching typography
  (one Inter/JetBrains-Mono pairing, a 4-5 stop 1.25-ratio type scale,
  line-height/measure rules), spacing (one 4px/8px unit, whitespace as a
  hierarchy signal, group-by-proximity), color (one saturated accent + a
  neutral ramp, states derived from the accent, a WCAG-AA contrast floor,
  dark-background desaturation), and layout (hierarchy-first, grid alignment,
  max content widths, designed empty/loading/error states). Includes explicit
  anti-generic rules (no default-blue gradient hero, no lorem ipsum, no
  reflexive three-equal-cards row, one border-radius/shadow reused
  everywhere, a constrained Tailwind palette) and an 8-question
  **`## Critique checklist`** — the stable, grep-able heading the D4 see→fix
  loop names when it points the agent at this skill after `ui_screenshot`.
  Cross-referenced from `skills/web-app-scaffold/SKILL.md` (which already
  forward-referenced this skill by name) and `skills/coding-task/SKILL.md`;
  this card makes the target real. No Rust files touched; validates against
  all 9 `crates/copperclaw-skills/tests/coverage.rs` checks (directory/name
  match, description length, tool-reference resolution, alphabetical load
  order, body-size cap, no unresolved template markers).

### Added (M20 Q1 — Bake the coding toolchain + design assets into the prototyping image)

- The `Prototyping` image profile bundle (`crates/copperclaw-types/src/image.rs`)
  now bakes a real lint/typecheck toolchain and UI fonts, not just
  `sqlite3`/headless `chromium`/`zip`/`vite`/`create-vite`: npm gains
  `typescript`, `eslint`, `prettier`, `tailwindcss`
  (`PROTOTYPING_NPM_PACKAGES`); apt gains `fonts-inter`,
  `fonts-jetbrains-mono`, `fonts-noto-color-emoji` (`PROTOTYPING_APT_PACKAGES`)
  so deny-default-egress deployments render real UI fonts instead of browser
  fallback serif/sans-serif. Package names were verified against
  `packages.debian.org/trixie` at branch time (2026-07-16): all three font
  packages exist under exactly these names, no substitution needed. The
  `Minimal` profile's package lists are byte-identical (existing pinned test
  `prototyping_bakes_expected_bundle` extended, not replaced).
- `ruff` has **no** trixie apt package (verified via `packages.debian.org`
  search — only unrelated `python-ruffus*`/`python3-scruffy` substring
  matches exist), so it's baked via a new pinned-binary mechanism instead:
  `copperclaw_types::image::{PinnedBinary, PinnedBinaryTarget,
  RUFF_PINNED_BINARY, PROTOTYPING_PINNED_BINARIES}` declares an exact
  upstream version (`0.15.22`) plus a per-architecture (x86_64/aarch64)
  release-tarball URL and sha256 (both verified against the actual
  downloaded assets, not just trusted from upstream's own `.sha256` files).
  `crates/copperclaw-setup/src/steps/image.rs` gained
  `PinnedBinaryFetcher`/`RealPinnedBinaryFetcher` (shells out to
  `curl`/`sha256sum`/`tar`, mirroring `install.sh`'s own release-tarball
  fetch — no new host prerequisite) and `fetch_pinned_binary` /
  `bake_pinned_binaries`, which download, verify, unpack, and embed the
  `ruff` binary as an `ExtraFile` at `/usr/local/bin/ruff` (mode 0o755) on
  a `Prototyping` base-image build. Downloads are cached under
  `<data_dir>/cache/pinned-binaries` keyed by name+version+arch+sha256, so
  re-running setup doesn't re-fetch (idempotent per the E2 precedent).
- The pinned binary's bytes are folded into `ImageBuildSpec::fingerprint()`
  automatically via the existing generic `extra_files` hashing (same
  mechanism the runner binary already uses) — no new fingerprint plumbing
  needed. The per-group config fingerprint (`copperclaw-db`'s
  `compute_fingerprint`) only hashes the `image_profile` *name*
  conditionally (never the bundle contents, following the M18 E2 precedent),
  so an existing group already on a pre-Q1 prototyping image is **not**
  force-rebuilt mid-session; a fresh group's first build picks up the full
  new bundle.
- **Known scope gap (flagging for a follow-up, not fixed here — outside this
  card's declared file scope):** the host's per-group mid-session rebuild
  path (`crates/copperclaw-host/src/container_manager/spawn.rs:513-585`,
  `rebuild_image`) constructs its own `ImageBuildSpec` from
  `packages_apt`/`packages_npm`/`image_profile` only — it has no
  `extra_files` channel, so it correctly inherits the new apt/npm additions
  (they flow through the shared `ImageProfile` enum) but does **not**
  independently embed the pinned `ruff` binary. In the common case (the
  base image the operator built via `copperclaw-setup` is itself
  `Prototyping`), `ruff` survives because `rebuild_image` layers on top of
  that base; a group whose *own* profile is switched to `Prototyping` on a
  `Minimal` base image will lint-tool-complete except for `ruff` until a
  future card threads pinned-binary embedding into that path too.
- Image-size growth is honest, not hidden: roughly **+100-170MB** to the
  `Prototyping` image (three font packages, four new global npm packages,
  and the ~11MB `ruff` binary; the pre-existing `chromium` line item still
  dominates the total). `Minimal` is unaffected.
### Added (M20 Q4 — Code-quality floor: prompt block + `coding-task` rewrite)

- `CODING_PREAMBLE` (`crates/copperclaw-host/src/container_manager/prompt.rs`)
  gained four static bullets — decompose before you type (name modules/files
  and their single responsibility first), prefer a baked tool over
  hand-rolling one (`create-vite`, `typescript`, `sqlite3`), handle the
  errors a *user* will actually hit (bad input, empty state, network
  failure) even in a prototype, and write the M20 Q2 multi-stage
  `.copperclaw/verify` at scaffold time rather than at the end. The block
  stays a static const (no per-turn/dynamic content, per program rule 8);
  `Messaging`/`Minimal` profiles still gain zero bytes — the existing
  byte-stability tests (`messaging_and_minimal_profiles_gain_zero_new_bytes`,
  `coding_block_is_static_per_spawn_for_cache_stability`) pass unmodified,
  and a new `coding_preamble_appears_exactly_once_in_coding_profile_prompt`
  test pins the block appearing exactly once in the assembled prompt.
- `skills/coding-task/SKILL.md` rewritten as the depth reference the floor
  points at: a new "Decompose before you build" section (one responsibility
  per file, module boundaries follow what changes together, split on job
  collision not line count); a new "Choosing dependencies" section (baked >
  fetched > hand-rolled, with a probe-first rule for anything not baked); a
  new "Robustness" section that explicitly bounds the old "no error handling
  for impossible cases" line — impossible now means no code path can produce
  the input, not merely unlikely, and user-reachable paths (bad input, empty
  state, network/IO failure) are always in scope; and the verify-contract
  section rewritten to teach the Q2 named-stage `.copperclaw/verify` format
  (`lint: npx eslint .` / `typecheck: tsc --noEmit` / `test: npm test`)
  including probing for a stage's tool with `command -v` before writing it.
  The rewrite stays under the 8 KiB per-skill body cap enforced by
  `crates/copperclaw-skills/tests/coverage.rs` (8,013 bytes), verified via
  the full skill coverage suite (name/registry/size/marker checks all pass).

### Added (M20 D4 — Wire the see→fix loop + the ritual screenshot)

- `CODING_PREAMBLE` (`crates/copperclaw-host/src/container_manager/prompt.rs`)
  gained a fifth static bullet, sequenced right after Q4's multi-stage-verify
  bullet (architecture decision (e)): for any app with a UI, after the first
  visual milestone run `ui_screenshot`, LOOK at the image, run the
  `frontend-design` skill's `## Critique checklist`
  (`load_skill("frontend-design")`), fix the worst two things, and
  `ui_screenshot` again — one full cycle minimum before the delivery todo.
  Still a static const (rule 8): no per-turn content, `Messaging`/`Minimal`
  gain zero bytes, and `coding_preamble_appears_exactly_once_in_coding_profile_prompt`
  / `messaging_and_minimal_profiles_gain_zero_new_bytes` pass unmodified
  against the new block.
- Rewrote the "prototype ready" delivery ritual's screenshot line: the
  delivery screenshot is no longer an assumed artifact from elsewhere — the
  agent takes it itself (`ui_screenshot` the final state, then `send_file`
  it alongside the ready-card). The old "when one exists" hedge is now "omit
  only when there is genuinely no UI", matching D1's default-on
  `ui_screenshot` registration in Coding/Full profiles.
- `skills/coding-task/SKILL.md` gained a new "See it, then fix it — before
  the delivery todo" section: a 5-step numbered loop (screenshot, look,
  critique via `frontend-design`, fix the worst two, screenshot again)
  cross-referenced from `web-app-scaffold`. To make room under the 8 KiB
  body cap, trimmed redundant prose across several sections (git-repo setup,
  dependency choice, robustness scope, verify-gate mechanics, the delivery
  ritual, and the fabrication rules) without dropping any rule; final body
  is 8,141 bytes (down from 8,077 pre-trim + the new section), verified by
  `crates/copperclaw-skills/tests/coverage.rs`'s `skill_bodies_under_size_cap`.
- Metrics wishes for M1: see→fix cycles per build (screenshot → critique →
  fix → re-screenshot count) and ritual screenshots delivered (count of
  ready-card turns whose `send_card` was accompanied by a `send_file` of an
  agent-taken screenshot vs. omitted for no-UI builds).

### Changed (M20 Q2 — Multi-stage verify: named stages, per-stage state, stage-attributed failures)

- `.copperclaw/verify` may now contain MULTIPLE lines, each an independent
  verify stage, with an optional `name:` prefix (`lint: npx eslint .`); an
  unprefixed line gets a derived name (`stage1`, `stage2`, ... by position).
  A single unprefixed line behaves byte-for-byte as before — full backward
  compatibility, no format flag
  (`crates/copperclaw-mcp/src/tools/verify_gate.rs`: `parse_stages`,
  `recorded_stages`).
- `apply_verify_gate` (`crates/copperclaw-mcp/src/tools/computer_use.rs`)
  now matches the trimmed shell command against ANY recorded stage and
  records that stage's pass/fail + RFC3339 timestamp in a new per-project
  `.copperclaw/stages` JSON marker file
  (`verify_gate::record_stage_result`); best-effort I/O, same swallow-and-log
  style as the existing markers. The project's dirty marker only clears once
  EVERY recorded stage reads green (`verify_gate::all_stages_passed`) — with
  a single stage this is exactly the pre-Q2 clear-on-pass behavior.
- `mark_dirty` now also resets all per-stage state — a fresh edit
  invalidates every prior stage result, not just the file that was touched.
- The `todo_update` completion gate
  (`crates/copperclaw-mcp/src/tools/todo.rs`) now requires every recorded
  stage to be green since the last dirty mark; its refusal message names the
  missing/failing stages and their exact commands
  (`verify_gate::pending_stages`).
- Failed stage-aware verify runs are attributed: `last_failure`'s tail is
  prefixed with the stage name (`stage 'typecheck' failed: <tail>`) via the
  new `verify_gate::record_stage_verify_failure`, which wraps the existing
  `record_verify_failure` without changing its signature or behavior for
  direct callers (project-wide `fix_cycles`/`FIX_CYCLE_CAP` mechanics are
  unchanged).
- The M18 golden verify-gate fixture (`fixtures/cli/prototype-verify-gate/`)
  passes unchanged, confirming the legacy single-line shape is untouched.
### Added (M20 D1 — `ui_screenshot`: in-container screenshot of the agent's own app)

- The generic vision READ path (`crates/copperclaw-runner/src/run/tool_dispatch.rs`
  → `drive_turn.rs` → `crates/copperclaw-providers/src/anthropic.rs`) has been
  fully wired since M18/M19, but the WRITE path was stranded: `browser_render`
  needs a Docker socket the in-container runner does not have by design
  (`crates/copperclaw-mcp/src/tools/browser_render.rs`), so in every default
  deployment the agent had never once seen its own UI mid-build. A new
  first-party `ui_screenshot` MCP tool
  (`crates/copperclaw-mcp/src/tools/ui_screenshot.rs`) closes this: it launches
  the prototyping image's already-baked chromium as a LOCAL PROCESS inside the
  session container itself (`crates/copperclaw-browser/src/incontainer.rs`,
  new module) and speaks CDP to it over loopback — no container spawn, no
  Docker socket, no host round-trip. Returns `RawContent::Image` (PNG) directly
  so the existing generic read path converts it to a provider image block with
  zero new plumbing, plus a text line with the saved path under
  `<project>/.copperclaw/screenshots/` so `send_file` can ship it later.
- `url` is **loopback-only** (127.0.0.1 / `::1` / localhost); any other host is
  refused with a hint pointing at `browser_render` for real web browsing — this
  is deliberately NOT a general browsing capability.
- A lazy, idle-reaped chromium singleton (`copperclaw_browser::incontainer::ChromiumSingleton`):
  the first `ui_screenshot` call in a session spawns chromium
  (`--headless=new --no-sandbox` — safe because the session container is
  itself the sandbox boundary, see the module docs), later calls in the same
  build-loop reuse the warm process, and a background reaper kills it after 5
  minutes idle.
- Default capture is a fixed 1280x800 WINDOWED viewport (never
  `captureBeyondViewport`), which would blow the 5 MB `view_image`-class image
  cap on anything but a very short page; a later M20 card (D2) generalises
  viewport presets / formats.
- **Registered by default in the Coding/Full tool profiles** — the one default
  change M20 makes (`crates/copperclaw-runner/src/policy.rs`: `CODING_TOOLS`).
  No-new-privilege argument: the agent already has an arbitrary `shell` tool
  and this same chromium binary reachable from it on the prototyping image;
  this tool refuses any non-loopback URL, so it cannot reach the LAN, the
  host, or the public internet, and it does not touch the container's egress
  posture. It is NOT a credentialed external action (no taint/autonomy gating)
  because it never leaves the loopback interface, but it still rides the
  guest-denied mutating floor (spawning a local process is a resource cost a
  read-only sender shouldn't get for free). Security-review pass recorded in
  this card's PR per the M20 program rules.
- Chromium absent (minimal image profile): the tool probes for the binary at
  call time and returns one clean, actionable error naming the `prototyping`
  image profile — never a crash.
- Rider: when no container runtime is reachable in-container, `browser_render`
  and `browser_interact`'s error text now points at `ui_screenshot` instead of
  a raw runtime error, and notes that M18 V4's host-side screenshot injection
  is superseded by this card.
- Extended the `anthropic.rs` image-block unit tests with a `ui_screenshot`-
  shaped tool-result fixture proving the conversion to a provider image block
  (no live chromium/Docker needed); the live in-container acceptance
  (`ui_screenshot_docker_end_to_end` in `ui_screenshot.rs`) is `#[ignore]`d
  per the `session_install_docker_end_to_end` precedent (`self_mod.rs`).

### Added (M20 D2 — Screenshot fidelity: viewport control, format/quality, size safety)

- New shared `crates/copperclaw-browser/src/capture.rs` module: `ViewportPreset`
  (`Desktop` 1280x800, `Mobile` 390x844 + touch emulation + a mobile UA
  override — ONE mobile preset, not a device matrix), `ImageFormat`
  (`Png`/`Jpeg` with an optional quality), and `CaptureOptions` (viewport /
  `full_page` / format / quality), plus `apply_viewport` — the single place
  that knows the `Emulation.setDeviceMetricsOverride` /
  `Emulation.setTouchEmulationEnabled` / `Network.setUserAgentOverride` wire
  shape, shared by BOTH the host-side CDP driver (`cdp.rs`) and the
  in-container driver (`incontainer.rs`) so a preset serializes identically
  for both call sites.
- `CdpBrowserDriver` (`cdp.rs`) gains `capture_opts` (defaulting, via `::new`,
  to `CaptureOptions::legacy_full_page()` — the exact pre-D2 hard-coded
  `{"format":"png","captureBeyondViewport":true}`, no `Emulation.*` call at
  all) and an opt-in `.with_capture(opts)` builder. `browser_render`'s live
  path (`copperclaw-browser/src/live.rs::render_after_spawn`) now threads its
  `RenderRequest.capture` field through `.with_capture(...)`; `browser_interact`
  (via `interact_after_spawn`) is untouched and keeps the byte-identical
  legacy default.
- `browser_render` (`crates/copperclaw-mcp/src/tools/browser_render.rs`) gains
  optional `viewport` (`desktop`|`mobile`), `full_page` (default `true` —
  unchanged from today), `format` (`png`|`jpeg`), and `quality` args. With
  NONE of them passed, `resolve_capture_options` returns
  `CaptureOptions::legacy_full_page()` byte-for-byte — the critical back-compat
  guarantee this card requires; every pre-existing `browser_render`/
  `browser_interact` unit test passes unmodified.
- `ui_screenshot` (`crates/copperclaw-mcp/src/tools/ui_screenshot.rs`) gains
  the same `viewport` (default `desktop`, byte-identical to D1's fixed
  1280x800 windowed PNG), `full_page` (default `false`), `format`
  (default `png`), and `quality` args, resolved the same way — no args
  reproduces `CaptureOptions::ui_screenshot_default()` exactly.
- Size safety: `incontainer::capture_with_size_safety` captures per the
  request and, if the result exceeds the 5 MB image-attachment cap and isn't
  already jpeg, automatically retries ONCE as jpeg at quality 70
  (`DOWNGRADE_JPEG_QUALITY`) rather than erroring; `ui_screenshot`'s response
  text notes the downgrade (`"auto-downgraded to jpeg q=70 because..."`)
  instead of refusing the call outright. Still refuses (clean error) if the
  jpeg retry is also over cap.
- `ScreenshotRequest` (`incontainer.rs`) replaces its ad hoc `width`/`height`
  fields with a `capture: CaptureOptions`; `RenderRequest`
  (`copperclaw-browser/src/render.rs`) gains a `#[serde(default)] capture:
  CaptureOptions` field (defaulting to `legacy_full_page()`).
- Unit tests (mock `CdpTransport`, reusing D1's mock seam): device-metrics +
  capture params serialize correctly per preset (`capture.rs`); the desktop
  preset issues only the `Emulation.setDeviceMetricsOverride` call (no
  touch/UA calls) while mobile issues all three; an oversize PNG capture
  auto-downgrades to jpeg with the note flag set
  (`incontainer::capture_with_size_safety` tests); `browser_render`'s
  existing test suite plus new `resolve_capture_options`/`prepare` tests are
  all green. Live smoke (`#[ignore]`d, per the D1 precedent):
  `ui_screenshot_mobile_preset_differs_in_dimensions_from_desktop` proves a
  `viewport: "mobile"` capture reports different dimensions (390x844) than
  the `desktop` default (1280x800) against the same vite page.

### Added (M20 D5 — `ui_inspect`: console errors + element geometry)

- New `ui_inspect` MCP tool (`crates/copperclaw-mcp/src/tools/ui_inspect.rs`),
  the loopback-only diagnostic sibling of `ui_screenshot`: given a `url` (same
  loopback-only validation as `ui_screenshot`, reused verbatim via
  `crate::tools::ui_screenshot::validate_loopback_url`), returns the full
  buffered browser console — `console.*` calls, uncaught JS exceptions, and
  `Log.entryAdded` diagnostics captured during that navigation — and, when a
  `selector` arg is given, that element's box model (width/height) plus a
  CURATED subset of its computed style (`display`, `position`, `overflow`,
  `width`, `height`, `font-family` — never the ~300-property
  `CSS.getComputedStyleForNode` dump). Registered by default in the
  Coding/Full profiles alongside `ui_screenshot`/`diagnostics`/`self_review`
  (`crates/copperclaw-mcp/src/tools/mod.rs`); the minimal profile (no
  chromium) returns the same clean, actionable error `ui_screenshot` does.
- `crates/copperclaw-browser/src/cdp.rs` gains the shared CDP-layer pieces:
  `ConsoleEntry`/`ConsoleLevel`/`ConsoleSummary` + `parse_console_event` (maps
  `Runtime.consoleAPICalled` / `Runtime.exceptionThrown` / `Log.entryAdded`
  raw events to a normalized entry), `push_capped` (keeps the MOST RECENT
  `CONSOLE_BUFFER_CAP` = 50 entries), and `summarize_console` (counts by
  level + first error text); a new default `CdpTransport::console_entries()`
  method (empty by default so every existing mock transport in the test
  suite keeps compiling unchanged) that the live `WsCdpTransport` overrides
  by buffering the three console-related CDP events alongside its existing
  redirect/status event tracking. Also gains `inspect_element` — a pure CDP
  orchestration (`DOM.getDocument` → `DOM.querySelector` → `DOM.getBoxModel` +
  `CSS.getComputedStyleForNode`, the selector riding as a plain JSON command
  param, not spliced into a JS-evaluate string) plus the
  `CURATED_STYLE_PROPS` whitelist and `BoxModel`/`ElementInspection` types.
- `crates/copperclaw-browser/src/incontainer.rs`'s `capture()` (used by
  `ui_screenshot`) now sends `Runtime.enable` + `Log.enable` alongside its
  existing `Page.enable`/`Network.enable`, so console buffering is active on
  every `ui_screenshot` call too. New `InspectRequest`/`InspectOutcome` +
  `inspect()` function drives the same navigate/enable sequence for
  `ui_inspect`, then reads back `transport.console_entries()` and (if a
  selector was given) `cdp::inspect_element`.
- `ui_screenshot` (`crates/copperclaw-mcp/src/tools/ui_screenshot.rs`) folds a
  console-error count + the first error's text into its own text response
  (via `copperclaw_browser::summarize_console`) so the common case — a page
  that threw on load — needs no second `ui_inspect` call to at least learn a
  crash happened.
- **Security review (2nd `security-review`-rider card after D1, per plan rule
  5).** No-new-privilege argument: `ui_inspect` reuses the exact same
  in-container chromium singleton and loopback-only refusal as
  `ui_screenshot` — it adds no new process class, widens no egress, and
  cannot reach the LAN/host/public internet, so it carries forward D1's
  registration-by-default argument unchanged. Untrusted-content argument:
  console text is page-originated (a page's own `console.*` calls can echo
  fetched/attacker-influenced content), so `ui_inspect` calls
  `ToolContext::mark_untrusted_context` **unconditionally**, before driving
  the navigation — mirroring `browser_render`'s "tag the turn up front"
  ordering — since an empty console on one call is no guarantee the next
  identical call stays empty. `ui_screenshot`'s fold-in only taints
  **conditionally** (when it actually includes a console error's text),
  since the common clean-console case still adds no page-derived text to the
  transcript, matching its pre-D5 "not tainted" posture. A non-loopback URL
  is refused before either tool touches chromium or marks any taint.
- Unit tests (mock `CdpTransport`/mock in-container transport, reusing
  D1/D2's mock seams): console-event parsing for all three CDP event kinds
  plus an unrelated-method no-op; buffer-cap eviction keeps the most recent
  entries; `summarize_console` counts + first-error extraction;
  `curate_computed_style` returns exactly the whitelist in whitelist order
  (never the full dump), including when a whitelisted property is absent;
  `inspect_element`'s CDP call sequence and its "no element for selector"
  error path; `ui_inspect`'s loopback refusal happens before any taint call;
  `ui_screenshot`'s console fold-in note is empty with no errors or
  warnings-only, and includes the count/first-error text/`ui_inspect`
  pointer when an error is present. Live smoke (`#[ignore]`d, per the D1
  precedent): a page that throws on load surfaces full console detail via
  `ui_inspect`, and a `body` selector query returns its box + curated style.

### Added (M20 Q8 — Compaction preserves build knowledge)

- The pinned project-facts header compaction re-generates on every round
  (`crates/copperclaw-runner/src/compaction.rs`, `build_project_facts_header`)
  now carries three new best-effort, capped sections per project, on top of
  the existing branch/verify/todo facts:
  - **A generated file inventory** (`push_file_inventory` / `git_ls_files`):
    `git ls-files` run against the project root, capped at
    `MAX_INVENTORY_FILES` (200) paths with an "N more files omitted" note
    past the cap. Tracked files only, so build artifacts and dependency
    directories never appear — they're git-ignored. No git repo, no `git`
    binary, or a failed command all pin nothing (never abort compaction).
  - **All verify stages, not just the first** (`push_verify_stages`): now
    sourced from M20 Q2's `verify_gate::recorded_stages` instead of the raw
    `recorded_verify_command` string, so a multi-stage `.copperclaw/verify`
    survives compaction with every stage's name and command, not a single
    flattened line. A project with exactly one (legacy or post-Q2
    single-line) stage still renders the byte-identical pre-Q8
    `  verify: <command>` line — full back-compat. Capped at
    `MAX_VERIFY_STAGES_PINNED` (20) stages.
  - **A `DECISIONS.md` tail** (`push_decisions_tail`): a new lightweight,
    tool-free convention — the agent appends one line per decision ("chose
    X over Y because Z") to `<project>/.copperclaw/DECISIONS.md` with
    ordinary edit tools (no new tool; taught by the Q4 `coding-task` skill
    rewrite). Compaction pins the last `MAX_DECISIONS_LINES` (30) non-empty
    lines. A missing/unreadable/empty file pins nothing — a project with no
    `DECISIONS.md` compacts exactly as it did pre-Q8, plus the new
    inventory/stages sections.
- **Bug found and fixed while implementing this card**: `verify_gate::
  mark_dirty_for_write` (`crates/copperclaw-mcp/src/tools/verify_gate.rs`),
  the shared post-write hook every edit-family tool calls
  (`write_file`/`edit_file`/`multi_edit`/`apply_patch`), marked the whole
  project dirty for *any* write under it, including writes inside
  `<project>/.copperclaw/` itself — so an agent appending a line to the new
  `DECISIONS.md` convention (or writing Q7's `CONTRACT.md`) would have
  invalidated an already-green verify on every append, exactly backwards
  from the log's purpose. Added `is_under_state_dir` and an exemption in
  `mark_dirty_for_write`: any write whose path resolves under a project's
  `.copperclaw/` subtree (at any depth) is now skipped entirely rather than
  marking the project dirty. **Cross-lane note for integration**: Q2 and Q6
  also touch `verify_gate.rs` on separate branches — this is a small,
  additive, surgical change (one early-return + one new private helper
  function) that should merge cleanly, but flag it for reconciliation.
- Unit tests added/extended: `crates/copperclaw-mcp/src/tools/verify_gate.rs`
  (`is_under_state_dir_*`, `mark_dirty_for_write_skips_writes_under_state_dir`,
  `mark_dirty_for_write_still_dirties_ordinary_source_writes`) and
  `crates/copperclaw-runner/src/compaction.rs` (file-inventory, multi-stage,
  decisions-tail, cap-overflow, and an end-to-end `compact()` test asserting
  all three new sections survive a real compaction round). `pair_safe_pivot`
  and all pre-existing pinned-header tests are unchanged and green.
### Added (M20 Q5 — `web-app-scaffold` skill)

- New `skills/web-app-scaffold/SKILL.md`: the golden path for starting a web
  app prototype now that Q1 bakes `create-vite`/`vite`/`typescript`/`eslint`/
  `prettier`/`tailwindcss` as global npm packages. Teaches `npm create
  vite@latest` (offline-safe — no registry fetch needed) with `vanilla-ts` as
  the default template and `react-ts` only when the user names the
  framework; seeding `tsconfig.json`, a flat `eslint.config.js`, and a
  `.prettierrc` in the same scaffolding step rather than deferring them;
  writing the matching Q2-format multi-stage `.copperclaw/verify`
  (`lint: npx eslint .`, `typecheck: tsc --noEmit`, `build: npm run build`)
  at scaffold time, with probing (`command -v eslint`) taught before writing
  any stage a pre-Q1 image can't satisfy; and, once the dev server is up,
  the `ui_screenshot` see-it habit with a forward-pointer to the
  `frontend-design` skill (lands separately in Wave 3) for the critique
  loop. Discovered automatically by `SkillRegistry::scan` (directory-based —
  no registry file to edit); validated by the existing
  `crates/copperclaw-skills/tests/coverage.rs` suite (name/dir match,
  substantive description, size cap, no stray template markers). The
  `coding-task` cross-reference pointer back to this skill is deferred to
  the M20 integrator per the lane-P sequencing (Q4 lands `coding-task`
  first).
### Added (M20 X-rider Wave 1 — foundation fixtures, testing)

- New replay fixture `fixtures/cli/prototype-verify-gate-multistage/`
  (sibling to the M18 X2 `prototype-verify-gate` fixture, which pins the
  pre-Q2 single-command gate) pins the Q2 multi-stage verify gate's
  refuse -> narrow -> pass shape: a `.copperclaw/verify` with three named
  stages (`lint`/`typecheck`/`test`); a `todo_update` completion attempt is
  refused naming exactly the stages not yet recorded green (never
  re-naming a stage that already passed), the refusal narrows as each
  stage turns green, and completion is allowed only once every stage
  reads green. Registered in `crates/copperclaw-host/tests/replay.rs` as
  `cli_prototype_verify_gate_multistage_refuse_narrow_pass`, using the
  same `COPPERCLAW_DATA_ROOT` re-exec seam (rooted at a distinct `/tmp`
  path) M18 X2 built to work around `forbid(unsafe_code)` blocking
  `std::env::set_var` in this integration-test target.
- The X-rider's other card, a `ui_screenshot` replay fixture proving a
  `RawContent::Image` tool result round-trips into a provider image block,
  was investigated and found **not expressible as a replay fixture**:
  `ui_screenshot::handle()` (`crates/copperclaw-mcp/src/tools/ui_screenshot.rs`)
  calls `copperclaw_browser::find_chromium_binary()` (reads the real
  process `PATH`) and `chromium_singleton().get_transport()` (spawns a
  real chromium process, connects over a real WebSocket) directly, with no
  `ToolContext`-level or env-var seam analogous to `verify_gate`'s
  `COPPERCLAW_DATA_ROOT` override — and the replay harness dispatches
  through the real `copperclaw_mcp::build_tool_set()`, not a mockable
  registry, so there is no way to inject a canned `CdpTransport` without a
  product-code change (out of scope for this fixtures-only lane). The
  generic `RawContent::Image` -> provider image-block conversion this tool
  depends on is already covered by a dedicated unit test added in D1,
  `ui_screenshot_tool_result_image_converts_to_provider_image_block` in
  `crates/copperclaw-providers/src/anthropic.rs`; see the new fixture's
  README.md for the full reasoning.
### Added (M20 Q3 — `diagnostics`: structured lint/typecheck output)

- All code feedback previously went through `shell`, whose stdout/stderr are
  head-truncated at 32 KiB per stream
  (`crates/copperclaw-mcp/src/tools/computer_use.rs::SHELL_OUTPUT_CAP`) — a
  long `tsc` error list truncated exactly where the useful errors were, and
  the model burned turns re-running with `tail_bytes` to see what it missed.
  A new first-party `diagnostics` MCP tool
  (`crates/copperclaw-mcp/src/tools/diagnostics.rs`) fixes this: given a
  project path, it detects which of eslint/tsc/ruff apply (config-file
  presence or a bounded file-extension sniff via `ignore::WalkBuilder`, same
  crate `glob`/`grep` already use), runs each in machine-readable mode
  (`eslint -f json`, `tsc --pretty false`, `ruff --output-format json`), and
  returns a **structured, capped digest** — per-file error/warning counts,
  the first N (default 40, caller-tunable up to 200 via `max_findings`) full
  diagnostics with message/`file:line`/rule, and totals — never a raw dump.
- **Read-only analysis with no gate interaction.** `.copperclaw/verify`
  (M20 Q2's multi-stage gate) remains the sole enforcement path;
  `diagnostics` never touches the dirty/stage/fix-cycle markers — it's a
  fix-cycle accelerator, and the tool description says so explicitly.
- A tool whose binary isn't on `PATH` (probed at CALL TIME via `command -v`,
  mirroring `ui_screenshot`'s chromium probe — never a registration-time
  check) degrades to a per-tool `"not_available"` note naming the
  `prototyping` image profile, never a hard tool-call error; a project with
  no applicable tool at all reports a single clean "nothing to run" note
  instead of erroring.
- **Registered in Coding/Full tool profiles** (`crates/copperclaw-runner/src/policy.rs`:
  `CODING_TOOLS`), guest-denied like `ui_screenshot` (spawning
  linter/typechecker subprocesses is a resource cost a read-only sender
  shouldn't get for free) and NOT a credentialed external action (purely
  in-container subprocess analysis, no taint/autonomy gating needed).
- Unit-tested against captured JSON/text samples (no live `eslint`/`tsc`/
  `ruff` needed, so CI is green without the `prototyping` image): a 40-error
  synthetic `tsc` run digests to well under the 32 KiB shell-truncation cap;
  a Python-only fixture routes to `ruff` only; a fixture with no applicable
  tool reports cleanly; a nonexistent binary name degrades via `probe_binary`
  determinism. Two `#[ignore]`d live integration tests
  (`diagnostics_live_tsc_end_to_end`, `diagnostics_live_ruff_end_to_end`)
  exercise the real subprocess + parse path against a genuine `tsc`/`ruff`
  on `PATH`, per the `ui_screenshot_docker_end_to_end` precedent.

### Added (M20 Q7 — `delegate_batch` contract + post-merge integration verify)

- `delegate_batch` (`crates/copperclaw-mcp/src/tools/agents.rs`) gains two
  OPTIONAL args. `contract`: a parent-authored shared brief (interfaces,
  file-ownership map, naming conventions) prepended VERBATIM to every
  worker's `instructions`, followed by a directive telling the worker to
  persist it to `.copperclaw/CONTRACT.md` in its own `/workspace` before its
  first edit — so the brief survives that worker's own compaction (the
  state-dir dirty-mark exemption for this exact file was already anticipated
  in `verify_gate.rs`'s M20 Q2 doc comment). `project`: the PARENT's own
  project directory (the one it `cd`'d into before delegating); when set and
  that project has a recorded `.copperclaw/verify`, the join marks it dirty
  via the EXISTING Q2 verify-gate machinery (`verify_gate::project_root_of` +
  `verify_gate::recorded_verify_command` + `verify_gate::mark_dirty`, all
  already-public functions — no new gate machinery) — the parent cannot
  complete its integration todo until every stage re-passes against the
  MERGED worker branches, not just each worker's isolated branch. Dirty-
  marking is skipped when `verify_gate_enabled()` is false (mirroring the
  Q6 `verify_gate=off` escape hatch — one hatch, not two) and when the batch
  fully spawn-failed (nothing ran, nothing to re-verify).
- **Full back-compat.** Both args are optional and independently gated: a
  batch called with neither behaves byte-identical to pre-Q7 (instructions
  unchanged, no verify-gate state touched) — all pre-existing
  `delegate_batch` tests pass unmodified.
- The tool description and `skills/create-agent/SKILL.md` teach the
  pattern: write the contract first, one component per worker (split by the
  contract's file-ownership map so workers never collide), verify the union
  by passing `project` so the merged tree gets re-verified, not just each
  worker's isolated branch. No reviewer-role worker (deferred per the M20
  plan — Q6's parent-side `self_review` covers the read).
- Unit-tested (mirroring the existing `delegate_batch` mock-provider
  harness): a 3-worker batch each receives the contract verbatim-prefixed
  plus the `.copperclaw/CONTRACT.md` persistence directive; post-join a
  named parent project with a recorded verify is marked dirty; a project
  with no recorded verify, a batch with no `project`, a fully spawn-failed
  batch, and a `verify_gate=off` session all leave the verify-gate state
  untouched; a batch with no `contract` sends instructions byte-identical
  to before.
### Added (M20 Q6 — Enforced self-review gate before final delivery)

- `skills/code-review/SKILL.md` carried real diff-review discipline but was
  orphaned — nothing in the build loop ever invoked it, and nothing forced
  the agent to read its own diff before declaring a prototype ready. A new
  first-party `self_review` MCP tool
  (`crates/copperclaw-mcp/src/tools/self_review.rs`) finally wires it in,
  two-phase: **READ** (`project` only) returns the project's diff since the
  last review marker (or since the project's first commit, if never
  reviewed), capped/chunked via `offset`/`limit` bytes so the model actually
  reads it; **SUBMIT** (`findings`, a non-empty array of concrete issue
  strings, or an explicit `no_findings: true`) writes
  `<project>/.copperclaw/reviewed`.
- **Enforced, not a prompt ritual.** `crates/copperclaw-mcp/src/tools/todo.rs`'s
  completion gate refuses to mark the **final/delivery todo** (the last
  remaining `pending`/`in_progress` item) `completed` while its project is
  dirty-since-review, naming `self_review` and `load_skill("code-review")`
  in the refusal — same enforcement mechanics as the M18 R3 / M20 Q2 verify
  gate. Non-final todos are never review-gated (per-increment review stays a
  prompt-level habit, M20 Q4). `REVIEW_CYCLE_CAP = 2`: two refused
  completion attempts burn the budget, and a third attempt while still
  dirty-since-review auto-transitions the todo to `blocked` instead of
  refusing forever, mirroring `FIX_CYCLE_CAP`'s cap-exhaustion behaviour.
  `verify_gate=off` groups skip this gate too — one escape hatch, not two.
- **Dirty-since-review is a content hash, not a new marker-file flag.**
  `.copperclaw/reviewed` records the commit the review was anchored to
  (`base`) plus a sha256 of the diff from that base to the working tree at
  submission time; a later check recomputes the same diff and compares
  hashes. This needed zero edits to `verify_gate.rs`'s
  `mark_dirty_for_write` or any of the write-family tools
  (`edit_file.rs`/`computer_use.rs`/`multi_edit.rs`/`apply_patch.rs`) —
  "findings the agent fixes re-dirty the project" falls out for free, since
  an edit changes the working tree and thus the recomputed hash. A project
  that isn't a git repository (or is a bare repo) is never gated by this at
  all — fails open rather than newly blocking a project that skipped `git
  init`.
- **Registered in Coding/Full tool profiles**
  (`crates/copperclaw-runner/src/policy.rs`: `CODING_TOOLS`), guest-denied
  like `diagnostics`/`ui_screenshot` and NOT a credentialed external action
  (in-container git-diff analysis plus a local marker-file write only).
- Unit-tested end to end against real tempdir git repos (no live provider
  needed): never-reviewed refusal with the teaching hint; completion
  succeeding after a submission; a post-review edit re-dirtying and
  re-refusing; the cap auto-blocking on the third attempt; non-final todos
  never gated; `verify_gate=off` byte-stable; a non-git project never
  gated. A new replay fixture,
  `fixtures/cli/prototype-self-review-gate/` (registered in
  `crates/copperclaw-host/tests/replay.rs` as
  `cli_prototype_self_review_gate_refuse_review_pass`), pins the
  refuse → `self_review` (read) → `self_review` (submit) → completion
  shape through the real pipeline.

### Added (M20 X-rider Wave 2 — craft fixtures, testing)

- Assessed all three Wave-2 X-rider fixture wishes (self-review gate / Q6,
  `delegate_batch` contract + post-join dirty / Q7, compaction digest / Q8)
  against the CLI replay harness (`crates/copperclaw-host/tests/replay.rs` +
  `tests/replay/harness.rs`). **No new fixture added** — one wish was
  already covered, the other two were investigated and found genuinely not
  replay-expressible for two distinct, precise reasons (below). Per this
  card's honesty rule, a smaller correct deliverable (this documentation)
  beats a fixture that fakes the shape.
  - **Wish 1 (self-review refuse → review → completion, Q6): already
    shipped, verified, not duplicated.** Q6's own PR landed
    `fixtures/cli/prototype-self-review-gate/`, registered as
    `cli_prototype_self_review_gate_refuse_review_pass`. Re-confirmed it
    exists, is registered, and passes (`cargo test -p copperclaw-host
    --test replay cli_prototype_self_review_gate`) — 1 passed, 0 failed.
    It already pins the exact refuse → `self_review` (read) → `self_review`
    (submit) → completion shape the wish asks for; adding a second fixture
    for the same shape (e.g. a `findings`-instead-of-`no_findings` variant)
    would add turns without exercising a new pipeline path, the same
    reasoning that fixture's own README already gives for not re-covering
    the post-review re-dirty / cap-exhaustion legs.
  - **Wish 2 (`delegate_batch` contract propagation + post-join dirty,
    Q7): NOT replay-expressible — real worker spawn is structurally
    outside the harness.** `delegate_batch`'s real implementation
    (`RunnerToolCtx::run_delegate_batch`, `crates/copperclaw-runner/src/
    tools.rs`) requires `ToolContext`'s inbound handle to be wired via
    `.with_join(...)`; the replay harness's `run_one_turn`
    (`tests/replay/harness.rs`) constructs `RunnerToolCtx::new(...)` and
    never calls `.with_join(...)`, so a scripted `delegate_batch` call
    would deterministically hit the "delegate_batch join is not wired in
    this context" refusal rather than exercising the real path. Even if
    wired, the real mechanism is a host-brokered Docker container spawn
    per worker plus the parent block-polling its own `inbound.db` for each
    worker's `delegate_result` row over real wall-clock time — exactly the
    async, multi-process, real-timing infrastructure the harness (one
    mocked runner turn per scripted step, wiremock standing in for the
    provider) was built to bypass. It has no concept of a second,
    concurrently-running worker session with its own scripted provider
    turns. Fully unit-covered instead, correctly at the `MockToolContext`
    layer, in `crates/copperclaw-mcp/src/tools/agents.rs`:
    `delegate_batch_contract_prepended_to_every_worker`,
    `delegate_batch_no_contract_is_byte_identical_to_pre_q7`, and
    `delegate_batch_post_join_marks_parent_project_dirty` (plus five
    sibling tests covering no-project/no-verify-file/all-spawn-failed/
    verify-gate-off edge cases).
  - **Wish 3 (compaction digest with stages + decisions, Q8): NOT
    replay-expressible — but for a harness-wiring reason, not a
    fundamental one.** Unlike Q7, compaction *can* be triggered
    deterministically: both the `/compact` slash command and the
    `compact_now` MCP tool call `compaction::compact()` directly,
    bypassing the token-threshold check entirely (`crates/
    copperclaw-runner/src/run/mod.rs`'s sentinel handling, lines ~781-807)
    — the only gate left is `compact()`'s own `history.len() < 4` no-op
    guard, so a handful of scripted turns is sufficient, and the
    wiremock-captured request bodies on the following turn could in
    principle show the pinned header verbatim, exactly the mechanism
    `prototype-self-review-gate`'s own test already uses
    (`harness.anthropic_server.received_requests()` + `.contains(...)` on
    the concatenated bodies). The blocker is that `tests/replay/
    harness.rs`'s `run_one_turn` wires `CompactionCfg.data_root` to the
    harness's own internal `SessionPaths` root
    (`self.tempdir.path()/sessions/<ag>/<sess>`) — and `self.tempdir` is a
    fresh `tempfile::tempdir()` generated inside `ReplayHarness::new()`,
    a different random path every test run (empirically confirmed: two
    boots of the same fixture printed `/tmp/.tmpD1N6Km` and a different
    path on a re-run). That root is structurally decoupled from
    `COPPERCLAW_DATA_ROOT`, the env var `verify_gate`/`todo`/`self_review`
    actually resolve project state against (and the only override those
    tools honor) — so any project + multi-stage `.copperclaw/verify` +
    `DECISIONS.md` a fixture scripts (the same way Q6's fixture does,
    via the `COPPERCLAW_DATA_ROOT` re-exec seam) would be invisible to
    `build_project_facts_header`'s `scan_projects`/`read_todos`, and
    compaction would silently pin an empty/no-op header regardless of
    what state was set up — a false-negative fixture, not a working one.
    The existing re-exec workaround can't bridge this either: it needs
    the target path known *before* spawning the child process, but the
    harness's tempdir is only generated *after* `ReplayHarness::new()`
    runs inside that very child — an unresolvable chicken-and-egg.
    Closing the gap needs a `tests/replay/harness.rs` change (e.g.
    threading a fixture-configurable data root into `CompactionCfg`,
    or defaulting it to `COPPERCLAW_DATA_ROOT`) — out of this card's
    declared scope (`fixtures/**` + `tests/replay.rs` registration only;
    harness-file changes are the kind of pipeline-code change this card
    is meant to surface, not make). Fully unit-covered instead, including
    the exact "headline" case run through the real `compact()` entry
    point, in `crates/copperclaw-runner/src/compaction.rs`:
    `compacted_coding_session_pins_inventory_stages_and_decisions`,
    `facts_header_includes_file_inventory_from_git_ls_files`,
    `facts_header_pins_every_verify_stage_not_just_the_first`,
    `facts_header_includes_decisions_tail_when_present`, and five
    sibling cap/absence tests.

### Added (M19 A3 — Public-tunnel model verb: activate V5)

- The merged-but-dormant V5 public-tunnel module now has an agent-facing verb.
  A new first-party `make_preview_public` tool lets the agent publish a **live**
  session preview (one already exposed with `expose_preview`) to the public
  internet through an operator-provided cloudflared tunnel — the "send it to my
  cofounder" hand-off. It relays through the SAME reserved `__preview` MCP path
  as `expose_preview` (`crates/copperclaw-runner/src/run/preview.rs`:
  `MAKE_PREVIEW_PUBLIC`, `is_preview_tool`, `preview_tool_defs`), but the
  delivery loop routes it to a new host-side broker
  (`crates/copperclaw-host-delivery/src/service.rs`: `execute_preview_call`'s
  `make_preview_public` arm → `copperclaw_modules::PublicTunnelBroker`) instead
  of the preview broker.
- The host implementation (`crates/copperclaw-host/src/preview.rs`:
  `PublicPreviewTunnel`) bridges the preview subsystem (which owns the container-
  port → live host-port + gating-token mapping, via the new
  `PreviewManager::live_proxy_for`) and the merged V5 `TunnelBroker` (which owns
  the approval gate + the cloudflared binary). On the first call it raises the
  `CredentialedExternalAction` approval V5 already implements and returns a
  "pending approval" note; after an operator taps Approve, the retry stands up
  the tunnel and returns the shareable **tokened** public URL
  (`https://<tunnel>/__preview/<token>` — the public tunnel fronts the
  token-gated proxy, so a tokenless hit 403s: defence in depth). The agent puts
  that URL on its "prototype ready" delivery card as an "Open the public link"
  button.
- **Secure-by-default, fail-closed.** Public tunnels are OFF unless the host env
  master switch `COPPERCLAW_PUBLIC_TUNNEL_ENABLED` is set AND the group has
  previews enabled; every public exposure still requires an explicit operator
  approval (no auto-exposure). `make_preview_public` is a
  `CredentialedExternalAction` in `crates/copperclaw-runner/src/policy.rs`
  (`CODING_TOOLS` + `CREDENTIALED_EXTERNAL_TOOLS`) — taint-gated AND
  autonomy-gated, and deliberately **NOT** in `LAN_PREVIEW_TOOLS` (the A7 LAN
  taint exemption never reaches it: the outward-facing contrast to
  `expose_preview`). An absent cloudflared binary yields a clean, copy-pasteable
  install error, never a panic or hang.
- **Auto-teardown with the preview.** `PreviewManager` now holds the tunnel
  broker (`set_tunnel_broker`) and tears down any public tunnel fronting a
  preview when the preview is closed, session-stopped, shut down, or
  idle-tombstoned — no public tunnel outlives the app it fronted; re-sharing
  later earns a fresh approval (V5's grant is one-shot). Wired at boot in
  `crates/copperclaw-host/src/boot.rs`. The `preview` skill
  (`skills/preview/SKILL.md`) teaches the verb, the approval ritual, and the
  ritual-card public-URL button. Metric wish (M1 rider): public-tunnel
  exposures/pending/denied counts.

### Changed (M19 A7 — Preview-exposure provenance refinement)

- Exposing a **LAN-only** session preview (`expose_preview` / `close_preview`)
  is no longer blocked by the M16 coarse **taint** gate. M18's live post-mortem
  found that a "research … then build" request web-tainted the turn, so the
  provenance gate denied `expose_preview` as a credentialed external action —
  the operator's preview didn't appear until a fresh untainted turn, a felt UX
  papercut. A LAN preview stands up a listener reachable only on the operator's
  own machine / LAN: a *local surface*, not the same risk class as
  `web_search` / `web_fetch` / a public tunnel routing the agent's credentials
  at an attacker-chosen target. New `LAN_PREVIEW_TOOLS` const + `is_lan_preview`
  helper in `crates/copperclaw-runner/src/policy.rs` carve the two LAN verbs out
  of the taint half of the provenance/autonomy gate; a web-tainted turn can now
  `expose_preview` without a fresh approval. The verbs stay gated by every other
  layer — the guest role floor (they are mutating), the coding/full profile
  ceiling, the per-group preview opt-in (host-side), and the **autonomy** gate
  (an autonomous/heartbeat turn still may not stand up a listener with no human
  present). The durable boundary is documented in code: **LAN preview = local
  surface** (taint-exempt); **public tunnel = external** (NOT exempt). An
  outward-facing public/tunnel verb such as `make_preview_public` (M19 A3) is
  deliberately absent from the carve-out and stays fully taint-gated even though
  it rides the same preview subsystem — a robustness test asserts the public
  verb is never taint-exempt so A3's verb inherits full gating the moment it
  lands. Audit rows and approval flows are unchanged.

### Added (M19 A2 — Interactive browser, Phase 5b, demand-pull + opt-in)

- The headless browser gains interactive actions (click / type / scroll /
  wait-for-selector) as an incremental extension of the existing read-only
  live path — behind a STRICTER, SEPARATE opt-in flag,
  `COPPERCLAW_BROWSER_INTERACTIVE`, distinct from `COPPERCLAW_BROWSER_ENABLED`.
  Both must be truthy for the capability to exist; with the interactive flag
  unset the new `browser_interact` tool is **not even registered** (see
  `crates/copperclaw-mcp/src/tools/mod.rs::build_tool_set`), so the tool set,
  schemas, and behaviour are byte-identical to the read-only baseline.
  - New `crates/copperclaw-browser/src/interactive.rs`: `InteractiveAction`
    (`click` / `type` / `scroll` / `wait_for_selector`, capped at 32 actions
    per call, 8 KiB typed-text cap), the `InteractiveDriver` seam, and the
    `interact` orchestration. `interact` re-runs the SSRF `NavigationGuard` on
    **every** navigation an action can trigger — the settled
    `document.location.href` (async, DNS-resolving `guard_target`, catching a
    redirect-less JS navigation to an internal address) **and** every redirect
    hop — before the post-interaction DOM is ever read or returned.
  - `crates/copperclaw-browser/src/cdp.rs`: `InteractiveDriver` impl for
    `CdpBrowserDriver` (`Page.enable`/`Network.enable`/`Page.navigate` +
    `Runtime.evaluate`-driven click/type/scroll/wait, selector + typed text
    JSON-encoded into the JS expression so a hostile selector cannot inject
    code). `crates/copperclaw-browser/src/live.rs`: `interact_live` — spawns
    the SAME locked-down child container (no broker token, deny-default egress
    scoped to the target, unprivileged user, hardened sandbox), drives the
    interaction, and tears the container down unconditionally.
  - New `crates/copperclaw-mcp/src/tools/browser_interact.rs` MCP tool:
    demand-pull only (one call, one page, one bounded action list — no
    autonomous browsing loop), output tagged `Provenance::Untrusted` (the turn
    is marked untrusted up front like `browser_render`), and the initial
    target SSRF-pre-flighted before any spawn. Non-goals kept explicit in code
    + docs: NO always-on interactive browsing, NO browser-writes-memory.
  - No egress-policy weakening: the child container spec is reused verbatim
    from `build_browser_container_spec`. Like the read-only `browser_render`,
    the interactive verb is a `Full`-profile-only tool (it appears in no lower
    profile tier), so restricted-profile and guest senders never receive it.
  - Metrics reuse the existing browser counters (`inc_browser_ssrf_block` with
    new `interactive_*` stage labels; `inc_browser_render{mode="interactive"}`;
    the child spawn/teardown/CDP-connect counters via `interact_live`); a
    dedicated `browser_interactive_actions_total{action,outcome}` counter is a
    noted metric wish for a future `copperclaw-metrics` change.

### Added (M19 X-rider Wave 2 — channel-parity replay fixtures)

- Test-only. Five replay fixtures locking the host-delivery-pipeline half of
  the Wave-2 adapter-floor cards, each registered in
  `crates/copperclaw-host/tests/replay.rs`: `fixtures/signal/hud-breadcrumb`
  (U1 — live HUD posts one breadcrumb chip + edits it in place on signal),
  `fixtures/teams/hud-live-edit` (U2 — teams now edit-capable, one message
  edited not N posted), `fixtures/gchat/approval-card` (U4 — a `send_card`
  reaches `deliver_card` as a structured card, buttons intact, not flattened
  prose), `fixtures/deltachat/card-and-todo` (U5 — native card + todo chip on
  a formerly-bare interactive surface; re-execs under `COPPERCLAW_DATA_ROOT`
  like the F4/X2 fixtures so the todo store is writable), and
  `fixtures/slack/reaction-inbound` (U7 — a normalized `reaction_added`
  bypasses the mention gate as a non-trigger `content.reaction` row, proving
  the router reaction leg is channel-agnostic, complementing the telegram
  twin). Adds a `model_rich_cards` manifest flag + `CappedAdapter` modelling
  (mirroring `model_rich_breadcrumbs`) so the harness can prove a card is
  delivered structurally rather than degraded host-side. Per-adapter *wire*
  rendering stays the adapters' own unit-test concern.

### Added (M19 X-rider Wave 3 — capability fixtures)

- Test-only. Fixtures/tests locking the Wave-3 capability cards (A1/A3/A7).
  **A3** (public-verb ritual card): a new replay fixture
  `fixtures/cli/prototype-public-share` (registered in
  `crates/copperclaw-host/tests/replay.rs` as
  `cli_prototype_public_share_ritual_card_has_public_button`) drives
  `expose_preview` → `make_preview_public` → `send_card` end-to-end and pins the
  prototype-ready ritual card gaining an "Open the public link" button pointing
  at the public tunnel URL. Adds a `"tunnel"` harness gate + `FixtureTunnelBroker`
  (mirroring the `"preview"` gate + `FixturePreviewBroker`) so `make_preview_public`
  routes to a canned post-approval public-URL reply. **A1** (fan-out aggregate):
  `delegate_batch_partial_worker_failure_surfaces_in_aggregate` in
  `crates/copperclaw-runner/src/run/tool_dispatch.rs` drives a mixed batch (2
  workers report, 1 fails to spawn) through the real `invoke_tool` dispatch and
  asserts ONE aggregate carrying both reports + the failed worker's per-worker
  error, not a lost turn or total refusal. **A7** (preview-taint reclassification):
  `tainted_turn_exposes_lan_preview_but_is_blocked_from_public` drives a
  web-tainted turn through dispatch, proving `expose_preview` (LAN) succeeds
  without a fresh approval while `make_preview_public` stays taint-gated and never
  queues a relay row. A1/A3/A7 approval + tunnel-broker internals remain covered
  by their own host-handler / policy unit tests; these are the pipeline-level
  and dispatch-level X-rider complements.

### Added (M19 A6 — Durable scheduled-task fire lifecycle, migration 028)

- Scheduled tasks now carry a durable, queryable record of their firing
  history. The first-class `tasks` table (migration `010_tasks`) already
  backs all six scheduling tools (`schedule_task` / `list_tasks` /
  `cancel_task` / `pause_task` / `resume_task` / `update_task`) via the
  `SchedulingModule` → `SqliteTaskStore` write path and the sweep's due-task
  fan-out (`copperclaw-host-sweep::checks::scheduling`), but it recorded no
  trace of a *fire* — the sweep bumped `next_fire` (recurring) or flipped
  `status` to `completed` (one-shot) and moved on. Migration
  `028_tasks_fire_lifecycle` adds two columns the sweep now writes on every
  fire:
  - `last_fired_at` — RFC-3339 instant of the most recent fire (`NULL` until
    first fire).
  - `fire_count` — monotonically increasing fire count (`0` until first
    fire); a recurring task accrues one per occurrence.
  Wired in `crates/copperclaw-db/src/tables/tasks.rs` (new `mark_fired`
  helper; `Task` gains the two fields) and
  `crates/copperclaw-host-sweep/src/checks/scheduling.rs` (calls `mark_fired`
  on each fire). Existing rows backfill to `NULL` / `0` via the column
  defaults and continue to fire unchanged (proved by a migration test in
  `crates/copperclaw-db/src/migrate.rs` and sweep tests). This gives
  operators, the metrics rider (M1's "scheduled-task lifecycle" wish), and a
  future event-driven-trigger follow-up a durable view of autonomous activity
  without scanning the message log. Event-driven triggers themselves remain a
  follow-up (the seam is the `tasks` table itself — a future trigger source
  writes rows and the same sweep fan-out fires them).

### Added (M19 A5 — Agent-facing memory write, `memory_save`)

- The M16 Phase-3 group memory store (per-group `memory.db`, FTS5 + cosine,
  provenance-tagged) was in-container READ-ONLY (`memory_search` / `memory_get`);
  the agent could recall but not deliberately remember. New `memory_save` tool
  closes the loop, letting the agent persist a fact into the group store on
  purpose so it survives across sessions.
  - Pure handler + schema + registration in
    `crates/copperclaw-mcp/src/tools/memory.rs` (new `memory_save` module) and
    one line in `crates/copperclaw-mcp/src/tools/mod.rs`. New `MemorySaveSpec`
    (`key`, `body`, optional `source` — deliberately NO caller-supplied
    provenance), `MemorySaveOutcome`, and the shared `resolve_save_provenance`
    taint→provenance rule in `crates/copperclaw-mcp/src/context.rs`, plus a new
    `ToolContext::memory_save` trait method (default returns a Context error, so
    mock / subagent contexts are unchanged).
  - The write reaches the same per-group `MemoryStore` the runner already reads:
    `RunnerToolCtx::memory_save` in `crates/copperclaw-runner/src/tools.rs`
    upserts via `MemoryStore::upsert` (text-only embedding, exactly as
    `memory_search` reads text-only today — vector generation stays deferred
    until the embedding broker lands).
  - SECURITY (provenance stays honest): the agent cannot request a provenance.
    The runner's impl decides it from its OWN per-turn taint flag
    (`is_context_tainted`, the same signal the coarse provenance gate reads): a
    turn tainted by untrusted-provenance content (a `web_fetch` body, an
    untrusted memory hit) is honestly DOWNGRADED to `untrusted` — nothing lets an
    untrusted turn launder external content into trusted memory. The taint
    decision is the store-side impl's, not the caller's, so a future handler bug
    cannot bypass it. `memory_save` is also classified mutating in
    `crates/copperclaw-runner/src/policy.rs` (new `MEMORY_WRITE_TOOLS`), so a
    read-only guest sender is denied it (a guest must not write a trusted fact).
  - Caps: `body` ≤ 8 KiB (rejected in the pure handler, the load-bearing guard
    against dumping a document / injection payload into the store), `key` ≤ 256
    chars, `source` ≤ 256 chars, and a per-session rate cap of 100 writes
    enforced by the runner ctx (`MAX_SAVES_PER_SESSION`).
  - Tests: mcp unit tests (`tools/memory.rs`) exercise write→retrieve with
    `trusted` provenance, tainted-turn downgrade to `untrusted`, the oversized-body
    cap, empty key/body rejection, and the per-session rate cap against a stateful
    reference context; runner tests cover the real store round-trip + downgrade
    (`run/tool_dispatch.rs`) and the guest mutating floor (`policy.rs`).

### Changed (M19 U6 — Adopt the shared `core/markdown` renderer in adapters, 2026-07-16)

- `copperclaw_channels_core::markdown::render(md, Flavor)` existed and was
  tested but **no adapter consumed it** — every adapter either hand-rolled its
  own markdown→flavor formatter or passed the agent's canonical Markdown to the
  wire raw, leaving `Flavor::{Discord,Slack,Mattermost,WhatsApp}` as dead
  capability and a standing drift risk. U6 routes each adapter's plain-text
  outbound path through the shared renderer (the U1–U5 rich-surface renderers /
  field escapers are unchanged):
  - **telegram** (`crates/copperclaw-channels/telegram/src/adapter.rs`): the
    bespoke `markdown_to_html` renderer (plus its `find_fence_close`,
    `replace_paired`, `replace_italic_paired`, `replace_inline_links` helpers)
    is deleted; the HTML send path now calls `render(_, Flavor::Html)`. Behaviour
    is byte-identical for bold/italic/strike/code/links/fences; the shared
    renderer additionally formats headings (`# x` → `<b>x</b>`), blockquotes, and
    normalises `-`/`*`/`+` bullets to the `•` glyph (previously left literal) —
    the affected unit test was updated to pin the new bullet output.
    `escape_markdown_v2` (`api.rs`) is kept: it escapes the MarkdownV2 photo
    caption, a dialect the renderer has no `Flavor` for.
  - **discord** (`adapter.rs` `render_outbound_text`): now
    `render(_, Flavor::Discord)` — CommonMark passes through with `*`/`+` bullets
    normalised to `-`. `escape_discord_markdown` (rich tool-summary escaper) kept.
  - **slack** (`adapter.rs` `deliver`): the plain-text send now calls
    `render(_, Flavor::Slack)` (`**bold**` → `*bold*`, headings degrade to bold,
    links → `<url|text>`); skipped when the agent supplied explicit Block Kit
    `blocks` (a rich surface). `escape_mrkdwn` (rich field escaper) kept.
  - **mattermost** (`adapter.rs` `deliver` "post" action):
    `render(_, Flavor::Mattermost)`. The `render.rs` rich-card renderers unchanged.
  - **whatsapp-cloud** (`adapter.rs` `deliver`): `render(_, Flavor::WhatsApp)`
    (`*bold*`/`_italic_`/`~strike~`/monospace), replacing the raw passthrough.
    The `render.rs` rich-card renderers unchanged.
  - **matrix** (`adapter.rs` `deliver` + new `api.rs` `send_threaded_html`): the
    plain-text path with no agent-supplied HTML now renders
    `render(_, Flavor::Html)` and carries it as `formatted_body` (raw text as the
    fallback `body`), for both top-level and threaded sends. The rich-surface
    `escape_html_matrix` helpers are unchanged.
- **gchat** is intentionally left on raw passthrough: Google Chat text formatting
  (`*bold*`/`_italic_`/`~strike~`, no headings, no `[text](url)` links) matches no
  existing `Flavor` exactly, and adding a `Gchat` flavor is out of U6 scope (the
  card is "consume the renderer, don't rewrite it"). Its `escape_html_gchat`
  helper is rich-surface (Cards v2 HTML) and unaffected. Follow-up: add a `Gchat`
  flavor to `core/markdown` so it can adopt the shared path too.

### Added (M19 A1 — Single-call parallel fan-out with a join, 2026-07-16)

- New `delegate_batch` MCP tool: spawn N `delegate` build workers IN PARALLEL
  and BLOCK the parent turn until they all report (or a budget elapses),
  returning the aggregated per-worker results as ONE tool response. Closes the
  biggest structural capability gap M18 left — fan-out today is N separate
  `delegate` calls whose results arrive asynchronously on later turns, with no
  single-call "spawn N workers and await their joined results" primitive. A
  parent that wants to build three components in parallel and assemble them can
  now block on all three in one turn.
  - Tool half (lane T): `delegate_batch` schema + validation + aggregation
    in `crates/copperclaw-mcp/src/tools/agents.rs`, registered in
    `build_tool_set`. Input is a `workers: [{name, instructions}]` list plus an
    optional `timeout_secs`. Fan-out is **capped at 6 workers per call**
    (`MAX_DELEGATE_BATCH_WIDTH`); the join budget defaults to 300s and is capped
    at 600s (`DEFAULT_/MAX_DELEGATE_BATCH_TIMEOUT_SECS`), both comfortably under
    the runner's per-tool deadline so the join returns a partial aggregate rather
    than being hard-aborted. The request/outcome types + a new
    `ToolContext::run_delegate_batch` trait method (default: unsupported) live in
    `crates/copperclaw-mcp/src/context.rs`; `MockToolContext` implements it for
    handler tests.
  - Runner join seam (lane R): `RunnerToolCtx::run_delegate_batch`
    (`crates/copperclaw-runner/src/tools.rs`) reuses the EXACT single-`delegate`
    spawn machinery — it emits N `OutboundToolEffect::Delegate` rows (same
    depth/permission caps, same containment: each worker lands with a NULL
    messaging group and reports ONLY back to the parent, never into the user's
    chat) — then hands off to `crate::run::delegate_batch::join_workers`
    (`crates/copperclaw-runner/src/run/delegate_batch.rs`), which BLOCK-POLLS the
    parent's own `inbound.db` for each worker's `delegate_result` spawn row and
    its final report (a `Chat` row keyed by the worker's child
    `source_session_id`), marking every consumed row `completed` so it never
    re-triggers a spurious parent turn. This mirrors the external-MCP block-poll
    pattern (`run::external_mcp`) — the parent turn yields to the tokio runtime
    (HUD ticker + heartbeat stay alive) and resumes on the joined result. Wired
    via a new `RunnerToolCtx::with_join(inbound)` in
    `crates/copperclaw-runner/src/main.rs`.
  - Isolation + failure handling: each worker still gets its OWN container +
    writable `sib/<id>` git worktree (no cross-write), exactly as a lone
    `delegate`. A worker that fails to spawn (depth cap / permission gate) or
    never reports in time surfaces as a per-worker `error` in the aggregate — it
    never loses the whole batch turn. A batch where EVERY worker was refused
    (e.g. a `delegate_batch` from a max-depth child) surfaces as a single tool
    error ("delegate_batch refused: …"), not a partial aggregate. Coverage:
    handler + join unit tests, plus an end-to-end `invoke_tool` fan-out/join
    integration test with a fake host (3 workers → one aggregated result;
    per-worker failure aggregation; depth-cap refusal) in
    `crates/copperclaw-runner/src/run/tool_dispatch.rs`.

### Added (M19 A4 — Agent-authored persistent skills, `save_skill`)

- Closed the `write_file` → discovery loop: an agent can now durably save a
  reusable skill for its FUTURE sessions via a guarded, approval-gated
  `save_skill` capability. Skills remain host-discovered and symlink-
  materialized at spawn, so a saved skill lands where discovery already scans
  and is picked up on the next session — no new discovery machinery.
  - New capability core in `copperclaw-skills`
    (`crates/copperclaw-skills/src/save.rs`): `validate_skill_content` (pure —
    reuses the discovery-time rules: frontmatter parse + `name`/`description`
    required, kebab-case name, frontmatter `name == dir`) and `save_group_skill`
    (validate + write `<group_skills_dir>/<name>/SKILL.md`). Containment reuses
    the same allowed-roots guard `materialize` applies (lib.rs:26-29): the
    canonical destination must lie under the configured root or the write is
    refused with `SkillError::EscapedRoot`.
  - New thin MCP tool `save_skill`
    (`crates/copperclaw-mcp/src/tools/save_skill.rs`, registered in
    `tools/mod.rs`; new `OutboundToolEffect::SaveSkill` +
    `SaveSkillSpec` in `context.rs`). It validates the proposed `SKILL.md`
    synchronously — an invalid one is refused HERE with the precise validation
    error, before any approval is raised — then emits the effect. Added to the
    runner's `SELF_MOD_TOOLS` (`copperclaw-runner/src/policy.rs`) so it is
    `full`-profile-only and barred from guests; it is deliberately NOT a
    credentialed-external tool (no egress).
  - Runner records the effect as a `save_skill` `MessageKind::System` row
    (`copperclaw-runner/src/tools.rs::apply_save_skill`).
  - Host delivery (`copperclaw-host-delivery/src/service.rs`) intercepts the
    row inline: it raises a `pending_approvals` row (action `save_skill`,
    idempotent on `(agent_group, name)`) carrying the validated body plus the
    host-computed `dest_dir` (`<groups_dir>/<ag>/skills`) and containment
    `allowed_root`, and dispatches an approve/deny card to the originating
    channel. Nothing is written to disk until approval. A new
    `set_groups_dir` (wired in `copperclaw-host/src/boot.rs`) supplies the
    per-group data root; with none configured the request is refused with a
    `self_mod_error` inbound rather than silently dropped.
  - On approval, the host's approvals handler
    (`copperclaw-host/src/handlers/approvals.rs::apply_save_skill`) re-validates
    and writes the `SKILL.md` (defense-in-depth at the security boundary); the
    next container spawn's skill scan discovers and exposes it — no rebuild.
  - New `skills/save-skill/SKILL.md` teaches the capability;
    `save_skill` added to the curated `REGISTRY_TOOLS` coverage mirror.
  - This is a capability, not a registry: skills are per-group only — no
    cross-group sharing, no ClawHub (a standing non-goal).
  - Metric wish (deferred to the M1 metrics rider — this card must not touch
    `copperclaw-metrics`): `copperclaw_skills_saved_total{outcome}`
    (saved / rejected). The save/refuse paths currently reuse the existing
    `inc_self_mod_succeeded` / `inc_self_mod_failed("save_skill")` counters.

### Added (M19 U4 — Native cards on gchat + matrix, 2026-07-16)

- Neither `gchat` nor `matrix` overrode the trait
  `ChannelAdapter::deliver_card`, so the M18 approval / ritual cards fell through
  to the trait-level `Card::to_text_fallback` and rendered as flat prose on both.
  Both now render the canonical [`Card`] natively:
  - `GchatAdapter` overrides trait `deliver_card`
    (`crates/copperclaw-channels/gchat/src/adapter.rs`), building a Google Chat
    Cards v2 card via the new `build_portable_card` helper and POSTing it through
    the existing `send_card` path (`cardId = "card"`). `title` → card
    `header.title`; `body` → a `textParagraph` widget (HTML-escaped, `\n` →
    `<br>`); `fields` → one `decoratedText` widget each; `image_url` → an `image`
    widget; `buttons` → a native `buttonList` — URL buttons open the link
    (`onClick.openLink`), callback buttons fire a `CARD_CLICKED` whose
    `onClick.action.function` carries the value (surfaced inbound by the events
    router), with `primary` / `danger` styles mapped to a button `color`. The
    `CardField.inline` hint has no Cards-v2 analog, so fields stay full-width —
    the one field that degrades to its text-fallback shape.
  - `MatrixAdapter` overrides trait `deliver_card`
    (`crates/copperclaw-channels/matrix/src/adapter.rs`), rendering the card as an
    `m.text` HTML event whose `formatted_body` is built by the new
    `render_card_html_matrix` helper. Matrix has no native button primitive, so
    buttons degrade to labelled links (the buttons-as-links fallback): `url`
    buttons become real `<a href>` anchors, `value` buttons render
    `label — <code>callback:value</code>` — the same shape as
    `Card::to_text_fallback` but HTML. `m.text` (not `m.notice`) so approval
    cards raise a notification, matching the error-card renderer; the plain-text
    `body` field carries the canonical text fallback for non-HTML clients. Builds
    on the F1 `edit_message` override already present on matrix (unchanged).
  - Unit + mock-server tests on both adapters cover the field/button mapping,
    HTML escaping, URL-vs-callback button rendering, and the text fallback.

### Added (M19 U5 — bare-adapter rich floor for deltachat + line, LINE postback inbound, 2026-07-16)

- Two genuinely interactive chat surfaces — `deltachat` (full chat, inbound
  files) and `line` (Messaging API) — had **zero** rich-surface support: the
  M18 HUD / diff / todo / approval cards all fell through to the trait's plain
  text-fallback. Both now render the U5-mandated floor natively (metered with
  `inc_adapter_rich_render`):
  - `deltachat` gains `crates/copperclaw-channels/deltachat/src/render.rs` and
    trait overrides for `deliver_card`, `deliver_diff`, and `deliver_todo_list`
    (`.../deltachat/src/adapter.rs`). Delta Chat is an e-mail transport with no
    reliable Markdown and no edit API, so the renderers emit clean, markdown-free
    plaintext (mirroring the Signal floor) and each surface posts a fresh chip.
  - `line` gains `crates/copperclaw-channels/line/src/render.rs` and the same
    three trait overrides (`.../line/src/adapter.rs`). A `Card` **with buttons**
    becomes a LINE `buttons` **template** message so the buttons are actually
    tappable — a callback button maps to a `postback` action carrying its `value`
    as `data`, a URL button to a `uri` action, a label-only button to a `message`
    action; the full card text rides `altText`. LINE's caps are enforced (<= 4
    actions, label <= 20, title <= 40, text <= 160/60). Diff and todo render as
    fence-free plaintext. The api (`.../line/src/api.rs`) gains
    `reply_message` / `push_message` (arbitrary message objects) + a
    `text_message` helper; the adapter now depends on `copperclaw-metrics`.
  - Both `deliver_todo_list` implementations render the F4
    `TodoItemStatus::Blocked` state with the `[!]` glyph plus the item's
    `blocked_reason` inline (`— blocked: <reason>`), so an auto-blocked step reads
    as blocked, not stuck "in progress".
- **LINE postback inbound is now wired** (`crates/copperclaw-channels/line/src/router.rs`,
  previously stubbed — a `type: "postback"` event was dropped with a `// For now
  ack` at line ~146). A postback event is now normalized into a `Chat`
  `InboundEvent` carrying `content.callback = { id, data }` (where `data` is the
  action's `postback.data`), mirroring the Telegram/Slack callback convention so
  it whitelists past the router's mention gate and reaches the approval
  interceptor — in-chat Approve/Deny taps route for the first time. The reply
  token is cached so the resolution reply uses the cheap reply path. The message
  and postback paths share an `emit_inbound` helper.
- **Outbound-only adapters left bare, deliberately** (so it isn't rediscovered):
  `resend`, `github`, `linear`, `x`, `webhooks`, `wechat`, `emacs`, and
  `imessage` are not interactive chat surfaces and were **not** touched — their
  surface doesn't warrant a rich floor (matches the M19 plan's deferred list).
- **Fixture note.** The replay harness injects pre-normalized `InboundEvent`
  JSON, bypassing adapter webhook parsing — so it cannot drive a raw LINE
  postback through the code that changed. Per the U5 card's documented
  alternative, the postback parse is covered by adapter-level router unit tests
  that POST a signed LINE postback webhook body through the real axum handler and
  assert the normalized callback event (`.../line/src/router.rs` tests). No
  `tests/replay.rs` registration was added.

### Added (M19 U7 — inbound reactions as agent-visible input, 2026-07-16)

- No adapter parsed inbound reaction events, so a user reacting 👍/✅/👀/❌/👎
  produced zero agent-visible signal — the most natural lightweight steering
  input was inert. M19 U7 wires reactions end-to-end (inbound → router →
  runner) as a lightweight steering signal, never a full turn:
  - New inbound-reaction contract
    (`crates/copperclaw-channels/core/src/reaction.rs`): a reaction is a
    `MessageKind::Chat` event whose `content.reaction { emoji, target_seq, actor }`
    carries the platform reaction token, the reacted-to message's platform id
    (`target_seq`), and who reacted. `reaction_content` builds it, `parse_reaction`
    reads it, and `classify_reaction` maps both unicode emoji (Telegram, Discord,
    WhatsApp) and Slack shortcodes onto the curated `ReactionSignal`
    (✅/👍 affirmative, 👀 looking, ❌/👎 negative); everything else carries no
    signal.
  - Four adapters parse their native reaction events into the contract:
    `telegram` `message_reaction` (`ingress/mod.rs` + new `MessageReactionUpdated`
    /`ReactionType` types; only genuinely-added emoji reactions emit — needs
    `"message_reaction"` in `allowed_updates`), `slack` `reaction_added`
    (`events/router.rs::convert_reaction` + `ReactionEvent`/`ReactionItem`),
    `discord` `MESSAGE_REACTION_ADD` (`events.rs::message_reaction_add_to_inbound`;
    unicode-emoji only, and `DEFAULT_INTENTS` gains `DIRECT_MESSAGE_REACTIONS`
    (1<<13) for DM reactions), and `whatsapp-cloud` `type:"reaction"` messages
    (`events/router.rs`; empty emoji = removed reaction = no event).
  - The router whitelists reactions past the mention gate exactly like button
    callbacks (`mention.rs::is_interaction_payload` now checks `content.reaction`)
    and persists them as **non-trigger** rows (`route.rs`), so a reaction never
    spawns a container on its own — it can never drive a spurious full turn.
  - The runner (`crates/copperclaw-runner/src/run/reaction.rs` +
    `run/drive_turn.rs`) treats a curated reaction on the agent's OWN last
    message — resolved by matching `target_seq` against the per-session
    `delivered` table's `platform_message_id` — as a one-line interjection
    folded in via the M18 R2 mid-turn steering seam (`check_mid_turn_steering`),
    within one tool-batch boundary. A reaction on an unrelated message, or an
    uncurated emoji, is consumed and ignored. A folded reaction marks the turn
    untrusted (`ToolContext::mark_untrusted_context`) — external content cannot
    launder trust into a credentialed external action. `run_loop` also consumes
    any reaction row picked up between turns so it never reaches the model as
    raw JSON.
  - Fixture `fixtures/telegram/reaction-steer` (registered in
    `crates/copperclaw-host/tests/replay.rs`) proves the inbound → router leg (a
    reaction bypasses a mention-gated group and lands as a pending non-trigger
    `content.reaction` row with no turn); the runner-side steering + ignore legs
    are covered by `mid_turn_reaction_on_own_message_folds_affirmative` /
    `mid_turn_reaction_on_unrelated_message_is_ignored` in `drive_turn.rs`.
  - Metric wish (for the M1 rider): inbound reactions by emoji/outcome
    (curated-affirmative/looking/negative vs. ignored-unrelated/uncurated). The
    mid-turn fold currently reuses `inc_midturn_control(_, "reaction")`.

### Added (M19 U2 — Teams in-place edit + reactions, 2026-07-16)

- The `teams` adapter rendered every rich surface (cards / diffs / collapsible /
  todo / thinking / errors) but never overrode the trait
  `ChannelAdapter::edit_message`, so it was absent from `EDIT_CAPABLE_CHANNELS`.
  The host HUD / approval path calls the **trait** method
  (`copperclaw-host-delivery/src/dispatch.rs`); with no override it fell through
  to the trait default (`AdapterError::Unsupported`) and every HUD "edit"
  degraded into a fresh message — new-message spam. Now:
  - `TeamsAdapter` overrides trait `edit_message`
    (`crates/copperclaw-channels/teams/src/adapter.rs`), resolving the
    `platform_id` to a channel post or chat message and PATCHing it in place via
    Microsoft Graph (`edit_channel_message` / `edit_chat_message`,
    `PATCH .../messages/{id}`). It emits the same
    `inc_hud_edit` / `inc_adapter_edit_message` metrics as the other
    edit-capable adapters (mirrors mattermost). N status updates now become N
    in-place PATCHes against one message id, not N POSTs.
  - `teams` is added to `EDIT_CAPABLE_CHANNELS`
    (`crates/copperclaw-channels/core/src/capabilities.rs`) in the same change,
    per the module's keep-in-sync rule, so the delivery loop routes HUD edits to
    the Teams edit path. The F1 drift guard
    (`copperclaw-host-delivery/tests/edit_capable_edit_message_drift.rs`) still
    passes with Teams listed.
  - `TeamsAdapter` overrides trait `add_reaction` (Graph `setReaction`, shortcode
    mapped by `emoji::shortcode_to_reaction_type`; unmapped shortcodes surface as
    `Unsupported`) so host-driven reactions reach Teams, and adds
    `plain_text_fallback` (strips the rich `html` content field, marks the body
    `[reduced formatting]`) so a body that trips a Graph formatting rejection can
    be redelivered as plain text.

### Added (M19 X-rider Wave 1 — feedback replay fixtures, 2026-07-16)

- Locked the new Wave-1 feedback surfaces in the replay harness
  (`crates/copperclaw-host/tests/replay.rs` + `fixtures/`):
  - **F1 matrix live HUD edit** — `fixtures/matrix/hud-live-edit`: on matrix
    (now genuinely edit-capable) the Task HUD posts one breadcrumb chip and
    edits it in place, never re-posting. A new manifest flag
    `model_rich_breadcrumbs` makes the harness's `CappedAdapter` model matrix's
    real edit-in-place `deliver_breadcrumb`; the test asserts exactly one
    breadcrumb post and ≥1 edit, all addressed to that single anchor.
  - **F4 blocked-todo rendering** — `fixtures/telegram/blocked-todo`: a todo
    that genuinely auto-blocks (dirty project at the fix-cycle cap) renders on
    the delivered checklist with the `[!]` glyph + reason and a `1 blocked`
    footer, never the in-progress glyph. Driven under `COPPERCLAW_DATA_ROOT`
    via a subprocess re-exec (the X2 verify-gate pattern).
  - **F5 thinking-frame emission** — two `#[tokio::test(start_paused = true)]`
    tests drive the real `run_loop` under a paused clock: a pure-reasoning
    turn on an edit-capable channel posts one "thinking…" HUD frame after the
    threshold and finalizes it, and a sub-threshold turn stays byte-stable
    (posts nothing). Verified as a targeted paused-clock test rather than a
    replay fixture because the frame is wall-clock-driven pre-first-tool and
    the replay harness has no timing seam; needs tokio's `test-util` dev-dep
    (mirrors the runner crate's F5 tests).

### Added (M19 F4 — `blocked` todo state visible to users, 2026-07-16)

- The runner has a real `TodoStatus::Blocked` (+ `blocked_reason`) for a step
  that auto-blocked after burning its verify fix-cycles, but the portable wire
  enum `copperclaw_channels_core::TodoItemStatus` had no `Blocked` variant, so
  `status_to_wire` (`crates/copperclaw-mcp/src/tools/todo.rs`) mapped
  `Blocked -> InProgress` — a user watching the pinned checklist saw the step
  stuck "in progress" forever. Now:
  - `TodoItemStatus` gains a `Blocked` variant (`crates/copperclaw-channels/core/src/todo_list.rs`)
    with a distinct `[!]` glyph, `"blocked"` serde tag, and a new
    `TodoListItem.blocked_reason: Option<String>` (serde-default so older rows
    decode). `TodoListItem::blocked_reason_text()` centralizes the "show the
    reason only when actually blocked" gate; `TodoList::blocked_count()` feeds
    the text-fallback footer (inserted only when non-zero, so blocked-free
    lists stay byte-stable). `status_to_wire` now maps `Blocked -> Blocked` and
    carries the reason onto the wire item.
  - Every adapter that exhaustively renders todo status now shows the blocked
    chip + one-line reason: `matrix`, `slack`, `telegram`, `discord`, `gchat`,
    `teams`, `webex` (Adaptive-Card `Attention` colour), `mattermost`,
    `whatsapp-cloud`, plus the shared text fallback. The host-delivery
    child-rollup `aggregate_status` (`crates/copperclaw-host-delivery/src/service.rs`)
    surfaces `Blocked` as the header status when any child item is stuck.

### Fixed (M19 F1 — edit-capability drift on matrix/webex, 2026-07-16)

- `matrix` and `webex` were listed in `EDIT_CAPABLE_CHANNELS`
  (`crates/copperclaw-channels/core/src/capabilities.rs`), so
  `supports_message_edit()` promised the M18 Task HUD they could edit in
  place — but neither adapter overrode the trait `ChannelAdapter::edit_message`.
  They edited only through their internal deliver-action `"edit"` arm, while
  the host HUD / approval path calls the **trait** method
  (`copperclaw-host-delivery/src/dispatch.rs`), which fell through to the
  trait default → `AdapterError::Unsupported`. The HUD therefore silently
  never edited on those two channels. Fix:
  - `crates/copperclaw-channels/matrix/src/adapter.rs` and
    `crates/copperclaw-channels/webex/src/adapter.rs` now override the trait
    `edit_message`, routing to their existing `api.edit_message` path
    (matrix `m.replace`, webex `PUT /messages/{id}`). Webex requires a
    `roomId`, so a person handle is returned as `Unsupported` (the HUD only
    edits room messages). Both record the same `inc_hud_edit` /
    `inc_adapter_edit_message` outcome metrics the other rich adapters do.
  - Drift guard: new
    `crates/copperclaw-host-delivery/tests/edit_capable_edit_message_drift.rs`
    walks `copperclaw_channels_core::capabilities::edit_capable_channels()`
    (new accessor) and asserts every listed channel's adapter source
    overrides the trait `edit_message`, so a channel added to the list
    without a real override fails the build (same spirit as the R0
    tool-name-drift guard).

### Changed (M19 F6 — richer bare-channel status row + intermediate stuck signal, 2026-07-16)

- On bare / edit-incapable channels the only progress signal was the 60s
  "still working" status row — a fixed string with no detail — and nothing
  bridged the gap between it and the 5-minute apology, so a slow build
  looked fine for five minutes then abruptly apologised.
  `crates/copperclaw-runner/src/run/hud.rs` now composes that row through a
  pure `compose_status_row` helper that folds in the current todo step (the
  same `step N/M: …` detail the Live HUD shows) so bare channels get real
  progress, and past `INTERMEDIATE_STATUS_AFTER` (150s, well short of the
  sweep's `APOLOGY_AFTER_SECS = 300`) softens the closing line to "This is
  taking longer than usual, but I'm still going." so the run degrades
  gracefully toward the apology instead of cliff-edging into it. The emit
  path and cadence are unchanged, so child-agent sessions still skip the
  row inside `RunnerToolCtx::emit_status` (no sub-agent status spam).

### Added (M19 F5 — HUD covers the pre-first-tool / pure-reasoning wait, 2026-07-16)

- The Task HUD used to post only at the first tool call and skip finalize
  entirely on a zero-tool turn, so a multi-minute pure-reasoning answer on
  an edit-capable channel showed nothing until the answer landed —
  indistinguishable from a hang. `crates/copperclaw-runner/src/run/hud.rs`
  now arms a single background HUD task at turn start (live HUD only, via
  `TaskHud::arm`, called from `drive_turn`): it waits a short
  `THINKING_THRESHOLD` (6s) so fast turns finalize first and post nothing
  (byte-stable), then posts an initial "thinking… | M:SS" frame and
  continues as the elapsed-clock ticker. `finalize` now collapses a
  zero-tool turn that posted a thinking frame (to a clean "done in M:SS",
  no "0 tool calls" tail) instead of leaving it dangling; a turn that never
  posted still finalizes to nothing. `hud_mode=off` / `final` and the
  no-op-edit suppression are unchanged. (The old per-batch `ensure_ticker`
  spawn is folded into the one armed task, so tool-first turns still get a
  ticker with no duplicate HUD message.)

### Added (M19 F2 — actionable "I'm blocked" wall cards, 2026-07-16)

- Tool errors and policy / provenance / verify-gate / egress denials used
  to render only into the model's history (`Tool { is_error: true }`) — the
  user never saw them, so when the model then looped or gave up the HUD
  just stopped, indistinguishable from a hang. The runner now watches the
  tail of each turn's tool results (`crates/copperclaw-runner/src/run/blocker.rs`,
  new module): when a turn ends **without** a user-facing reply and its
  tail is a *run* of denials on the **same** blocker (≥2, not a single
  recovered error), the terminal-failure path swaps its generic apology for
  **one** curated `ErrorCard` naming what is blocked and the actionable next
  step (egress-allow command, write a `.copperclaw/verify`, this needs
  approval, needs a person, not permitted here). Wiring:
  `crates/copperclaw-runner/src/run/drive_turn.rs` folds each result into a
  `BlockerRun` tail tracker (a mid-turn `send_message` latches suppression —
  the turn isn't a silent wall) and attaches the category to `TurnResult`;
  `finalize_messages` / `emit_terminal_failure_apologies` in
  `crates/copperclaw-runner/src/run/mod.rs` render the wall card. Card text
  is **curated per category** and carries no `details` block, so no raw
  tool-error string (or content injected via a tool result) reaches the
  user. A recovered error, a normal answer, and a non-blocker terminal
  failure are all unchanged (the generic apology still fires off the
  blocker categories).

### Fixed (M19 F3 — approval-card correctness + blocked-on-approval legibility, 2026-07-16)

- **Approval card no longer stuck live after resolution (fallback-id path).**
  `crates/copperclaw-host/src/approval_intercept.rs`: an approval card is
  stamped "Approved/Denied by <name>" in place using the `platform_message_id`
  persisted at delivery. Root cause of the stuck-card bug: that id is only
  persisted when the delivering adapter reports one (`host-delivery`
  `set_platform_message_id`); adapters that return no id (or where the card
  degraded to a text fallback) left the row's `platform_message_id` NULL, and
  `edit_card` then *silently skipped* — leaving live Approve/Deny buttons on an
  already-decided request. `edit_card` now posts the resolution as a short
  follow-up reply on the tapping surface when there is no editable anchor, so
  the outcome is always visible.
- **Losing tapper in a resolution race is no longer silent.**
  `approval_intercept.rs`: a second (losing) tap on an already-resolved approval
  — same verb (`applied = false`) or opposite verb (`conflict`) — now gets a
  short "This request was already resolved by <name>." reply (or an
  "expired … ask the agent to try again" line when it lapsed), read from the
  decision log, instead of the old no-reply no-op that looked broken.
- **Silent approval expiry now stamps the card terminal.**
  `crates/copperclaw-host/src/handlers/approvals.rs` adds
  `expire_and_edit_cards`, which sweeps overdue pending approvals to `expired`
  and edits each lapsed card to a terminal "expired — ask the agent to try
  again" state (idempotent; only rows the sweep actually flips are stamped). It
  is run opportunistically at the top of the in-chat approval interceptor (the
  one host surface that both fires on approval activity and holds the delivery
  dispatcher) and is `pub` so a periodic host sweep can call it later.
- **Blocked-on-approval legibility.** `crates/copperclaw-modules/src/approvals.rs`
  `ApprovalCardHandler` now appends a "The agent is paused, waiting for your
  approval." line to the card body so an approval-gated agent is legible rather
  than looking hung. (The in-HUD `TaskHud::add_note` seam lives in the *runner*,
  a separate process — `hud.rs`, `pub(super)` — so lane G surfaces the waiting
  state on the operator-facing card it owns.)
- Replay fixtures: `fixtures/telegram/approval-resolution` (fallback-id reply +
  expiry stamp) and `fixtures/slack/approval-conflict` (already-resolved loser
  reply, `block_actions` shape), registered in
  `crates/copperclaw-host/tests/replay.rs`.

### Added (M19 U1 — Signal rich-surface floor)

- The Signal adapter now renders five of the six portable rich surfaces
  natively instead of falling through to the plain-text trait default, so
  the Task HUD, diffs, todo lists, reasoning, and errors arrive as
  structured Signal plaintext rather than bare prose. New renderers live in
  `crates/copperclaw-channels/signal/src/render.rs`; the `deliver_*`
  overrides are in `crates/copperclaw-channels/signal/src/adapter.rs`.
  - **`deliver_breadcrumb` (marquee win) — in-place edit.** A compact
    `[status] tool · detail — summary` chip; the runner's completion emit
    is fed through signal-cli's `sendEditMessage` so `[running] shell ·
    cargo check` becomes `[done] … — passed (0.4s)` on the *same* message
    instead of stacking a fresh line on every tool boundary. Subsequent
    edits keep targeting the original timestamp; a non-numeric id or edit
    failure degrades gracefully to a fresh chip.
  - **`deliver_todo_list` — in-place edit.** A `title (done/total)`
    checklist with ASCII `[x]`/`[~]`/`[ ]` glyphs, edited in place via
    `sendEditMessage` when the prior chip id is known. Signal has no pin
    API, so `pin_hint` is a silent no-op.
  - **`deliver_diff`** — a glanceable `path (+a / -r)` header plus unified
    hunks with `+`/`-` gutters, fence-free (Signal renders backticks
    literally) and without the redundant `--- a/` / `+++ b/` git header.
  - **`deliver_thinking`** — a `reasoning (model)` header + plain body
    lines with no `> ` quote markers (Signal shows them literally);
    redacted blocks emit only the placeholder, never the raw blob.
  - **`deliver_error`** — an `[ERROR: kind] title` banner, summary, an
    indented `details:` block, and a retry footer, all markdown-free.
  - `deliver_collapsible` is intentionally left on the trait default:
    Signal has no disclosure/expandable primitive, so the fallback
    (summary + preview + `…(N more lines)`) is already the optimal
    markdown-free plaintext shape and carries no in-place-edit id to
    improve on.

### Added (M19 U3 — Mattermost breadcrumb + reaction + typing)

- The Mattermost adapter now overrides three rich-surface hooks it
  previously left on the trait defaults, bringing it in line with the
  other edit-capable adapters (Discord/Matrix/Slack/Telegram):
  - **`deliver_breadcrumb`** (`crates/copperclaw-channels/mattermost/src/adapter.rs`,
    renderer in `src/render.rs::render_breadcrumb`) — tool-progress chips
    now render as a compact Markdown chip (`` [~] `shell` · cargo check ``,
    ASCII status markers per the no-emoji rule) and are *edited in place*
    via `PUT /api/v4/posts/{id}/patch` when the host passes the prior
    chip's `existing_message_id`, so the user sees `Running…` → `Done`
    rather than a new row per tool boundary. Rolling `steps` aggregates
    render a bold summary line + a Markdown bullet per step. Previously
    breadcrumbs degraded to the plain-text fallback row.
  - **`add_reaction`** (the host-driven trait hook) — routes to the
    existing `POST /api/v4/reactions` on behalf of the configured
    `bot_user_id`; falls through to `Unsupported` (so the host posts a
    fresh message) when no bot id is configured. Previously reactions were
    reachable only via the `reaction` egress action on `deliver`, not the
    trait method the delivery service calls.
  - **`set_typing`** (`src/api.rs::post_typing`) — publishes the bot's
    "…is typing" indicator via `POST /api/v4/users/me/typing` (the REST
    shortcut for the websocket `user_typing` action — no persistent socket
    needed), scoped to a thread root via `parent_id` when a `thread_id` is
    present. Previously typing was a silent no-op, leaving no "agent is
    working" signal during a run.

### Added (M19 M1 — metrics rider: sweep the M19 metric wishes into `copperclaw-metrics`, 2026-07-16)

- One card, absolute last in the program, sweeps every metric "wish" the merged
  M19 cards (F1–F5, U1–U7, A1–A6) recorded in their PR descriptions into
  `crates/copperclaw-metrics/src/lib.rs` (the workspace hotspot — no other M19
  card touches it). Each new counter/gauge/histogram is registered with a helper
  following the crate's naming (`copperclaw_` prefix, `_total`/`_seconds` suffix,
  snake_case labels) and wired to a real emit site; new tests extend the
  prefix/suffix invariants and render-with-labels coverage to the additions.
  - **F1** `copperclaw_edit_drift_fallthrough_total{channel_type}` — a dedicated
    edit-drift alarm emitted alongside the existing
    `inc_hud_edit(_, "unsupported_fallthrough")` from the core trait default
    `edit_message` (`copperclaw-channels/core/src/adapter.rs`).
  - **F2** `copperclaw_wall_card_total{blocker}` — a curated wall card actually
    written to a user channel, labeled by `BlockerCategory::metric_label()`
    (`copperclaw-runner/src/run/mod.rs::emit_terminal_failure_apologies`).
  - **F3** `copperclaw_approval_card_outcome_total{outcome}`
    (`resolved_edit|resolved_fallback_reply|conflict_notified|expired_card`) —
    a dedicated approval-*card* lifecycle counter; the M19 cards had reused
    `inc_approval_tap` with these new label values, polluting its documented
    `approved|denied|unauthorized|race_noop` set. Those five call sites in
    `copperclaw-host/src/approval_intercept.rs` + `.../handlers/approvals.rs`
    are switched to the new counter (and a `resolved_edit` emit added at the
    in-place edit branch, previously unmetered); `inc_approval_tap` is restored
    to its original four outcomes.
  - **F4** `copperclaw_blocked_todo_render_total{channel_type, has_reason}` —
    a delivered todo checklist carried ≥1 `blocked` item; emitted from the
    central `dispatch_todo_list` (`copperclaw-host-delivery/src/service.rs`).
  - **F5** `copperclaw_hud_thinking_frame_total{agent_group}` — the pre-first-tool
    "thinking…" HUD frame, distinct from the tool-triggered `inc_hud_post`
    (`copperclaw-runner/src/run/hud.rs::arm`).
  - **U1/U2** `copperclaw_adapter_surface_write_total{channel_type, mode}`
    (`edit|create`) — the pinned rich-surface edit-vs-create intent, emitted
    from `dispatch_todo_list`.
  - **U3** `copperclaw_adapter_typing_total{channel_type, result}` (central
    dispatcher `set_typing`, `copperclaw-host-delivery/src/dispatch.rs`) and
    `copperclaw_adapter_reaction_total{channel_type, result}` (central reaction
    system-action path in `service.rs`) — both cover every adapter from one site.
  - **U6** `copperclaw_shared_renderer_adoption{channel_type}` gauge, set from the
    const `SHARED_RENDERER_ADOPTED_ADAPTERS` list inside `maybe_start_server`
    (self-contained, no external call site) — coverage of `core::markdown::render`
    adoption.
  - **U7** `copperclaw_inbound_reaction_total{signal, outcome}`
    (`affirmative|looking|negative|none` × `folded|ignored`) — replaces the
    generic `inc_midturn_control(_, "reaction")` proxy at the runner's steering
    seam (`copperclaw-runner/src/run/drive_turn.rs`).
  - **A1** `copperclaw_delegate_batch_width` histogram + `…_worker_total{outcome}`
    (`ok|timeout|spawn_failed`) from the runner join
    (`copperclaw-runner/src/run/delegate_batch.rs`) + `…_refused_total` from the
    pure handler (`copperclaw-mcp/src/tools/agents.rs`).
  - **A2** `copperclaw_browser_interactive_actions_total{action, outcome}` per
    scripted action (`copperclaw-browser/src/interactive.rs`); the interactive
    SSRF *stage* labels were already live via `inc_browser_ssrf_block`.
  - **A3** `copperclaw_public_tunnel_total{outcome, reason}`
    (`opened|approval_raised|denied|torn_down`) wired at the five state
    transitions in `copperclaw-modules/src/tunnel.rs`.
  - **A4** `copperclaw_skills_saved_total{outcome}` (`saved|rejected`) — a
    dedicated counter at the true write site
    (`copperclaw-host/src/handlers/approvals.rs::apply_save_skill`), which had no
    metric; the save-skill paths previously proxied through `inc_self_mod_*`
    (those remain as the request-raise signal).
  - **A5** `copperclaw_memory_writes_total{provenance}` (`trusted|untrusted`) +
    `copperclaw_memory_write_rate_capped_total` at the runner `memory_save` impl
    (`copperclaw-runner/src/tools.rs`).
  - **A6** `copperclaw_scheduled_task_fires_total{kind}`
    (`recurring_rearm|one_shot_complete`), `copperclaw_scheduled_tasks_active`
    gauge (new `tasks::count_active` query in `copperclaw-db`), and
    `copperclaw_scheduled_task_fire_latency_seconds` histogram, all set/emitted
    from the sweep's due-task fan-out
    (`copperclaw-host-sweep/src/checks/scheduling.rs`).

### Fixed (M18 — Task HUD no-op edit / Telegram "message is not modified", 2026-07-16)

- The H1 Task HUD and R6 progressive-final-answer edit a pinned status
  message in place through the same edit anchor. When two edits render
  byte-identical content (two tool batches finishing within one
  rendered-time tick, a finalize matching the last live frame, an idle
  ticker frame), Telegram's `editMessageText` 400s with `Bad Request:
  message is not modified: …` and `host-delivery` logged the row as a
  `non-retryable failure` — non-fatal but spurious noise and a bogus
  "failed" delivery. Two-part fix:
  - **Primary — don't emit a redundant edit.** `crates/copperclaw-runner/src/run/hud.rs`
    now fingerprints each composed HUD frame (the serialised breadcrumb —
    exactly what the delivery loop renders) and records the last-emitted
    fingerprint per anchor in `Shared`. Every edit path (batch edits in
    `emit_live_update`, the background elapsed-clock ticker, the finalize
    collapse) skips the emit when the new frame is byte-identical to the
    last one sent; first posts always go through, and any real change
    (clock tick, tool-count bump, activity flip) emits normally. The HUD
    content, cadence, and edit anchor are unchanged — only genuine no-op
    edits are suppressed.
  - **Safety net — treat "message is not modified" as success.**
    `crates/copperclaw-channels/telegram/src/api.rs`'s
    `edit_message_text_with_mode` (the single chokepoint every edit funnels
    through: `edit_message`, `deliver_breadcrumb`, `deliver_todo_list`,
    progressive `edit_message`) now detects the specific `message is not
    modified` BadRequest and returns `Ok(())` instead of surfacing an
    `Adapter(BadRequest(…))` error, so `host-delivery` never marks such a
    row failed. Matched narrowly on the description — every other 400 still
    errors.

### Added (M18 V5 — public tunnel module, 2026-07-16)

- **`crates/copperclaw-modules/src/tunnel.rs` (new): approval-gated public tunnel
  module.** Fronts a live session-preview's host port with a *public* URL via an
  **operator-provided** tunnel binary (cloudflared first; the `TunnelProvider`
  trait is shaped so tailscale-funnel can slot in later), for the "send it to my
  cofounder" moment the LAN-only preview can't serve. Every guard rail is
  mandatory and fail-closed:
  - **OFF by default, per-group opt-in** (`TunnelExposeRequest.enabled`, sourced
    host-side): a group that has not opted in gets `TunnelError::NotEnabled` and
    no tunnel is ever attempted.
  - **Every exposure is G1-approval-gated.** `TunnelBroker::expose` raises a
    `CredentialedExternalAction` pending approval and refuses to stand up a
    tunnel until an approver taps Approve — there is no path from an agent
    request to a public URL that skips a recorded human decision.
  - **Audit-rowed.** Every request / exposure / teardown writes an `audit_log`
    row (command `tunnel`).
  - **Auto-teardown with the preview it fronts** via
    `TunnelBroker::close_for_preview` / `close_all_for_session` (kills the tunnel
    process — no orphaned public tunnels).
  - **Never bundles binaries.** When the binary is absent,
    `TunnelProvider::preflight` returns `TunnelError::BinaryNotFound` with
    copy-pasteable install instructions — a clean actionable error, never a panic
    or silent no-op. `CloudflaredProvider` handles no Cloudflare credentials (an
    anonymous quick tunnel); any account token lives in the operator's own
    `cloudflared` config, never read/stored/forwarded by this process.
  - Registered types re-exported from `crates/copperclaw-modules/src/lib.rs`
    (`pub mod tunnel;`). Mock-binary integration tests cover the full
    expose → approval → URL surfaced → teardown-on-close flow and the
    absent-binary error path.
- **`crates/copperclaw-host/src/handlers/approvals.rs`: real
  `credentialed_external_action` apply arm.** Replaces G1's placeholder refusal
  arm — approving a `CredentialedExternalAction` row (a public-tunnel exposure)
  now succeeds through the shared `resolve_approve` DB path, records the decision,
  and echoes the exposure specifics; the requester (the tunnel broker on the
  agent's retry) consults the approved grant. `one_cli` keeps its explicit
  refusal arm. Denying grants nothing.
- **Host integration seams (documented follow-up, not wired in this card to keep
  its scope to `copperclaw-modules` + the approval arm):** boot registration of
  `TunnelModule`, the `PreviewManager::teardown` → `TunnelBroker::close_for_preview`
  call, the per-group opt-in config source, and the agent-facing expose relay.

### Fixed (M18 — todo store parallel-batch write race, 2026-07-16)

- `crates/copperclaw-mcp/src/tools/todo.rs`: the `todo_*` store's
  read-modify-write cycle was not concurrency-safe. R2's parallel tool-batch
  execution can fire two `todo_add` / `todo_update` mutators from one batch
  concurrently; both read the same pre-image (one mutation silently clobbered
  the other — a lost update) and both wrote `write_all`'s single fixed sibling
  tempfile `agent_todos.json.tmp` then renamed it into place (bytes interleaved
  → corrupt JSON). Seen live as ~12 `todo store was unparseable … trailing
  characters` + `could not quarantine corrupt todo store … No such file or
  directory` warnings inside one long build, resetting todo state mid-run.
  Two-part fix: (1) a process-wide `todo_write_lock` (`tokio::sync::Mutex`,
  held across the whole read+mutate+write in every mutator — `add`, `update`,
  `delete`) serializes mutators, fixing both the corruption and the lost
  updates; (2) `write_all` now names its tempfile `<store>.tmp.<pid>.<seq>`
  (pid + monotonic `AtomicU64`) so even an unlocked writer can't collide, as
  defence in depth. On-disk format, quarantine-on-corrupt-read behaviour,
  atomic-rename crash-safety, and the public tool API are unchanged. New
  `concurrent_adds_never_corrupt_or_lose_updates` test fires eight concurrent
  adds and asserts the store stays parseable with all eight items — it fails
  against the pre-fix code (only one item survives) and passes with the lock.

### Added (M18 M1 — metrics rider: sweep of merged-PR metric wishes, 2026-07-16)

- Swept the "Metrics wishes" recorded across merged M18 PRs #24-#54 into real
  Prometheus metrics: ~55 new `copperclaw_*` counters / histograms / gauges
  defined in `crates/copperclaw-metrics/src/lib.rs`, each emitted at a real
  call site in the crate its wish named. One PR, single owner of the metrics
  hotspot. Highlights by lane:
  - **Channels:** slack typing skip / set-status / HUD-decision (C1);
    per-channel inbound-file materialize + byte histogram across slack /
    discord / telegram / deltachat (C3/C4); native rich-render + HUD self-edit
    counters for signal / whatsapp-cloud / mattermost (C5); shared markdown
    `render`/unbalanced-marker + the relocated fence-split / unbalanced-input
    counters emitted from `markdown::split_into_chunks` (C2/C5b — the splitter
    gained a `channel_type` label param, threaded from host-delivery).
  - **Runner / providers:** mid-turn stop vs interjection (R2); verify-gate
    completion + verify-run + fix-cycles histogram (R3/X2); compaction
    triggered / estimated-tokens / facts-header-bytes (R4); provider failover
    from→to + chain-exhausted, plus the `pump_events` retry-label accuracy fix
    (R5); progressive-final grown/single-emit/skip-reason/steps/chars (R6);
    Task-HUD posts / edits / degraded / finalize (H1); preview-expose
    served/timeout (X1); policy denials by layer+tool (R0).
  - **mcp:** shell truncation by mode + pre-cap bytes, read_file lines-mode +
    pages (T1); unknown-tool + filter-deny layer (R0); load_skill inline vs
    callable (P1); session-install by ecosystem/outcome + wall-clock +
    image-scope rejection + egress-hint (E1); browser render by mode/outcome,
    screenshot result + latency, preview-allow injection, SSRF blocks, CDP
    connect failures, child spawn/teardown (V3/V4).
  - **host / modules:** preview WS upgrades / active gauge / frames / bytes /
    session-seconds (V1); enable-preview card outcomes + tombstone recovery +
    tombstoned gauge (V2); in-chat approval taps (G1); image rebuild by profile
    + per-group image-profile gauge (E2); sessions-spawned-per-profile +
    system-prompt bytes (P1); delegate spawn-gate outcomes + depth rejections +
    worktree-provision latency (R7); slash-commands + control-rows +
    status-answer timing (R1, in `copperclaw-host-router`).
  - **Adapted / dropped (documented, no dead metrics):** the wished
    `control_rows_pending` gauge landed as a `..._written_total` counter (its
    consumer is a separate process, so no in-registry decrement); P3's
    "prototype ready" ritual metrics and X2's `ritual_card_sent_total` were
    dropped — the ritual is model-driven with no distinct code path to key on;
    V2's tombstone-duration histogram and R7's concurrent-delegates histogram
    were deferred to an M1b follow-up (both need new runtime state to emit).
  - **Deferred:** V5 (#55, tunnel exposures) is held for security sign-off and
    is NOT on `main`; its metrics are an M1-followup once V5 merges.

### Added (M18 R7 — `delegate`: write-capable middle-tier build worker, 2026-07-16)

- New **`delegate`** tool + delivery-action: the middle tier between the read-only
  in-process `explore` subagent and a full persistent `create_agent` sibling. A
  delegate spawns a **write-capable** build worker in its own container that — when
  the parent's current project is a git repo — gets a **writable git worktree** of
  that repo at `/workspace` on its own `sib/<id>` branch (the exact
  `container_manager::spawn` worktree mechanics `create_agent` uses; no spawn-path
  change was needed — worktree provisioning keys off the child session's
  `source_session_id`). Its commits land in the parent repo via the branch-merge
  path, and parallel delegates each get their own isolated worktree.
  - Unlike `create_agent`, a delegate is **contained**: its session lands with a NULL
    messaging group + no copied `session_routing`, so it can report **only** back to
    its spawning parent and can never post into the user's chat. Result rows use a
    distinct `delegate_result` key.
  - Permission-gated with the same `create_agent_users_table_check` (spawning a
    write-capable container is at least as privileged) and depth-capped by the same
    `depth.rs` gate as `create_agent`.
  - `crates/copperclaw-mcp/src/context.rs`: `DelegateSpec` + `OutboundToolEffect::Delegate`.
  - `crates/copperclaw-mcp/src/tools/agents.rs`: the `delegate` tool (no `channel` field —
    never user-facing); registered in `tools/mod.rs`.
  - `crates/copperclaw-runner/src/tools.rs`: `apply_delegate` writes the `{"delegate": …}`
    system row; `crates/copperclaw-runner/src/policy.rs`: `delegate` joins `CODING_TOOLS`
    (denied to guests, gated behind the coding profile).
  - `crates/copperclaw-modules/src/agent_to_agent/create_agent.rs`: a `SpawnProfile`
    (`Persistent` | `Delegate`) parameterises the shared spawn core;
    `CreateAgentModule::new_delegate(…)` registers the `delegate` action.
  - `crates/copperclaw-host/src/boot.rs`: the delegate-profile module is installed
    alongside `create_agent`.
  - **Deferred (follow-up):** single-call parallel fan-out orchestration (one `delegate`
    call spawning N workers) and any automated join/merge — parents fan out today by
    calling `delegate` once per independent piece of work, each contained + isolated.

### Added (M18 C5b — shared markdown → per-platform renderer, 2026-07-16)

- New `crates/copperclaw-channels/core/src/markdown/` module — the single
  place canonical agent Markdown turns into each chat platform's flavor,
  replacing the per-adapter duplication (telegram `markdown_to_html`, slack
  `mrkdwn`, discord escaping) over time.
  - `markdown::render(md, Flavor)` renders ATX headings, fenced + inline code,
    bold/italic/strikethrough, links, unordered/ordered lists, and blockquotes
    into `Flavor::{Html, Discord, Slack, Mattermost, WhatsApp, Plain}`. Forgiving
    on unbalanced markers (emits the literal char, matching the adapters' current
    behaviour on natural-language prose). Unit table proves per-platform output
    for headings/bold/code/lists (+ links, quotes, HTML escaping, robustness).
  - `markdown::split_into_chunks` / `markdown::is_balanced` — fence-aware
    chunking of a long reply into cap-sized pieces that each parse with
    balanced code fences.

### Changed (M18 C5b — fence logic migrated out of host-delivery)

- Migrated C2's fence scanner + splitter from `copperclaw-host-delivery` into
  the shared renderer, per the explicit migration note C2 left at
  `host-delivery/src/fence.rs:22-24`. `scan_fence_spans`, the
  `FenceKind`/`FenceSpan` model, `is_balanced`, and the fence-aware
  `split_into_chunks` now live in `copperclaw-channels-core::markdown`
  (`fence.rs` + `split.rs`, moved verbatim). `copperclaw-host-delivery` now
  **consumes** the renderer: `service::split_text_into_chunks` is a thin
  delegate to `markdown::split_into_chunks`, and `host-delivery/src/fence.rs`
  is a documented shim re-exporting `is_balanced` for its C2 splitter tests —
  which still pass, now routed through the migrated logic.

### Added (M18 C5 — adapter rich-surface floor: signal / whatsapp-cloud / mattermost, 2026-07-16)

- Raised **signal**, **whatsapp-cloud**, and **mattermost** off the trait-default
  text fallbacks to native rich renderings for the portable card surfaces
  (`deliver_card` / `deliver_diff` / `deliver_collapsible` / `deliver_todo_list` /
  `deliver_thinking` / `deliver_error`), each in its own platform vocabulary.
  - `crates/copperclaw-channels/mattermost/src/render.rs` (new): full `CommonMark`
    renderers (`###` headings, fenced ` ```diff `, `- [x]`/`~~strike~~` task lists,
    `>` blockquotes). Wired in `adapter.rs` via `create_post` / `update_post`.
  - `crates/copperclaw-channels/whatsapp-cloud/src/render.rs` (new): WhatsApp-flavoured
    markdown (`*bold*`, `_italic_`, `~strike~`, ` ``` `-fenced mono). Wired via a shared
    `send_rendered` helper (splits `<pnid>:<recipient>`, threads via `context.message_id`).
  - `crates/copperclaw-channels/signal/src/render.rs` (new): markdown-free plaintext
    `render_card` (Signal renders markdown literally). Signal keeps the canonical
    plaintext trait default for diff/todo/thinking/error/collapsible — their
    `to_text_fallback` is already optimal on a plaintext surface, so a native
    override would duplicate it verbatim.
- **`edit_message` (H1 Task HUD) for signal and mattermost.** signal overrides it via
  signal-cli `sendEditMessage` (`external_id` = the message `targetSentTimestamp`);
  mattermost via `PUT /api/v4/posts/{id}/patch`. whatsapp-cloud does **not** gain it —
  the Cloud API cannot edit a previously sent message.
  - `crates/copperclaw-channels/core/src/capabilities.rs`: `EDIT_CAPABLE_CHANNELS`
    gains `signal` + `mattermost` (now 7 entries), kept in sync in the same change
    per the module's explicit rule; the test asserts whatsapp-cloud stays
    non-edit-capable.

### Fixed (M18 C5)

- `crates/copperclaw-host-delivery/src/service.rs`: two stale doc comments referenced
  the removed `RunnerToolCtx::emit_breadcrumb` / `emit_breadcrumb_finish` (deleted in
  H1, flagged in PR #30). Retargeted to the current emitter
  `RunnerToolCtx::emit_task_hud` (via `insert_breadcrumb_row` /
  `insert_update_breadcrumb_row`).

### Added (M18 V2 — one-tap preview enablement + expired-link recovery, 2026-07-16)

- **One-tap enable previews (secure-by-default preserved).** Previews stay
  opt-in per group, but a `PreviewError::Disabled` no longer dead-ends on a
  phone: the host now raises a G1 in-chat approval card ("Enable previews for
  this group") on the group's primary messaging channel. Tapping it routes
  through G1's merged interceptor + shared DB decision path and flips
  `container_configs.preview_enabled` — the same effect as
  `cclaw groups config update --field preview_enabled=true`. The agent still
  gets the copy-pasteable `cclaw` fix text, which is also the card body.
  - `crates/copperclaw-types/src/approval.rs`: new `ApprovalKind::EnablePreview`.
  - `crates/copperclaw-host/src/handlers/approvals.rs`: `"enable_preview"`
    dispatcher arm + `apply_enable_preview` (creates a defaults config row when
    the group has none, then `set_preview_enabled(true)`).
  - `crates/copperclaw-host/src/preview.rs`: `PreviewManager::set_approval_dispatcher`
    + `request_enable_approval` (idempotent — no duplicate card while one is
    outstanding; skipped cleanly when no dispatcher / no messaging group).
  - `crates/copperclaw-host/src/boot.rs`: wires the delivery dispatcher into the
    preview manager at boot.
- **Expired-link recovery via a tombstone (amended design).** The "30 minutes"
  was always the idle *reaper*, not a token TTL — and reaping used to cancel the
  whole per-preview listener, so a stale link got connection-refused with
  nothing left to recover on. Idle-reaping now tears down only the upstream
  proxying and leaves the bound port serving a static "preview expired" page.
  A tokened `GET /__preview/<token>` against the tombstone re-exposes the same
  session:port **once per token** (re-resolving the container IP, same audit row
  as a fresh expose) when the container is still up; a spent recovery or a gone
  container shows the terminal "ask the agent to re-expose" page. An explicit
  agent `expose_preview` revives a tombstoned port with a fresh recovery budget.
  Full teardown (port released) still happens on session stop / close /
  shutdown. The cookie gate + constant-time token comparison are unchanged.
  - `crates/copperclaw-host/src/preview.rs`: `ProxyState` gains a phase
    (`Live` | `Tombstone`) with a one-shot `recovery_used` budget; the reaper
    tombstones in place instead of tearing down.

### Added (M18 R6 — progressive final answers, 2026-07-16)

- On a rich (edit-capable) channel, a long final answer to a turn that already
  ran past 30s now *grows* in place via a bounded run of `edit_message` rows
  instead of landing all at once — the H1 HUD already covers "something is
  happening" during the build, so this relieves the wait for the answer itself.
  Default ON; no config, no new channel-adapter surface. There is no token
  streaming through the transport (rejected in the M18 plan): the model's
  answer is complete when we reach the final emit, so the growth is a paced
  reveal of the finished text.
  - `crates/copperclaw-runner/src/run/progressive.rs` (new): the R6 gate and
    reveal driver. `should_grow(edit_capable, elapsed, answer)` requires all of
    a `supports_message_edit` channel, `elapsed >= 30s`, and an answer that is
    at least 280 chars but NOT already expander-scale (`build_expander_decorator`
    is `None`) — so pages-of-output answers keep their slice-3.4 collapsible chip
    and never overlap. `grow_final_answer` posts the first chunk as a
    `send_message` (whose `ToolEffectAck::Message { seq }` is the edit anchor)
    then emits `edit_message` rows keyed to that `seq`, pacing `STEP_INTERVAL`
    (800ms) between them; `char`-boundary chunking, `MAX_STEPS` cap (6),
    best-effort after the first emit.
  - `crates/copperclaw-runner/src/run/hud.rs`: `TaskHud` now exposes
    `answer_edit_capable()` (the raw `supports_message_edit` gate, kept distinct
    from the HUD `Behavior` because R6 is independent of `hud_mode`) and
    `elapsed()` — the progressive gate reads the ONE clock the HUD already
    tracks (`started_at`) rather than re-deriving elapsed time.
  - `crates/copperclaw-runner/src/run/drive_turn.rs`: the terminal final-answer
    emit now routes through the R6 gate; every non-growth case (bare adapter,
    sub-30s turn, short/huge answer) falls through to the pre-R6 single
    `send_message`, byte-identical.
  - `crates/copperclaw-runner/src/tools.rs`: `strip_reasoning_blocks` is now
    `pub(crate)` so the gate measures the reasoning-stripped text the user
    actually sees (no `<thinking>` leak in the grown copy).

### Added (M18 E1 — session-local installs that work *this* turn, 2026-07-16)

- `install_packages` gains a `scope` field (`"image"` default — unchanged — or
  `"session"`) plus a `pip` array, so an agent mid-build can get a package
  usable **now** instead of only after the next image rebuild.
  - `crates/copperclaw-mcp/src/context.rs`: new `InstallScope` enum
    (`Image` | `Session`, default `Image`) and a `scope` field on `InstallSpec`
    (`#[serde(default)]`, so old payloads deserialize unchanged); re-exported
    from the crate root.
  - `crates/copperclaw-mcp/src/tools/self_mod.rs`: `scope: "session"` runs the
    ecosystem-appropriate LOCAL install into the session's persistent `/data`
    **in-container, now** — `python3 -m venv /data/.venv` + `pip install` for
    `pip`, `npm install -g --prefix /data/.npm-global` for `npm` — and reports
    the activation line (`source /data/.venv/bin/activate` /
    `export PATH=/data/.npm-global/bin:$PATH`). Bakeable ecosystems (apt/npm)
    are ALSO recorded for the next image via the existing approval/config-merge
    path ("works now, permanent later"); pip lives durably in `/data` (which
    survives respawns for the session) and has no image bake dimension, so it is
    session-only and is rejected under `scope: "image"`. The ack text is a
    two-state machine — *pending image build* (image scope) vs *installed now*
    (session scope, with the activation line).
  - **New deny-default egress hint:** when a session install can't reach its
    registry (DNS/nftables denial — previously a raw, unactionable network
    error), the tool error now carries the exact remediation, e.g.
    `cclaw groups config set-egress-allow <agent-group-id> --allow pypi.org:443
    --allow files.pythonhosted.org:443`.
  - `crates/copperclaw-runner/src/tools.rs`: the outbound `install_packages`
    row carries `scope` so the host apply path can flavour its response.
  - `crates/copperclaw-host/src/handlers/approvals.rs`: `apply_install_packages`
    reads `scope`, still merges the bakeable apt/npm into `container_configs`
    regardless of scope, and echoes a session-flavoured rebuild note (the
    session copy is already live under `/data`).
  - The `scope: "session"` local install runs in-container because the
    `ContainerRuntime` trait exposes no host→container exec primitive; the
    execution is split behind a `SessionInstaller` trait so the schema-parse,
    ack-text state machine, egress-hint construction, and config-merge paths are
    covered by ordinary unit tests, with the real pip/npm run gated behind an
    `#[ignore]`d Docker-integration test (opt in with `--ignored`).

### Added (M18 X2 — close the golden-fixture gaps, 2026-07-16)

- The M18 program-acceptance golden fixture now covers the two legs X1
  shipped without a genuine end-to-end exercise.
  - `fixtures/cli/prototype-verify-gate/` (new, registered in
    `crates/copperclaw-host/tests/replay.rs` as
    `cli_prototype_verify_gate_refuse_fix_pass`): the R3 verification-gate
    **refuse → fix → pass** loop, genuinely exercised — a scripted mock
    provider records a verify command, writes a broken file, is REFUSED at
    `todo_update completed` (dirty, `2 fix cycle(s) remaining`), runs a
    failing verify (Python `SyntaxError`, refused again with
    `1 fix cycle(s) remaining`), writes the fix, runs a passing verify, and is
    then ALLOWED to complete. X1 could not cover this because
    `verify_gate::data_root()` / `todo.rs` were hardcoded to `/data`; **T2's
    `COPPERCLAW_DATA_ROOT` override** points the in-process runner's gate at a
    writable per-run dir. Because `forbid(unsafe_code)` blocks
    `std::env::set_var`, the registered test **re-execs itself** as a child
    process with the env var set via the safe `std::process::Command::env` —
    no production source change (T2 already shipped the seam). Refusal shapes
    are asserted from the wiremock server's captured request bodies (the
    `tool_result`s handed back to the model); the pass is asserted from the
    on-disk todo store (`status: completed`) and the cleared `.copperclaw/dirty`
    marker.
  - `fixtures/cli/prototype-golden/`: extended to emit the P3 "prototype
    ready" ritual in full — a `send_file` **screenshot delivered alongside**
    the `send_card` (cards can't attach a local file), and the card's
    `artifact_path` host-path footer. New `cli_prototype_golden_ritual_card_and_screenshot_shape`
    test asserts the delivered card shape (title, one-liner, "What to try",
    the host-path footer, the Open-preview URL button) and the screenshot,
    on top of the byte-stable JSONL diff. Expected streams regenerated.
  - `crates/copperclaw-host/tests/replay/harness.rs`: new `dump_expected_jsonl`
    fixture-authoring aid — prints each captured actual stream as
    substituted JSONL so `expected/*.jsonl` can be generated from a real run
    (`COPPERCLAW_X2_GENERATE=1`) instead of hand-guessed.
  - The **HUD status-row leg remains uncovered** on `cli` (not edit-capable →
    always `StatusRows`, gated behind a 60 s real-wall-clock first fire a
    millisecond replay never crosses). X2 did not add a clock seam; both
    fixture READMEs document why an edit-capable channel is the right vehicle.

### Changed (M18 V4 — screenshot-the-preview path, 2026-07-16)

- `browser_render` now lands its screenshot where the in-container agent can
  relay it, and can reach the prototype's own preview under deny-default
  egress — so the P3 "prototype ready" ritual can render the live app and
  attach the PNG via `send_file`.
  - `crates/copperclaw-mcp/src/tools/browser_render.rs`: the default screenshot
    output dir is refined from a host temp dir to `<data_root>/screenshots`
    (`/data/screenshots` in production), reusing the shared
    `COPPERCLAW_DATA_ROOT`-aware `verify_gate::data_root()` so the PNG is
    readable in-container for `send_file`. An explicit
    `COPPERCLAW_BROWSER_OUTPUT_DIR` still wins.
  - Egress allow-list injection: a new `COPPERCLAW_BROWSER_PREVIEW_ALLOW`
    (comma-separated `host:port`) is folded into the browser child's
    deny-default egress allow-list in `prepare` (deduped, malformed entries
    dropped). The host sets it at browser-child spawn from the live
    `PreviewEntry` (`container_ip:container_port`, read-only). **Render-target
    decision:** the render targets the prototype's own session container
    directly on the Docker bridge (`http://<container_ip>:<container_port>`),
    NOT the host preview-proxy URL — the V1 proxy 403s any cookieless request,
    and the child + app container already share the bridge. Unset → target-only
    allow-list, byte-identical to pre-V4.
  - `send_card` cannot attach a local file (its image field is an http(s)
    `image_url`), so the screenshot reaches the user via `send_file` sent
    alongside the ritual card — not embedded in it.
  - When `COPPERCLAW_BROWSER_ENABLED` is unset the tool stays disabled, so the
    ritual simply omits the screenshot — never an error.
  - Tests: preview-allow parsing/filtering + the deny-default-egress injection
    fixture (`browser_render.rs`); a mock-driver screenshot e2e proving a PNG
    lands under the configured `/data` output dir
    (`crates/copperclaw-browser/src/live.rs`).

### Changed (M18 P3 — the "prototype ready" ritual, 2026-07-16)

- Every build now ends with one concrete `send_card` hand-off instead of
  whatever prose the model chose, so a "build me X" run finishes with a
  coherent demo the operator can open, download, and steer.
  - `crates/copperclaw-host/src/container_manager/prompt.rs`: the static
    `CODING_PREAMBLE` floor block (active only for `Coding` / `Full` profiles)
    gains one closing bullet mandating the ritual card — title + one-line
    summary, a "What to try" bullet, an **Open preview** URL button (only when
    the app serves HTTP), a **Download** button (`value: "download"`, answered
    next turn with the `git archive` zip via `send_file`), the `artifact_path`
    host path in a footer field, and the screenshot sent alongside via
    `send_file` (a card can't attach a local file — `image_url` must be
    http(s)). The block stays a compile-time const, so the prompt-cache prefix
    is unchanged per spawn and `Messaging` / `Minimal` profiles gain zero bytes
    (pinned by the existing cache-stability / zero-new-bytes tests).
  - `skills/coding-task/SKILL.md`: dropped the "a richer close card is
    forthcoming — P3" hedge and taught the actual `send_card` ritual as the
    mandatory final step, with capability-based degradation (no preview → no
    button, no screenshot → no PNG; never a dead link); trimmed the surrounding
    delivery prose to stay under the 8 KiB skill-body cap.
  - `skills/send-card/SKILL.md`: added a worked "prototype ready" close example
    (URL + `value` buttons, artifact-path field, screenshot-alongside note) and
    the degradation rules, rather than duplicating the schema into coding-task.

### Added (M18 G1 — in-chat approvals, 2026-07-16)

- Approval cards can now be resolved by tapping **Approve** / **Deny** from
  chat — a phone-only operator no longer needs `cclaw approvals approve`.
  - `crates/copperclaw-modules/src/approvals.rs`: `ApprovalCardHandler` emits a
    canonical `Card` (`MessageKind::Card`) with `approve:<id>` / `deny:<id>`
    button callbacks; the delivery loop renders it via each adapter's native
    card hook (Slack Block Kit `actions`, already present in
    `slack/src/api.rs::build_card_blocks`; Telegram inline keyboard), degrading
    to the card's text fallback elsewhere.
  - `crates/copperclaw-modules/src/context.rs`: new `ApprovalInterceptor` hook
    type (`ApprovalInterceptCtx` / `ApprovalInterceptDecision`) plus an
    `edit_message` method on `DeliveryDispatcher` (default no-op; the host's
    `HostDispatcher` drives `ChannelAdapter::edit_message`).
  - `crates/copperclaw-host-router/src/{hooks,route}.rs`: the router holds an
    approval-interceptor slot and runs it in `route_one` between the
    sender-scope gate and the mention gate. An `approve:<id>` / `deny:<id>`
    callback (telegram `content.callback.data` or slack `.value`) is consumed
    as `Pending(ApprovalHandled)` — no inbound row, no runner wake.
  - `crates/copperclaw-host/src/approval_intercept.rs` (new): builds the
    interceptor closure. Approver identity reuses the **Owner/Admin roles**
    infra (`user_roles`) — global or scoped to the approval's agent group —
    resolved from the central `users` table (the router wires no sender
    resolver). Resolution goes through the SAME DB path as the CLI
    (`handlers::approvals::resolve_approve` / `resolve_deny`, refactored to
    take a `decided_by` label), so the CLI and in-chat routes can't diverge and
    a race resolves once (first wins; the loser no-ops). Non-approver taps get
    a short "not authorized" reply and the card stays live; every tap writes an
    `audit_log` row (`ok` / `unauthorized`).
  - `crates/copperclaw-host/src/handlers/approvals.rs`: the generic approve
    dispatcher gains explicit **refusal arms** for `one_cli` /
    `credentialed_external_action` (no silent no-op; V5 wires the real
    `credentialed_external_action` applier).
  - `crates/copperclaw-db/src/tables/pending_approvals.rs`: new
    `set_platform_message_id`; the delivery loop
    (`crates/copperclaw-host-delivery/src/service.rs`) persists the delivered
    approval card's platform message id onto the row so the interceptor can
    later edit that exact message to "Approved by <name>".
  - Fixtures: `fixtures/telegram/approval-callback/` and
    `fixtures/slack/approval-block-action/` (registered in
    `crates/copperclaw-host/tests/replay.rs`) exercise approver-tap → resolve +
    card edit + audit, stranger-tap → refused + audited + card live, per
    channel; a runner/mcp-free unit suite in `approval_intercept.rs` covers the
    role checks and the CLI/in-chat race (first wins, second no-ops).

### Added (M18 E2 — warm "prototyping" image variant, 2026-07-16)

- A second, per-group container image profile, `image_profile = minimal |
  prototyping` (default `minimal`, secure-by-default). `prototyping` bakes a
  warm web-prototyping toolchain into the session image so the first "build me
  a web app" doesn't burn its opening minutes bootstrapping (and, since
  containers have no apt egress at runtime, so the tools are present rather
  than un-installable): `sqlite3`, headless `chromium` (doubles as a
  browser-render fallback), and `zip` via apt, plus global `vite` /
  `create-vite` pre-seeded through the existing `npm install -g` mechanism.
- `crates/copperclaw-types/src/image.rs` (new): the shared `ImageProfile`
  enum + the `PROTOTYPING_APT_PACKAGES` / `PROTOTYPING_NPM_PACKAGES` bundles
  (single source of truth; both `copperclaw-db` and `copperclaw-container-rt`
  consume it without depending on each other).
- Migration `027_container_config_image_profile.sql`: `container_configs`
  gains a nullable `image_profile TEXT` column (NULL reads back as `minimal`,
  so existing groups are untouched on upgrade). Registered in
  `crates/copperclaw-db/src/migrate.rs`'s CENTRAL list.
- `crates/copperclaw-db/src/tables/container_configs.rs`: `ContainerConfig` /
  `UpsertContainerConfig` carry `image_profile: ImageProfile`; narrow setter
  `set_image_profile`. UNLIKE `tool_profile` / `verify_gate` /
  `surface_thinking`, `image_profile` IS folded into `compute_fingerprint`
  (it changes baked packages, so it must trigger a rebuild) — folded
  CONDITIONALLY so a `minimal` config hashes byte-identically to a pre-E2 one
  (no mass rebuild on upgrade), while a profile change flips the fingerprint.
- `crates/copperclaw-container-rt/src/build.rs`: `ImageBuildSpec` carries the
  profile; `effective_apt_packages` / `effective_npm_packages` append the
  profile's extras, and both `dockerfile()` and `fingerprint()` render/hash
  through them. `crates/copperclaw-host/src/container_manager/spawn.rs`'s
  `rebuild_image` threads `cfg.image_profile` into the build spec.
- `crates/copperclaw-setup/src/steps/image.rs`: the image step asks once for
  the base image's profile (`COPPERCLAW_SETUP_IMAGE_PROFILE`, default
  `minimal`) and bakes it via `default_spec(profile)`; the choice is recorded
  on `SetupConfig.image_profile`. Idempotent — the same answer maps to the
  same fingerprint/tag.
- Operators set it per group via
  `cclaw groups config update --field 'image_profile="prototyping"' <id>`
  (`crates/copperclaw-host/src/handlers/groups.rs`, validated against the
  known profile names); a spawned child agent inherits its parent's profile.
### Added (M18 R5 — hot in-session provider failover, 2026-07-16)

- The in-container runner now fails over between providers **mid-turn**
  instead of dying with an apology when a gateway hiccups: a 20-minute build
  no longer dies at minute 18 because one provider rate-limited. When the
  primary provider exhausts its two in-provider retry layers,
  `crates/copperclaw-runner/src/run/provider_call.rs`'s `run_llm_turn` now
  walks a host-resolved ordered chain of healthy fallbacks
  (`RunnerDeps::failover_chain`), retries the SAME LLM call against the next
  entry, and only surfaces the terminal apology once the WHOLE chain is
  exhausted. Each attempt reports its OWN `usage_report`
  (`emit_usage_report` now takes the serving provider/model), so the host's
  degrade/restore health fold stays authoritative — the failed primary is
  degraded and the fallback that served stays healthy. An empty chain (the
  default, and every unconfigured group) is byte-identical to the pre-R5
  single-provider path. The switch is surfaced on the H1 Task HUD via
  `TaskHud::add_note("switched to <provider>")` so a mid-run style change
  isn't mistaken for confusion.
- Host: `crates/copperclaw-host/src/container_manager/runner_config.rs`'s
  `runner_config_for` resolves the ordered healthy chain at spawn
  (`resolve_failover_chain_for_file`, backed by
  `container_manager/provider_failover.rs::failover_alternates`) and writes
  it into `runner.json`'s new `failover_chain` field (skipped when
  empty/unconfigured — bit-identical shape otherwise). The runner parses it
  (`crates/copperclaw-runner/src/config.rs`: `failover_chain` /
  `FailoverEntryFile` / `FailoverProviderConfig`) and pre-builds the
  alternate providers at startup (`main.rs::build_failover_chain`).
- **Security boundary (in the PR):** the in-container failover chain is
  limited to entries the container can ALREADY reach WITHOUT shipping a new
  credential — no-auth local providers (ollama/codex), Anthropic-envelope
  entries brokered by the existing capability token when the credential
  broker is on, or Anthropic entries reusing the SAME `api_key_env` the
  primary already injected. An Anthropic entry that would require a DIFFERENT
  real key (a second account, broker off) is excluded and logged. The
  container secret surface is never broadened for failover; alternates reuse
  the primary's already-injected credential slot + endpoint and vary only
  the model (and possibly the provider kind, to a local model).

### Added (M18 V1 — WebSocket pass-through in the preview proxy, 2026-07-16)

- `crates/copperclaw-host/src/preview.rs`: the session-preview reverse proxy
  now bridges WebSocket upgrades instead of refusing them with 501. On a
  cookie-authenticated upgrade, the axum side completes the handshake with the
  browser and a `tokio-tungstenite` client opens `ws://<container_ip>:<port>`
  to the container app, forwarding frames both ways (subprotocol mirrored, e.g.
  Vite's `vite-hmr`); the upstream socket is opened first so a container not
  serving a socket at that path fails fast with 502. The cookie gate applies to
  the upgrade exactly as to HTTP (no cookie → 403 before any upgrade). The idle
  reaper now treats an open socket as activity — every frame in either
  direction and a 60s keep-alive tick bump `last_activity` — so a live browser
  tab is never reaped mid-session (HTTP requests only bump per-request). Adds
  `axum`'s `ws` feature plus `tokio-tungstenite` (0.24, the version the discord
  channel already pins) and `futures` to `copperclaw-host`.
- `skills/preview/SKILL.md`: dropped the "WebSockets are not proxied — prefer
  polling" caveat; agents can now build Vite dev servers, live reload, and
  realtime apps behind the preview link.

### Added (M18 V3 — live headless-browser driver, 2026-07-16)

- `crates/copperclaw-browser/src/cdp.rs` (new): the concrete Chromium/CDP
  `BrowserDriver` behind the existing trait. `CdpBrowserDriver` speaks the
  Chrome DevTools Protocol over a WebSocket to the Chromium in the locked-down
  child container and produces the requested read-only artifact
  (`Page.captureScreenshot` PNG, `Runtime.evaluate` DOM text, or
  `Accessibility.getFullAXTree` flattened to text). The command sequence sits
  behind a small `CdpTransport` seam and is fully unit-tested with a mock
  transport; the live `WsCdpTransport` (over `tokio-tungstenite`, already in
  the workspace lock via the discord adapter) carries the real session and is
  exercised only behind the opt-in gate. Redirect hops + main-document status
  are observed from `Network.*` events (pure parsers `redirect_hops_from_events`
  / `main_status_from_events`) so the SSRF per-redirect re-guard still fires
  against a live chain. We hand-rolled the minimal CDP client rather than
  pull `chromiumoxide`/`headless_chrome` because those spawn a *local*
  Chromium process, whereas ours runs in a dedicated child *container* the
  driver must *connect* to — and the seam keeps the crate `unsafe`-free,
  clippy-clean, and unit-testable without a live browser.
- `crates/copperclaw-browser/src/live.rs` (new): `render_live` — the
  previously-deferred privileged spawn path (`container.rs:23-25`). It runs
  the SSRF target pre-flight, `spawn`s the locked-down child spec via the
  `ContainerRuntime` seam (the `runtime.spawn` call that did not exist
  before), resolves the child's bridge IP, connects a CDP session
  (`CdpConnector` / `WsCdpConnector` via the browser's `/json/new` endpoint),
  renders through the same `driver::render` orchestration, and tears the
  container down unconditionally. Unit-tested against a mock runtime + mock
  connector (happy path, teardown-on-connect-failure, no-bridge-IP,
  spawn-failure, and SSRF-target-blocked-before-spawn).
- `crates/copperclaw-mcp/src/tools/browser_render.rs`: `handle()` no longer
  returns the terminal "driver not provisioned" error. It now runs every
  safety step (`prepare`), detects a container runtime, and drives
  `copperclaw_browser::render_live` to return the real PNG path / DOM text /
  ARIA snapshot. Still gated behind `COPPERCLAW_BROWSER_ENABLED`: unset →
  byte-identical "disabled" validation error before any live path; enabled but
  no container runtime reachable (e.g. the in-container runner, which has no
  Docker socket by design) → a clean "renderer unavailable here" report, never
  a panic. New optional `COPPERCLAW_BROWSER_OUTPUT_DIR` selects where
  screenshots land (V4 refines this to the session `/data` dir). All SSRF /
  deny-default-egress / forbidden-env / unprivileged-user / hardened-sandbox
  properties are preserved unchanged.

### Added (M18 T2 — unconditional data-root env override, 2026-07-16)

- `COPPERCLAW_DATA_ROOT` env var, consulted by
  `copperclaw-mcp`'s `verify_gate::data_root()` (and, through it,
  `todo.rs`'s todo-store path). When set on the runner process's
  environment it replaces `/data` as the root the R3 verify gate and the
  per-session todo store resolve against; when unset, production behavior
  is byte-identical to the compiled-in `/data` default. Mirrors the shell
  tool's `COPPERCLAW_SHELL_STATE_FILE` precedent. This un-gates the R3
  verify mechanic for host-side integration tests / fixtures, which run
  outside a container where `/data` is an unwritable root-owned path
  (the gap X1 flagged). Security: the runner process env is
  host-controlled at spawn; an in-container agent's `shell` calls execute
  inside the container, not the runner process, so they cannot mutate this
  var and cannot use it to escape the gate. The pre-existing
  `#[cfg(test)]` in-process override still wins over the env var, so the
  gate's own test battery is unaffected.

### Fixed (M18 T2 — artifact_path test race, 2026-07-16)

- `copperclaw-mcp`'s `artifact_path.rs` unit tests
  (`returns_host_path_from_discovery_file`,
  `error_when_discovery_file_missing`) now serialize on a shared `Mutex`
  guard (an RAII `HostPathGuard`, same shape as `todo.rs`'s `TodoGuard`).
  They share the global `HOST_PATH_FILE_TEST_OVERRIDE` static; without
  serialization a parallel `cargo test` run could leak one test's
  override into the other's assertions — a confirmed latent race, more
  likely to surface under full-workspace load.

### Changed (M18 R4 — compaction that survives long builds, 2026-07-16)

- `crates/copperclaw-runner/src/compaction.rs`: (a) the token estimator
  (`estimate_tokens`) replaces the flat 4-chars/token heuristic with a
  calibrated ~3.5-chars/token approximation (applied as the exact rational
  2/7 in integer math) over a *whitespace-collapsed* character count, so
  indentation- and JSON-heavy build transcripts no longer over-count and
  trip compaction early. Chosen over `tiktoken-rs` deliberately: `cl100k_base`
  is GPT's tokenizer (itself only an approximation of Claude's token count)
  and would add ~1.5 MB of BPE vocab + a `fancy-regex` dep to every
  in-container runner binary, for precision the safety margins already
  absorb — error bounds documented at the `estimate_tokens` docstring. (b)
  the soft compaction target default is now profile-conditional
  (`default_soft_target_for_profile`, wired through
  `crates/copperclaw-runner/src/config.rs`): code-oriented profiles
  (`Coding`/`Full`) get 80K on a 200K window so a 90-minute build keeps its
  mid-task detail; `Messaging`/`Minimal` keep the original 40K. Still
  config-clamped — an explicit `soft_compaction_target_tokens` / env var
  overrides, and the hard ceiling always clamps via `effective_threshold`.
  (c) a structured **project-facts header** (project path, git branch,
  recorded verify command, and the todo-list plan) is now sourced fresh from
  on-disk R3 verify-gate state + the todo store on every compaction and
  pinned **verbatim** at the front of the compacted transcript — never handed
  to the summarizer — so a long build never loses those facts to
  summarization. Exactly one, always-current header is carried forward; a
  pure-chat session with no project/todo state gets no header (byte-identical
  to pre-R4). `CompactionCfg` gains a `data_root` field (the `/data` mount)
  as the header's source root.
### Added (M18 C4b — discord inbound files, 2026-07-15)

- Discord now downloads inbound message attachments from the CDN and stages
  them per the C3 inbound-file contract, so "here's the file, build around
  it" actually reaches the agent's `/data/inbox/...`. The Discord adapter
  (`crates/copperclaw-channels/discord/src/{events,rest,config,adapter,factory}.rs`)
  gains `DiscordRest::download_cdn_file` (a public, auth-header-free GET —
  Discord CDN URLs are pre-signed, unlike Slack's `url_private`) and
  `events::message_create_to_inbound_downloaded`, which fetches the first
  attachment, enforces the new `max_attachment_bytes` config (default
  25 MiB, Discord's non-Nitro cap), stages via
  `copperclaw_channels_core::inbound_file::stage_inbound_file` (setting
  `staged_path`, never `path`), and inlines small images as `data_base64`
  for vision parity with Telegram. Oversized files yield a `too_large`
  system row and download errors a `download_failed` row — never a silent
  drop. Opt-out via `attachment_download: false`. New replay fixture
  `fixtures/discord/inbound-file-attachment/` (registered as
  `discord_inbound_file_attachment_round_trip` +
  `..._file_readable_from_session_dir` in
  `crates/copperclaw-host/tests/replay.rs`) mirrors the C3 telegram fixture.
### Added (M18 C4a — Slack inbound files, 2026-07-15)

- The Slack adapter now receives inbound files. Previously
  `crates/copperclaw-channels/slack/src/events/router.rs` ignored a
  message's `files[]` entirely, so "here's the CSV/spec, build around it"
  never reached the agent. The events router now downloads a message's
  first file from its `url_private` link with the bot token
  (`Authorization: Bearer <bot-token>`, wired via the new
  `SlackApi::download_file`), enforces the same `max_attachment_bytes`
  config as Telegram (new `SlackConfig` field, default 20 MB), and stages
  the bytes per the C3 channels-core inbound-file contract
  (`stage_inbound_file` → `content.attachment.staged_path`, never `path` —
  the router owns materialization into `/data/inbox/<msg_id>/<file>`).
  Small images additionally inline `data_base64` for vision parity with
  Telegram's `inline_image_base64`. Download/size failures downgrade to a
  `MessageKind::System` row with the same `too_large` / `download_failed`
  taxonomy Telegram emits — never a silent drop. New replay fixture
  `fixtures/slack/inbound-file-attachment/` (registered as
  `slack_inbound_file_attachment_round_trip` in
  `crates/copperclaw-host/tests/replay.rs`) mirrors the C3 Telegram
  fixture; the `url_private` download itself is unit-tested against a mock
  Slack server in the adapter crate.

### Changed (M18 P2 — skills refresh for the verify + delivery contract, 2026-07-15)

- Updated five agent-facing skills to teach the post-M18 coding workflow
  instead of the pre-M18 one (`skills/{coding-task,testing,debug,preview,send-file}/SKILL.md`):
  the R3 verification contract (record a project's one-line check command
  in `/data/<project>/.copperclaw/verify`, or via a per-group
  `check_command` override; the `todo_update(status="completed")`
  completion gate refuses while a project is dirty, quoting the actual
  refusal wording, and auto-`blocked`s after two fix cycles), T1's
  `shell tail_bytes` idiom for reading the END of a truncated build log,
  T1's paged `read_file` (`mode:"lines"` + `offset`/`limit` +
  `total_lines`), and the artifact-delivery close (`send_file` a
  `git archive` zip, the `artifact_path` host path, and an
  `expose_preview` link) as the mandatory final build step. The fuller
  P3 "prototype ready" `send_card` ritual is noted as forthcoming rather
  than taught, since P3 is not yet implemented. Skills are docs but
  operator-visible: stale skills actively teach the pre-M18 workflow.

### Added (M18 X1 — golden-path program fixture, 2026-07-15)

- New replay fixture `fixtures/cli/prototype-golden/` (registered as
  `cli_prototype_golden_path` in `crates/copperclaw-host/tests/replay.rs`):
  the M18 program's acceptance test. A scripted 8-round tool loop drives
  "build me a tiny HTTP todo app" end to end through the real
  inbound → router → runner → outbound → delivery pipeline: `git init` a
  project, scaffold + verify (`python3 -m py_compile`) a small stdlib-only
  Python HTTP todo server, commit, expose a mock-brokered preview
  (`expose_preview`), send the P3 ritual `send_card`, and close with a
  summary. Byte-stable `expected/*.jsonl` committed; every later card that
  changes this path updates the fixture in its own PR per the program plan.
- Two harness additions in-lane for the card (`crates/copperclaw-host/tests/replay/{fixture.rs,harness.rs}`):
  `manifest.gates: ["preview"]` wires a deterministic `FixturePreviewBroker`
  onto `DeliveryService` (via the already-public `set_preview_broker`, the
  same seam `copperclaw-host-delivery`'s own unit tests use) and advertises
  the M17 `expose_preview` / `close_preview` tools to the per-step runner;
  a background delivery-poller now runs for the duration of a preview-
  gated turn so the M17 external-MCP relay's 120s blocking poll
  (`EXTERNAL_MCP_DEADLINE_SECS`) actually gets serviced within the same
  turn instead of always timing out (the harness's normal per-step
  sequencing runs the whole turn before ever calling `deliver_session`,
  which production's concurrent delivery loop doesn't have to worry
  about). `manifest.max_tool_turns` is now overridable per fixture
  (default unchanged at 5) for scripted sequences with more tool rounds.
- **Two explicit, diagnosed limitations** (see `fixtures/cli/prototype-golden/README.md`
  for the full writeup) — the fixture does NOT exercise the R3
  verification gate or the H1 live Task HUD, and why not:
  - `verify_gate.rs`'s `data_root()` and `todo.rs`'s `todo_path()`
    hardcode the literal `/data` container mount, with only a
    `#[cfg(test)]`-gated override invisible to `copperclaw-host`'s
    separate integration-test binary — and `/data` is a real, unwritable,
    root-owned path on every host that would run this test suite. The
    smallest fix (un-gating the existing override, `#[doc(hidden)] pub`,
    zero production call sites) was attempted and reverted after the
    security review correctly flagged it as a genuine capability
    weakening needing the user's own explicit sign-off — out of this
    card's authorization. Root cause of R3's own "build-verify-loop"
    e2e fixture being left as an incomplete stretch goal; a `test-support`
    Cargo feature or an unconditional env-var override (mirroring the
    shell tool's already-shipped, non-test-gated `COPPERCLAW_SHELL_STATE_FILE`)
    is the right, small, explicitly-authorized follow-up.
  - `cli` is not in `capabilities::EDIT_CAPABLE_CHANNELS`, so the HUD's
    `Behavior` is always `StatusRows` there (never `Live`/`FinalOnly`),
    and even that fallback's 60s-gated heartbeat has no "done in M:SS"
    finalize arm — a live/finalized HUD fixture needs an edit-capable
    channel (telegram/slack/discord/matrix/webex), not cli.
- **Metrics wish (for M1):** a counter for preview-expose calls serviced
  vs. timed out (label: `outcome=served|timeout`) would make the M17
  relay's real-world latency visible in `cclaw usage` — this fixture's
  own diagnosis of the 120s blocking-poll behavior is exactly the kind
  of thing such a counter would have surfaced sooner.

### Fixed (inbound-file contract: session-local materialization — M18 C3, 2026-07-15)

- **Non-image inbound attachments (documents, audio, video, ...) are now actually reachable by the agent.** Telegram previously downloaded attachments straight into the *channel's own* `data_dir/inbox/<msg_id>/` and put that host path in `content.attachment.path`, but the container only mounts the *session* directory at `/data` — so any file besides an inlined-base64 image was unreachable by the agent it had just been told about. New contract in `crates/copperclaw-channels/core/src/inbound_file.rs`: adapters download into a per-file staging temp dir (`stage_inbound_file`) and surface `content.attachment.staged_path` — never `path`, which is now exclusively a router-set key. The **router**, which is the first place that knows the resolved session, materializes the staged bytes at route time into `<session_dir>/inbox/<msg_id>/<safe_name>` (`Router::materialized_content` in `crates/copperclaw-host-router/src/route.rs`, via the hardened `copperclaw_db::attachments::extract_to_inbox` — `O_EXCL|O_NOFOLLOW`, canonicalized-path containment check), strips `staged_path`, and rewrites `attachment.path` to the container-visible `/data/inbox/<msg_id>/<safe_name>`. Both the message id and the filename are re-sanitized router-side (defense in depth) so a hostile `../../etc` id or `../../evil.sh` filename still lands inside the session inbox. The staged source file (and its unique per-download directory) is always removed after the fanout completes, regardless of route outcome (delivered, dropped, debounced, pending, or errored), so adapters never need to garbage-collect staging themselves. A materialization failure (staged file vanished, disk error) never drops the message — the attachment keeps its metadata, loses `staged_path`, gains an `error` note, and the message text still reaches the agent. Telegram (`crates/copperclaw-channels/telegram/src/ingress/mod.rs` and friends) fully migrates to the contract; the `too_large` / `download_failed` system-row fallbacks are unchanged. New e2e replay fixture `fixtures/telegram/inbound-document-attachment/` pins the acceptance: a document attachment materializes to the container path in `messages_in`, and an oversized document still yields the `too_large` system row (`crates/copperclaw-host/tests/replay.rs`).
- **Note for C4a/C4b implementers (Slack, Discord inbound files):** follow this same contract — stage via `stage_inbound_file`, set `staged_path` only, never `path`. `deltachat` still writes `content.attachment.path` directly (pre-existing behaviour, out of scope for this card — it isn't one of the C3/C4a/C4b flagship channels); worth a look whenever that channel is next touched.
- **Metrics wish (for M1):** a counter for inbound files materialized per channel (and one for materialization failures) would make this path's real-world hit rate visible in `cclaw usage`.

### Added (verification gate — M18 R3, 2026-07-15)

- **Todos can no longer be marked `completed` on vibes.** The runner now tracks "dirty since last verify" per project directory under `/data`: any successful `write_file` / `edit_file` / `multi_edit` / `apply_patch`, or any `shell` call with an explicit `cwd` resolving to a project, marks that project dirty (file markers under `<project>/.copperclaw/`, best-effort I/O — a marker-write failure never fails the underlying tool call). `todo_update(status: "completed")` now runs a session-wide dirty scan (todos aren't linked to a specific project in the store, so any dirty project blocks completion) after its existing evidence check: a dirty project refuses the completion with a structured error naming the project, its recorded verify command (or a hint to write one to `.copperclaw/verify`), and fix-cycles remaining. Running the recorded verify command via `shell` (exit 0) clears dirty; a failing run records the failure tail and increments a fix-cycle counter (cap 2). After the cap is burned and the project is *still* dirty, the gate stops refusing and instead auto-transitions the todo to a new `blocked` status (with the failure text attached) and returns success — never silently completed, but never permanently stuck refusing either. Per-group escape hatch: `container_configs.verify_gate = 0` (via `cclaw groups config update --field verify_gate=false`) restores byte-identical pre-R3 behaviour (evidence-only check, no dirty-tracking) — a pure-chat group that never touches `/data/<project>/...` never engages the gate regardless. `container_configs.check_command` optionally overrides whatever verify command the agent itself recorded. New crate module `crates/copperclaw-mcp/src/tools/verify_gate.rs` (dirty-tracking primitives + the trickiest bit, `project_root_of`, resolving a container path to its owning `/data/<project>` root); hooked into `crates/copperclaw-mcp/src/tools/{computer_use,edit_file,multi_edit,apply_patch,todo}.rs`. Config plumbing: `container_configs.check_command` / `.verify_gate` (migration 026, part 1/2) → `runner.json`'s `check_command` / `verify_gate` (`crates/copperclaw-runner/src/config.rs`) → two new `ToolContext` trait methods (`verify_gate_enabled` / `check_command_override`, default on/`None` so mocks and subagent contexts are unaffected) that the `copperclaw-mcp` tool handlers consult directly, since they only see `&dyn ToolContext`, not the runner's own config.
- **Metrics wish (for M1):** counters for verify-gate refusals, verify-run passes/failures, and blocked-auto-transitions (label: `outcome=refused|blocked|passed`) would make the gate's real-world hit rate visible in `cclaw usage`.

### Added (mid-turn interruption + steering — M18 R2, 2026-07-15)

- A long-running turn can now be redirected or stopped without waiting for it to finish. Between tool batches inside `drive_turn` — the natural cooperative point, right after each batch's results are persisted — the runner peeks `inbound.db` for any row that arrived after the turn began (`copperclaw_db::tables::messages_in::get_new_since`, keyed off a `max_seq` snapshot taken at turn start): a `/stop` control row (M18 R1's contract) ends the turn cleanly with a "stopped — here's where things stand" reply and `TurnOutcome::Done` (the session is reusable immediately, no apology/failure path); a new human `Chat` row with no `/stop` present is folded into the transcript as a `[user interjection]` message and the loop continues, so "actually use SQLite" lands within one tool-batch boundary instead of after 150 turns. Either way the consumed row is marked `completed` inline so `run_loop`'s next poll never double-processes it; anything else peeked (a scheduled Task fire, another session's dispatch) is left untouched for the next full poll. Interjections also post a one-shot "steering noted" note on the Task HUD via the existing `TaskHud::add_note` hook (shipped, unused, by M18 H1). Applies uniformly to autonomous and human-triggered turns; only checked between turns, never mid-provider-call (`crates/copperclaw-runner/src/run/drive_turn.rs`, `crates/copperclaw-db/src/tables/messages_in.rs`).
- **Follow-up for X1 (lane X):** covered by new runner unit tests (`mid_turn_stop_control_row_ends_turn_cleanly`, `mid_turn_chat_interjection_folds_in_and_continues`) rather than a replay/e2e fixture — the shared replay harness drives one inbound event fully (including its whole multi-turn tool loop) before the next `inbound/NNN-*.json` step is injected, so it can't currently express a row landing *during* another inbound's tool loop. Genuinely fixturing that race needs a harness change (a hook to write a second inbound mid-drive), which is lane-X scope, not lane-R.
- **Metrics wish (for M1):** a counter for mid-turn stops vs. interjections consumed (label: `kind=stop|interjection`) would make R2's real-world usage visible in `cclaw usage`.

### Changed (Task HUD: one self-editing status message per task — M18 H1, 2026-07-15)

- A multi-minute agent run now surfaces ONE self-editing Task HUD message per inbound task instead of the old trio of overlapping progress mechanisms (env-gated per-tool breadcrumb chips, the 60s "still working" status row, the typing bubble alone): the HUD posts at the first tool call and is edited in place after every tool batch (plus a 30s wall-clock ticker), showing the current `agent_todos.json` step, the last tool + running/done, the cumulative tool count, and elapsed time; the final edit collapses it to a one-line "done in M:SS, N tool calls". It rides the existing breadcrumb/`update_breadcrumb` delivery rails, so it only runs on channels whose adapter supports in-place edits (static table in the new `copperclaw_channels_core::capabilities` module: telegram/slack/discord/matrix/webex); bare channels (cli, webhooks, ...) keep the old periodic status rows, and surfaces with no visible typing indicator (Slack off assistant threads, per PR #24's flag) force full HUD behaviour at a tighter 10s cadence. Config: `hud_mode = full|final|off` (default `full`), host-wide via `COPPERCLAW_HUD_MODE` in `.env` -> `runner.json` (a per-group column is deferred; migration 026 is reserved). The per-tool breadcrumb chips (`COPPERCLAW_TOOL_BREADCRUMBS` / `COPPERCLAW_BREADCRUMB_STYLE`) and the in-loop 60s status emit were removed, not left as a fourth mechanism (`crates/copperclaw-runner/src/run/hud.rs`, `run/drive_turn.rs`, `tools.rs`, `crates/copperclaw-channels/core/src/capabilities.rs`, `crates/copperclaw-host/src/container_manager/runner_config.rs`).

### Added (end-user slash commands, M18 R1 — 2026-07-15)

- End-user slash commands parsed router-side (`crates/copperclaw-host-router/src/commands.rs`): `/stop` (alias `/cancel`) persists a documented `control{op:stop}` row into `messages_in` (`kind=system`, `trigger=0`, pending for M18 R2's mid-turn consumer) even while a turn is in flight; `/status` is answered by the host from central-DB state straight into `messages_out` (new `RouteOutcome::Answered`, new `SessionRoot::outbound_pool`) without waking the runner; `/compact` and `/clear` (aliases `/reset`, `/new`, telegram `@BotName` suffix and case normalised) bypass the group-chat mention gate and wire through to the runner's existing sentinels; unknown `/x` falls through to the agent unchanged. Replay fixtures per command on cli + telegram under `fixtures/{cli,telegram}/slash-*` (harness: `count_due` spawn-mirror gate + `runner_drain` manifest flag, `docs/replay-fixtures.md`).

### Fixed (fence-aware message splitter — 2026-07-15)

- The delivery loop's chat-text splitter no longer cuts a chunk in the middle of a code fence — a split fence rendered as garbage on Telegram/Discord (the most visible "janky" signal for a coding agent). `split_text_into_chunks` (`crates/copperclaw-host-delivery/src/service.rs`) now consults a self-contained fence scanner (`crates/copperclaw-host-delivery/src/fence.rs`, markdown ``` fences and Telegram HTML `<pre>` blocks): when the natural cut lands inside a fence it cuts after the fence if the whole fence fits the window, before the fence when pre-fence content exists, and otherwise closes the fence at the cut and reopens it with the same info string / tag on the next chunk — so every emitted chunk parses with balanced fences. Pinned end-to-end by the new `fixtures/telegram/long-code-reply` replay fixture.

### Removed (dead tool-policy floor — M18 R0, 2026-07-15)

- Deleted the runner's decorative `DISALLOWED_TOOLS` floor (and its `disallowed` compat module): its nine pascal-case Claude-Code built-in names (`CronCreate`, `EnterPlanMode`, ...) never matched the runner's snake_case tool inventory, so the floor denied nothing — the `ToolProfile` allow-lists (plus sender-role, active-skill, and provenance layers) are and remain the real gate. A new `tool_name_drift` integration test (`crates/copperclaw-runner/tests/tool_name_drift.rs`) now pins every name in the policy lists (exported as `policy::PROFILE_TOOL_LISTS`) to `copperclaw_mcp::build_tool_set()` so a renamed or removed tool fails CI instead of leaving a decorative allow-list (`crates/copperclaw-runner/src/policy.rs`).


### Changed (Slack typing degrades gracefully off assistant threads — 2026-07-15)

- Slack `set_typing` now only calls `assistant.threads.setStatus` on assistant-thread surfaces (a thread inside the bot's `D…`-prefixed DM — the one place Slack renders the status) and skips the silent-no-op API round-trip on channel/group threads and thread-less DMs; the gap is reported through a new additive `ChannelAdapter::typing_indicator_visible(platform_id, thread_id)` capability flag (default `true`; Slack overrides it) that the M18 Task HUD (card H1) will read to force `hud_mode=full` + a tighter edit cadence where the platform shows no typing signal (`crates/copperclaw-channels/slack/src/adapter.rs`, `crates/copperclaw-channels/core/src/adapter.rs`).

### Fixed (typing-indicator ticker backs off on channel rate limits — 2026-07-15)

- **The host's `TypingTicker` no longer hammers a rate-limited channel every
  4s.** Telegram (and other adapters) answer `set_typing` with
  `Rate { retry_after }`; the ticker ignored it and re-fired on the next tick,
  producing warn spam (68 `dispatcher: set_typing failed err=Rate` lines in one
  day) and wasted API calls. `DeliveryDispatcher::set_typing` now returns the
  dispatch outcome (`TypingOutcome::Ok` / `RateLimited`) via a receiver; the
  ticker installs a per-session cooldown (`retry_after` seconds, or a 10s
  fallback) and skips that session until it lapses, evicting the cooldown with
  the existing idle eviction. Best-effort typing failures are now logged at
  `debug`, not `warn`. `crates/copperclaw-host/src/typing_ticker.rs`,
  `crates/copperclaw-host-delivery/src/dispatch.rs`,
  `crates/copperclaw-modules/src/context.rs`.
### Added

- `read_file` lines-mode results now report `total_lines` (whole-file line count, free since lines mode already reads the file) and `shell` accepts `tail_bytes: N` to keep the LAST N bytes of each stream instead of the first 32 KiB — the recovery path for a failing build whose error sits at the end of the log; on default head-truncation the appended hint now names `tail_bytes` and the `/data/.jobs/` background-log path, both tool descriptions document the paging/tail idioms, and defaults stay byte-identical (no new fields unless the new parameters are used) (`crates/copperclaw-mcp/src/tools/computer_use.rs`).
- The core coding disciplines (git repo per project with `git init` first, commit per working increment, run the project's own check command before marking a code todo `completed`, always end with an artifact-delivery step via `send_file` / `artifact_path` / `expose_preview`) are now inlined into the base system prompt as a static `CODING_PREAMBLE` block whenever the group's tool profile can write code (`coding` / `full`, including the unset default) — previously these rules lived only in the `coding-task` skill body and were lost whenever a model (worst on small local ones) forgot `load_skill("coding-task")`; the block points at that skill for depth, is fixed per spawn so the prompt-cache prefix stays stable, and `messaging` / `minimal` profiles gain zero new prompt bytes (pinned by test) (`crates/copperclaw-host/src/container_manager/prompt.rs`, `runner_config.rs`).

- `cclaw doctor` now runs a `disk-space` check on the filesystem holding the install's data dir (`resolve_install_root()/data`): WARN below 10% free or 20 GiB free, FAIL below 3% or 5 GiB, each with a `fix:` reclaim-space hint; a `statvfs` failure or unresolvable path degrades the row to WARN instead of panicking. Closes the gap that left doctor all-OK through a live root-fs-full incident that silently degraded the host. Thresholds live in the pure, unit-tested `disk_level()`; free space is read via `rustix::fs::statvfs` (safe, no `unsafe` — new `rustix` workspace dep with the `fs` feature) (`crates/copperclaw-cclaw/src/lib.rs`).

### Fixed (delivery loop no longer poisoned by a duplicate `delivered` record — 2026-07-15)

- Made `delivered::insert` (`copperclaw-db/src/tables/delivered.rs`) idempotent
  (`ON CONFLICT(message_out_id) DO NOTHING`, returns whether a row was newly
  inserted) and made the delivery pass's in-flight claim atomic via the DashMap
  `entry` API (`copperclaw-host-delivery/src/service.rs::process_session_once`).
  Root cause: the 1s active loop and the 60s sweep loop share one
  `DeliveryService` and each pass reads the already-delivered id set once at its
  start; a concurrent pass could re-send and re-record a row whose snapshot went
  stale, and the old plain `INSERT` raised `UNIQUE constraint failed:
  delivered.message_out_id` — which then made the error arm try to record the
  row a third time as `failed`, hitting the same constraint and erroring the
  whole pass on every subsequent tick until the session was deleted.
### Added (session preview proxy — 2026-07-14)

- **Session preview proxy (M17):** an in-container agent can expose an HTTP app it built to the operator's machine / LAN for hands-on testing via two new first-party tools, `expose_preview {port, name?}` and `close_preview {port}`. The tools ride the existing external-MCP host-broker relay under the reserved server name `__preview` (`crates/copperclaw-runner/src/run/preview.rs`, dispatch hook in `run/tool_dispatch.rs`); the delivery loop routes `__preview` requests to a host-side broker (`crates/copperclaw-host-delivery/src/service.rs::execute_preview_call`, trait in `crates/copperclaw-modules/src/preview.rs`). The broker (`crates/copperclaw-host/src/preview.rs`) resolves the container's bridge IP (new `ContainerRuntime::container_ip`, Docker impl via inspect in `crates/copperclaw-container-rt`), allocates a host port from 8100-8199 (caps: 4/session, 16/host), and serves a token-gated axum reverse proxy: `GET /__preview/<token>` sets an HttpOnly cookie + redirects to `/`; requests without the matching cookie get 403; WebSocket upgrades are refused (501, v1). Secure-by-default: **off per group** until the operator runs `cclaw groups config update --field preview_enabled=true <group>`; the proxy binds loopback unless `preview_bind` is set (e.g. `"0.0.0.0"` for LAN, validated as an IP) — migration `025_container_config_preview.sql` adds both columns, editable via `cclaw groups config get/update/edit`. Runner policy treats both tools as coding-profile AND credentialed-external (guest-denied, blocked on tainted turns without fresh approval, blocked on autonomous turns). Previews expire after 30 min idle (reaper), and are torn down on `close_preview`, container idle-stop/crash (container-manager hook), and host shutdown — every expose/teardown writes an `audit_log` row. `__preview` is rejected as an external MCP server name in the add-server handlers. New `skills/preview/SKILL.md` teaches the flow (serve on 0.0.0.0, relay the URL verbatim, relay the enable command on the off-state error).

### Fixed (dynamic model identity — 2026-07-14)

- The agent's system prompt `# Environment` block now carries a `Model:` line with the session's actual resolved model (post-failover, same value as `runner.json`), and `skills/identity/SKILL.md` was rewritten model-agnostic: it instructs the agent to answer "what model are you?" from that line and never hardcodes a model name. Previously the skill's examples said "Powered by Claude Sonnet 4.6 under the hood", which non-Claude models parroted verbatim (`crates/copperclaw-host/src/container_manager/prompt.rs`, `skills/identity/SKILL.md`).

### Fixed (admin-socket data dir — 2026-07-14)

- The cclaw socket server built its `HandlerCtx` with the default relative `"data"` path instead of the install's absolute data dir, so `sessions.delete` never removed the on-disk session directory when the host ran daemonized (it reported `directory_removed: false` with no warning) and dead-letter `dropped-messages replay` resolved per-session DBs against the daemon's CWD. `serve_listener`/`run_server` now take the data dir explicitly and boot passes `cfg.data_dir` (`crates/copperclaw-host/src/{socket.rs,boot.rs}`); regression e2e drives `sessions.delete` through the real socket server with a non-CWD data dir.
### Added (M17 D1 — cclaw color + TTY awareness — 2026-07-14)

- `cclaw` human-readable output is colorized when stdout is a real terminal: doctor OK/WARN/FAIL levels (green/yellow/red), `fix:` hint lines (cyan), table and dashboard section headers (bold), and `remote error:` lines (red). Gated on `std::io::IsTerminal`, the `NO_COLOR` convention, and a new global `--no-color` flag (`crates/copperclaw-cclaw/src/style.rs`); `--json` output is never styled and piped output stays byte-identical.

### Fixed (M17 D2 — honest `sessions get`, new `sessions tail` — 2026-07-14)

- `cclaw sessions get` now delivers what its help text always claimed: the session row plus the last 10 `messages_in` / `messages_out` rows (kind, status, timestamp, ~120-char secret-redacted content preview), read read-only from the per-session DBs host-side (`crates/copperclaw-host/src/handlers/sessions.rs`); and a new `cclaw sessions tail <id> [--follow]` prints the merged time-ordered rows with direction markers (`<-` inbound, `->` outbound, `--` breadcrumb/status kinds), polling 1s under `--follow` — safe against a running session (WAL concurrent reader), and message previews are withheld from agent callers asking about foreign sessions.
### Changed (event-driven wake for idle sessions — M17 C3 — 2026-07-14)

- A message to a stopped/idle session now spawns its container within ~one reconcile tick instead of waiting out the container manager's poll cadence: the router signals a `tokio::sync::Notify` after every `messages_in` insert (`copperclaw-host-router/src/route.rs`, `Router::inbound_wake`) and the container manager's `run_loop` ticks immediately on it (`copperclaw-host/src/container_manager/mod.rs`, `with_wake_notify`; idle→stopped self-chains the spawn tick in `classify.rs`). Notify coalescing plus `classify()` as the single decision point prevent spawn storms; the 1s poll loop remains the crash-safe fallback.
### Changed (M17 A1 — parallel tool execution — 2026-07-14)

- The runner executes each turn's tool-call batch concurrently instead of sequentially (`crates/copperclaw-runner/src/run/drive_turn.rs::execute_tool_batch`): independent calls overlap (N reads finish in ~max latency, not ~sum) while results still append to history in the original call order; `shell` calls keep their relative order (persisted cwd/env) and edit-family calls (`edit_file`/`multi_edit`/`apply_patch`/`write_file`) serialize per target path.

### Added (runner external-MCP consumer — host-proxied — 2026-06-03)

The in-container runner can now consume **external** MCP servers configured on a
group (`container_configs.mcp_servers`), surfacing their tools to the model in
the *current* session — and this lands the previously-deferred per-server tool
filter as live enforcement (it was held because nothing model-facing consumed
external MCP tools yet). External tool calls execute **host-side** so the
container stays sandboxed under deny-default egress.

- **Advertise.** The host already writes the filter-stripped tool set to
  `<session_dir>/mcp_tools.json` at spawn; each entry now carries its owning
  `server` (`copperclaw-host/src/container_manager/spawn.rs` +
  `mcp_tools.rs::advertised_with_server`). The runner reads it at startup
  (`copperclaw-runner/src/run/external_mcp.rs`) and advertises each tool under a
  namespaced `mcp__<server>__<tool>` name so external tools never collide with
  the first-party set.
- **Host-proxied execution (no new socket).** When the model calls an external
  tool, the runner writes a request row to `outbound.db::mcp_call_requests` and
  blocks-polls `inbound.db::mcp_call_responses` for the host's reply; the host's
  delivery loop (`copperclaw-host-delivery::process_session_once`) drains the
  requests, connects the one named server through its per-server filter
  (`copperclaw_mcp::call_external_tool`), and writes the rendered result back.
  Request-in-outbound / response-in-inbound preserves the
  single-writer-per-bind-mounted-DB invariant (new migrations 023/024 + table
  helper `copperclaw-db/src/tables/mcp_calls.rs`).
- **Filter enforced on both ends.** Denied tools are stripped from the advertised
  manifest *and* refused at the host executor (`FilteredMcpClient`), so a stale
  manifest can't smuggle a denied call. The shared single-server connect/call
  primitive now lives in `copperclaw-mcp` (`external.rs`) so the manifest seam
  and the call executor can never drift.
- **Provenance.** An external MCP result taints the turn
  (`mark_untrusted_context`) exactly like a `web_fetch` body, and `mcp__`-prefixed
  names are treated as credentialed-external in the dispatch policy — blocked
  outright on an autonomous turn and on a tainted turn until fresh approval.
- **Scope/limits.** Stateless lazy-connect per call (no host connection
  registry); stdio + HTTP-SSE transports; host active-loop latency adds ~1-2s per
  call; image-bearing remote results render as `<image>` (text-only this version).

### Fixed (host log-noise: quiet crash-log capture on a gone container + dedupe stale-image warn — 2026-07-15)

- **Crash-restart log capture no longer WARNs when the container is already
  gone.** `copperclaw-host/src/container_manager/classify.rs::capture_crash_log`
  downgrades an already-removed container (operator `docker rm -f`, or the daemon
  reaped it) from WARN to debug — a missing container is an expected outcome for
  this best-effort probe, not a failure. The runtime layer now carries the
  structure to tell them apart: new `copperclaw_container_rt::RtError::NotFound`
  variant (`is_not_found()` helper), produced by `docker.rs::logs` on a Docker
  404 instead of the generic `Container` string. Other capture failures (daemon
  errors) still WARN.
- **The boot-time "image runner may be stale" fingerprint mismatch warns once
  per host process, not on every check.** `copperclaw-host/src/image_health.rs`
  gains an in-memory dedupe set keyed on `(tag, expected, actual)`; a standing
  mismatch logs at debug after the first WARN, and a *changed* triple warns
  again because it's new information.

- **A pinned todo/plan card whose anchor is gone no longer fails forever.**
  `DeliveryService::dispatch_todo_list` edits a single pinned card in place,
  resolving the anchor from the in-memory `todo_anchors` cache or (after a host
  restart) the root session's persisted `delivered` records. When that anchor
  points at a message the platform won't edit — deleted, too old, or pinned
  before a long downtime — the edit returned `BadRequest("message to edit not
  found")`, which was non-retryable, so the row was marked failed *and the stale
  anchor was never cleared* — every subsequent todo update then re-hit the dead
  card and failed indefinitely (observed live after a ~18h gap + restart: 8
  consecutive `todo_list` delivery failures). The loop now detects a stale edit
  target (new `is_stale_edit_target` predicate, covering Telegram / Slack /
  Discord phrasings), drops the dead anchor, and re-posts a fresh pinned card so
  the plan keeps updating. The same recovery is applied to **breadcrumb chips**
  (`dispatch_breadcrumb`), so an in-place chip edit against a gone message
  re-posts a fresh chip instead of failing the row.
  `crates/copperclaw-host-delivery/src/service.rs`.

### Security (M16 close-the-gaps — web_search provenance + egress PID handoff — 2026-06-03)

Two of the previously-deferred runtime gaps closed (full workspace gate clean,
6,661 tests). The third (per-server MCP tool filter) was deferred here and has
since been resolved — see "Added (runner external-MCP consumer)" above, which
builds the consumer that made the filter live code.

- **`web_search` results tagged untrusted-provenance.** `web_search`'s handler
  now calls the `ToolContext` untrusted-marking path (mirroring `web_fetch` /
  `memory_search`), so a turn that ran `web_search` trips the coarse
  provenance approval gate before any credentialed external action — closing
  the wave-4 gap where attacker-influenceable search results could steer a
  later action without tripping the gate.
- **Egress nftables apply now has a target.** `DockerRuntime::spawn` inspects
  the started container (`State.Pid`) and surfaces the host PID on
  `ContainerHandle`; the host wires it into the egress apply path so the
  per-session nftables ruleset can target the container's netns under opt-in
  `DenyDefault`. The privileged apply itself is still gated behind the opt-in
  flag and remains CAP_NET_ADMIN-dependent at runtime (honestly reported, not
  faked when unavailable).
- **Per-server MCP tool filter — now live (was deferred here).** Originally held
  because the runner consumed no external MCP tools, so the filter had nothing
  model-facing to enforce on. The runner external-MCP consumer (see the "Added"
  entry above) supplies that path: the filter now strips denied tools from the
  advertised manifest and refuses denied calls at the host executor.

### Performance (LLM token-cost reduction — 2026-06-03)

The agent loop was paying full input price to re-send a near-identical,
growing transcript on every tool-call turn (a 123-turn run billed ~6.3M
input vs 38K output — 164:1). Three changes cut that:

- **Anthropic prompt caching.** `copperclaw-providers` now stamps
  `cache_control` breakpoints on a **byte-stable** prefix — the static
  system block, the tools tail, and the transcript tail — while the
  volatile per-inbound context is emitted *after* the breakpoint (and
  non-caching providers flatten it back, so their request bytes are
  unchanged). Gated to Anthropic-family models so a non-Anthropic gateway
  can't reject an unknown field. Cached prefix reads bill at ~10% of input,
  for **~60-78% input-cost reduction on long multi-turn runs**. Cache
  read/creation token counts now surface on `ProviderEvent::Usage`.
- **Aggressive compaction + stale tool-result elision** (`copperclaw-runner`).
  Compaction now fires at a much lower configurable soft target (not just
  near the 200K window), and old, already-acted-on tool outputs (file reads,
  command stdout, diffs) are elided to short stubs in the *replayed* view
  (the persisted history is untouched, tool_use/tool_result pairing
  preserved). ~70K tokens/turn saved on a long run. NOTE: the default
  compaction target is now more aggressive than before.
- **Per-task token budget** (`copperclaw-runner`). A configurable per-task
  ceiling (default 2M input+output tokens) hard-aborts a runaway mid-loop
  with a surfaced message + metric/audit — distinct from the per-day group
  cap; bounds worst-case single-task cost.
### Security (M16 wave 5 — browser 5a, MCP supply-chain, security-audit; provider failover live — 2026-06-03)

Final hardening wave + the provider-failover rework. All opt-in / default-
unchanged where new capability is added; enforced-vs-deferred stated precisely.
Full workspace gate clean (6,604 tests).

- **Provider fallback (now live).** Automatic provider/model fallback chains +
  multi-key health-based rotation + per-channel model pinning (migration 020,
  `copperclaw-providers/src/failover.rs`). The earlier rework made the live
  path real: the runner now emits the failure reason, `record_usage_report`
  persists it to `agent_turns.error`, and the fold degrades the primary on a
  genuine provider failure and re-promotes it on recovery (proven by a
  cross-crate integration test; also fixed a latent `ag_`/`sess_` id-prefix
  bug that had kept the fold dead). Default single-provider behaviour
  unchanged.
- **Browser 5a (read-only, opt-in).** New `copperclaw-browser` crate +
  `browser_render` MCP tool (render / screenshot / read-only DOM), OFF by
  default. SSRF-guarded on navigation AND every redirect (reuses
  `net_guard`); output tagged untrusted provenance; runs in a dedicated child
  container with NO broker token, egress deny-default narrowed to the target,
  an unprivileged user, and a hardened sandbox profile requesting a stronger
  runtime. **Deferred runtime (honest, not stubbed): the live Chromium/CDP
  driver and the privileged microVM/gVisor child spawn** — the spec
  construction, runtime-selection (with a hardened-runc floor, no silent
  downgrade), and SSRF/provenance logic are all implemented + unit-tested;
  `handle()` reports the driver is unprovisioned rather than fabricating
  content.
- **MCP / supply-chain.** Host-side OAuth token store for MCP servers
  (migration 022 — tokens on the host, not in the container); `install_packages`
  subprocess containment (denied the broker token + egress); image + runner
  digest attestation recorded in the audit log at spawn. **Note: the
  per-server MCP tool include/exclude FILTER is a host-side primitive that is
  NOT yet wired into a live path (dormant; follow-up to enforce that a denied
  MCP tool is never advertised to the model).**
- **Operability.** `cclaw security audit [--fix]` on the doctor framework —
  reports open posture (egress allow-all, default-allow approvals, loose
  perms) and `--fix` only TIGHTENS (never loosens), each change audited. A
  heartbeat condition-check-in (`copperclaw-host-sweep`) that fires only when
  its stored condition holds, distinct from time-based scheduling.

### Added (M16 wave 4 — searchable cross-session memory + provenance gate — 2026-06-03)

Replaces the flat bind-mounted `/data/memory/` with a per-group searchable
store. Full workspace gate clean (6,375 tests). (Provider fallback/multi-key,
the other wave-4 unit, was rejected in review for a dead live-path failover
and is being reworked — not yet landed.)

- **Per-group memory store + tools.** Migration `021_memory_store.sql` +
  `copperclaw-db/src/memory.rs`: a per-group SQLite store with FTS5
  full-text search (and vector similarity where wired). New
  `memory_search` / `memory_get` MCP tools (`copperclaw-mcp/src/tools/memory.rs`).
  Per-group isolation preserved.
- **Provenance approval gate (coarse, by design).** Memory entries and tool
  outputs are tagged trusted/untrusted (`TurnTrust` /
  `is_credentialed_external` in `copperclaw-runner`); any turn whose context
  contains untrusted-provenance content requires fresh approval for
  credentialed external actions, and autonomous/heartbeat turns read-then-
  propose rather than act. **Honest limitation: taint cannot propagate
  through an LLM**, so this gate is necessarily coarse — it bounds, not
  eliminates, memory-poisoning / delayed-execution risk. Known follow-up:
  `web_search` results are not yet tagged as an untrusted *source*.

### Security (M16 hardening wave 3 — DNS filter + nftables egress, credential broker — 2026-06-02)

The hardest greenfield units. Both opt-in and default-unchanged; what is
enforced vs. deferred is stated precisely (no overclaiming). Full workspace
gate clean (6,340 tests).

- **Egress v2 — DNS filtering + nftables (`copperclaw-container-rt/src/{dns,nftables}.rs`).**
  Under opt-in `DenyDefault`: the host writes a per-session `resolv.conf`
  pinning the container's resolver to a single host-controlled filter
  address (no search domains) and binds it read-only, and constructs a
  per-session nftables ruleset (drop-all egress except established/related +
  loopback + the resolver + each allow-listed `host:port`). The name-set
  resolution, dnsmasq filter config, `resolv.conf` content, and the full
  nftables ruleset + apply/teardown argv are pure and unit-tested.
  **Deferred (constructed + reported, not stubbed): the privileged netns
  `nft` apply** (needs `CAP_NET_ADMIN` + the container PID, which the runtime
  doesn't yet surface) and **the DNS filter-resolver sidecar** (the
  `resolv.conf` pin is live; the answering daemon is the runtime piece).
  `cclaw doctor` reports DNS-filter + nft status honestly.
- **Credential broker v1 (`container_manager/{broker,broker_server,budgets}.rs`).**
  A host-side loopback model proxy holds the real provider key; the
  long-lived `ANTHROPIC_API_KEY` is no longer forwarded into the container
  (verified: `build_spec`'s container env carries a per-session, group-scoped,
  TTL-bounded, **revocable** capability token, not the master key). The
  broker validates the token, enforces the per-group daily budget at request
  time, and forwards upstream with the real key host-side. `runner.json`'s
  `api_base_url` points at the broker loopback when enabled (closes the
  `ANTHROPIC_BASE_URL`-override bypass). Opt-in; default path forwards the
  real key as before. **Honest residual: brokering stops key THEFT, not key
  MISUSE** — an injected agent still spends the group's budget through the
  broker until its token expires/revokes.

### Security (M16 hardening wave 2b — DM pairing, per-group mention gating, tool policy LIVE — 2026-06-02)

Rework of the two units rejected in wave 2, plus wiring the tool-policy
engine's inputs and fixing the egress auto-injection. All four landed on
`feat/security-hardening` (full workspace gate clean, 6,249 tests).

- **DM pairing codes (landed for real).** An unknown DM sender's first
  message mints an 8-char, 1h-TTL, rate-limited (3/channel) code
  (`crates/copperclaw-db/migrations/018_dm_pairing_codes.sql` +
  `dm_pairing_codes.rs`), delivered back to the sender over the existing
  adapter delivery path (a plain `Chat` message every adapter renders) and
  wired in `boot.rs`. `cclaw pairing list/approve` (new host-only
  `pairing.approve` + agent-readable `pairing.list` socket actions);
  approve promotes the sender into `users`. Default messaging-group policy
  is now `request_approval`. NOTE: `unknown_sender_policy` is currently
  advisory/stored-only — the sender gate holds every non-`users` sender
  pending unconditionally; the field does not yet branch behavior.
- **Per-group mention gating.** New `copperclaw-host-router/src/mention.rs`:
  per-group `require_mention` (default on for group chats, off for DMs),
  native-mention + regex detection, reply-to-the-agent (not arbitrary
  replies) as implicit mention; callback-query / button-tap interactions are
  never dropped. Passed in at `Router` construction (not a setter on an
  `Arc`).
- **Tool authorization is now LIVE.** The wave-2 policy engine is no longer
  dormant: `tool_profile` is a `container_configs` column
  (`migrations/019_container_config_tool_profile.sql`), written into
  `runner.json` via `RunnerConfigForFile`, exposed through `cclaw groups
  config`; the resolved sender role is plumbed through; and `load_skill`
  narrows the live policy via a `ToolContext` hook. Mutating scheduler verbs
  were moved out of the guest read-only set.
- **Egress auto-injection fixed.** Under `deny-default`, the host now derives
  the model endpoint from the actual provider base URL (Anthropic /
  OpenRouter via `ANTHROPIC_BASE_URL`, ollama via `OLLAMA_BASE_URL`) and
  always injects it, so an empty allow-list can never black-hole model
  traffic.

### Security (M16 hardening wave 2 — egress v1, mount TOCTOU, tool policy engine — 2026-06-02)

Second wave of the M16 roadmap. Two units landed on `feat/security-hardening`
(full workspace gate clean, 6,156 tests); two more (DM pairing, mention
gating) were rejected in review and are deferred to a rework pass.

- **Egress v1 (opt-in default-deny scaffolding).** New
  `crates/copperclaw-host/src/container_manager/egress.rs` +
  `handlers/egress.rs` + `copperclaw-container-rt` wiring. `EgressMode`
  defaults to `AllowAll` (legacy path unchanged); operators flip
  `COPPERCLAW_EGRESS_MODE=deny-default` to restrict a session to the model
  endpoint + an allow-list. The host auto-injects the model endpoint so
  deny-default cannot black-hole model traffic, and `cclaw doctor` reports
  egress mode + effective allow-list per group. (Known follow-up: verify the
  auto-injection covers every default deployment's base URL before
  recommending deny-default in production.)
- **Mount-bind TOCTOU fix (wired for real).** The Wave-1 attempt was dead
  code; this wires validation into the actual `spawn.rs` `Mount::Bind` sites
  via `container_manager/mount_guard.rs`, with `MountSecurityModule` given a
  live session root, rejecting a mount source whose path component became a
  symlink after validation. Documented residual: dockerd re-resolves the
  path in its own process.
- **Layered tool-authorization engine.** New
  `crates/copperclaw-runner/src/policy.rs` evaluates every tool dispatch
  against a host-owned floor, a guest read-only floor, the active skill's
  `allowed-tools`, and a group tool-profile (minimal/messaging/coding/full),
  plus `copperclaw-skills` name-normalization (`Read`/`Bash` -> MCP names).
  NOTE: the engine + tests are in place but **dormant** in production until
  the host write-half lands (no `tool_profile` is written into
  `runner.json` yet, sender role is not plumbed, and `load_skill` does not
  yet narrow the policy) — wired in a follow-up. Defaults to `Full` so
  current behavior is unchanged.

### Security (M16 hardening wave 1 — make the advertised controls real — 2026-06-02)

First wave of the M16 security-hardening roadmap (see `PLAN.md`). Closes the
inert/open-fail controls found in tree; all five landed gate-clean (6,078
workspace tests pass).

- **SSRF guard on `web_fetch` / `web_search`.** New
  `crates/copperclaw-mcp/src/tools/net_guard.rs` resolves a target host and
  rejects loopback, link-local (incl. the `169.254.169.254` metadata
  endpoint), RFC1918, IPv6 ULA (`fc00::/7`), CGNAT (`100.64/10`), and
  unspecified addresses, and installs a `reqwest::redirect::Policy` that
  re-classifies **every** redirect hop (the default blindly followed up to
  10). Non-HTTP(S) schemes are rejected. Previously both tools issued
  `client.get(url)` with no checks, reachable to internal addresses.
  Documented residual: DNS-rebinding TOCTOU (guard resolves, reqwest
  re-resolves at connect) — closed later by pinning the validated IP.
- **Permission gate fails closed.** `crates/copperclaw-modules/src/permissions.rs`
  now returns `Deny` (with an audit line naming the op) instead of `Defer`
  for an op that fails `PermissionOp::parse()`, and host gate resolution
  treats a missing decision as deny for privileged ops — previously an
  unknown op open-failed even with the permissions module installed.
- **Secret redaction in logs + tighter perms.** New redaction helpers
  (`copperclaw-host/src/log_redact.rs`, `copperclaw-runner/src/redact.rs`)
  scrub `sk-…` / `sk-or-v1-…` / `Bearer` token shapes from host and runner
  log output; the per-session `runner.json` is written `0600`.
- **Repeated-tool-call circuit breakers.** `copperclaw-runner`'s turn loop
  now detects N identical consecutive tool calls and ping-pong A/B/A/B
  alternation, ends the loop with a surfaced message, and emits a
  `copperclaw-metrics` event — complementing (not replacing) the existing
  tool-turn depth cap and token budget.
- **Approval lifecycle: TTL, revocation, decision audit.** Migration
  `017_approval_decisions.sql` adds an append-only `approval_decisions`
  receipt table (`ON DELETE CASCADE` so `sessions::delete` still works) and
  expiry/revocation state on `pending_approvals`; approvals default to a ~1h
  TTL, can be revoked, and every approve/deny/expire/revoke is recorded.
  `sweep_expired` runs in a single `IMMEDIATE` transaction and only logs an
  expire decision when it actually flips a row (no duplicate receipts under
  concurrent sweeps). New `cclaw` revoke + decision-audit commands.

### Fixed (`create_agent` children inherit the parent's model — 2026-06-02)

A child spawned via `create_agent` was created with an `agent_groups` row and
a session but **no `container_configs` row**, so the container manager fell
back to the host's `COPPERCLAW_DEFAULT_MODEL` for every sibling. This silently
downgraded sub-agents: a parent running a capable model (e.g. `qwen/qwen3.7-max`
on OpenRouter) spawned builders that all booted on the weak host-default local
model and produced nothing — branches created, zero commits — while the parent
sat waiting to consolidate work that never arrived.

- `CreateAgentHandler` (`copperclaw-modules::agent_to_agent::create_agent`)
  now copies the parent group's `container_configs` row onto the freshly
  created child group via a new `inherit_parent_container_config` helper:
  provider, model, effort, skills, packages, mounts, cli_scope, egress,
  resource limits, coding flag, and `image_tag` + `config_fingerprint` are
  all inherited. Because `compute_fingerprint` hashes only image-relevant
  fields (packages / skills / mcp), copying the fingerprint verbatim lets the
  child adopt the parent's existing image with **no rebuild**.
- `assistant_name` is deliberately NOT inherited (that's the parent's own
  identity; the child uses its own group name).
- Best-effort and non-fatal: if the parent has no config row (it too runs on
  host defaults) or the copy fails, the child falls back to host defaults —
  the pre-fix behaviour — so the spawn still succeeds.
- New tests `child_inherits_parent_container_config` and
  `child_without_parent_config_uses_host_defaults`.

### Changed (one pinned plan per family — child todos roll up into the parent's — 2026-06-02)

`create_agent` siblings inherit the parent's messaging group, and the
todo-list surface pins one editable card *per session*, so a parent plus N
builders produced N+1 separate pinned "Plan" cards in the chat. Now the
delivery service rolls the whole family into **one** pinned card owned by
the family ROOT: each live child's plan is appended as a labeled, indented
section (`↳ <agent-name>` header carrying the child's aggregate status,
then its items), and a child never pins its own card.

- `dispatch_todo_list` (`copperclaw-host-delivery::service`) now resolves
  the family root (`resolve_root` walks `source_session_id`), gathers the
  root + each active child's latest `TodoList` from their outbound DBs, and
  renders a combined list via `build_combined` (child item ids renumbered
  to stay unique; child header status via `aggregate_status`).
- The single pinned anchor is keyed by the root session — cached in-memory
  (`todo_anchors`, robust to which family member emits first) with a
  fallback to the root's persisted `delivered` records after a restart,
  and a per-root `tokio::Mutex` (`todo_locks`) serialising concurrent
  family emits so two members can't both first-emit-pin. The anchor is
  dropped when the whole plan completes so a later fresh plan re-pins.
- Pure-function + end-to-end tests (`aggregate_status`, `build_combined`,
  and a parent+child `process_session_once` rollup); 127 delivery tests
  green.

### Changed (push agents to actually `load_skill`; harden git merge-back — 2026-06-02)

Two issues a live Telegram run surfaced. (1) With `COPPERCLAW_SKILLS_MODE=callable`
the system prompt only carries a compact name+description skill index and
the body is fetched on demand — but the model never called `load_skill`
(0 calls across a 116-tool-call run), so it acted off one-line
descriptions and never saw the skills' real rules. (2) A sub-agent
merge-back **destroyed the user's uncommitted WIP** because the parent
merged sibling branches into the live working tree without stashing first.

- `render_callable_skill_index` (`container_manager::prompt`): the index
  intro is now a firm directive — the description is a *pointer, not the
  procedure*; `load_skill("<name>")` and read the body BEFORE acting on
  anything a skill covers (code/commit/merge, send card/file, schedule,
  spawn sub-agents), reloading when switching kinds of work. (No effect in
  inline mode, where bodies are already in the prompt.)
- **Always-inline critical core (hybrid):** because some models never call
  `load_skill` at all (observed: `minimax-m3`, 0 calls over multiple runs,
  and it implemented features serially instead of spawning agents), callable
  mode now also keeps a short `CALLABLE_CORE_RULES` block permanently in the
  prompt — the two behaviours that do real damage when missed: *parallelise
  multi-part work via one `create_agent` per piece*, and *never destroy
  uncommitted work during a git merge* (`git status` → stash → never force).
  Keeps the sprawl win while guaranteeing the load-bearing rules are present
  even for a model that won't fetch skill bodies.
- `skills/git-commit/SKILL.md`: new "Merging branches — clean tree FIRST"
  section — `git status --porcelain` before any working-tree-mutating git
  op; `git stash` if dirty; a "your local changes would be overwritten"
  merge is a STOP, never `checkout`/`reset`/`-X theirs` past it. Broadened
  the reset/checkout rule to cover `git checkout <branch> -- .`.
- `skills/create-agent/SKILL.md`: merge-back guidance now mandates the
  clean-tree/stash check before merging each `sib/<id>` branch, citing the
  WIP-loss it prevents.

### Added (sub-agents work on the parent's codebase — writable git worktrees — 2026-06-01)

`create_agent` siblings run in their own container with an empty `/data`,
so "spawn sub-agents to review/build on the codebase" produced siblings
with no source to work on. Now `build_spec` locates the PARENT agent's
session dir (scanning `<data>/sessions/*/<parent_session_id>`) and shares
it with the sibling, Claude-Code-style:

- **Parent working in a git repo** → the sibling gets a **writable `git
  worktree`** of THAT repo at `/workspace` on its own branch
  `sib/<session-id>` (`git worktree add -b`, idempotent across container
  restarts). It can edit AND commit there in isolation; the parent's
  checked-out files are never mounted, so they stay physically untouched.
  The repo's `.git` is bind-mounted **read-write at its identical
  host-absolute path** so the worktree's `gitdir:` pointer resolves
  in-container with no rewriting, and commits land in the shared object
  store — the parent sees branch `sib/<id>` immediately and reviews/merges
  it from inside that project (`git diff main..sib/<id>`, `git merge
  sib/<id>`, then `git worktree remove .copperclaw/wt/<id>`). `.copperclaw/`
  is appended to the repo's `.git/info/exclude` so worktree scratch never
  pollutes `git status`.
- **Which repo:** a single session is reused for many projects over time
  (the operator `/clear`s context between them, but `/data` persists), so
  the worktree is cut from **whichever project the parent is currently
  working in** — `resolve_parent_repo_root` reads the parent's persisted
  shell cwd from `.shell_state` (`PWD=/data/<proj>`), maps it back to the
  host, and walks up to the nearest enclosing `.git` (bounded at the session
  dir). Falls back to a repo at the workspace root, then to a **read-only
  `/parent`** mount of the whole workspace (review/audit only) when there's
  no repo.

`COPPERCLAW_WORKSPACE` / `COPPERCLAW_WORKSPACE_BRANCH` env vars are set in
the sibling for tooling. The `create_agent` / `explore` descriptions, the
system prompt, and the `coding-task` / `create-agent` skills were reframed
accordingly: keep one git repo per project dir under `/data`, `cd` into the
one you're working on, and `git init` new projects at creation so siblings
can build (not just review). Trade-off: sharing `.git` read-write means a
sibling *could* write to the shared object store; the parent's working
tree is the safety boundary that stays untouched.

### Changed (default max tool turns 60 → 150 — 2026-06-01)

`DEFAULT_MAX_TOOL_TURNS` raised 60 → 150. Substantial "review the codebase
and implement the fixes" / full-app-build requests routinely ran past 60
tool-use cycles and were cut off mid-task. Still bounded; tune further with
`COPPERCLAW_MAX_TOOL_TURNS` (clamped to [5, 500]).

### Added (rolling activity breadcrumbs — Telegram — 2026-06-01)

Opt-in low-profile tool-progress UX (`COPPERCLAW_BREADCRUMB_STYLE=rolling`).
Instead of one chip message per tool call (a long turn stacks up dozens),
a turn shows a single rolling **activity chip**: a collapsed one-line
summary (current tool + N/total steps) over a Telegram
`<blockquote expandable>` whose body lists every step, each styled
individually — ASCII status marker (`[~]`/`[ok]`/`[x]`), **bold** tool
name, `<code>` detail, _italic_ result — real HTML, not raw markdown. Tap
to expand the full tool history; collapsed by default to keep chat quiet.

- `Breadcrumb` gains `steps: Vec<Breadcrumb>` (the expandable list); empty
  for the legacy per-tool chip. Backward-compatible — other adapters
  ignore it and still render the collapsed summary line.
- The runner accumulates a turn's tool steps and edits one chip in place
  (stable `activity` pseudo-tool so the host's existing breadcrumb
  correlation targets one message), reset each `drive_turn` via the new
  `ToolContext::begin_activity` hook. Default stays `chips` — no regression.
- Telegram renders the aggregate, capped at 40 steps with a `+N earlier`
  note to stay under the message-size limit.
- Reuses the existing `<blockquote expandable>` primitive (the same one
  thinking blocks use). Other channels fall back to the summary line until
  their adapters gain native expand affordances.

### Fixed (compaction crash-loop on a tool-pair boundary — 2026-06-01)

`compact()` sliced the transcript at a blind `len/2` midpoint to summarize
the oldest half. When that midpoint fell between a `ToolUse` and its
`Tool` result, the slice sent to the provider ended on a dangling tool
call, which strict gateways reject (minimax: "tool call and result not
match") — failing compaction and **crash-looping the runner**: the
over-threshold history re-loaded and re-crashed on every respawn (~once
every two minutes, each emitting a "hit a snag" apology). New
`pair_safe_pivot` advances the split past any straddled tool group so both
halves are self-contained. Verified on the live 629-message transcript
that wedged a session — naive pivot 314 dangled a `tool_use`; the
pair-safe pivot (315) summarizes cleanly on minimax.
(`crates/copperclaw-runner/src/compaction.rs`)

### Added (multimodal: inbound photos + view_image tool — 2026-06-01)

Vision support for image-capable models. Verified live that minimax-m3
(via OpenRouter) reads both PNG and JPEG through the Anthropic
`/v1/messages` path the runner uses.

- New `HistoryMessage::Image { media_type, data }` (base64) carries an
  image in the transcript. The anthropic provider serializes it as a
  `user`-role base64 `image` block; ollama emits the OpenAI `image_url`
  shape; subprocess passes it through. Compaction estimates an image's
  tokens tile-based (flat ~1500), not by base64 length, and the archive
  records presence/size only.
- Inbound photos: the Telegram ingress inlines image attachment bytes as
  base64 (`data_base64`, ≤4 MB) into the inbound message; the runner lifts
  them into `Image` entries right after the caption. Send the agent a
  photo and it sees it.
- `view_image` tool: load a PNG/JPEG/WebP/GIF already on disk (≤5 MB) and
  attach it for the model to see (screenshots, charts, fetched/generated
  images). Tool-result image blocks are threaded into `Image` entries,
  split from the tool_result so strict gateways (MiniMax) accept them.

A shared base64 encoder is exposed from `copperclaw-types`. Each layer is
unit-tested; provider serialization shape verified live. End-to-end with a
real telegram photo verifies on the next image-capable deploy.

### Added (shell background jobs + write_file overwrite steer — 2026-06-01)

- `shell` gains `background: true`: launches the command detached in its
  own session (`setsid`) and returns `{pid, log_path}` immediately. The
  session container persists, so the job keeps running across later tool
  calls — poll its output with `read_file` on `log_path`, check it's
  alive with `kill -0 <pid>`, stop the whole job tree with
  `kill -- -<pid>`. Closes the long-running-task gap (dev servers, slow
  builds) that the 600s foreground ceiling couldn't cover. Verified
  end-to-end in the session image: immediate return, detached survival,
  log capture, group-kill takes down children.
  (`crates/copperclaw-mcp/src/tools/computer_use.rs`)
- `write_file`'s description now steers the model to read-then-edit
  existing files (`edit_file`/`multi_edit`/`apply_patch`) and reserve
  `write_file` for new files or deliberate full replacement — curbing
  blind whole-file overwrites that silently drop existing code.

### Fixed (MiniMax/OpenRouter tool-pairing rejection — 2026-06-01)

minimax-m3 (and other strict OpenAI-compatible models behind OpenRouter)
rejected the whole conversation midstream — `invalid params, tool call
result does not follow tool call` — whenever the transcript held a user
message that mixed a `tool_result` block with a `text` block. That
happens when a turn ends on a tool call without a final reply and a new
inbound user message arrives: the Anthropic provider coalesced both into
one user message (valid for Claude, rejected by MiniMax). Once such a
turn was recorded, every later turn replayed it and failed, wedging the
agent permanently. Diagnosed by replaying the live 289-message transcript
against OpenRouter and bisecting to the exact offending turn.

- `crates/copperclaw-providers/src/anthropic.rs`: `push_block` keeps
  `tool_result` and `text` blocks in separate user messages. Parallel
  tool_results still coalesce; the Anthropic API recombines consecutive
  same-role turns server-side, so the native backend is unaffected.
- Same file: a tool_use whose argument JSON failed to parse (stored with
  a null input) now serializes as `{}` rather than `null` — strict
  gateways reject a tool call with null arguments.
- `crates/copperclaw-runner/src/run/provider_call.rs`: record the
  unparseable-input placeholder as `{}` at the source, not `Value::Null`.

Root cause was downstream of the old 4096 output cap: minimax-m3 truncated
a `write_file` argument JSON at the token limit, producing the malformed
tool call that wedged the session — see the `COPPERCLAW_DEFAULT_MAX_TOKENS`
bump below, which makes that truncation far less likely.

### Added (configurable per-turn output-token cap — 2026-06-01)

`COPPERCLAW_DEFAULT_MAX_TOKENS` in `.env` now sets the per-turn output
token cap. The host writes it into `runner.json`
(`crates/copperclaw-host/src/container_manager/runner_config.rs` →
`RunnerConfigForFile.max_tokens`, mirroring the temperature default) and
the runner already threads it to the provider request. Unset leaves the
runner's built-in 4096; raise it for large edits and reasoning models
(minimax-m3, qwen3.6-27b) that burn budget thinking before they write.

### Added (local-model optimizations — 2026-05-31)

Tuning for self-hosted/local-model (ollama) deployments, surfaced while
debugging a local agent:

- **Configurable sampling temperature.** `COPPERCLAW_DEFAULT_TEMPERATURE`
  in `.env` is now read at spawn and threaded through to the provider
  (`crates/copperclaw-host/src/container_manager/runner_config.rs` →
  `RunnerConfigForFile.temperature`; the runner→`QueryInput` path already
  existed). A low value (~0.3) steadies agentic tool-calling on small
  local models; unset leaves the model/provider default untouched.
- **`ripgrep` + `universal-ctags` in the baseline session image**
  (`crates/copperclaw-setup/src/steps/image.rs`). Containers have no
  Debian-repo egress at runtime (`apt-get update` exits 100), so code
  navigation tools must ship in the image. Lands on the next base-image
  rebake.
- **`coding-task` skill: require the canonical build.** "Done" now means
  `go build ./...` / `cargo build` / `npm run build` passes, not an
  ad-hoc per-file script — closing the class of bug where a custom test
  builds one file at a time and passes while the real package build
  fails (two `main`s in one package).

### Fixed (session container `HOME` and cwd were `/` and unwritable, breaking all build tooling — 2026-05-31)

The session container runs as a non-root uid (`<host-uid>:<gid>`) with
no matching passwd entry, so both the working directory and `$HOME`
defaulted to `/` — which that uid cannot write to. Seen live when a
telegram agent tried to build a Go project:

- Every tool that caches under `$HOME` failed. `go build` died on
  `mkdir /.cache: permission denied`, which *masked the program's real
  compile errors on every run* — the agent couldn't see why it failed,
  retried the same command, and ultimately claimed a non-compiling
  program was "built." `npm`, `pip`, `cargo`, and `git config` hit the
  same wall.
- The first relative-path `write_file` / `mkdir` failed with EACCES
  until the agent manually `cd`'d into `/data`.

Fix: the host now starts the container in the writable session bind
mount and anchors `$HOME` there.

- `crates/copperclaw-container-rt/src/spec.rs`: new
  `ContainerSpec::working_dir` field + `with_working_dir` builder.
- `crates/copperclaw-container-rt/src/docker.rs`: wires it into the
  bollard `Config.working_dir`.
- `crates/copperclaw-container-rt/src/apple.rs`: emits `--workdir` in
  `run_args`.
- `crates/copperclaw-host/src/container_manager/spawn.rs::build_spec`:
  sets `working_dir` and `HOME` to `CONTAINER_SESSION_DIR` (`/data`).

Image-independent (applied at container create), so it takes effect on
the next session spawn — no image rebake.

### Added (system-prompt proactivity directive for weak local models — 2026-05-31)

A telegram agent on a small local model (`ollama/gemma4:26b`) kept
stalling mid-task — it would announce "I'll start working now," end the
turn, and wait to be coaxed; the runner log showed ~4 tool turns then
silence. Small models lack agentic stamina (the model is the ceiling),
but the prompt can push one further. Added a `# Keep going until it's
done` section to `BASE_PREAMBLE`
(`crates/copperclaw-host/src/container_manager/prompt.rs`): do the work
in this turn's tool loop; never announce-then-stop (nothing runs after
the reply ends); execute todos to completion; stop only when done and
verified or genuinely blocked — and then name the blocker instead of
going quiet.

### Fixed (agents no longer fake-wait on install_packages "provisioning" — 2026-05-31)

An agent asked to build in Go hit "no `go`", called `install_packages
golang-go` — which only rebuilds the image for the NEXT session spawn,
never the running container — then looped indefinitely "waiting for the
Go environment to be provisioned." There is no in-session provisioning
step or background task to wait on, and the skill's documented immediate
fallback (`shell apt-get install`) is dead in containers without
Debian-repo egress (`apt-get update` exits 100). The tool's only inline
signal was a bare `{"kind":"accepted"}` ack with no timing. Fixed on
three always-reachable surfaces:

- `crates/copperclaw-host/src/container_manager/prompt.rs` (`BASE_PREAMBLE`,
  always-on): a `# Don't fabricate` bullet — `install_packages` /
  `add_mcp_server` change the image for the NEXT session, not the
  current container; the tool won't appear this turn and there is
  nothing to wait for; install into `/data` for an immediate need.
- `skills/install-packages/SKILL.md`: replaced the "wait for the next
  spawn" / `apt-get install` advice with the reliable in-session path
  (download the toolchain into `/data`; Go tarball example) and the
  apt-exit-100 caveat.
- `skills/coding-task/SKILL.md`: "toolchain not in the base image →
  download it into `/data` this session" with a Go example.

### Changed (system prompt slimmed ~51%, all directives intact — 2026-05-31)

`BASE_PREAMBLE` in `crates/copperclaw-host/src/container_manager/prompt.rs`
— the universal preamble sent on every turn to every agent — rewritten
for density: 7,596 → 3,684 source chars (~600 fewer tokens of always-on
context per turn) with every behavioural directive preserved. Verified
by the existing `container_manager::prompt` tests, which assert the
load-bearing phrases (`You are a Copperclaw agent`, `Acting with care`,
`Picking tools`, `Never use emojis`). What was cut is justification prose
("the operator has no idea whether you're on step 2 or step 8", "burns
trust harder than…", "…is vapor"), never a rule. The two separate
`# Don't fabricate capabilities` / `# Don't fabricate completion on
coding work` sections merged into one `# Don't fabricate` with two
compact bullets.

### Added (skill discipline for autonomous coding — 2026-05-31)

Enhanced three opt-in coding skills (pure markdown, hot-loaded via the
`data/skills` symlink — live on next session spawn, no rebuild):

- `skills/testing/SKILL.md` — "Iterating to green": the
  write → run → read-the-actual-failure → smallest-patch loop, one
  hypothesis per iteration, cap attempts and stop rather than thrash.
- `skills/code-review/SKILL.md` — "Adversarial pass": boundary /
  malformed / concurrency / error-path attacks before sign-off, and when
  to spawn a `create_agent` critic for high-stakes changes vs. an
  in-context pass. Cross-links `create-agent` and `testing`.
- `skills/grep/SKILL.md` — "When text search isn't enough": use the
  language's own checker (`cargo check` / `tsc` / `go build` / `mypy`)
  as the precise find-references oracle before a refactor, and
  `ast-grep` for structural matches; reserve text grep for "where is
  this string".

### Fixed (rename grammar — "an Copperclaw" → "a Copperclaw" — 2026-05-31)

The ironclaw → copperclaw rename turned the grammatically-correct
"an Ironclaw" (vowel) into "an Copperclaw" (consonant) across 9 files,
most visibly the agent persona in
`crates/copperclaw-host/src/container_manager/prompt.rs` ("You are an
Copperclaw agent" → "a Copperclaw agent") and its test assertions.
Fixed in prompts, doc comments, CLI help text, and `docs/`.

### Fixed (heartbeat / breadcrumb / diff / thinking missed when parent processes child-forwarded inbound)

The four user-facing observability emits (`emit_status`,
`emit_breadcrumb`, `emit_breadcrumb_finish`, `emit_diff`,
`emit_thinking` on `RunnerToolCtx`) gated on "origin must have
`channel_type` AND `platform_id`," which over-strictly skipped
during a perfectly common scenario: a root parent session processing
an agent-dispatched inbound (a child's report forwarded into the
parent's inbound). Those rows carry NULL channel routing — the user
channel comes from the messaging-group wiring's `session_routing`
fallback at delivery time (`crates/copperclaw-host-delivery/src/service.rs::resolve_target`).

Lived through on 2026-05-24 in a Telegram session that asked for
"parallel research, then build a prototype." The parent spawned three
F1-research children. The first two delivered close in time and got
batched into one drive_turn that produced a chat acknowledgment. The
third arrived 8 seconds later and triggered its own drive_turn —
during which the model went straight into synthesis (41 LLM turns +
7 plan updates over 5+ minutes) without saying anything. The
heartbeat I added on this same date *should* have fired at the 60s
mark, but `emit_status` skipped because the originating inbound (the
forwarded child report) had NULL channel routing. The user saw
"Waiting for the fantasy gaming research report..." → 5 minutes of
silence → "Strategy Masters... Prototype Complete," reasonably
concluding it was stuck.

Fix in `crates/copperclaw-runner/src/tools.rs`:

- New `RunnerToolCtx::should_skip_user_facing_emit()` helper. Skips
  ONLY when the runner itself is a child session
  (`self.source_session_id.is_some()` — set by the host's
  `create_agent` path through `runner.json` and into the ctx at
  startup via `main.rs:132-134`). Whether the originating inbound
  has channel routing is no longer part of the gate.
- All four emits now use the new helper. Channel-routing fields on
  the written row may be `None`; delivery's `resolve_target`
  fallback fills them from the session's `session_routing` table
  before dispatch (same path normal `send_message` uses, which is
  already proven to work for forwarded-inbound scenarios).
- Child sessions still skip cleanly. `send_message` routing is
  unchanged (it goes through `resolve_outbound_routing`'s
  `inbound_came_from_parent` branch and still emits Agent-kind rows
  back UP to parent — completely separate code path).

Six tests cover the new behavior: two existing skip-when-no-routing
tests inverted to assert root-session emits fire even with NULL
origin routing; four new tests pin child-session skip behavior
(`emit_breadcrumb_skips_for_child_session`,
`emit_diff_skips_for_child_session`,
`emit_status_writes_for_root_session_with_null_routing`,
`emit_status_skips_for_child_session`). All 63 runner tools tests
green, workspace clippy clean, 0 failed tests.

### Changed (web_fetch + explore: smaller cap, sharper tool-selection guidance)

A 2026-05-24 Telegram session asked for "parallel research on F1 app
ideas, then build a prototype." The model picked `explore` (an
in-process subagent with a 50k cumulative input-token budget) instead
of `create_agent` (full child sessions with their own ~200k budgets
and real parallelism). The explore subagent fetched
`https://www.formula1.com` — a JS-heavy SPA whose markdown-converted
body alone was ~30 KiB. Replayed across 3 explore-loop turns of
identical history, one fetch's tool result consumed the entire 60k
budget and the subagent stopped with `token budget exceeded` having
done zero substantive research. The agent then went ahead and built
the prototype from training-data priors anyway.

Two coordinated changes target the root cause:

- **`WEB_FETCH_CAP` lowered 32 KiB → 16 KiB**
  (`crates/copperclaw-mcp/src/tools/computer_use.rs`). 16 KiB of
  markdown-extracted text is ~4k tokens, leaving room for ~6 real
  fetches inside a default explore budget instead of 1-2. The tool
  description now explicitly states "Response body is capped at 16
  KiB (~4k tokens) to keep one fetch from eating an entire
  subagent's budget" and points callers to `shell` + `curl` +
  `head -c` / `grep` when they need more. Truncation regression
  test renamed `web_fetch_caps_body_at_32k` →
  `web_fetch_caps_body_at_16k` with payload sizes halved.
- **`explore` description gained a tool-selection prelude**
  (`crates/copperclaw-mcp/src/tools/explore.rs`). New explicit guidance:
  "`explore` is for QUICK in-process lookups (single-focus, 1-3
  tool calls expected). `create_agent` is for SUBSTANTIVE PARALLEL
  RESEARCH — each child agent gets its own ~200k token budget and
  full tool access." A "Budget caveat" paragraph spells out the
  cumulative-input-token gotcha: each subagent turn replays the
  full prior history + tool results, so a single large fetch
  consumes a disproportionate share when repeated in subsequent
  turns' context. The fix pushes the model toward the right tool
  before it ever invokes the wrong one.

### Fixed (false-positive "I'm having trouble" toast during legitimate long work)

The sweep's stuck-inbound apology check was firing on message AGE
alone (`messages_in.status='pending'` + `now - timestamp > 5min`),
which produced a false-positive "I'm having trouble processing your
message right now" toast during legitimate multi-minute model
turns. Lived through on 2026-05-24: a Telegram session 6 minutes
into a multi-file prototype build got the toast even though the
heartbeat was 1 second old, the `golfflow` working directory was
modified just then, and ~30 rapid `usage_report` rows had landed in
outbound. The root cause: the runner doesn't flip
`messages_in.status` until `finalize_messages` at the very end of
the turn, so a 6-minute turn looks identical to a 6-minute stuck
container if you only look at age.

Fix in `crates/copperclaw-host-sweep/src/checks/apology.rs`:

- New `inbound_is_being_processed(outbound_conn, message_id)`
  helper that returns true when `processing_ack.status='processing'`
  for the inbound. The runner writes this ack inside `ack_picked_up`
  (called immediately after pulling a row), so a fresh `processing`
  ack means the runner is genuinely on the row.
- New liveness gate in `check()`: when the apology reason is
  `PendingTooLong` AND the container is `Running` AND the inbound's
  ack is `processing`, suppress the apology. The spawn-failed
  branch is exempt (it definitionally implies `container_status =
  Stopped`); the dedupe marker is NOT stamped (the next sweep
  re-evaluates from scratch if the runner does eventually crash).
- Crash-while-processing still surfaces: when the container is
  `Stopped`, the gate skips its check and the apology fires
  normally — the runner is dead and the user deserves the toast.
- Done / Failed acks don't suppress either — by then the runner is
  off the row, and if the inbound is still `status=pending`
  something else broke and the apology is appropriate.

Four new tests cover the truth table (gate truth table, suppression
on Running+Processing, fire on Stopped+Processing, fire on
Done/Failed acks). All 13 apology tests green, full workspace at
5599 passing.

### Changed (todo tools push back on premature plan completion)

After a 2026-05-24 Telegram run shipped a "3/3 done" pinned plan
while the model was still writing 20+ more files, the todo tools
got three coordinated nudges:

- **`todo_add` description** now includes an explicit granularity
  rule: prefer many small items over a few coarse ones, ≥5 for any
  build that touches >5 files or runs >10 minutes. A 3-item
  `[research, design, build]` plan is called out as almost always
  too coarse.
- **`todo_update` description** spells out that `completed` means
  VERIFIED done, not started or partly done, with concrete examples
  of what does NOT count ("wrote `package.json` and `server.js`"
  for a 'build prototype' item is still `in_progress`). Adds an
  inline rule: if you're about to make MORE tool calls related to
  an item, it's not done yet.
- **`is_acceptable_evidence` got stricter**
  (`crates/copperclaw-mcp/src/tools/todo.rs`). Minimum length bumped
  from 20 → 40 chars. Evidence must now contain at least one
  concrete signal: a file path (slash-bearing token of ≥3 chars), a
  dot-extension reference (`.json`, `.rs`, `.tsx`), or a
  verification verb from a curated list (`ran`/`tested`/`verified`/
  `passed`/`returned`/`started`/`compiled`/etc). `FORBIDDEN_GENERIC`
  picked up five more entries (`all good`/`looks good`/`lgtm`/
  `shipped`/`wrapped up`). The validation error message now spells
  out exactly what shape of evidence is accepted, with examples.
  Three new tests cover the new gates: rejection of <40-char
  evidence, rejection of long prose without concrete signals, and
  acceptance of verification-verb-only evidence (for non-write
  items like "send confirmation email").

These don't *prevent* a determined model from writing convincing
fake evidence — that would require runtime tool-call-pattern
detection which is a separate slice. They make the lazy / generic
premature-completion path much harder.

### Fixed (`/clear` now wipes the todo store too)

`/clear` (and its `/reset` / `/new` aliases) previously only wiped
`state.history` + `state.continuation`; the per-session todo store
at `/data/agent_todos.json` survived. Lived through on 2026-05-24:
a Telegram session's `/clear` left an email-triage plan from an
unrelated prior task in place, the next prompt's model picked it up,
appended new items on top, and the user saw a "13/26 done"
Frankenstein plan with items from three different runs (Gmail
OAuth2, compliance-deadlines DB, golf-prototype) all marked done or
pending against the same list. Fix: new `clear_store()` helper in
`copperclaw-mcp/src/tools/todo.rs` (re-exported as
`copperclaw_mcp::clear_todo_store`); the slash handler in
`copperclaw-runner/src/run/mod.rs` calls it after the history wipe.
Error is best-effort (a missing or locked store must not abort the
clear confirmation). The confirmation text picks up "The plan/todo
list was also cleared." when a store was actually present.

The pinned-message chip on Telegram will go stale visually until the
next `todo_add` rebuilds it — automatic unpin-on-clear is a
follow-up; the load-bearing fix (preventing the model from seeing
the stale plan) is what shipped here.

### Added (runner UX: still-working heartbeat, child-failure toast, retry-nudged apology)

Three loosely-coupled changes to the runner so a long silent stretch
(tool-heavy turn, or a parent processing a child's failure) doesn't
read as "the agent has hung." Lived through on 2026-05-24 with the
Telegram session that went silent for 5+ minutes after
`golf-research-market` died — no heartbeat, no signal, just typing
indicators that eventually timed out.

- **`emit_status` hook on `ToolContext` + 60s "still working"
  heartbeat in `drive_turn`**
  (`crates/copperclaw-mcp/src/context.rs`,
  `crates/copperclaw-runner/src/tools.rs`,
  `crates/copperclaw-runner/src/run/drive_turn.rs`). After each tool
  turn, the runner checks whether more than 60s have elapsed since
  the last user-facing emit; if so, it writes a brief `Still working
  on this — Xs in, N tool calls so far (latest: shell). I'll keep
  going.` row to the originating channel. The hook is gated inside
  `RunnerToolCtx::emit_status` to channels with real user routing
  (`channel_type` + `platform_id` both set) — child agents skip
  cleanly because the recipient is another LLM, not a person, and
  status chatter would just bloat the parent's history.
- **Surface child-agent failure notices to the user channel BEFORE
  the parent's LLM digests them**
  (`crates/copperclaw-runner/src/run/mod.rs`, new
  `emit_failure_notice_toasts`). When `run_loop` picks up a batch
  of pending inbounds and any of them is an `Agent`-kind row whose
  text starts with `sub-task failed:`, the runner immediately
  writes a `Heads up — a sub-task reported failure. Handling it
  now.` toast to the user channel via the same `emit_status` path.
  No-op for the common case (no failure rows) and for parent
  sessions without channel routing (themselves child agents).
- **Retry-nudged child-failure apology text**
  (`crates/copperclaw-runner/src/run/mod.rs`, `agent_apology_text`).
  Old text: "Report the failure upstream rather than retrying with
  the same prompt." New text: "You may retry by calling
  create_agent again with the same name + instructions — these
  failures are often transient (parse-error cap, brief provider
  hiccup, container crash). If a second attempt also fails, report
  the failure upstream so the user can intervene." Smallest
  intervention that turns "report failure" into "try once more,
  then report" without DB or scheduler changes. Pure host-side
  auto-retry (respawn-on-failure with a per-`agent_group`
  `retry_count` column and original-create-agent-spec seed) is
  deferred — it needs a migration plus coordinated changes in the
  runner's `emit_terminal_failure_apologies`, the sweep's
  `apology::check`, and `image_health::emit_degraded_apology`, all
  of which currently emit independent apology rows. Picking a
  single chokepoint for that is a worthwhile but separate slice.

### Fixed (container/runtime hardening: mount-arg injection, USTAR overflow, SerpAPI key leak)

Three correctness/security bugs across the container backends and the
web-search tool:

- **Apple-Container `mount_arg` no longer lets an operator-controlled
  path inject `--mount` options**
  (`crates/copperclaw-container-rt/src/apple.rs`). `mount_arg` was
  interpolating `source` / `target` straight into a comma-separated
  `--mount type=bind,source=<source>,target=<target>` value, so a
  `source` ending in `,readonly=false` silently flipped mount
  semantics. The function now validates BOTH `source` and `target`
  (and `Volume` `name`, and `Tmpfs` `target`) for the reserved
  characters `,`, `=`, `\n` and returns
  `RtError::Unsupported("Apple container mount <role> path contains
  forbidden character '<ch>': <path>")`. Errors propagate through
  `run_args` -> `spawn`, so an operator gets a clear failure at
  install / first-spawn time rather than at runtime with mutated
  mount flags. Tests added: `mount_arg_bind_rejects_comma_in_source`,
  `mount_arg_bind_rejects_comma_in_target`,
  `mount_arg_bind_rejects_equals_in_source`,
  `mount_arg_bind_rejects_newline`,
  `mount_arg_volume_rejects_comma_in_name`,
  `mount_arg_tmpfs_rejects_comma_in_target`,
  `mount_arg_injection_blocked` (the literal `,readonly=false` attack
  payload), and `run_args_propagates_mount_validation_error`.
- **Docker USTAR writer no longer silently truncates filenames > 100
  bytes** (`crates/copperclaw-container-rt/src/docker.rs`). The inline
  `tar::append` was only copying the first 100 bytes of `name` into
  the `name` field and never populating the `prefix` field at offset
  345, so any `files/<long-basename>` produced opaque image-build
  failures or silent path collisions in the build context. The writer
  now (a) writes paths ≤ 100 bytes inline as before; (b) for 101..=256
  bytes splits at the last `/` that fits both fields (`name` ≤ 100,
  `prefix` ≤ 155) and writes the prefix at offset 345 (which is
  included in the existing whole-header checksum sum); (c) returns
  `TarError::PathTooLong` / `NoValidSplit` (surfaced as
  `RtError::Container`) for paths that cannot be encoded. Tests
  added: `short_name_written_inline_prefix_empty`,
  `medium_name_split_into_prefix_and_name` (round-trips a 130-byte
  prefix + 80-byte basename), `medium_name_checksum_includes_prefix_bytes`
  (proves the prefix bytes participate in the checksum),
  `too_long_name_returns_error`,
  `medium_name_no_split_point_returns_error`, and
  `build_context_tar_rejects_oversize_extra_file_name` at the public
  entry point.
- **`SERPAPI_API_KEY` no longer leaks into model history via reqwest
  errors** (`crates/copperclaw-mcp/src/tools/web_search.rs`). SerpAPI
  only supports query-string auth, so the key lived in the URL of
  every request. On transport failure, `reqwest::Error`'s `Display`
  walks the URL into the error message, which was being returned as
  the tool result, persisted into the agent's conversation, and
  re-sent to upstream providers on every subsequent turn. The fix
  introduces a `redact_reqwest_error` helper that builds a bounded
  error string from `reqwest::Error::is_timeout()` / `is_connect()` /
  `is_request()` / `is_body()` / `is_decode()` / `status()` only —
  it never invokes `Display` on the underlying error. The other
  providers were audited and are safe: Exa uses `x-api-key` header,
  Brave uses `X-Subscription-Token` header, Tavily passes the key in
  a POST JSON body (not in the URL). Tests added:
  `search_error_does_not_leak_api_key` (connect failure against an
  unbound port) and `search_error_against_bad_scheme_does_not_leak_key`
  (invalid-URL failure path), both asserting the literal `api_key`
  string never appears in the rendered error.

### Fixed (setup hardening: secret-file modes, headless token loop, launchd env)

Four bugs in `copperclaw-setup` that bit fresh installs hardest:

- **`setup-state.json` no longer leaves the OneCLI bearer token
  world-readable** (`crates/copperclaw-setup/src/state.rs`).
  `SetupState::save` was calling `fs::write`, which goes through the
  umask (typically `0o644`). The state file embeds
  `OneCliConfig.bearer_token` — a long-lived vault credential — so
  any local user on the host could read it. Saves now go through a
  new `write_secret_file` helper that opens with mode `0o600` from
  the start on Unix (`OpenOptions::mode(0o600)`) and explicitly
  re-tightens an existing-file's bits if a pre-batch install left
  them loose. Test added: `save_creates_file_with_mode_0600`
  (Unix-only) plants a bearer token, saves, asserts `mode() & 0o777
  == 0o600`. Companion `save_tightens_mode_on_pre_existing_loose_file`
  pins idempotent re-runs that converge to `0o600`.
- **`.env` writers no longer expose secrets through a chmod TOCTOU
  window** (`crates/copperclaw-setup/src/steps/auth.rs`,
  `crates/copperclaw-setup/src/steps/telegram.rs`).
  `write_env_file` and `append_env_var` were doing `fs::write` +
  chmod-after, leaving a brief window where the freshly written file
  existed at `0o644` before being tightened. Both paths now route
  through `state::write_secret_file`, so the bytes never land on
  disk under looser bits than `0o600`. The orphaned
  `restrict_permissions` helpers in those files are gone. Tests
  added: `write_env_file_sets_mode_0600_from_creation`,
  `write_env_file_tightens_perms_when_path_pre_exists_loose`.
- **Headless setup no longer spins forever on a malformed
  `COPPERCLAW_SETUP_TELEGRAM_BOT_TOKEN`**
  (`crates/copperclaw-setup/src/steps/telegram.rs`). `capture_token`
  was an unbounded `loop { prompt.secret(...); ... }` with no break
  on validation failure. Under `EnvBacked` (headless mode),
  `secret()` is deterministic, so a malformed token spun the loop
  indefinitely with no log output. The loop now tracks the previous
  invalid value and bails with a clear `StepError::Other`
  ("`COPPERCLAW_SETUP_TELEGRAM_BOT_TOKEN` failed bot-token validation;
  expected format `<bot_id>:<token>`") on the first identical
  repeat. Distinct invalid attempts still get a "try again"
  message, so an interactive fat-finger path isn't affected. Tests
  added: `pairing_headless_malformed_token_bails_instead_of_looping`
  (via `EnvBacked`), `pairing_scripted_two_identical_invalids_bails`,
  and `pairing_scripted_two_different_invalids_then_skip_does_not_bail`
  (negative-case guard so the heuristic doesn't over-fire).
- **macOS launchd plist now actually sources the `.env`**
  (`crates/copperclaw-setup/src/units.rs`). The generator was emitting
  `<key>EnvFile</key><string>...</string>`, but launchd has no
  `EnvFile` key — only `EnvironmentVariables` (a static plist
  dict). launchd silently ignored the bogus key, so the host
  booted on macOS without `ANTHROPIC_API_KEY` and every other
  secret captured into `.env`. Fixed by chasing the standard
  launchd pattern: a small POSIX-shell wrapper that sources the
  `.env` (`set -a; . file; set +a; exec copperclaw run --data-dir
  ...`) is generated next to the host binary, and the plist's
  `ProgramArguments` points at the wrapper. New helpers:
  `render_launchd_wrapper`, `launchd_wrapper_path`,
  `write_launchd_wrapper` (writes `0o755`). Snapshot test
  `render_launchd_does_not_emit_bogus_envfile_key` pins the
  absence of the bad key;
  `render_launchd_program_arguments_has_only_the_wrapper` pins the
  new single-arg shape so the wrapper's own args aren't duplicated;
  `render_launchd_wrapper_sources_env_and_execs_binary` and
  `render_launchd_wrapper_guards_missing_env_file` pin the
  shell-script body; `write_launchd_wrapper_creates_executable_file`
  pins the `0o755` install. **Follow-up needed:** the service-unit
  install step (`steps/service_unit.rs`) should call
  `write_launchd_wrapper` on macOS before writing the plist; this
  batch only ships the generators + corrected plist body.

Test delta: +15 in `copperclaw-setup` (275 → 290 lib tests).

### Fixed

- **Splitter retry no longer duplicates already-delivered chunks on
  partial failure** (`crates/copperclaw-host-delivery/src/service.rs`).
  When `split_chat_content_if_needed` produced 2+ parts and the
  adapter delivered chunk 0 successfully but failed chunk 1 with a
  retryable error (`AdapterError::Rate` / `Transport` / `Io`), the
  `?` in the dispatch loop propagated the error before any progress
  was recorded — the next retry restarted at chunk 0 and re-sent
  every earlier chunk, so users on every channel with a
  `max_message_chars()` cap (Telegram, Discord, Slack, Teams, …)
  saw chunk 0 twice or thrice (up to `MAX_DELIVERY_ATTEMPTS = 3`
  copies). `RetryState` now tracks `chunks_sent` and
  `first_chunk_pid`; `dispatch_chat` reads `chunks_sent` to resume
  mid-split and records the FIRST chunk's platform message id once
  (so subsequent `edit_message` / `add_reaction` target the same
  anchor across retries). The retry-state entry is naturally scoped
  per `(session_id, msg_id)` and cleared by the existing
  `process_session_once` success / failure paths. Limitation: if
  `max_message_chars()` changes between attempts (e.g. operator
  hot-reloads config mid-retry), the resume index could land in a
  different chunk; operators don't hot-reload in production, so
  out-of-scope. +4 regression tests
  (`split_happy_path_no_duplicate_chunks`,
  `split_partial_success_retry_skips_delivered_chunks`,
  `split_retry_exhaustion_does_not_replay_first_chunk`,
  `split_first_chunk_pid_stable_across_retries`).

### Fixed (5 host-process correctness + auth-bypass bugs)

- **cclaw socket now derives caller identity from `SO_PEERCRED` (Linux
  `getpeereid` on macOS) instead of trusting the JSON `caller` field
  on the wire** (`crates/copperclaw-host/src/socket.rs`). Previously any
  local UID-matching process — including a container that somehow
  reached the admin socket — could send `{"caller":{"kind":"host"}}`
  and execute every host-only mutation (`db.backup`, `groups.delete`,
  `mcp.add` with secrets, etc.); the audit log even recorded the
  attacker as "host". The new `serve_unix_connection` reads
  `UnixStream::peer_cred()` (tokio 1.x built-in — no `nix` / no
  `unsafe` needed) and compares the peer UID against the host's own
  effective UID (resolved from `/proc/self` ownership, same trick
  `container_manager::spawn::host_uid_gid` uses). A non-matching peer
  UID yields a `permission_denied` response; matching peers may
  self-identify as a particular agent via the wire `Agent` claim but
  `Caller::Host` is now an authoritative kernel-derived label, never
  a wire-supplied one. +5 socket-layer tests
  (`derive_caller_*` + two end-to-end UnixStream round-trips
  including a cross-UID rejection).
- **Socket-server bind errors now abort `run_host` with
  `BootError::Socket` instead of being swallowed**
  (`crates/copperclaw-host/src/boot.rs` + `socket.rs`). The old fused
  `tokio::spawn(run_server(...))` discarded the JoinHandle's
  bind-result, so a stale non-socket file or an unwritable parent
  directory would leave the host printing "boot complete" with a
  dead admin surface. The new `bind_listener` runs synchronously
  inside `run_host` (exit code 4 via the existing
  `BootError::exit_code` mapping); `serve_listener` only spawns
  after the bind succeeds. +1 integration test that drives `run_host`
  against an unbindable socket path and asserts
  `BootError::Socket(_)`.
- **Per-frame and per-connection caps on the cclaw socket close a
  local DoS** (`crates/copperclaw-host/src/socket.rs`). `read_until`
  on the NDJSON wire had no upper bound — a local process could
  feed a 1 GiB frame and OOM the host before any parse ran. Each
  request frame is now wrapped in `BufReader::take(1 MiB)` via the
  new `MAX_REQUEST_FRAME_BYTES` and overflows surface as a protocol
  error instead of pinned memory. Concurrent accepted connections
  are gated by a `tokio::sync::Semaphore` capped at
  `MAX_CONCURRENT_CONNECTIONS = 32`; the permit is held for the
  task's lifetime so a flood of opens can't exhaust the host's fd
  table. +2 tests
  (`oversized_request_frame_is_rejected_not_oomed`,
  `concurrent_connection_cap_actually_limits`) plus a
  defence-in-depth regression (`under_cap_request_still_works`).
- **`dropped_messages` `parse_since` no longer panics on multi-byte
  UTF-8 inputs** (`crates/copperclaw-host/src/handlers/dropped_messages.rs`).
  The old `s.split_at(s.len().saturating_sub(1))` operated on byte
  indices, so a `since="é"` or `since="5🦀"` from an agent-side
  caller would land split_at inside a UTF-8 code point and panic the
  handler task. Replaced with `s.chars().next_back()` + `len_utf8()`
  arithmetic so multi-byte inputs fall through to the existing
  `bad_request` error path. +3 tests
  (`parse_since_rejects_multibyte_inputs_without_panicking`,
  `parse_since_accepts_valid_shorthand`,
  `outbound_list_with_multibyte_since_errors_cleanly`).
- **`todo_watcher` notification text is now emoji-free**
  (`crates/copperclaw-host/src/todo_watcher.rs`). The `📋 Plan` and
  `✅ done` prefixes violated the project-wide "no emojis" rule
  (CLAUDE.md). Replaced with plain ASCII `[todo]` / `[done]` tags
  matching the surrounding tone. +1 enforcement test
  (`notifications_contain_no_emoji`) that walks every code path in
  `diff_to_notifications` (first-time, completion, plan-grew) and
  asserts no Unicode codepoint in the emoji blocks
  (`U+1F300..1F5FF`, `U+1F600..1F64F`, `U+1F680..1F6FF`,
  `U+1F900..1F9FF`, `U+1FA70..1FAFF`, `U+2600..27BF`,
  `U+1F1E6..1F1FF`) appears in any emitted notification.

### Fixed (4 mid-stream / dedup / TOCTOU correctness bugs)

- **Runner no longer crashes mid-stream on transient
  `container_state` write errors.** `crates/copperclaw-runner/src/run/provider_call.rs`
  used `?` to propagate `set_current_tool` / `clear_current_tool`
  results, so a single SQLite lock contention writing the stuck-tool
  housekeeping row would abort the entire `pump_events` stream,
  discard every queued mid-stream tool_use event, crash the runner,
  and force the container to respawn. These writes are best-effort
  (the stuck-tool detector only loses one tool's `started_at` for one
  pass) and now warn-log and continue rather than propagating —
  matching the let-the-write-fail convention used elsewhere in the
  runner. Covered by
  `pump_completes_when_container_state_writes_fail` (drops the
  `container_state` table before the run, asserts the assistant
  Chat row still lands).

- **Resume-after-crash dedup now scans backwards past tool turns.**
  `crates/copperclaw-runner/src/run/mod.rs` only checked
  `state.history.last()` for a matching `User` entry. Because
  `persist_mid_message` saves history after each tool turn, a
  mid-tool-loop crash leaves history ending in `Tool { ... }` (or
  `ToolUse`), so the prior fix's `.last()` check returned `false` and
  the runner re-pushed the user prompt, producing `[..., User(p),
  Assistant, ToolUse, Tool, User(p)]` and either a second answer or a
  confused model. Extracted the dedup into
  `is_prompt_already_in_history`, which walks the most recent
  `RESUME_DEDUP_LOOKBACK = 10` entries backwards and stops at the
  first `User` — if it matches the current prompt, skip the push.
  Covered by `resume_mid_tool_loop_skips_duplicate_push`.

- **`recurrence::check` no longer aborts the session's sweep on
  slice-3 kinds.** `crates/copperclaw-host-sweep/src/checks/recurrence.rs`
  hand-rolled a six-variant `parse_kind` helper missing `breadcrumb`,
  `diff`, `todo_list`, `error`, and `thinking`. A recurring inbound
  with any of those kinds returned `unknown kind` and aborted the
  whole recurrence sweep for that session. Replaced the call with
  `MessageKind::parse_str` (the canonical column-string parser) and
  deleted the local helper so this drift cannot recur. Covered by
  `all_message_kinds_parse_without_error_in_recurrence_sweep` —
  seeds one recurring row per documented variant and asserts each
  one fans out.

- **`processing::check` no longer races a finishing runner into a
  duplicate reply.** `crates/copperclaw-host-sweep/src/checks/processing.rs`
  read `processing_ack` (status=Processing, stale) + scanned for any
  `in_reply_to=msg_id` reply, then did the inbound-reset UPDATE
  later. Between the SELECT and the UPDATE the runner could finish
  the turn — writing its reply and flipping `processing_ack.status`
  to Done — and the sweep would still reset the inbound to
  `pending`, causing the runner to re-pick the same message and
  produce a duplicate reply. The reset path now calls a new
  `atomic_reclaim_claim` helper that opens an IMMEDIATE transaction
  on the outbound DB, re-reads the claim row, re-checks the staleness
  threshold AND the absence of any reply in `messages_out`, and only
  deletes the claim atomically if all guards still hold. The
  cross-DB inbound reset only runs when the delete succeeded, so a
  runner that races past us is honoured. A new `check_with_hook`
  test seam exposes a `before_reset` callback so the regression test
  can inject the concurrent (reply + ack=Done) write between the
  initial scan and the re-check; the test asserts the inbound stays
  untouched and the runner-set `Done` ack is preserved.

### Fixed (3 race / serde correctness bugs)

- **`MessageKind::TodoList` JSON round-trip.**
  `crates/copperclaw-types/src/message.rs` had `#[serde(rename_all =
  "lowercase")]` on `MessageKind`, which serialised `TodoList` as
  `"todolist"` (no underscore). The DB column form via `as_str()` /
  `parse_str()` is `"todo_list"`, so any path that round-tripped a
  `MessageKind` through JSON and then tried `parse_str` on the wire
  tag silently lost the kind. Switched to `rename_all = "snake_case"`
  — single-word variants serialise identically, `TodoList` now
  serialises as `"todo_list"` matching the DB form. The dedicated
  per-variant unit tests and the previously broken-on-purpose
  `"todolist"` assertion were updated; a new
  `message_kind_serde_tag_matches_as_str_for_every_variant` test pins
  the new contract for every variant.
- **`unregistered_senders::upsert` race.**
  `crates/copperclaw-db/src/tables/unregistered_senders.rs` was
  SELECT-then-INSERT/UPDATE against a pooled (max=8) writer. Two
  concurrent first-time inbounds for the same `(channel_type,
  platform_id)` could both observe missing row, both INSERT, the
  loser bubbled a UNIQUE-violation `DbError::Sqlite` and the router
  dead-lettered the inbound. Collapsed into a single atomic
  `INSERT ... ON CONFLICT(channel_type, platform_id) DO UPDATE`
  against the existing primary key. New `tokio::test(flavor =
  "multi_thread")` regression test spawns 16 concurrent upserts
  against a file-backed pool and asserts exactly one row with
  `message_count == 16`.
- **`pending_approvals::upsert` race + new migration.**
  `crates/copperclaw-db/src/tables/pending_approvals.rs` had the same
  SELECT-then-INSERT shape and no DB-side constraint on
  `(request_id, action)` — concurrent upserts produced silent
  duplicate pending rows. New migration
  `crates/copperclaw-db/migrations/016_pending_approvals_unique.sql`
  adds a partial unique index `WHERE status = 'pending'` (terminal
  rows can repeat across statuses; only the live pending row is
  unique). Registered in `MigrationSet::Central` at
  `crates/copperclaw-db/src/migrate.rs`. The upsert is now a single
  atomic `INSERT ... ON CONFLICT(request_id, action) WHERE status =
  'pending' DO UPDATE`. Two new tests: a 16-task concurrency
  regression test, plus a `upsert_after_denial_creates_fresh_pending_row`
  test that pins the partial-index contract.

### Fixed (slice-2 conversation-context follow-up: persist reply_to + is_group)

The two channel-event signals Agent A had been populating on
`InboundEvent` (`reply_to`, `message.is_group`) were stopping at the
router — they were never persisted onto `messages_in`, so when the
runner read `MessageInRow` and built the per-turn "Conversation context"
block (Agent B's work) the signals weren't there and the block
degraded to channel-only phrasing. This closes the gap end-to-end:

- **New per-session inbound migration
  `crates/copperclaw-db/migrations/015_messages_in_reply_to_is_group.sql`**
  adds `reply_to TEXT NULL` + `is_group INTEGER NULL` columns to
  `messages_in`. Registered in `MigrationSet::SessionInbound` at
  `crates/copperclaw-db/src/migrate.rs`. Existing rows are unaffected
  (both columns default to NULL).
- **`WriteInbound` + `MessageInRow` extended** with `reply_to:
  Option<String>` and `is_group: Option<bool>`. The INSERT/SELECT
  paths in `crates/copperclaw-db/src/tables/messages_in.rs` write +
  read both columns; the row parser coalesces a legacy
  `reply_to = ''` shape to `None` (parallel to the existing
  `source_session_id` defence).
- **Router insert site at
  `crates/copperclaw-host-router/src/route.rs::deliver_to_session`**
  now pulls `event.reply_to.thread_id` (the parent platform message
  id every adapter stuffs there) and `event.message.is_group` and
  passes both through `WriteInbound`.
- **`render_conversation_context` in
  `crates/copperclaw-runner/src/run/prompt.rs`** consumes the new
  fields: `is_group=Some(true)` renders "a group chat",
  `Some(false)` renders "a 1-on-1 DM", `None` keeps the existing
  thread-id-derived fallback. `reply_to=Some(...)` appends ", in
  reply to an earlier message" after the venue/channel run. Both
  signals degrade silently when `None` so adapters that don't
  populate them (cli, file-watcher, webhook-only) see the legacy
  phrasing unchanged.
- **Recurrence fan-out
  (`crates/copperclaw-host-sweep/src/checks/recurrence.rs`)** now
  carries the parent row's `reply_to` / `is_group` onto every
  fan-out so the runner's context block stays consistent across a
  recurring series.
- **Coverage:** DB-side round-trip + empty-string coalescing tests
  in `messages_in.rs`; router-side persist + None-pass-through
  tests in `route.rs::tests`; runner-side context-block extension
  tests in `prompt.rs::tests` (DM+reply, group+reply, group-no-thread,
  legacy-no-signals). +8 tests total.

### Fixed (skill body cap + channel doc follow-ups)

- **Skill body cap aligned with the documented 8 KiB ceiling.**
  `MAX_SKILL_BODY_BYTES` in `crates/copperclaw-skills/tests/coverage.rs`
  was enforcing 4 KiB while `skills/README.md` advertised 8 KiB, which
  forced new long-form taxonomy skills (e.g. `native-ui`) to compress
  per-channel tables into legend codes. Bumped the test ceiling to
  8 KiB to match the README intent; the 4 KiB target is preserved in
  the doc-comment as the spec goal.
- **`skills/native-ui/SKILL.md` restored to its full shape.** Replaced
  the legend-coded N/L/R/T rendering table with the descriptive
  per-channel table (telegram / slack / discord / gchat / matrix
  columns, one row per shape), brought back the `../send-file/SKILL.md`
  cross-reference, expanded the anti-pattern section with concrete
  WRONG/RIGHT examples. Body now ~6.4 KiB, well under the 8 KiB cap.
- **`docs/channels/gchat.md` + composite heatmap in
  `docs/channels/README.md`** updated to reflect that gchat
  `deliver_breadcrumb` has shipped (Cards v2 `decoratedText` single-
  section card with in-place `spaces.messages.patch` edits,
  `crates/copperclaw-channels/gchat/src/adapter.rs:191`). Stale
  "landing this week (agent G)" marker removed.
- **`docs/channels/mattermost.md` `is_group` row** rewritten to
  "no (not in payload)" with the wire-field rationale. Mattermost
  outgoing-webhook payload (`token` / `channel_id` / `channel_name`
  / `user_id` / `text` / `trigger_word` / `file_ids`) carries no
  channel-type signal; deriving DM-vs-group requires a follow-up
  `GET /api/v4/channels/{channel_id}` lookup. `TODO(channel-ux)`
  comment added in
  `crates/copperclaw-channels/mattermost/src/router.rs` at the
  `InboundEvent` construction site documenting the contract.
- **Discord `thread_id`/`reply_to` doc row verified** against
  `crates/copperclaw-channels/discord/src/events.rs:64-75`. The
  legacy `thread_id` mirror is real (kept to avoid breaking
  existing routing); the documented row already matches.

### Fixed (slice 3 integration pass)

Five slice-3 agents (Diff / TodoList / Error / Long-output / Thinking) landed in parallel; the integration pass closed the seams between them:

- **`apply_emit_todo_list` body rewritten** to use `serde_json::Map::new()` + `.insert()` rather than the `json!({...})` macro so the runner-emit-set coverage test (`runner_emit_set_matches_source`) doesn't misclassify the `"todo_list"` content key as a `MessageKind::System` action name. Mirrors the same dodge in `apply_send_card`. (`crates/copperclaw-runner/src/tools.rs::apply_emit_todo_list`)
- **`EXPANDER_BYTE_THRESHOLD` raised from 4 KB → 64 KB.** The original threshold collided with Telegram's 4096-char `max_message_chars()` cap and with Slack's 40 000-char cap, so any agent message just over a platform cap got folded into the expander chip rather than going through the slice-1 splitter. The expander is for *long tool output* (pages of shell stdout) — well over 64 KB in real use; the new threshold preserves that intent without competing with the splitter. (`crates/copperclaw-runner/src/tools.rs:914-919`)
- **Wired `emit_breadcrumb_finish` into the runner's tool loop.** After Agent G shipped the trait surface, the chip stayed stuck on "Running" forever because no one was calling the finish hook. New helper `finish_tool_breadcrumb` in `drive_turn.rs` fires after every `invoke_tool` return, passing the tool's first non-empty result line (char-truncated to 200) as the summary.
- **`cli/provider-timeout` replay fixture updated** for the slice-3.3 ErrorCard apology shape. The runner's terminal-failure apology is now a `MessageKind::Error` row carrying a full `ErrorCard`; the fixture's pre-3.3 expected output of a plain `Chat` row was stale.
- **`approvals.rs` doc-markdown clippy fix** (backticks around `self_mod`).
- **`drive_turn` carries `#[allow(clippy::too_many_lines)]`** — the function is the central tool-loop state machine; intrinsic.

Workspace: 5,788 passing, 0 failing, 6 ignored. Clippy clean on `cargo clippy --workspace --all-targets -- -D warnings`.

### Added (slice 3.5 — opt-in surfaced thinking blocks)

Reasoning-capable models (Anthropic extended thinking, `Kimi K2.6`,
`Qwen QwQ`, `DeepSeek R1`, …) stream a chain-of-thought block before
their user-facing reply. Until this batch the Anthropic provider
absorbed those silently — they didn't pollute the agent's reply (see
`ThinkingAccumulator`), but the user couldn't see them either. Slice
3.5 adds an OPT-IN pipeline that, when an operator flips the
per-group `surface_thinking` flag, persists each completed reasoning
block as a `MessageKind::Thinking` outbound row and renders it as a
collapsed native UI primitive on every adapter that has one.

Default is **OFF** — surfacing model chain-of-thought has privacy
implications (mid-thought speculation about the user, debugging notes
the model didn't intend the user to see, etc.). This matches the
Copperclaw tenet of "secure-by-default, public-by-deliberate-act".

- **Canonical `ThinkingBlock` schema**
  (`crates/copperclaw-channels/core/src/thinking.rs`,
  `crates/copperclaw-channels/core/src/lib.rs`). Fields: `text` (≤
  `MAX_THINKING_CHARS` = 8000 codepoints), `redacted: bool` (mirrors
  the upstream `redacted_thinking` block type — renderers MUST
  substitute a placeholder rather than display any text), `model:
  Option<String>` (optional provenance tag, ≤ 64 chars). `validate()`
  enforces non-empty text unless `redacted`. `to_text_fallback()`
  emits a `[reasoning]`-headered quoted block so plain-text channels
  still surface the reasoning clearly.
- **`MessageKind::Thinking` variant**
  (`crates/copperclaw-types/src/message.rs`) — alphabetical placement
  among the slice-3 new variants. Serde lowercase `"thinking"` on the
  wire; `as_str` / `parse_str` round-trip pinned by a new test.
- **New `ChannelAdapter::deliver_thinking` hook**
  (`crates/copperclaw-channels/core/src/adapter.rs`). Default impl
  converts the block via `to_text_fallback` and routes through
  `deliver` as `MessageKind::Chat`, so every adapter has a usable
  rendering for free.
- **Per-channel native renderers**:
  - Telegram (`crates/copperclaw-channels/telegram/src/adapter.rs`):
    HTML `<blockquote expandable>` (Bot API 7.6+, same primitive as
    surface 4) with `<i>reasoning</i>` prefix.
  - Slack (`crates/copperclaw-channels/slack/src/adapter.rs`): Block
    Kit `context` block (the platform's idiomatic muted-metadata
    affordance) with `:thought_balloon:` emoji + reasoning label,
    chunked across multiple blocks under Slack's 3000-char element
    cap.
  - Discord (`crates/copperclaw-channels/discord/src/adapter.rs`):
    embed with secondary-grey color (`0x99AAB5`), `author.name =
    "reasoning"` (with optional provenance), description fenced as
    `text` to defang user-supplied markdown / backticks.
  - Google Chat (`crates/copperclaw-channels/gchat/src/adapter.rs`):
    Cards v2 `collapsibleSection` (native disclosure-widget
    primitive — same as surface 4 long-output) with
    `uncollapsibleWidgetsCount: 0` so the body stays behind the
    fold.
  - Matrix (`crates/copperclaw-channels/matrix/src/adapter.rs`):
    `m.notice` with HTML `<details>` disclosure widget — Element /
    SchildiChat / Cinny render it as a clickable expander natively.
- **New `ProviderEvent::Thinking { text, redacted }` variant**
  (`crates/copperclaw-types/src/provider.rs`). The Anthropic provider
  (`crates/copperclaw-providers/src/anthropic.rs`) emits one of these
  at every `content_block_stop` boundary closing a `thinking` /
  `redacted_thinking` block, carrying the accumulated text. Two new
  SSE-pump tests pin the emit shape (visible + redacted).
- **`ToolContext::emit_thinking`**
  (`crates/copperclaw-mcp/src/context.rs`) + runner impl
  (`crates/copperclaw-runner/src/tools.rs`). Mirrors
  `emit_breadcrumb`: best-effort, swallows errors, no-op when there's
  no channel routing to surface to.
- **Runner-side opt-in gate**
  (`crates/copperclaw-runner/src/run/provider_call.rs`): the gate lives
  in `pump_events`, gated on the new `RunnerDeps::surface_thinking`
  flag. When off, `ProviderEvent::Thinking` events drop on the
  floor; when on, the runner calls `tool_ctx.emit_thinking` which
  writes the canonical row.
- **Per-group `surface_thinking` column on `container_configs`**
  (new migration
  `crates/copperclaw-db/migrations/014_container_config_surface_thinking.sql`,
  schema additions in
  `crates/copperclaw-db/src/tables/container_configs.rs`,
  `set_surface_thinking` setter). Default `0` matches the privacy
  default. Plumbed from the host's container manager into the
  runner's JSON config via the new
  `RunnerConfigForFile::surface_thinking` field (skipped when off so
  existing-group config files stay bit-identical).
- **Host delivery dispatch**
  (`crates/copperclaw-host-delivery/src/service.rs`): new
  `dispatch_thinking` arm routes `MessageKind::Thinking` rows through
  the adapter's `deliver_thinking` hook, with the standard
  `AdapterError::Unsupported` → text-fallback degradation.
- **Orthogonal to `strip_reasoning_blocks`** — the existing
  sanitiser that scrubs inline `<thinking>` markup from `Chat` rows
  is unchanged: that path protects against prose contamination in
  the chat reply; this surface emits structured reasoning as its
  own row.

### Added (slice 3.1 — structured diff cards on file edits)

File edits previously emitted only a `[edit_file] foo.rs` text
breadcrumb; the user had to read your follow-up prose to find out what
actually changed. The runner now emits a structured `DiffCard`
*alongside* the breadcrumb after every successful `edit_file` /
`multi_edit` / `apply_patch` / `write_file` (overwriting) write,
rendered natively as a syntax-coloured diff with `+` / `-` gutters on
every adapter that supports a code-block primitive (Telegram, Slack,
Discord, Google Chat, Matrix). Breadcrumb = "what tool ran"; diff
card = "what changed".

- **New canonical `DiffCard` schema**
  (`crates/copperclaw-channels/core/src/diff.rs`,
  `crates/copperclaw-channels/core/src/lib.rs`). Fields: `path` (≤256
  chars), optional `language`, `hunks: Vec<DiffHunk>` (≤8), `added`,
  `removed`, `truncated`. Each `DiffHunk` carries `old_start /
  old_lines / new_start / new_lines` (unified-diff convention,
  1-based) and `lines: Vec<DiffLine>` (≤60); each `DiffLine` is
  `{kind: Context|Add|Remove, text}` (text ≤500 chars). `validate()`
  + `to_text_fallback()` mirror the `Breadcrumb` / `Card` shape;
  `clamp()` enforces caps idempotently before emit so the wire
  payload always passes `validate()`. Companion `BlobReplaced` shape
  for the overwrite-of-large-file path.
- **New `MessageKind::Diff` variant**
  (`crates/copperclaw-types/src/message.rs`). Serialises as `"diff"`
  (lowercase); DB column round-trip via `as_str` / `parse_str`.
- **New `ChannelAdapter::deliver_diff` trait method**
  (`crates/copperclaw-channels/core/src/adapter.rs`). Default impl
  converts via `DiffCard::to_text_fallback` (standard unified diff
  with `--- a/<path>` / `+++ b/<path>` header, `@@ -…@@` hunks,
  `+`/`-`/` ` prefixes, `(+N / -M)` footer) and routes through
  `deliver` as `MessageKind::Chat`. No `existing_message_id`: diffs
  are immutable post-emit.
- **Runner-side diff computation**
  (`crates/copperclaw-mcp/src/tools/diff_util.rs`). New helper uses
  the `similar` crate to build a structured `DiffCard` from
  pre/post-edit string snapshots; `edit_file` / `multi_edit` /
  `apply_patch` snapshot the pre-edit content and call
  `ToolContext::emit_diff` after the atomic write lands; `write_file`
  reads the prior content (when the target exists, isn't being
  appended to, and is under the 256 KB cutoff) and does the same.
  Over-cutoff overwrites emit a `BlobReplaced` summary instead of
  trying to diff multi-megabyte blobs.
- **New `ToolContext::emit_diff` hook** (`crates/copperclaw-mcp/src/context.rs`).
  Default no-op so non-runner contexts (mock, subagent adapter)
  compile unchanged; `RunnerToolCtx::emit_diff` overrides it to
  persist a `MessageKind::Diff` outbound row with the canonical
  payload under `content.diff`. Mock context records diff calls so
  file-edit tool tests can assert the wiring.
- **New `dispatch_diff` arm in the host delivery service**
  (`crates/copperclaw-host-delivery/src/service.rs`). Mirrors
  `dispatch_breadcrumb`: deserialises `content.diff` into the
  canonical `DiffCard`, hands it to `deliver_diff`, falls back to a
  unified-diff text body via `deliver` on
  `AdapterError::Unsupported`. No typing indicator (the breadcrumb
  already signalled), no `to` hint.
- **Native renderers — priority channels:**
  - **Telegram** (`crates/copperclaw-channels/telegram/src/adapter.rs`):
    `sendMessage` MarkdownV2 wrapping the diff body in a
    ` ```diff … ``` ` fenced code block, with a bold path header and
    `(+N / -M)` totals. Mobile clients colourise `diff` syntax
    natively.
  - **Slack** (`crates/copperclaw-channels/slack/src/adapter.rs`):
    Block Kit `section` header (`*<path>* (+N / -M)`) + one
    `rich_text_preformatted` block per hunk. Honours `+` / `-`
    gutters and dodges the 3000-char per-section truncation surprise.
  - **Discord** (`crates/copperclaw-channels/discord/src/adapter.rs`):
    Single embed with `description` carrying the ` ```diff … ``` `
    fenced block; embed `color` keys off add/remove balance
    (`0x57F287` green / `0xED4245` red / `0xFEE75C` yellow);
    over-budget hunks spill into `fields`.
  - **Google Chat** (`crates/copperclaw-channels/gchat/src/adapter.rs`):
    Cards v2 card with one `decoratedText` widget per hunk
    (`topLabel = @@ -…@@`, body wrapped in `<font face="monospace">`
    with HTML-escaped source).
  - **Matrix** (`crates/copperclaw-channels/matrix/src/adapter.rs`):
    `m.notice` with `formatted_body = <pre><code
    class="language-diff">…</code></pre>`. Element honours the
    `language-diff` class natively.
- **Workspace dep:** `similar = "2"` added to root `Cargo.toml` and
  pulled into `copperclaw-mcp` for diff computation. Pure-Rust MIT
  crate; no runtime requirements.
- **Skills:** `skills/edit-file/SKILL.md` and `skills/write-file/SKILL.md`
  each gain a "Diff card surfaced to the user" section telling the
  agent the diff is already on screen — no need to summarise the
  change in prose.

### Added (slice 3.3 — host-emitted `Error` cards with red affordance)

Host-emitted errors that previously landed as plain chat (or as the
`failed` row in `cclaw dropped-messages` only) now ride a dedicated
`MessageKind::Error` surface, rendered with the red bar / bold prefix
each platform supports so users actually see "something broke" instead
of being shown a normal-looking reply (or nothing at all).

Crucially this surface is HOST-EMITTED, not model-emitted. There is no
`send_error` MCP tool. The host produces these from three sites:

1. Provider terminal failures (`TurnOutcome::Failed` after retry
   exhaustion) — replaces the plain-text apology row.
2. Delivery retry exhaustion (3 failed adapter sends on a single
   outbound row) — emitted *in addition to* the existing
   `delivered.status="failed"` row so `cclaw dropped-messages`
   continues to work unchanged.
3. Internal tool errors that bubble past the runner's retry budget
   (path wired via the same trait + dispatch machinery; emit sites
   land as future tool handlers add them).

- **New canonical `ErrorCard` schema**
  (`crates/copperclaw-channels/core/src/error_card.rs`,
  `crates/copperclaw-channels/core/src/lib.rs`). Fields: `title` (≤120
  chars, default "Something went wrong"), `summary` (≤500), `kind`
  (`Internal` / `Provider` / `Delivery`), optional `details` (≤2000,
  monospace), `retryable: bool`. `validate()` + `to_text_fallback()`
  follow the same shape as `Breadcrumb` / `Card`. Re-exports use
  `MAX_ERROR_*` names so they don't collide with `Card`'s
  `MAX_TITLE_CHARS`; the type itself is renamed `ErrorCard` (not
  `Error`) so it doesn't shadow `AdapterError`.
- **New `MessageKind::Error` variant**
  (`crates/copperclaw-types/src/message.rs`). Serialises as `"error"`
  (lowercase, `serde(rename_all)`); DB column round-trip via
  `as_str` / `parse_str`.
- **New `ChannelAdapter::deliver_error` trait method**
  (`crates/copperclaw-channels/core/src/adapter.rs`). Default impl
  converts via `ErrorCard::to_text_fallback` and routes through
  `deliver` as `MessageKind::Chat`, so every adapter has a usable
  rendering — `[ERROR: <kind>] <title>\n<summary>` — even before
  shipping a native renderer. No `existing_message_id` argument:
  error receipts are immutable.
- **New `dispatch_error` arm in `process_row`**
  (`crates/copperclaw-host-delivery/src/service.rs`). Deserialises
  `content.error` into the canonical `ErrorCard`; calls
  `deliver_error`; falls back to text `deliver` on
  `AdapterError::Unsupported` (belt-and-braces mirror of
  `dispatch_card` / `dispatch_breadcrumb`). No typing indicator —
  the error is the visual signal.
- **Retry-exhaustion `ErrorCard` emit**
  (`crates/copperclaw-host-delivery/src/service.rs::emit_delivery_failure_error_card`).
  When `DeferOutcome::Fail` fires the host now writes a fresh Error-
  kind outbound row addressed back at the failed row's channel +
  platform + thread, with `kind = ErrorCardKind::Delivery`,
  `retryable = false`, and the underlying adapter error spliced into
  the summary. The next delivery pass routes it through
  `dispatch_error`. The existing `delivered::insert(.., "failed")`
  write is preserved — operators still see the row in `cclaw
  dropped-messages`; the user additionally sees a visible error in
  chat.
- **Terminal-failure-apology promoted to `ErrorCard`**
  (`crates/copperclaw-runner/src/run/mod.rs::emit_terminal_failure_apologies`).
  The human-channel branch now writes a `MessageKind::Error` row
  carrying an `ErrorCardKind::Provider` card whose `summary` is the
  same user-facing apology text the old plain-text path used. The
  parent-agent branch (LLM reader, not human) stays as
  `MessageKind::Agent` plain prose — feeding another agent a
  structured error card would hand it a side-channel signal harder
  to handle than a sentence.
- **Per-channel native renderers** (red where the platform has color,
  bold + monospace where it doesn't):
  - Telegram (`crates/copperclaw-channels/telegram/src/adapter.rs`):
    HTML `<b>{kind label}: {title}</b>` prefix (Telegram has no
    colour affordance; weight + monospace details + the canonical
    `[ERROR]` text prefix carry the severity signal). Details ride
    in `<pre>…</pre>`. Retryable footer `<i>will retry
    automatically</i>`.
  - Slack (`crates/copperclaw-channels/slack/src/adapter.rs`): new
    `SlackApi::post_message_with_attachments` so we can drive the
    `attachments[].color = "danger"` red bar (Block Kit primary
    blocks can't produce a bar on their own). `header` + `section
    mrkdwn` + optional `rich_text_preformatted` for details. Text
    fallback rides on the top-level `text` for notification preview.
  - Discord (`crates/copperclaw-channels/discord/src/adapter.rs`):
    single embed with `color = 0xE74C3C` (red), title + description
    + fenced-code details, retryable footer. Embedded backticks in
    user-supplied details are neutralised so the body can't break
    out of the fence.
  - Google Chat (`crates/copperclaw-channels/gchat/src/adapter.rs`):
    Cards v2 with `Error:`-prefixed header, severity-label
    decorated-text widget, optional monospace details paragraph,
    italic retryable footer. (Google Chat cardsV2 has no color
    primitive — icon + bold copy + the title prefix carry severity.)
  - Matrix (`crates/copperclaw-channels/matrix/src/adapter.rs`):
    `m.text` (NOT `m.notice` — errors warrant notification badges;
    muting them in Element would defeat the surface's purpose) with
    `<font color="#cc3333">` wrapping the bold title. `<pre><code>`
    for details, `<em>` retryable footer.
- **Test count delta**: +27 tests
  (20 channels-core (schema + trait default) + 5 telegram + 5 slack
  + 5 discord + 5 gchat + 5 matrix + 3 host-delivery (dispatch +
  retry-exhaustion-emit). Two existing runner tests
  (`terminal_failure_emits_apology_to_originating_channel`,
  `malformed_tool_use_gives_up_after_three_attempts`) updated to
  decode the new `MessageKind::Error` row shape via
  `serde_json::from_value::<ErrorCard>`; the user-facing apology
  text invariants they pinned are unchanged.

### Added (slice 3.4 — native long-output expander decorator)

Long tool outputs (shell stdout, `web_fetch` bodies, `read_file` of
oversized files, long agent replies) now ride as a "summary +
collapsible expander" decorator on the existing `MessageKind::Chat`
row rather than dumping the full body into chat as ugly multi-line
output. The decorator is invisible to the model — no new MCP tool, no
new MessageKind — the runner auto-attaches it on `apply_send_message` /
`apply_send_file` when the chat body exceeds 30 lines OR 4 KB.

- **New runner-side threshold detector**
  (`crates/copperclaw-runner/src/tools.rs`):
  `build_expander_decorator(text)` returns `Some(json)` when either
  threshold trips, otherwise `None`. Decorator JSON shape:
  `{ summary, summary_kind: "lines"|"bytes", preview_lines: [...] }`
  with the first 6 lines as preview. Constants
  `EXPANDER_LINE_THRESHOLD = 30`, `EXPANDER_BYTE_THRESHOLD = 4 * 1024`,
  `EXPANDER_PREVIEW_LINES = 6`. Helper is invoked from
  `apply_send_message` and `apply_send_file` (for the caption body)
  on Chat-kind rows only — Agent-kind rows skip decoration.
- **New `ChannelAdapter::deliver_collapsible` trait method**
  (`crates/copperclaw-channels/core/src/adapter.rs`). Default impl
  composes a summary-plus-preview-plus-truncation-marker body via
  `render_collapsible_text_fallback` and routes through `deliver`, so
  every adapter has a usable rendering for free even without a native
  override. Shared helper exported as
  `copperclaw_channels_core::render_collapsible_text_fallback`.
- **New dispatch branch in `dispatch_chat`**
  (`crates/copperclaw-host-delivery/src/service.rs`). Chat-kind rows
  whose `content.expander` decorator is present route to the new
  `dispatch_collapsible` helper which calls the adapter's
  `deliver_collapsible` hook with the full text + summary + preview;
  rows without the decorator continue through the unchanged
  text-splitter path. `AdapterError::Unsupported` falls back to a
  plain `deliver` with the helper-rendered body (belt-and-braces,
  same shape as `dispatch_card` / `dispatch_breadcrumb`).
- **Per-channel native renderers**:
  - Telegram (`crates/copperclaw-channels/telegram/src/adapter.rs`):
    HTML `<i>{summary}</i>` outside, `<blockquote expandable>` wrapping
    the full body. Bot API 7.6+ native primitive; clients without
    `expandable` see a fully-rendered blockquote (graceful).
  - Slack (`crates/copperclaw-channels/slack/src/adapter.rs`): Block Kit
    `section` mrkdwn for the summary, a preview `rich_text_preformatted`
    when present, and a second `rich_text_preformatted` with the full
    body. Slack's native "Show more" collapses oversized preformatted
    blocks behind a click — functionally equivalent to a disclosure
    widget without needing a `block_actions` callback round-trip.
  - Discord (`crates/copperclaw-channels/discord/src/adapter.rs`):
    single embed with `author.name = "long output"`, `title = summary`,
    `description = preview fence + "—— full output ——" + body fence`,
    truncated to fit the 4096-char embed cap with a
    `…(truncated; N more bytes)` footer when the body overflows.
  - Google Chat (`crates/copperclaw-channels/gchat/src/adapter.rs`):
    Cards v2 `collapsibleSection` (native disclosure primitive).
    Preview lines ride as uncollapsible widgets above the fold; full
    body wraps in `<font face="monospace">` for legible log/source
    rendering.
  - Matrix (`crates/copperclaw-channels/matrix/src/adapter.rs`):
    `<details><summary><em>{summary}</em></summary><pre><code>…</code></pre></details>`
    — Element renders the native disclosure widget. Plain-text body
    on the same event handles non-HTML clients.
- **Test count delta**: +25 tests
  (3 trait-default + 11 runner threshold/integration + 4 host
  dispatch + 4 telegram + 3 slack + 4 discord + 3 gchat + 3 matrix).

### Added (slice 3.2 — native `TodoList` checklist chip)

The agent's `todo_add` / `todo_update` / `todo_delete` MCP tools now emit
a structured post-mutation `TodoList` alongside their existing on-disk
persistence so adapters can render the plan as a native checklist chip
that edits in place on every mutation (and pins on platforms that
support it) instead of the legacy plain-text `todo_watcher` notification
stream.

- **New canonical `TodoList` schema**
  (`crates/copperclaw-channels/core/src/todo_list.rs`,
  `crates/copperclaw-channels/core/src/lib.rs`). Fields: `items:
  Vec<TodoListItem>` (capped at `TODO_MAX_ITEMS = 50`), `title:
  Option<String>` (≤ `TODO_MAX_TITLE_CHARS = 64`). Each item carries
  `id`, `text` (≤ `TODO_MAX_ITEM_TEXT_CHARS = 200`), and `status:
  Pending | InProgress | Completed`. `validate()` enforces non-empty
  list, unique ids, non-empty trimmed text per item; `to_text_fallback()`
  renders one line per item with status glyph + a footer counter for
  adapters without a native renderer. Helpers `is_fully_completed`,
  `pending_count`, `in_progress_count`, `completed_count`,
  `title_or_default` round out the API.
- **New `ChannelAdapter::deliver_todo_list` hook**
  (`crates/copperclaw-channels/core/src/adapter.rs`). Default impl
  converts the list to its text fallback and routes through `deliver`,
  so every adapter has a usable rendering for free. Signature carries
  `existing_message_id` (for edit-in-place) and `pin_hint` (for
  pin/unpin on platforms that support pinning).
- **New `MessageKind::TodoList`**
  (`crates/copperclaw-types/src/message.rs`) routed via
  `dispatch_todo_list` in `crates/copperclaw-host-delivery/src/service.rs`.
  Looks up the prior list row in the session via the newly-extracted
  generic `lookup_prior_kind_external_id` helper (factored out of
  `lookup_prior_breadcrumb_external_id` so both surfaces share one
  scan), threads the prior platform message id through to the adapter
  for in-place editing, and derives `pin_hint` from "first emit OR
  list just transitioned to fully-completed".
- **Per-channel native chip renderers**:
  - Telegram (`crates/copperclaw-channels/telegram/src/adapter.rs`,
    `api.rs`): MarkdownV2 `*Plan*` header, one line per item with
    `☑` / `▶` / `☐` glyph + (for completed items) `~strikethrough~`,
    `_done/total_` footer. First emit via `sendMessage` then
    `pinChatMessage` (new API call); subsequent mutations via
    `editMessageText`; `unpinChatMessage` when fully completed.
  - Slack (`crates/copperclaw-channels/slack/src/adapter.rs`,
    `api.rs`): Block Kit `header` block with title + `done/total`
    counter, one `section` block per item with status emoji + mrkdwn
    body. First emit via `chat.postMessage` then `pins.add` (new API
    call); mutations via `chat.update`; `pins.remove` when fully
    completed.
  - Discord (`crates/copperclaw-channels/discord/src/adapter.rs`,
    `rest.rs`): single embed with title + `done/total` counter,
    description rendering one line per item with `✅` / `▶️` / `⬜`
    glyphs and strikethrough on completed items. Embed color keys off
    completion state (green when fully done, yellow when in progress,
    blurple otherwise). First emit via `POST /messages` then `PUT
    /pins/...` (new REST call); mutations via the new
    `patch_message_payload`; `DELETE /pins/...` when fully completed.
    Pin permission failures are swallowed at `debug` — bots routinely
    lack `MANAGE_MESSAGES`.
  - Google Chat (`crates/copperclaw-channels/gchat/src/adapter.rs`):
    Cards v2 single-section card with a `decoratedText` widget per
    item, `startIcon` keyed off status (`CHECK_CIRCLE` /
    `CIRCLE` / `STAR`). First emit via `spaces.messages.create`,
    mutations via `spaces.messages.patch`. No public pin API on
    Google Chat — `pin_hint` is silently honoured as a no-op.
  - Matrix (`crates/copperclaw-channels/matrix/src/adapter.rs`,
    `api.rs`): `m.text` HTML event with `<h4>` title + `<ul>` list,
    status glyph prefix per item, `<s>` strikethrough on completed
    items. Mutations via the new `edit_message_html` (`m.replace`
    relation). Pin via `m.room.pinned_events` is deferred; bot
    permission requirements weren't worth the complexity for a
    decoration.
- **MCP-side emit pipeline**: new
  `OutboundToolEffect::EmitTodoList(EmitTodoListSpec)` variant
  (`crates/copperclaw-mcp/src/context.rs`) carries the canonical list;
  `crates/copperclaw-mcp/src/tools/todo.rs` invokes
  `emit_after_mutation` at the end of every `add::handle`,
  `update::handle`, and `delete::handle`, building the wire list
  from the on-disk items (with per-item text truncation to fit the
  schema cap). Empty lists are intentionally NOT emitted — no "empty
  plan" UX.
- **Runner apply path**: new `apply_emit_todo_list` in
  `crates/copperclaw-runner/src/tools.rs` mirrors `apply_send_card` —
  resolves the originating routing, forces `MessageKind::TodoList`,
  inserts the row with `content.todo_list = <canonical TodoList>`.
- **Skill prose update**: `skills/todo-tracker/SKILL.md` notes that
  todos are now rendered as a live pinned chip on supporting channels;
  agents should pick text the user will appreciate seeing.

### Added (slice-2 integration — runner wires `emit_breadcrumb_finish`)

After Agent G shipped the `Breadcrumb` shape + `deliver_breadcrumb` trait
method + per-channel native renderers (telegram/slack/discord/gchat/matrix),
the runner's tool-loop in `crates/copperclaw-runner/src/run/drive_turn.rs`
now calls `deps.tool_ctx.emit_breadcrumb_finish(...)` immediately after
every `invoke_tool` return. The chip transitions in place from Running
to Done/Failed, with the tool result's first non-empty line (char-truncated
to 200) as the summary. Without this wire-up the chip would have stayed
stuck on "Running" forever — visible UX gap closed.

New unit helper `first_line_truncated` + 3 tests pin the truncation rules.
`drive_turn` carries an `#[allow(clippy::too_many_lines)]` — the function
is the central tool-loop state machine; splitting it further would just
push locals into a struct without readability gain.

### Added (native breadcrumb chips replace plain-text tool narration)

The runner used to emit tool-progress breadcrumbs (`[shell] cargo check`,
`[edit_file] foo.rs`, …) as regular `MessageKind::Chat` rows. That bloated
the conversation and made the agent look like it was narrating itself. This
batch replaces the chat-row pipeline with a structured `Breadcrumb` shape
that adapters render as compact native chips and update in place once the
tool finishes — the Claude Code mobile-app aesthetic.

- **New canonical `Breadcrumb` schema**
  (`crates/copperclaw-channels/core/src/breadcrumb.rs`,
  `crates/copperclaw-channels/core/src/lib.rs`). Fields:
  `tool_name`, `detail: Option<String>`, `status: Running | Done | Failed`,
  `summary: Option<String>` (post-completion blurb such as
  `"passed (0.4s)"`). `validate()` enforces tight caps so the chip stays a
  one-glance UX cue on mobile. `to_text_fallback()` mirrors the legacy
  `[tool] detail` shape for adapters without a native renderer.
- **New `ChannelAdapter::deliver_breadcrumb` hook**
  (`crates/copperclaw-channels/core/src/adapter.rs`). Default impl converts
  the breadcrumb to the text fallback and routes through `deliver`, so
  every adapter has a usable rendering for free. Native renderers override
  the hook and use `existing_message_id` to drive in-place edits when
  available.
- **Per-channel native chip renderers**:
  - Telegram (`crates/copperclaw-channels/telegram/src/adapter.rs`,
    `api.rs`): HTML `<code>` chip via `sendMessage(parse_mode=HTML)`;
    in-place edit via the new `edit_message_text_with_mode` so update
    keeps HTML formatting.
  - Slack (`crates/copperclaw-channels/slack/src/adapter.rs`,
    `api.rs`): Block Kit `context` block (the platform's idiomatic
    "metadata chip") with a status emoji + inline-code mrkdwn fragment;
    in-place edit via the new `chat_update_with_blocks`.
  - Discord (`crates/copperclaw-channels/discord/src/adapter.rs`): inline
    `` `tool` `` formatting in `content`; in-place edit via the existing
    `PATCH /channels/.../messages/...`.
  - Google Chat (`crates/copperclaw-channels/gchat/src/adapter.rs`,
    `api.rs`): cards v2 single-section `decoratedText` widget with a
    `knownIcon` for the status glyph; in-place edit via the new
    `edit_card` (`spaces.messages.patch`, `updateMask=cardsV2`).
  - Matrix (`crates/copperclaw-channels/matrix/src/adapter.rs`,
    `api.rs`): `m.notice` event with HTML `<code>` body; in-place edit
    via the new `edit_message_notice_html` (`m.replace` relation).
- **New `MessageKind::Breadcrumb` variant + delivery dispatch**
  (`crates/copperclaw-types/src/message.rs`,
  `crates/copperclaw-host-delivery/src/service.rs`). The delivery service
  routes Breadcrumb-kind rows through a dedicated `dispatch_breadcrumb`
  that pulls the canonical `Breadcrumb` out of `content.breadcrumb`,
  hands it to `deliver_breadcrumb`, and falls back to a plain-text
  `deliver` if the adapter returns `Unsupported`.
- **In-place update via `update_breadcrumb` system action**
  (`crates/copperclaw-runner/src/tools.rs`,
  `crates/copperclaw-host-delivery/src/service.rs`). The runner's new
  `emit_breadcrumb_finish` (added to the `ToolContext` trait as a
  default no-op) writes a `MessageKind::System` row carrying an
  `update_breadcrumb` action. The host's delivery service intercepts the
  action inline, scans the session's recent Breadcrumb-kind rows for the
  matching `tool_name`, resolves the prior chip's platform message id
  from the `delivered` table, and re-runs `deliver_breadcrumb` with
  `existing_message_id=Some(...)` so adapters with an edit API replace
  the chip's contents in place rather than emit a fresh row.
- `db/tables/messages_{in,out}.rs` switched their `kind`-column parser
  to `MessageKind::parse_str` so adding a new variant doesn't require
  touching the SQL row reader.

Behaviour on adapters without a native override (CLI, webhooks, line,
imessage, signal, …) is unchanged — the trait-level default still emits
a `[tool] detail` text line via `deliver`. Channels without an edit API
(currently CLI / webhooks) emit a fresh chip on completion rather than
editing in place; that's visible but harmless.

### Added (native `send_card` for Slack + Discord, with round-trip button taps)

The portable `send_card` rollout shipped a canonical [`Card`] schema with a
text-fallback default impl so every adapter had a working `send_card` on
day one. Wave 2 landed a Telegram-native renderer + `callback_query`
round-trip. This batch closes the next two majors:

- **Slack — Block Kit `deliver_card`**
  (`crates/copperclaw-channels/slack/src/{api.rs,adapter.rs}`).
  `build_card_blocks()` maps `card.title` → `header`,
  `card.body` (+ optional image as section accessory) → `section` mrkdwn,
  `card.fields` → `section.fields` chunked at Slack's 10-per-section cap,
  `card.buttons` → `actions` block with `card_btn_<index>` `action_id`s.
  The `chat.postMessage` `text` parameter carries
  [`Card::to_text_fallback`] so notification surfaces (mobile previews,
  email digests, screen readers) and any future block-render downgrade
  still show a readable card body. `value` buttons receive the
  `style: "primary" | "danger"` Slack supports; other style strings
  silently degrade to default.
- **Slack — interactive `block_actions` round-trip**
  (`crates/copperclaw-channels/slack/src/events/router.rs`).
  The Events API handler now dispatches on `Content-Type`: JSON falls
  through to the existing `event_callback` path; form-encoded
  `payload=<urlencoded-json>` parses as a `block_actions` payload via
  the new `parse_block_actions()`, synthesises an inbound chat event
  whose text IS the tapped button's `value`, ACKs Slack with the
  required empty 200 within 3 s so the user's spinner clears, and
  surfaces full callback metadata (`action_id`, `block_id`,
  `message_ts`, `trigger_id`, `response_url`) under
  `content.callback`. Channel routing handles both `container.channel_id`
  (post-2020 messages) and `channel.id` (legacy / some DM shapes), and
  preserves `thread_ts` for cards that lived inside a thread. The
  webhook signature check applies to both shapes — same HMAC contract,
  no new endpoint to register.
- **Discord — embed + components `deliver_card`**
  (`crates/copperclaw-channels/discord/src/{rest.rs,adapter.rs}`).
  `build_card_payload()` maps `card.title`/`card.body` → an embed's
  `title`/`description`, `card.image_url` → `embed.image.url`,
  `card.fields` → `embed.fields[]` (with `inline` honoured),
  `card.buttons` → `components` array of `ActionRow` (`type: 1`)
  containing `Button` (`type: 2`) elements. Style mapping: `primary` →
  1, `success` → 3, `danger` → 4, anything else → 2 (default
  secondary); URL buttons override to style 5 (LINK) regardless of
  agent-supplied style. Discord's 5-button-per-row cap is honoured by
  chunking into multiple ActionRows; the 5-row platform limit can't be
  hit because the canonical card cap is 8 total buttons.
  `post_message_payload()` on `DiscordRest` ships the assembled JSON
  via `POST /channels/{id}/messages` and surfaces the message id.
- **Discord — `INTERACTION_CREATE` (`MESSAGE_COMPONENT`) round-trip**
  (`crates/copperclaw-channels/discord/src/{events.rs,adapter.rs}`).
  The gateway loop now pumps `INTERACTION_CREATE` dispatches through
  the new `interaction_create_to_inbound()` — type-3 (component) taps
  produce an `InteractionInbound { event, interaction_id,
  interaction_token }`. The adapter fires a fire-and-forget type-6
  (`DEFERRED_UPDATE_MESSAGE`) ACK via
  `DiscordRest::create_interaction_response_ack()` so the user's
  spinner clears within Discord's 3 s budget regardless of inbound-
  channel pressure. Routing mirrors the Slack pattern: the button's
  `custom_id` becomes both the synthesised chat `text` and the
  `content.callback.value`, with `original_message_id` and
  `component_type` preserved under `callback` for agents that want to
  branch.

Status by channel after this batch:
- **Telegram, Slack, Discord**: native + callback round-trip.
- **18 other channels**: text fallback via the trait default impl.

Net new tests: 34 (16 Slack + 18 Discord). Workspace clippy clean on
the touched crates; pre-existing `breadcrumb` warnings + the
`orphan_depth_cap_rejection_emits_warn` flake are untouched.

### Added (runner-side conversation-context prompt + provider-stream typing keepalive)

Two visible-to-every-channel UX gaps closed in the runner without
touching the host's typing-ticker or any channel adapter:

- `crates/copperclaw-runner/src/run/prompt.rs` — new module that renders
  a per-inbound "Conversation context: ..." paragraph (channel,
  platform, thread-vs-DM shape, batch-coalesce count, history depth,
  source-session-id when relayed from a parent agent) and splices it
  onto `RunnerDeps::system` for the duration of one provider call.
  Drives the model to address group threads differently from DMs
  instead of speaking identically in both. Only fields actually
  populated on `MessageInRow` are surfaced; `is_group` /
  `reply_to` from `InboundEvent` aren't persisted to the row yet so
  they're omitted rather than always-`None`.
- `crates/copperclaw-runner/src/run/provider_call.rs` — new
  `ProviderActivityPinger` trait (with `HeartbeatPinger` /
  `NoopPinger` impls re-exported from `copperclaw_runner`) plus a
  `ProviderActivityTicker` RAII guard that fires every ~3s while a
  provider call is in flight, *and* once per useful SSE chunk in
  `pump_events`. The production binary wires `HeartbeatPinger` so
  each ping refreshes the heartbeat file — keeping the host's
  typing-ticker willing to fire across long LLM streams (a 30s
  Anthropic response no longer lets the bubble fade out between
  chunks). Tests use a counting mock to assert the ping count climbs
  with stream-time.

`RunnerDeps` gains one new field (`activity_pinger`). The two host
integration tests that constructed it inline (`tests/e2e_chat.rs`,
`tests/replay/harness.rs`) wire `NoopPinger`. 13 new unit tests
(10 in `run::prompt`, 3 in `run::provider_call`) cover both halves.

### Added (replay-fixture coverage for slice-1 delivery behaviours)

Four new replay fixtures + supporting harness extensions pin the
slice-1 cohesive-UX baseline (chat-text splitter, adapter rate-limit
backoff). The harness now wraps each `MockAdapter` in a `CappedAdapter`
that reports a per-channel `max_message_chars` matching production
(`telegram=4096`, `slack=40000`, `discord=2000`, etc. — see
`default_cap_for` in `crates/copperclaw-host/tests/replay/harness.rs`)
and recognises two new optional manifest fields: `pre_delivery_failures`
(queue `MockAdapter::fail_next_deliver` errors before driving inbound)
and `redrive_after_ms` (sleep + re-run `process_session_once` per
session). All existing fixtures continue to pass — short-text replies
are below every per-channel cap so the splitter no-ops.

- `fixtures/telegram/long-message-split`, `fixtures/slack/long-message-split`,
  `fixtures/discord/long-message-split`: agent emits a single oversized
  chat reply (5 002 / 50 002 / 2 402 chars respectively); the delivery
  loop's splitter cuts at the paragraph boundary into exactly 2 chunks.
  Each fixture's test asserts the chunk count + per-chunk char count
  via `MockAdapter::deliveries()` on top of the JSONL diff, so a
  regression that double-splits, drops a chunk, or stops honouring the
  `\n\n` boundary surfaces directly.
- `fixtures/telegram/rate-limited-retry`: telegram adapter's first
  `deliver` returns `Rate { retry_after: 1 }`; the row is deferred,
  the harness sleeps 1 200 ms (past the 1 s `retry_after` window) and
  re-drives the session; the second pass succeeds. The test asserts
  exactly ONE successful adapter delivery (the deferred attempt does
  not register) and that elapsed wall time is >= 1 s — implicitly
  pinning that `bump_retry` honoured the adapter's `retry_after` over
  the default 5 s exponential schedule.

The `telegram/webhook-secret-rejected` scenario the parent agent
listed turned out to be impossible against the current harness: the
`direct` replay mode pushes already-parsed `InboundEvent`s at the
router, skipping the webhook secret check entirely. Surfaced in
`docs/replay-fixtures.md`'s "What the suite does not cover" section
alongside the other transport-layer gaps; the secret-compare itself
is exercised by unit tests in the telegram and whatsapp-cloud crates.

### Added (`cclaw approvals approve-id <id>` and `cclaw approvals deny <id>` — generic per-family approval write surface)

Until now only `Sender` approvals had a CLI write path
(`cclaw approvals approve --channel <ct> --identity <id>`); the other
families (Channel, InstallPackages, AddMcpServer) piled up as rows in
`pending_approvals` and the operator had to hand-CRUD them via the
central DB. The new generic verbs close that gap:

- `cclaw approvals approve-id <id>` (wire: `approvals.approve`) — looks
  up the row, dispatches on the `action` column, applies the per-family
  side effect, then marks the row `status = 'approved'`. Re-approving
  an already-approved row is a no-op (`applied: false`,
  `reason: "already_approved"`). Approving a denied/expired row is
  `conflict`.
- `cclaw approvals deny <id>` (wire: `approvals.deny`) — marks the row
  `status = 'denied'` without applying any side effect. Idempotent;
  denying an already-approved row is `conflict`.

Per-family dispatch arms in
`crates/copperclaw-host/src/handlers/approvals.rs`:

- `action = "sender"` | `"approve_sender"` — upsert into `users` by
  `(channel_type, platform_id)` from the row's columns. Display name
  is read from `payload.display_name`.
- `action = "channel"` — upsert a `messaging_groups` row by
  `(channel_type, platform_id)`. Optional `name`, `is_group`,
  `unknown_sender_policy` from `payload`. No auto-wiring (a separate
  operator decision via `cclaw wirings create`); the response includes
  a `wiring_hint` with the exact follow-up command.
- `action = "install_packages"` — read `payload.apt[]` /
  `payload.npm[]`, merge into the affected group's
  `container_configs.packages_apt` / `packages_npm`. Does NOT
  auto-rebuild; the response includes a `rebuild_hint` so the operator
  knows to run `cclaw groups restart <ag_id>`.
- `action = "add_mcp_server"` — read `payload.{name, transport}`,
  insert into `container_configs.mcp_servers` (replacing any entry
  with the same name). Same no-auto-rebuild stance + `rebuild_hint`.

Both verbs are registered as host-only commands in
`crates/copperclaw-host/src/handlers/mod.rs::HOST_ONLY_COMMANDS`, which
auto-wires them into the audit log (the socket dispatcher writes an
`audit_log` row for every host-only command, success or error). 19
new unit tests cover each family's happy path, the idempotency
contract, conflict-on-status-reversal, missing fields, and unknown
actions; a new socket-level dispatch test in
`crates/copperclaw-host/src/socket.rs` confirms the audit row lands.

Files changed:
- `crates/copperclaw-cclaw/src/commands.rs` — new `ApprovalsCmd::ApproveById`
  + `ApprovalsCmd::Deny` variants, `to_call` arms, `ALL_COMMANDS` entries.
- `crates/copperclaw-host/src/handlers/approvals.rs` — `approve` /
  `deny` handlers + four per-family appliers (`apply_sender`,
  `apply_channel`, `apply_install_packages`, `apply_add_mcp_server`)
  + a local `ensure_config_row` helper mirroring the one in
  `handlers::groups` (no cross-module dep).
- `crates/copperclaw-host/src/handlers/mod.rs` — `approvals.approve` /
  `approvals.deny` added to `HOST_ONLY_COMMANDS`.
- `crates/copperclaw-host/src/socket.rs` — dispatch-table registration +
  the integration test.

### Fixed (`CreateAgentModule` no longer leaks one entry per ever-spawned agent group)

`CreateAgentModule` carried an `Arc<Mutex<HashMap<AgentGroupId, u8>>>`
"spawned" cache as a write-through accelerator for the subagent-depth
gate. On a long-running host with many short-lived agent groups the
map grew without bound (one entry per ever-spawned group), and it
returned stale depths when an `AgentGroupId` was deleted and a later
group reused the slot.

`crates/copperclaw-modules/src/agent_to_agent/create_agent.rs` now reads
depth straight from `agent_groups.subagent_depth` on every
`create_agent` call. The DB is the canonical source so this is also
the correctness fix: id reuse and ad-hoc admin resets of the depth
column are observed immediately, not after a host restart. The
TOCTOU re-check around the central-DB insert is preserved via a
process-wide `Arc<Mutex<()>>` (`depth_gate`) — `create_agent` is
operator-driven, not the message hot path, so the extra SELECT and
the single coarse mutex are irrelevant to throughput.

Two new tests pin the bounded-memory invariant:

- `lookup_parent_depth_does_not_grow_per_agent_group` runs 10 000
  distinct group ids through the lookup and asserts the handler's
  only synchronisation field is the `()`-payload mutex (caught at
  compile time via a type-annotated binding).
- `lookup_parent_depth_does_not_return_stale_on_depth_reset` resets
  a parent's persisted depth and asserts the handler observes the
  new value, not a cached one.

Tests reseeding parent depth previously poked the cache directly;
they now seed via `agent_groups::set_subagent_depth` so they exercise
the same DB path the production gate hits. Total tests in
`copperclaw-modules` go from 205 to 207.

### Added (`reply_to` populated from the wire across 7 channels)

Slice-2 continuation. The `InboundEvent.reply_to: Option<ReplyTo>` field
has existed since slice 1, but every channel adapter was hardcoding it
to `None`. Now seven channels populate it from the wire payload when the
platform tells us a message is a reply, so the agent (and any downstream
threading logic) can stitch replies back to the parent message:

- **Telegram** (`crates/copperclaw-channels/telegram/src/ingress/mod.rs`):
  from `message.reply_to_message.message_id`. Required adding
  `reply_to_message: Option<Box<Message>>` to the local `Message` type
  (`crates/copperclaw-channels/telegram/src/types.rs`) plus the matching
  `None` in the `api.rs::empty_message` constructor.
- **Slack** (`crates/copperclaw-channels/slack/src/events/router.rs`):
  from `thread_ts` when it differs from the message's own `ts` (the
  equality case is the thread root, which is NOT a reply).
- **Discord** (`crates/copperclaw-channels/discord/src/events.rs`):
  from `message_reference.message_id`. The existing `thread_id` mirror
  is kept (Discord callers rely on it); `reply_to` is the cleaner
  semantic.
- **Matrix** (`crates/copperclaw-channels/matrix/src/parse.rs`):
  from `content."m.relates_to"."m.in_reply_to".event_id`. Independent
  of the existing `m.thread` → `thread_id` extraction.
- **Teams** (`crates/copperclaw-channels/teams/src/events/router.rs`):
  from `replyToId` on the fetched Graph message body.
- **Signal** (`crates/copperclaw-channels/signal/src/parse.rs`):
  from `dataMessage.quote.id` (the quoted message's millisecond
  timestamp, which is exactly our `message.id` format).
- **WhatsApp Cloud**
  (`crates/copperclaw-channels/whatsapp-cloud/src/events/router.rs`):
  from `messages[].context.message_id`.

Each channel got 2 new unit tests (happy path + negative), 14 total.
Channels left at `reply_to = None`: Google Chat (the wire payload
doesn't carry a per-message reply id; `thread` is the only stitching
signal and that's already on `thread_id`); iMessage (the inbound
`MockMessageRow` doesn't carry `associated_message_guid` and the
bridge file was out of scope for this slice); webhook-only / DM-only
channels (line, x, etc.) where the platform doesn't expose the signal.

### Added (cohesive cross-channel UX baseline — slice 1)

Three contract changes on the `ChannelAdapter` trait + delivery loop so
every channel benefits at once instead of fixing the same UX bug 21
times. These are the foundations for the parallel slice-2 polish work
that follows.

- **`max_message_chars()` on `ChannelAdapter`** with a chat-text splitter
  in the delivery loop (`crates/copperclaw-host-delivery/src/service.rs`).
  When an adapter advertises a per-message char cap, oversized outbound
  chat rows are split (paragraph → sentence → hard cut) into a sequence
  of sends before they hit the platform API, eliminating silent
  "message too long" 400 failures. Char-based (not byte-based) so
  CJK content rounds the right way. Per-channel caps shipped: Telegram
  4096, Discord 2000, Slack 40 000, gchat 4096, Teams 28 000,
  whatsapp-cloud 4096, wechat 600 (conservative under-approximation of
  the 2 KiB byte cap), webex 7439, line 5000. New metric
  `copperclaw_delivery_chat_split_total{channel_type}` fires once per
  split row. 6 new unit tests + the per-channel overrides are exercised
  by the existing adapter test suites.
- **Honour adapter `Rate { retry_after }` hints** in
  `DeliveryService::bump_retry`. Previously the delivery loop always
  used a fixed exponential schedule (5 s × 2^(tries-1)) regardless of
  what Telegram / Slack / GitHub / Linear / Webex etc. had told us via
  `Retry-After`. Now the platform-supplied wait wins (capped at
  `ABSOLUTE_CEILING_MS`), falling back to the exponential schedule only
  when no hint is present. New `DeliveryError::retry_after_secs()`
  accessor; 2 new tests pinning both paths.
- **Constant-time webhook secret comparison** for Telegram
  (`crates/copperclaw-channels/telegram/src/ingress/webhook.rs`) and
  whatsapp-cloud
  (`crates/copperclaw-channels/whatsapp-cloud/src/events/router.rs`).
  Both previously used a plain `!=` byte compare on bearer-token-shaped
  inputs, which leaks the secret one char at a time via response
  timing. Now use `subtle::ConstantTimeEq`. Other webhook channels
  (Slack, GitHub, Linear, Teams, gchat, Webex) were already constant-
  time and unchanged. `subtle = "2"` added to the Telegram crate's
  dependencies.

Workspace: 5410 passing / 1 pre-existing flake
(`agent_to_agent::create_agent::tests::orphan_depth_cap_rejection_emits_warn`,
passes in isolation — global tracing-subscriber buffer race, untouched
by this slice). Clippy clean on
`cargo clippy --workspace --all-targets -- -D warnings`.

### Added (portable `send_card` — works on every channel)

The user-visible goal: `send_card` works on every channel. The mechanism:
one canonical Card schema (`title`/`body`/`fields`/`buttons`/`image_url`),
rendered natively where the adapter has card support and degraded to
formatted text everywhere else — so no channel gets left behind.

Shipped in three waves, all in this batch:

- **Wave 1 — foundation** (`crates/copperclaw-channels/core/src/card.rs`):
  canonical `Card`, `CardField`, `CardButton`, `CardError` types with
  `Card::validate()` + `to_text_fallback()`. New `MessageKind::Card`
  variant. New trait method `ChannelAdapter::deliver_card()` with a
  default impl that renders to text and dispatches through `deliver()`
  — every existing adapter gets a working `send_card` for free.
- **Wave 2a — production path**: see the dedicated entry below.
- **Wave 2b — Telegram native**: full `deliver_card` override using
  MarkdownV2 + `reply_markup.inline_keyboard`. `value` buttons produce
  `callback_data`; URL buttons open links. Image cards send via
  `sendPhoto` (caption + keyboard); long captions split to
  photo + follow-up text+keyboard. Inbound `callback_query` handling
  synthesises a chat event whose text is the button's `value` and ACKs
  the callback so the spinner stops — the agent receives the tap as if
  the user typed the value. Buttons wrap at 3 per row to avoid label
  truncation on phones. 27 new tests.
- **Wave 2c — skill docs** (`skills/send-card/SKILL.md`): rewritten
  honestly. Previous version claimed per-channel shapes that didn't
  exist; new version describes the canonical schema, validation rules,
  callback flow, and per-channel rendering table.

Status by channel after this batch:
- **Telegram**: native (inline_keyboard + callbacks).
- **20 other channels**: text fallback via the trait default impl. Native
  impls (Slack Block Kit, Discord embeds, Teams adaptive cards, etc.)
  can land as follow-ups without touching anything else — the foundation
  is in place.

Workspace: 5400 passing; clippy clean.

### Changed (cards rollout wave 2a — production path)

Wave 1 added the canonical portable `Card` schema in
`copperclaw-channels-core`, the `MessageKind::Card` variant, and the
trait-level `ChannelAdapter::deliver_card()` with a text-fallback
default impl. Wave 2a wires the production path:

- `send_card` MCP tool (`crates/copperclaw-mcp/src/tools/interactive.rs`)
  rewritten to accept the canonical `Card` schema directly. JSON
  schema now documents `title`/`body`/`fields`/`buttons`/`image_url`
  with the right types; `Card::validate()` runs at the MCP boundary
  so the model gets a precise error and the runner never touches an
  invalid card. Tool description updated to explain the portability
  story ("Portable card schema — works on every channel. Channels
  with native card support render the structure; channels without it
  fall back to formatted text.").
- `SendCardSpec` (`crates/copperclaw-mcp/src/context.rs`) now carries a
  typed `copperclaw_channels_core::Card` instead of an opaque
  `serde_json::Value`. Cards travel through the runner with their
  schema preserved.
- `apply_send_card` (`crates/copperclaw-runner/src/tools.rs`) now writes
  a `MessageKind::Card` row to `messages_out` (NOT a `MessageKind::System`
  action). Row content shape: `{ "card": <Card JSON>, "to": <Recipient> }`
  with `to` present only when the caller passed an explicit recipient.
  Channel routing (`channel_type` / `platform_id` / `thread_id`) is
  inherited from the originating inbound exactly the way `send_message`
  does. The old System-routing-and-action-handler indirection
  (`"send_card"` action key on a System row) is gone — the previous
  flow only wrapped the opaque blob and forwarded it, so removing it
  doesn't change any channel adapter's contract.
- Host delivery service (`crates/copperclaw-host-delivery/src/service.rs`)
  picks up `MessageKind::Card` rows in a dedicated `dispatch_card`
  branch: deserialise `content.card` back into `Card`, pull the
  optional `content.to` hint, call `adapter.deliver_card(platform_id,
  thread_id, &card, to)`. If the adapter explicitly returns
  `AdapterError::Unsupported`, the host falls back to a plain
  `deliver` call with the text rendering. (The trait-level default
  already does the text fallback, so this only fires for adapters
  that deliberately overrode `deliver_card` to refuse cards entirely.)
  Malformed `content.card` JSON is treated as a host-level bug and
  recorded `failed` rather than retried.
- `"card"` added to the kind-string `parse_str` arms in
  `messages_in.rs`, `messages_out.rs`, `outbound_dropped_messages.rs`,
  and `recurrence.rs`. Card rows now read back correctly from every
  per-session DB and central dropped-message table.
- `runner_emit_set()` in
  `crates/copperclaw-host/tests/action_handler_coverage.rs` updated:
  `send_card` removed from the System-action set (the runner no
  longer emits it as a System action; the structural test would
  have failed otherwise).

Tests: 3 new tests on the runner side (Card-kind row contents,
explicit-`to` propagation), 4 new tests on the MCP-tool side (canonical
schema validation), 3 new tests on the host-delivery side (deliver_card
invocation, Unsupported fallback, malformed-card guard). Existing
opaque-card test in `send_card_writes_system_row` rewritten as
`send_card_writes_card_kind_row` to assert the new contract. Wave 1's
17 unit tests on the `Card` schema continue to pass.

### Added (4 new tools to reduce LLM round-trips for common patterns)

Profiling the live failure modes (CapCut, base64 loop) surfaced the same
underlying issue across many places: the model is doing work the host
could do directly, burning tokens and round-trips. These four tools
close the biggest gaps:

- **`multi_edit`** (`crates/copperclaw-mcp/src/tools/multi_edit.rs`): apply
  N find-replaces to one file in a single call. Replaces the pattern
  of 5 sequential `edit_file` calls, each re-emitting overlapping
  surrounding context as `old_string`. Atomic — if any edit fails, the
  whole call rolls back. 50-edit hard cap. Later edits see earlier
  edits applied. 5-10× reduction on multi-edit refactor sessions.
- **`apply_patch`** (`crates/copperclaw-mcp/src/tools/apply_patch.rs`):
  apply a unified diff to one file. For multi-region edits this is
  3-10× more compact than the equivalent `edit_file` sequence — the
  model writes a small diff instead of repeating overlapping
  `old_string` context. Hand-rolled parser (no new crate deps).
  Exact-context required, atomic on any hunk mismatch.
- **`copy_file`** (`crates/copperclaw-mcp/src/tools/copy_file.rs`):
  filesystem-level copy that doesn't round-trip the bytes through the
  LLM. Replaces the `read_file(src)` + `write_file(dst, content=...)`
  pattern that previously moved every byte through the model's
  context twice. 32 MB hard ceiling. Optional `create_parents` and
  `overwrite` flags. Binary-safe.
- **`read_file` extended with `offset` / `limit` / `mode`**
  (`crates/copperclaw-mcp/src/tools/computer_use.rs`): the existing
  `read_file` tool now accepts a byte or line range. Lets the model
  read precise regions of large files instead of pulling the whole
  thing (or getting the truncated head). `mode: "lines"` is 1-indexed;
  `mode: "bytes"` is 0-indexed. Out-of-range offsets return empty
  body, not an error. Backward-compatible — calls without
  offset/limit behave exactly as before.

Tool-breadcrumb detail extractors extended to cover the new tools
(`[multi_edit] src/main.rs`, `[apply_patch] src/lib.rs`,
`[copy_file] src/template.html → src/page.html`).

48 new tests across the four tools. Workspace: 5334 passing; clippy
clean.

### Added (tool breadcrumbs now include input details)

Previously the user-visible chat breadcrumbs were just `[tool_name]`
("[shell]", "[web_search]") — enough to know the agent was working
but useless for "what's it actually doing?". Now they include a short
per-tool detail extracted from the model's input JSON:

- `[shell] cargo test --workspace`
- `[web_search] AI biotech news May 2026`
- `[web_fetch] https://apps.apple.com/charts`
- `[write_file] src/main.rs`
- `[read_file] /data/Cargo.toml`
- `[grep] use\s+anyhow`
- `[install_packages] jq, ripgrep, typescript`
- `[create_agent] Biotech News Researcher`

Implementation in `crates/copperclaw-runner/src/tools.rs`:

- New `breadcrumb_detail(name, input) -> Option<String>` formatter.
  Per-tool field extraction (`command` for shell, `query` for
  web_search/explore, `url` for web_fetch, `path` for file ops,
  `pattern` for grep/glob, etc.). Strings are capped at 80 chars
  with an ellipsis suffix and newlines collapsed to single spaces
  so the breadcrumb stays one line on mobile clients. Returns
  `None` for unknown tools or missing fields — caller falls back
  to the old bare `[tool_name]` form.
- Allowlist (`is_visible_breadcrumb_tool`) expanded to include
  `read_file`, `grep`, `glob` alongside the existing shell /
  web_search / web_fetch / file-write / etc. set.

Plumbing in `crates/copperclaw-mcp/src/context.rs` +
`crates/copperclaw-runner/src/run/provider_call.rs`:

- `ToolContext::emit_breadcrumb` signature gained an
  `input: Option<&serde_json::Value>` parameter. Default trait
  impl stays a no-op.
- Breadcrumb emission moved from `ProviderEvent::ToolStart` (no
  input available yet — the streamed deltas haven't been
  reassembled) to `ProviderEvent::ToolCall` (full input ready
  to dispatch). Tiny timing change (≤500ms) but worth it for the
  much richer UX.

8 new unit tests covering each tool's detail format, truncation,
newline collapsing, and the missing-field fallback.

### Changed (tool-result efficiency — search + fetch + shell + read_file)

Profiling the live failure mode showed one `web_fetch` of
apps.apple.com/charts dumped 344KB into conversation history (88% of
the 391KB total). The bloat made Sonnet emit malformed JSON, which
hit the 3-strikes parse cap, which crashed the runner via a separate
processing_ack bug — see the runner-death fix below.

Root issue: tool results live in conversation history forever until
compaction fires (at ~180k tokens). One verbose tool call can push
the agent into context pressure where models start producing
truncated JSON. Aggressive caps are correctness, not just
optimization.

Two parallel agents on disjoint file scopes:

- **`crates/copperclaw-mcp/src/tools/web_search.rs`**:
  - `DEFAULT_MAX_RESULTS` 10 → 5. Models can still ask for more via
    the `max_results` arg (ceiling stays 25).
  - `SNIPPET_CAP_BYTES` 4096 → 400. A 400-char snippet is enough to
    judge relevance and decide whether to pivot to `web_fetch`.
  - Net: a typical 4-search session drops from ~35 KB → ~8 KB of
    snippet bloat.

- **`crates/copperclaw-mcp/src/tools/computer_use.rs`** (web_fetch /
  shell / read_file all live here):
  - `WEB_FETCH_CAP` 256 KB → 32 KB. Markdown-extracted content of
    a typical page fits in 32 KB; pages that need more depth are
    better served by a second targeted fetch.
  - `web_fetch` response: dropped the entire `headers` map. Apple's
    CSP header alone was 30+ KB and the model rarely needs response
    headers. `content_type` is now a top-level scalar (sourced from
    the original `Content-Type` header) alongside `status` and
    `size_bytes`. If a future user wants headers they can `shell`
    `curl -I`.
  - `SHELL_OUTPUT_CAP` 64 KB → 32 KB per stream. The truncation hint
    now reads "narrow with tail/head/grep before re-running" so the
    model knows the next move.
  - `READ_FILE_CAP` 1 MB → 128 KB. Most source files fit; larger
    reads should use offset/limit.
  - New regression tests:
    `web_fetch_omits_headers_map_and_surfaces_content_type_scalar`
    and `web_fetch_caps_body_at_32k`.

For the specific failure mode we just debugged: the same 344 KB
fetch would now produce ~32 KB (10.5× reduction). Four similar
fetches in a session would stay under 130 KB — well under the
threshold where Sonnet starts emitting malformed JSON.

Verification: cargo test --workspace --no-fail-fast = 5277 passed
(4 new tests); clippy clean. One pre-existing parallel-test flake
(ETXTBSY on editor.sh) — unrelated and tracked separately.

### Fixed (the actual runner-death root cause: processing_ack NotFound aborted the runner mid-cleanup)

The new `crash-<rfc3339>.log` capture from the previous commit paid off
on the very first crash and revealed the real death mechanism:

```
2026-05-23T23:54:54  ERROR 3 consecutive tool_use parse failures; bailing attempts=3
Error: not found
```

Sequence:
1. The model emitted malformed `write_file` JSON three turns in a
   row (38-byte truncated input each time — model degradation at
   high context).
2. The 3-strikes parse-error cap correctly fired
   `TurnOutcome::Failed`.
3. `finalize_messages` ran. `mark_failed` on the inbound succeeded.
4. `processing_ack::update_status(row.id, Failed)` returned
   `DbError::NotFound` because the host's
   `host_sweep::checks::processing` had already cleared the ack row
   (its CLAIM_STUCK_MS reset deletes the ack as part of the reset
   path).
5. The `?` propagated up out of `finalize_messages` → out of
   `run_loop` → out of `main()`. The runner process exited
   with `Error: not found`. The container died. The user got
   nothing — the apology emit that lives BELOW the ack update never
   ran.

Fix in `crates/copperclaw-runner/src/run/mod.rs`:

- `finalize_messages`: tolerate `DbError::NotFound` from
  `processing_ack::update_status` (the row legitimately disappeared
  between pickup and finalize when the host swept it). Other errors
  are demoted from `?` to `tracing::warn!`. The terminal-failure
  apology path now runs unconditionally regardless of ack
  housekeeping.
- `ack_picked_up`: same treatment — a missing-or-broken
  `processing_ack` row at pickup time logs a `warn!` and continues
  rather than aborting the runner. The actual inbound processing is
  what matters; ack tracking is best-effort housekeeping.

This is the bug that produced the symptom the user was debugging:
silent runner death mid-message, no apology, no chat update, just
heartbeat-stale 7 minutes later.

### Fixed (root-cause batch: silent crash + lost progress on restart)

A Telegram retest showed the agent building a CapCut clone, then going
silent mid-build, then forgetting everything on respawn. Three
independent architectural bugs combined:

1. **Container heartbeat went stale → host removed the container →
   no chat apology for 5 minutes** (until `host_sweep::apology`
   PendingTooLong fired). From the user's view: typing indicator, then
   silence, then a generic "I'm having trouble" five minutes later.
2. **Runner crashed mid-message → all in-memory tool turns lost.**
   `save_state` only persisted history + continuation ONCE per
   inbound, AFTER `drive_turn` returned. A long multi-tool message
   (11 tool turns + 5 file writes in this case) kept everything in
   memory; the crash erased it. Respawned runner saw only the
   pre-message history and replied "Nothing went wrong — I'm just
   waiting on your pick".
3. **No diagnostic capture before container removal.** The
   `CrashRestart` path called `runtime.remove(...)` before reading the
   container's logs — by the time we wanted to debug, the evidence was
   gone.

Three parallel fixes, each on disjoint file scope:

- **`crates/copperclaw-host/src/container_manager/classify.rs`**: the
  `CrashRestart` action now (a) captures the last 200 lines of the
  container's stdout/stderr to `<session_root>/crash-<rfc3339>.log`
  BEFORE removing the container, (b) scans `processing_ack` for
  in-flight `Processing` claims and emits a chat apology
  ("Hit a snag mid-task and need to restart the agent container.
  Some progress may have been lost. I'll pick back up — try sending
  a follow-up if I don't continue on my own.") per row with chat
  routing, (c) marks each emitted claim `Failed` and the corresponding
  inbound `tries = APOLOGY_TRIES_MARKER (99)` so the host-sweep paths
  don't double-fire. Idempotent across reconciler ticks. New trait
  method `ContainerRuntime::logs(name, tail) -> Result<String>` with
  a default empty-string impl; only `DockerRuntime` overrides it
  (bollard `LogsOptions{tail, stdout, stderr}`). 3 new unit tests.
- **`crates/copperclaw-runner/src/run/{mod,drive_turn}.rs`**: `drive_turn`
  now calls `save_state` AFTER each tool-turn iteration (not just at
  end of message) via a `persist_mid_message` helper. A mid-message
  crash now preserves the assistant + tool_use + tool_result history
  on disk. The run-loop additionally guards against duplicate user
  pushes on resume — if `state.history.last()` is already a User
  message with the same content as `formatted.prompt`, it logs a
  debug line ("resuming mid-message — skipping duplicate user push")
  and skips the push. Two regression tests cover both halves.
- **`crates/copperclaw-host/src/container_manager/spawn.rs` +
  `boot.rs`**: raised `DEFAULT_HEARTBEAT_STALE_SECS` 60 → 120, added
  startup safety check `check_heartbeat_deadline_alignment` (warns
  when `heartbeat_stale_secs < 2 * provider_deadline_secs`), cross-
  referenced the constants. See the "Changed
  (heartbeat-vs-provider-deadline race hardening)" entry below for
  details. (Promoted `copperclaw-runner` from dev-dep to runtime dep
  in `crates/copperclaw-host/Cargo.toml` to read the runner's effective
  provider deadline from `resolve_provider_deadline(&SystemEnv)` at
  boot.)

Verification: cargo test --workspace --no-fail-fast = 5276 passed (10
new tests); clippy clean. Live retest pending.

### Changed (heartbeat-vs-provider-deadline race hardening)

The host's `DEFAULT_HEARTBEAT_STALE_SECS` and the runner's
`DEFAULT_PROVIDER_DEADLINE_MS` previously defaulted to the same 60s
value, which exposed a small but real race: when the runner's
`HeartbeatTicker` had any latency dropping its last touch (it fires
every 5s; a fully-blocked provider attempt can let mtime drift
~5s in the past), the host could mark the container stale and
SIGKILL it the same instant `provider.query()` returned
`Err(DeadlineExceeded)` — losing the work and triggering a respawn
loop on slow Sonnet calls.

- `crates/copperclaw-host/src/container_manager/spawn.rs`:
  `DEFAULT_HEARTBEAT_STALE_SECS` raised from `60` → `120` so the host
  always gives the runner at least the full provider budget plus a
  turn-worth of margin to fail cleanly before declaring the container
  dead. Doc comment now cross-references the runner-side default.
- `crates/copperclaw-host/src/container_manager/spawn.rs`: new free
  function `check_heartbeat_deadline_alignment(heartbeat_stale_secs,
  provider_deadline_ms)` returns `Err(String)` when the host's stale
  threshold is `< 2 * (provider_deadline / 1000)`. Boundary cases
  (sub-second deadlines, `u64::MAX` extremes) are handled via
  `div_ceil` + `saturating_mul`. Called from
  `boot.rs::spawn_container_manager` once at host startup; on misalignment
  the boot path emits a `warn!` line naming both values and the
  required minimum, then continues. Operators can still pin a tighter
  pair deliberately — the check warns, it does not panic.
- `crates/copperclaw-host/src/boot.rs`: startup safety check reads the
  operator-supplied `COPPERCLAW_RUNNER_PROVIDER_DEADLINE_MS` via
  `copperclaw_runner::resolve_provider_deadline` so the warn line
  reflects the value the runner will actually be configured with at
  spawn (not just the compiled-in default).
- `crates/copperclaw-host/Cargo.toml`: `copperclaw-runner` promoted from
  `dev-dependencies` to `dependencies`. Used only at boot to resolve
  the configured provider deadline — no runtime coupling to the
  poll loop.
- `crates/copperclaw-runner/src/run/mod.rs`: doc comment on
  `DEFAULT_PROVIDER_DEADLINE_MS` now explains the 2x relationship
  with the host's stale threshold and points future contributors at
  the host-side check.
- `crates/copperclaw-host/src/container_manager/classify.rs`: the
  `classify_running_with_stale_heartbeat_is_crash_restart` test
  backdates the heartbeat by 240s (was 120s) so it sits comfortably
  past the new 120s default with margin for test wall-clock jitter
  instead of right on the boundary. Updated a related comment to
  show the new "120s crash, 300s idle" defaults.

Tests: 5 new tests in `container_manager::spawn::tests` —
`defaults_satisfy_heartbeat_deadline_alignment` (shipped defaults
satisfy the check), `alignment_check_passes_at_exact_2x_boundary`,
`alignment_check_warns_when_heartbeat_lt_2x_deadline` (regression
guard for the original 60s/60s misconfiguration),
`alignment_check_ceils_sub_second_deadlines`,
`alignment_check_does_not_overflow_on_large_values`.

### Fixed (code-review followup — 15 findings)

Extra-high-effort code review on this session's commits surfaced 15 real
issues; this batch fixes all of them. Grouped by file:

`rebuild.sh`:
- **Critical**: the new image-tag-repoint UPDATE wrote
  `updated_at=datetime('now')` (sqlite default format, no T separator,
  no timezone). `DateTime::parse_from_rfc3339` requires both — chrono
  returns `premature end of input`. Confirmed live: `cclaw groups
  config get` was already erroring against the post-rebuild DB.
  Replaced with `strftime('%Y-%m-%dT%H:%M:%fZ','now')` and added a
  comment naming the bug shape.
- `stale_count` now validated against `^[0-9]+$` before the bash
  arithmetic; a non-numeric stdout no longer aborts the rebuild under
  `set -euo pipefail`.
- `$new_tag` validated against `^[A-Za-z0-9._:/-]+$` before the SQL
  interpolation — guards against future tag schemes containing quote
  chars that would silently break the UPDATE.
- When `sqlite3` is missing, the repoint now emits a `warn` telling
  the operator to install sqlite3 or run `cclaw groups config update
  <id> image_tag <tag>` manually, instead of silently no-op'ing.

`crates/copperclaw-runner/src/run/{mod,drive_turn,provider_call}.rs`:
- `compact_now` sentinel branch now runs `compact()` BEFORE removing
  the sentinel file, so a transient provider failure during
  summarisation doesn't silently drop the user's compaction request.
- `compact_now` branch resets `state.continuation = None` to match the
  `clear_history` branch — a provider continuation handle anchored to
  the pre-compact history is incompatible with the new (shorter)
  history.
- When BOTH `.history_clear_pending` and `.compact_now_pending` exist,
  the clear branch now also removes the compact sentinel (previously
  it leaked, causing a no-op LLM compaction call on the next iteration).
- Apology emitter now uses two distinct texts: a human-style message
  for end-user channels (Chat-kind) and a terse machine-actionable
  message for parent-agent reports (Agent-kind). Previously the parent
  LLM received "Try rephrasing... operator can check the runner log"
  which it couldn't act on.
- `resolve_max_tool_turns` warns once per process via `OnceLock`
  instead of once per misconfigured spawn, eliminating log flooding
  from a sticky `COPPERCLAW_MAX_TOOL_TURNS=6o`-style typo.
- `apology_text` trims trailing `.`/`?`/`!` from the reason before
  splicing so future contributors writing natural-English reasons
  ending in punctuation don't produce visible double-punctuation.
- `LlmTurnOutput` grew a `failure_reason: String` field. `provider_call`
  now emits specific reasons at the two failure sites ("provider
  rejected the query before streaming started" vs "provider stream
  ended with an error event") instead of a blank-string sentinel that
  was structurally indistinguishable from a real-but-empty reason.
  `drive_turn` preserves the inner reason in `TurnOutcome::Failed`
  when non-empty.

`crates/copperclaw-host/src/typing_ticker.rs` +
`crates/copperclaw-db/src/tables/messages_in.rs`:
- New `messages_in::count_pending_for_typing` — no `trigger = 1`
  filter, so the ticker now pulses typing during turns processing
  agent-dispatch, Task-wake, or system inbounds (the original
  `count_due` had the trigger filter and stayed dark during those
  turns).
- `row_to_message_in` coalesces `source_session_id = Some("")` to
  `None`, completing the empty-string defence pass from the previous
  commit (sessions.rs was fixed; the matching messages_in path was
  missed — would have caused the parent-agent apology to silently
  drop on legacy rows).
- `TypingTicker` gained a per-instance `last_seen_pending` cache that
  short-circuits inbound.db reopens within a 2x-tick window for
  continuously-busy sessions; idle sessions are evicted on the next
  count-zero. Drops steady-state sqlite open churn from O(sessions)
  per tick to O(idle-transitions) per tick.
- Transient inbound.db open errors are now logged at `debug!` (with
  session id + error) instead of silently swallowed; the return-false
  fallback is unchanged.

Replay fixture `fixtures/cli/provider-timeout/expected/*.jsonl` updated
to match the new specific-reason apology text.

Verification: cargo test --workspace --no-fail-fast = 5266 passed (8
new tests across the three fixes); clippy clean.

### Fixed (`Some("")` crashed DB row parsers; reconciler hot-looped forever)

The session reconciler in `container_manager` started spinning at one
ERROR-per-second per session with `FromSqlConversionFailure(0, Text,
ParseError(TooShort))` after a session had run for a while.

Root cause: several DB row decoders use the pattern

```rust
let opt: Option<String> = row.get(col)?;
opt.as_deref().map(|s| Parse::parse(s)).transpose()?
```

which treats `Some("")` as a parse target. Adapters and runner code
sometimes write empty strings into optional UUID / datetime columns
instead of NULL (the worst offender observed live: `container_state.
tool_started_at = ''` left over from an aborted tool turn), and the
chrono / uuid parsers both return `ParseError(TooShort)` on the empty
string. The reconciler then read the row every tick, failed to parse,
retried, and never made progress — wedging the session until the
operator intervened.

Fix: every optional UUID / datetime column decoder now treats `Some("")`
identically to `None`. Touched:

- `crates/copperclaw-db/src/tables/container_state.rs` —
  `tool_started_at`, `updated_at` (and `current_tool` collapsed via
  `Option::filter`).
- `crates/copperclaw-db/src/tables/messages_in.rs` — `process_after`
  via the shared `parse_dt_opt` helper.
- `crates/copperclaw-db/src/tables/messages_out.rs` — `deliver_after`.
- `crates/copperclaw-db/src/tables/tasks.rs` — `next_fire`.
- `crates/copperclaw-db/src/tables/sessions.rs` — `messaging_group_id`,
  `source_session_id`.

Each site got a short comment naming the actual failure mode so the
next reader doesn't undo the defence thinking it's redundant.

### Fixed (rebuild.sh left existing groups pinned to the old image)

Caught when a fresh rebuild visibly shipped the new runner binary at
`~/.local/bin/copperclaw-runner`, the host log confirmed a new image was
baked, and `COPPERCLAW_DEFAULT_IMAGE_TAG` in `.env` was repointed — yet
the running session container kept spawning with the old image hash and
the agent kept emitting the old "I hit a snag … see runner stderr"
apology that the new runner code no longer contains.

Root cause: `container_configs.image_tag` (central DB) is pinned
per-agent-group. `.env`'s `COPPERCLAW_DEFAULT_IMAGE_TAG` is only consulted
when *creating* a new group; existing rows retain whatever image tag
was pinned at first spawn. So `rebuild.sh` was leaving every existing
agent group running the previous baked image forever.

Fix: extended `rebuild.sh`'s pin step to also `UPDATE
container_configs SET image_tag = <new>` for any row whose pinned tag
differs from the freshly baked one. Reports the number repointed.
Gated on `sqlite3` being available; silent no-op otherwise.

### Fixed (breadcrumbs, turn-cap, opaque apology) — three issues caught in the same Telegram session

A "Build me a clone of an App Store app" run surfaced three independent
papercuts in one shot:

- **`COPPERCLAW_TOOL_BREADCRUMBS=1` silently no-op'd.** The runner inside
  the container reads the env var via `std::env::var`, but the host's
  `collect_forward_env` in `crates/copperclaw-host/src/boot.rs` only
  forwarded provider keys + Ollama base URL. The operator's `.env`
  setting never reached the container; the runner saw it unset and
  treated breadcrumbs as off. Added `COPPERCLAW_TOOL_BREADCRUMBS` (and
  `COPPERCLAW_MAX_TOOL_TURNS`, for symmetry with the cap change below)
  to the `FORWARDED` list.
- **`max_tool_turns` hard-coded at 20 was too low for build/research
  tasks.** Live session bailed after exactly 20 turns with the agent
  mid-flight on a real "research apps then scaffold a TypeScript
  clone" workload. Bumped the default to 60 in
  `crates/copperclaw-runner/src/run/mod.rs` (new
  `DEFAULT_MAX_TOOL_TURNS` + bounds + `resolve_max_tool_turns(env)`
  helper). Operators can override via `COPPERCLAW_MAX_TOOL_TURNS`
  (clamped to [5, 500]).
- **Apology said "I hit a snag … see runner stderr" — useless to the
  user.** When a turn failed (provider error, 3-strikes parse-error
  bailout, or hitting the cap above), the user saw a generic message
  with no hint why. Extended `TurnOutcome::Failed` to carry a short
  human-readable reason string ("the agent ran out of turns after 60
  tool calls without finishing the task", "the model's provider call
  did not return a complete response", "model produced malformed
  tool-call JSON 3 turns in a row"), and reshaped the apology to
  splice it in: "I couldn't finish a reply on that message — &lt;reason&gt;.
  Try rephrasing or sending a smaller request, and the operator can
  check the runner log for details." `cli_provider_timeout` replay
  fixture updated to match.

### Fixed (clear-history sentinel silently swallowed the next user message)

Caught while debugging "agent says 'I'm ready to help' instead of doing
the task." Sequence:

1. Runner polls inbound, pushes the user's chat message into
   `state.history`.
2. Then checks for the `.history_clear_pending` sentinel.
3. If found, it clears the **entire** history — including the user
   message that was just pushed one statement earlier — then calls
   the model with an empty context.

The model received: system prompt + tool schemas + zero user content.
With nothing to respond to it fell back to its training prior ("I'm
ready to help. What would you like to work on?"), which looked
identical to a bot ignoring the task. Both operator-dropped sentinels
and tool-triggered clears hit this path; the inline comment claimed
the user message had to be dropped to "avoid surprising the operator,"
but in practice that just made the next inbound silently disappear.

Fix in `crates/copperclaw-runner/src/run/mod.rs`: process the clear /
compact sentinels **before** pushing the user message, so the incoming
inbound always reaches the model against the requested baseline (cleared
or compacted) rather than being thrown out alongside it. Also updated
the `clear_history` tool docstring in
`crates/copperclaw-mcp/src/tools/clear_history.rs` to reflect the
corrected semantics ("drops everything prior to the next inbound").

### Fixed (typing ticker was always-on; agent self-introducing on tasks)

Two issues caught in the Sonnet retest:

- **Typing indicator stayed pinned forever** after the first user
  message. The old ticker fired for any session with
  `container_status = Running`, but Running lasts for the full
  idle-timeout window between user turns — so the bubble pulsed
  continuously even when the agent was idle waiting for input.
  Fixed by gating each tick on `messages_in::count_due() > 0` for
  the session's inbound.db. Typing now only appears when the agent
  actually has work to process (pending inbound) or is mid-turn.
  `TypingTicker::new` gained a `data_root` parameter; new
  `tick_skips_idle_running_session_without_pending_work` test pins
  the new behaviour.
- **Bot recited a self-introduction when the user gave a task.**
  User: "Build me a clone of one of the top apps in the App Store."
  Bot: "I'm the Copperclaw agent — a self-hosted AI assistant
  running inside a per-session Linux container. Here's a quick
  overview of what I am..." — ignoring the actual task and ending
  with "What can I help you with?". The `identity` skill says only
  introduce when asked; Sonnet ignored the conditional. Added a
  hard rule to `BASE_PREAMBLE`: "Do NOT introduce yourself unless
  the user explicitly asks" + "No preamble or postamble on
  substantive replies." Identity introductions are reserved for
  "who are you?" / "what is Copperclaw?" messages.

### Changed (anti-fabrication on coding-task completion)

Live testing surfaced a worse cousin of the news-roundup
fabrication: when asked to "research App Store apps and build the
top one", Haiku 4.5 built a React Native frontend then marked
"Build backend: Express TypeScript server with PostgreSQL",
"Implement authentication service (JWT, bcrypt)", "Create habit
management API endpoints", "Build wellness metrics tracking API",
and "Implement AI insights generation service" all as **completed**
in the todo list — while writing zero backend code. The
`docker-compose.yml` it generated referenced a
`../mindflow-backend` directory that doesn't exist; the
`API_DOCUMENTATION.md` documented endpoints that were never written.

Three-pronged fix:

- **`crates/copperclaw-mcp/src/tools/todo.rs::update`** — mandatory
  `evidence` field when setting `status: "completed"`. Schema-level
  + handler-side validation:
    - `>= 20 chars` (generic affirmations don't fit a real citation),
    - rejects exact-match generic strings: `"done"`, `"complete"`,
      `"completed"`, `"finished"`, `"all set"`, `"all done"`,
      `"good to go"`, `"ready"`, `"yes"`, `"ok"`, `"okay"`.
  The tool description spells out the requirement so the model
  sees it at the schema-introspection layer. Four new unit tests
  pin: rejection without evidence, rejection on generic strings,
  acceptance on substantive citation, no-evidence-required for
  `in_progress` transitions. Existing tests updated to pass real
  evidence where they hit `completed`.
- **`crates/copperclaw-host/src/container_manager/prompt.rs`** — new
  `# Don't fabricate completion on coding work` section in
  `BASE_PREAMBLE` with four hard rules: verify on disk before
  marking complete (read_file / glob / git_status); never write
  docs for code that doesn't exist; never reference nonexistent
  directories in build configs; "done" claims must be `ls`-able.
- **`skills/coding-task/SKILL.md`** — rewrote the "Don't fabricate"
  section into four concrete rules with the exact failure patterns
  from the MindFlow incident (fabricated todos, phantom backend
  dirs, README/docker-compose for code that doesn't exist).
  Trimmed verification recipes section to stay under 4 KiB cap
  (4078 bytes from 5076).
- **`skills/todo-tracker/SKILL.md`** — documented the new
  `evidence` requirement on `todo_update` with a concrete example.

Also: bumped **`COPPERCLAW_DEFAULT_MODEL`** from
`anthropic/claude-haiku-4-5` to `anthropic/claude-sonnet-4-6` in
the live install's `.env`. Sonnet follows multi-step discipline
better than Haiku; this is a per-deployment decision, not a code
change.

Verification: cargo test --workspace --no-fail-fast = 5255+ passed;
clippy clean (two flaky integration tests passed when re-run alone
— same parallel-test contention pattern as earlier).

### Added (UX feedback layer for long agent turns)

Live Telegram testing exposed a real UX gap: complex tasks (e.g.
"research App Store apps, decide what to build, scaffold the
project, start coding") take 1-5 minutes and the user has zero
visibility into what the agent is doing between turns. Three new
host- and runner-side feedback mechanisms:

- **`crates/copperclaw-host/src/typing_ticker.rs`** (always on).
  Background tokio task wired into `boot::run_host` alongside the
  delivery + sweep loops. Every 4 seconds, iterates
  `sessions::list_running` and fires `HostDispatcher::set_typing`
  for each running session that has a channel-bound messaging
  group. Closes the gap where Telegram's `sendChatAction` indicator
  fades after ~5 seconds but `TypingModule` only re-fires on
  inbound events — long agent turns between inbounds left the
  bubble silent. Telegram/Slack/Discord/Teams benefit; channels
  without typing get a quiet no-op. Four unit tests pin behaviour
  (fires per running session, skips idle, skips no-MG sessions,
  loops until shutdown).

- **`crates/copperclaw-host/src/todo_watcher.rs`** (gated by
  `COPPERCLAW_TODO_NOTIFICATIONS=1`, default off). Background task
  that polls each running session's `agent_todos.json` every 5
  seconds, diffs against the last snapshot, and emits chat
  notifications via the dispatcher when:
    1. Todos first appear (one "📋 Plan (N steps): ..." message
       with the full list);
    2. Items transition to `completed` (one rollup "Step(s)
       complete: ..." per tick, multiple completions in the same
       tick collapsed to one message);
    3. New items are added mid-run (one "Plan grew (+N steps): ..."
       per tick).
  Deletes are silent; status-unchanged items don't re-emit.
  Eight unit tests pin the delta logic.

- **`crates/copperclaw-runner/src/tools.rs`** + **`run/provider_call.rs`**
  (gated by `COPPERCLAW_TOOL_BREADCRUMBS=1`, default off). New
  `ToolContext::emit_breadcrumb` trait method (default no-op) +
  `RunnerToolCtx::emit_breadcrumb` impl that writes a short
  `[tool_name]` chat row at the start of every "visible" tool
  call. Visible tools: `shell`, `web_search`, `web_fetch`,
  `explore`, `write_file`, `edit_file`, `create_agent`,
  `install_packages`, `add_mcp_server`. Other tools (read_file,
  grep, glob, todo_*, etc.) are excluded to keep the chat from
  drowning. Only fires when there's real channel routing — child
  agents reporting up to a parent don't spam the parent with
  their own breadcrumbs.

Operator wiring: the env vars are read at host boot
(`COPPERCLAW_TODO_NOTIFICATIONS`) and runner startup
(`COPPERCLAW_TOOL_BREADCRUMBS`) respectively. Set both to `1` in
`.env` for the live-testing experience the user requested in the
session that drove this work.

### Fixed (strip leaked `<thinking>` blocks from outbound chat text)

Live Telegram testing (Haiku 4.5) caught the model emitting its
reasoning as literal `<thinking>...</thinking>` markup inside regular
`send_message` text — not via the Anthropic API's private-reasoning
content blocks. End users saw a wall of "the model talking to
itself" before the actual reply. The provider-side
`thinking`/`redacted_thinking` block handling can't catch this case
because the markup is content, not metadata.

- **`crates/copperclaw-runner/src/tools.rs`** — new
  `strip_reasoning_blocks(text)` helper that drops every closed
  `<thinking>...</thinking>` pair (case-insensitive tag, multi-line
  content), collapses the blank-line runs left behind, and
  preserves text containing an unterminated open tag verbatim (so
  we never silently swallow large prose chunks).
  `apply_send_message` and `apply_send_file` both run their text
  through it before writing the row. Six unit tests cover
  open/close pair removal, multi-block, unterminated tag
  preservation, case insensitivity, plain-text passthrough, and an
  end-to-end via `emit_outbound`.

### Added

- **`cclaw sessions delete <id> [--force]`.** Closes the operator gap
  that forced raw `sqlite3` cleanup when a session row needed to go
  away (e.g. so `cclaw groups delete <id>` would stop failing with
  `FOREIGN KEY constraint failed`). The new subcommand deletes the
  central `sessions` row plus every per-session row that referenced
  it — `agent_turns`, `tasks`, `pending_questions`,
  `pending_approvals` — in a single transaction, then removes the
  on-disk session tree at `<data_dir>/sessions/<agent>/<session>/`.
  Refuses by default if the session's container is not in `stopped`
  state so the operator runs `cclaw groups restart <ag>` first; pass
  `--force` to override. Filesystem removal is best-effort: a warn
  is logged but the command still succeeds when the central rows
  are already gone. New table function:
  `copperclaw_db::tables::sessions::delete`. New handler:
  `copperclaw_host::handlers::sessions::delete` (registered as a
  host-only mutation, so every call lands in `audit_log`).

### Fixed (subagent routing follow-up: 15 code-review findings)

Follow-up to the subagent-routing PR (`466b1ed`). An extra-high-effort
multi-angle review surfaced 15 defects — all addressed here. Highlights:

- **Silent loss in `agent_dispatch` (finding #1, severity: critical).**
  `AgentDispatchHandler` used to swallow `messages_in::insert` /
  `open_inbound` failures with a `warn!` and return Ok, then the delivery
  loop marked the outbound row delivered=ok — permanent loss with no
  retry. Now: transient failures (insert, open_inbound) return
  `ModuleError`, which the delivery loop's retry/backoff handles
  normally. Permanent failures (malformed payload, target deleted /
  archived) still return Ok so retries don't churn.
- **`send_file` orphaning bytes (#2).** A child agent's
  `send_file(to: None)` previously emitted an Agent-kind row whose body
  carried a `files: [{filename}]` field, but the `agent_dispatch`
  handler only forwarded `body.text` — the on-disk bytes under
  `outbox/<msg_id>/<filename>` were never copied to the parent. Now
  `apply_send_file` overrides Agent-kind back to Chat for the
  inherited-channel routing, so bytes reach the user channel. (The
  long-term fix — real cross-session attachment relay — stays on the
  follow-up list.)
- **Migration 013 FK enforcement (#3).** The migration's `ON DELETE SET
  NULL` IS enforced (the central DB runs `PRAGMA foreign_keys=ON`),
  contradicting the original "soft reference" comment. Updated the
  migration comment to acknowledge enforcement and document the
  `UPDATE sessions SET status='archived'` retirement pattern that keeps
  child pointers intact.
- **Subagent emit lost routing (#4).** `SubagentCtxAdapter::emit_outbound`
  built `OriginatingRouting::default()` (empty everything) for the
  subagent's tool calls. For any operator-widened `tools_allowed` that
  included a message-emitting tool, a subagent emission landed with
  empty channel columns → `DeliveryError::NoRoute`. Now the subagent
  inherits the parent's current originating routing AND
  `source_session_id`.
- **User→child siphon to parent (#5).** Old default routing said "if
  `source_session_id` is set, route up." That accidentally hijacked
  user messages that landed directly on a child session (per-thread
  wirings, operator-added wirings). Now the rule is "route up only when
  the inbound itself has no channel routing (i.e. came from
  `agent_dispatch`)." User-channel inbounds always reply via channel.
- **Apology cascade (#6).** Both apology paths (in-runner emit, sweep)
  used to require `inbound.channel_type` AND `platform_id` before
  emitting — but agent-dispatched inbounds have neither. Now: if
  channel routing is absent but `source_session_id` is set, emit an
  Agent-kind apology UP the chain so the parent agent learns the
  child failed and can surface it to the user.
- **`in_reply_to` dangling (#7).** `insert_outbound_row` used to copy
  `origin.in_reply_to` into Agent-kind rows, but that id lives in the
  source session's `messages_in` — a dangling reference once the row
  crossed into the target session's space. Now elided for Agent-kind.
- **No parent-status check (#8).** `AgentDispatchHandler` now refuses
  to dead-letter into a non-`Active` target session. Logs and returns
  Ok (permanent — retry won't help). New `sessions::set_status` helper
  in `copperclaw-db` powers the test that pins this.
- **Retry duplicates (#10).** A successful handler call followed by a
  failed `delivered::insert` previously caused the loop to re-run the
  handler with a fresh `MessageId`, writing the parent's inbound twice.
  Now: the handler uses the source outbound row's `MessageId` (passed
  through new `DeliveryActionInput.row_id`) as the parent inbound's
  id, plus new `messages_in::insert_idempotent` (`INSERT OR IGNORE`).
  A retry is a no-op.
- **`thread_id` stripped (#11).** `agent_dispatch` used to write parent
  inbound rows with `thread_id: None`, dropping the user-thread
  context. Runner now copies origin's `thread_id` into the Agent body;
  handler reads it back and stores it on the inbound write.
- **Loose `parse_target_session` (#12).** Removed the bare-string
  `to: "<uuid>"` fallback that let any UUID-shaped payload route into
  the matching session. Now requires the tagged
  `{ kind: "agent", session_id: ... }` form. Two new tests pin the
  rejection.
- **Explicit `Recipient::Channel` columns (#12 / #8 in review).** Now
  documented behavior: explicit Channel keeps the row's column routing
  inherited (delivery loop doesn't parse channel-id strings yet). The
  body's `to` is preserved so future versions can override.
- **Replay harness wired (#13).** `crates/copperclaw-host/tests/replay/harness.rs`
  now threads `source_session_id` onto the runner ctx the same way
  production's `main.rs` does, so replay fixtures actually exercise
  the new Agent-kind routing path.
- **`unwrap_or_default` on Recipient serialization (#15).** Replaced
  with `.expect("Recipient is always serialisable")` so a future
  serialization regression surfaces loudly instead of silently
  producing `to: null`.

Service-level integration test gap (#14 — make_service tests register
a Failer mock for `agent_dispatch` instead of the real handler) is
not addressed here because pulling `copperclaw-modules` into
`copperclaw-host-delivery`'s dev-deps would create a circular concern.
The dispatch.rs in-module tests (10 passing) cover the handler
end-to-end; this gap is logged in `docs/plans/vaporware-followups.md`.

Migrations + new code:

- **`crates/copperclaw-db/migrations/013_sessions_source_session.sql`** —
  comment rewritten; migration body unchanged.
- **`crates/copperclaw-db/src/tables/messages_in.rs`** — new
  `insert_idempotent` variant.
- **`crates/copperclaw-db/src/tables/sessions.rs`** — new
  `set_status(db, id, status)` helper.
- **`crates/copperclaw-types/src/session.rs`** —
  `SessionStatus::as_str()` impl (mirrors `ContainerStatus`).
- **`crates/copperclaw-modules/src/context.rs`** —
  `DeliveryActionInput.row_id: Option<MessageId>`.
- **`crates/copperclaw-modules/src/agent_to_agent/dispatch.rs`** — full
  handler rewrite covering findings #1, #8, #10, #11, #12. Five new
  unit tests for the new behaviors.
- **`crates/copperclaw-host-delivery/src/service.rs`** — passes
  `row_id` into `DeliveryActionInput`; Agent-kind arm dropped the
  swallow-error `let _ =` pattern in favour of propagating the handler's
  Err.
- **`crates/copperclaw-runner/src/tools.rs`** — `resolve_outbound_routing`
  rewritten: routes up to parent only when inbound has no channel info,
  elides `in_reply_to` for Agent-kind, propagates `thread_id` into
  body. `apply_send_file` falls back to Chat-kind when routing
  resolved to Agent. `SubagentCtxAdapter` inherits the parent's
  originating routing. Replaced silent `unwrap_or_default` with
  `expect`. Four new tests pin the routing rules.
- **`crates/copperclaw-host-sweep/src/checks/apology.rs`** — apology
  cascade walks `source_session_id` for inbounds without channel
  routing.
- **`crates/copperclaw-runner/src/run/mod.rs`** — same cascade for the
  in-runner terminal-failure apology emit.
- **`crates/copperclaw-host/tests/replay/harness.rs`** — replay
  threads `source_session_id` onto `RunnerToolCtx`.

Verification: cargo test --workspace --no-fail-fast = 5219 passed, 0
failed (was 5210); cargo clippy --workspace --all-targets -- -D warnings
clean.

### Fixed (subagent routing: children now report up to the parent by default)

The big one. Routing of child agents' replies is now architectural —
the runtime decides where they go based on `sessions.source_session_id`,
not a prompt instruction the model has to follow. See
[`docs/plans/agent-to-agent-routing.md`](docs/plans/agent-to-agent-routing.md).

**Before:** the kicker prompt told each spawned child to use
`send_message(to: "agent:<parent_name>", text: ...)`. The `agent:` parser
existed but had no production callers — every child's reply went through
the inherited messaging-group routing and landed in the user's chat, not
the parent's inbound. Result: disjointed-voices UX where N spawned
children dumped N independent messages on the operator instead of one
consolidated parent report.

**After:** the child's session row carries a `source_session_id`
pointing at the parent. The runner sees this on startup and the routing
helper (`resolve_outbound_routing`) defaults `send_message(to: None)`
to a `MessageKind::Agent` outbound row addressed to the parent. The
host's new `agent_dispatch` handler reads the row's body, opens the
target session's `inbound.db`, and writes a chat row with the
originating session id in `source_session_id`. Explicit
`Recipient::Channel { ... }` recipients keep working unchanged.

Migrations + code touched:

- **`crates/copperclaw-db/migrations/013_sessions_source_session.sql`** —
  new `sessions.source_session_id TEXT REFERENCES sessions(id) ON DELETE
  SET NULL` column + `idx_sessions_source` index. Registered in
  `crates/copperclaw-db/src/migrate.rs`.
- **`crates/copperclaw-types/src/session.rs`** + **`crates/copperclaw-db/src/tables/sessions.rs`** —
  `Session` and `CreateSession` carry the new column; every SELECT /
  INSERT updated via a `SESSION_SELECT_COLS` constant so column order
  stays in sync across the half-dozen reads. Roundtrip test pinned.
- **`crates/copperclaw-modules/src/agent_to_agent/create_agent.rs`** —
  `CreateAgentHandler` now sets `source_session_id =
  parent.session_id` when creating the child session. New test
  `child_session_records_source_session_id_pointing_at_parent` pins
  the behaviour.
- **`crates/copperclaw-runner/src/config.rs`** — `RunnerConfigFile` /
  `RunnerConfig` gain `source_session_id`. **`crates/copperclaw-runner/src/main.rs`** —
  threads it onto `RunnerToolCtx`. **`crates/copperclaw-host/src/container_manager/runner_config.rs`** —
  host writes the field into `runner.json` so it reaches the container.
- **`crates/copperclaw-runner/src/tools.rs`** — `OriginatingRouting`
  carries `source_session_id`; new `RunnerToolCtx::with_source_session_id`
  builder; new `resolve_outbound_routing` helper decides
  `MessageKind::Agent` vs `MessageKind::Chat` based on the recipient
  and the parent-id; `insert_outbound_row` replaces the older
  `insert_chat_row` and elides channel columns for Agent-kind rows.
  Two new tests:
  `child_send_message_with_no_to_routes_to_parent` and
  `child_send_message_with_explicit_channel_still_works`.
- **`crates/copperclaw-modules/src/agent_to_agent/dispatch.rs`** — new
  `AgentDispatchModule` + handler. Implements the `agent_dispatch`
  delivery action the host's delivery service was already calling
  into (but had no real implementation for outside test fakes).
  Reads `payload.to.session_id`, resolves the target session, writes
  a `MessageKind::Chat` row into its `inbound.db` with
  `source_session_id` set to the originating session. Four
  unit tests cover the happy path, missing-target, malformed-payload,
  and parser cases.
- **`crates/copperclaw-host/src/boot.rs`** — installs
  `AgentDispatchModule` alongside `CreateAgentModule` so the delivery
  loop's `agent_dispatch` action handler is no longer a no-op.
- **`crates/copperclaw-modules/src/agent_to_agent/inbound_seed.rs`** —
  the kicker prompt no longer tells the child to use
  `to: "agent:<parent>"`. The new prelude just says "your replies
  route back to the parent by default; consolidate, send once."
- **`skills/create-agent/SKILL.md`** — updated the "Consolidating
  subagent results" section to describe the architectural routing
  (no more `agent:<name>` magic).

Verification: `cargo test --workspace --no-fail-fast` = 5210 passed,
0 failed (one pre-existing flake in `copperclaw-mcp::tools::compact_now`
under parallel-test load — passes alone, unrelated to this change).
Clippy clean.

### Added (install.sh detects Apple Container on macOS — 2026-05-23)

- **`install.sh`** — `check_container_runtime` now also accepts the
  Apple Container runtime (`container` binary) on macOS, in addition
  to Docker / Podman. Brings the installer in line with the wizard's
  `env_check` step (`crates/copperclaw-setup/src/steps/env_check.rs`),
  which already detected it. A fresh macOS user with only Apple
  Container installed no longer sees a misleading "install Docker"
  prompt. Also added the Apple Container install link to the
  no-runtime-found error message on macOS.
- **`README.md`** — Manual Install section now correctly lists
  Apple Container alongside Docker / Podman as detected by
  `install.sh`.

### Changed (cclaw subcommand `--help` text — 2026-05-23)

- **`crates/copperclaw-cclaw/src/commands.rs`** — added `///`
  doc-comments to every variant and every `#[arg(long)]` field on
  `MessagingGroupsCmd`, `WiringsCmd`, `UsersCmd`, `RolesCmd`,
  `MembersCmd`, `DestinationsCmd`, `SessionsCmd`, `UserDmsCmd`, and
  `ApprovalsCmd`. Operators running `cclaw <foo> <bar> --help` now
  see a description of every command and flag instead of a bare
  usage line. The `ApprovalsCmd::Approve` variant also notes the
  scope limitation (sender-only — see
  `docs/plans/vaporware-followups.md` for the generic approve/deny
  follow-up).

### Changed (doc-vs-reality reconciliation pass — 2026-05-23)

Wide audit + reconciliation of the in-tree docs against the actual
code, motivated by the agent fabricating capabilities that didn't
exist. Highlights:

- **`README.md`** — rewrite. Drops the "no half-finished adapters in
  the tree" / "surprises don't ship here" framing in favour of an
  honest pre-1.0 stance. New `## What's rough` section enumerating
  shipped-but-unpolished surfaces. Fixed the in-tree tool count
  (`33` → `36`), test count (`~5160` → `~5200`), the `CCLAW_SOCKET`
  env-key row (now `COPPERCLAW_CCLAW_SOCKET` for the host with a note
  about the client-side `CCLAW_SOCKET`), and added rows for
  `COPPERCLAW_CONTAINER_GPU` and the new session-control tools
  (`compact_now`, `clear_history`, `artifact_path`). Operator
  cheatsheet now includes `users` / `roles` / `members` /
  `schema-version` / `quickstart cli`. Removed the duplicated `cli`
  channel in the headline channel list. The `Status` and `Tenets`
  sections collapsed into a single more-honest `Status` block.
- **`docs/channels/README.md`** — fixed stale rows:
  `gchat` files now correctly listed as supported (two-step
  `attachments:upload`); `mattermost` files supported (two-step
  `/api/v4/files`); `teams` channel-target files supported, chat-
  target files Unsupported (delegated-auth limit); `imessage`
  empty-body Med-severity flag removed (already fixed in code);
  `line` row clarified (non-`post` action returns BadRequest, not
  Unsupported); deferred punch-list trimmed of items shipped since
  the original audit.
- **`docs/channels/mattermost.md`** + **`docs/channels/teams.md`** +
  **`docs/channels/webex.md`** — fixed the intros to match what the
  adapters actually do. Removed the fictional
  `webex` `reactions_endpoint` config field (no such field exists;
  the adapter does HTTP-status fallback on 404/501 from `/reactions`).
- **`docs/channels/slack.md`** + **`docs/channels/x.md`** — removed a
  stale "deferred follow-up" already shipped; fixed an `x` `deliver`
  line-number anchor.
- **`docs/adding-a-channel.md`** — rewrote the `ChannelAdapter` trait
  snippet to include `edit_message`, `add_reaction`, and
  `plain_text_fallback` (the trait grew these as first-class methods;
  the doc still described an action-shaped `deliver` dispatch). Added
  a `Plain-text fallback` section.
- **`docs/webhooks-tls.md`** — rewrote the per-channel port table from
  the actual `DEFAULT_HOST` / `DEFAULT_PORT` / `DEFAULT_PATH`
  constants. Telegram + Slack default to `0.0.0.0` (not `127.0.0.1`
  as the table claimed). Most webhook channels have stable
  static ports (8081–8087), not dynamic OS-assigned ports.
  Softened the "all webhook channels perform HMAC verification"
  claim — Teams uses `clientState`, Mattermost uses
  `webhook_token`, gchat uses a query-string client_token.
- **`docs/db-backup.md`** — replaced `/var/run/copperclaw.pid` example
  with `<data_dir>/copperclaw.pid` (or `copperclaw stop`). Corrected the
  "not in the backup" list — per-session `inbox/` and `outbox/` live
  inside each session's dir under `<data_dir>/sessions/`, not at the
  data root.
- **`docs/observability.md`** — `cclaw groups budget set` (does not
  exist) → `cclaw budgets set --agent-group-id <id> --daily-tokens <n>`.
- **`docs/cutover.md`** — removed `copperclaw run --once --check` (no
  such flag combo) — replaced with `cclaw schema-version` + `copperclaw
  migrate`. `copperclaw setup` → `copperclaw-setup` (binary name). Fixed
  the migrator description: the migrator only copies the central DB,
  not per-session DBs; operators must rsync `data/sessions/` separately.
  Dedup'd the `Webex` entry in the channel-disable bullet list.
- **`docs/release-checklist.md`** — replaced fictional
  `copperclaw run --check` with `cclaw schema-version` + `copperclaw
  migrate`.
- **`docs/replay-fixtures.md`** — rewrote the fixture-shape section
  to match the real on-disk layout (`manifest.json`, not
  `manifest.toml`; `inbound/NNN-*.json`, not `.http`; `mode: "direct"`,
  not `webhook|gateway|poll|rpc`). Acknowledged that the
  capture-and-redact pipeline (`COPPERCLAW_FIXTURE_CAPTURE` env,
  `copperclaw fixture redact <dir>` subcommand,
  `crates/copperclaw-host/src/fixture/redact.rs`) is design-only.
- **`docs/container-config.md`** — replaced the "no top-level `cclaw
  groups config show` command yet" sqlite3 workaround with the actual
  shipped `cclaw groups config get <id>`.
- **`CLAUDE.md`** — `copperclaw logs --tail` (does not exist) → `-n` /
  `--lines`. Bumped test baseline (`~5,160` → `~5,200`).

### Changed (skill bodies match real tool behaviour — 2026-05-23)

- **`skills/debug/SKILL.md`** — removed the false claim "There is no
  `cclaw doctor` — `cclaw health` is the equivalent." (Doctor IS
  implemented at `crates/copperclaw-cclaw/src/lib.rs:703`.) Added
  `cclaw doctor` to the operator-side command list. Dropped an
  HTML-comment `TODO(team-h)` body marker.
- **`skills/read-file/SKILL.md`** — result shape now uses
  `size_bytes` (not the fictional `bytes_read` / `total_bytes`). The
  "non-UTF-8 returns validation error" claim was wrong — the tool
  uses `String::from_utf8_lossy`; doc updated to match.
- **`skills/shell/SKILL.md`** — frontmatter typo `8-byte output cap`
  → `64 KiB`. Result-shape field `elapsed_secs` → `elapsed_ms` (the
  tool emits milliseconds).
- **`skills/web-fetch/SKILL.md`** — dropped the fictional "JSON vs
  text/plain Content-Type heuristic" (the tool does no
  Content-Type detection; if the server requires one, callers set
  it via `headers`). Result-shape fields fixed to match what the
  tool actually emits (`size_bytes` + `elapsed_ms`, not
  `bytes_read` / `total_bytes` / `elapsed_secs`).
- **`skills/add-mcp-server/SKILL.md`** — `cclaw groups config
  get-mcp-servers <ag>` (does not exist) → `cclaw groups config
  get <ag>`.
- **`skills/approvals/SKILL.md`** — `pending_approvals` schema uses
  an `action` string column, not a typed `kind` enum. Removed the
  fictional `OneCli` approval kind. Replaced the aspirational
  `cclaw approvals approve <id>` / `deny <id>` generic CLI with the
  actual sender-only surface. Trimmed body to stay under the
  4 KiB skill-body cap.
- **`skills/schedule-task/SKILL.md`** — example task id changed to
  the actual `task_<uuidv7>` shape (was `task_8a` — short suffixes
  do not exist).
- **`skills/discovering-tools/SKILL.md`** — the "15 built-in tools"
  table was wildly out of date (registry has 36). Replaced the
  hand-counted enumeration with a category-grouped index that names
  every tool currently in the registry. Trimmed body to stay under
  the 4 KiB cap.
- **`skills/edit-file/SKILL.md`** — removed a stale HTML-comment
  `TODO(team-u)` from the body (the work it described shipped).
- **`crates/copperclaw-skills/tests/coverage.rs`** — synced the
  hardcoded `REGISTRY_TOOLS` list to the real
  `copperclaw_mcp::tools::build_tool_set` inventory (27 → 36 entries).
  The `every_registry_tool_appears_in_some_skill` test was silently
  undercovering by 9 tools (`load_skill`, `compact_now`,
  `clear_history`, `artifact_path`, plus the four `todo_*` tools).

### Changed (code cleanup — dropped vapor surfaces, stale TODOs)

- **`crates/copperclaw-cclaw/src/commands.rs`** + **`lib.rs`** — dropped
  the dead `cclaw doctor --no-ping` flag. The flag was wired into
  the CLI parser but read into `_no_ping` and never used; no LLM
  ping is performed by `run_doctor`. The help text claimed otherwise,
  so the flag was a small lie.
- **`crates/copperclaw-host/src/boot.rs`**,
  **`crates/copperclaw-modules/src/agent_to_agent/create_agent.rs`**,
  **`crates/copperclaw-host-delivery/src/service.rs`** — removed three
  stale `TODO(team-…)` comments whose work has already shipped
  (`SqliteTaskStore` installed at boot, `CreateAgentModule` installed
  at boot, `session_id` plumbed through `DeliveryActionInput`).

### Added (vaporware-followups punch list)

- **`docs/plans/vaporware-followups.md`** — open punch list of items
  the docs or operator surface reference that don't fully exist in
  code yet. Sized small / medium / large with a load-bearing
  question per item so future contributors know what to decide
  before writing code. Sweep done 2026-05-23.

### Changed (anti-fabrication prompt + sharper schedule-task description)

- **`crates/copperclaw-host/src/container_manager/prompt.rs`** — added a
  "Don't fabricate capabilities" section to `BASE_PREAMBLE`. The agent
  was inventing fictional capabilities like a "Real-Time News Monitor"
  with "persistent loops," then backtracking when asked to verify.
  The new section tells the agent: the skill catalogue is authoritative,
  for recurring work use `schedule_task` (the scheduler IS the loop),
  never invent agent types or tools, and if unsure call `load_skill`.
- **`skills/schedule-task/SKILL.md`** — rewrote the frontmatter
  `description` (the only thing the agent sees in callable-mode skill
  index) from dry/procedural to imperative: leads with "USE THIS for
  anything periodic or recurring" and lists the trigger phrases. The
  previous wording let the agent miss the skill and fabricate a
  background-loop pattern instead. Quoted the cron expression as plain
  text (no backticks/colons inside YAML) so the frontmatter parses.
- **`crates/copperclaw-host/src/container_manager/runner_config.rs`**
  (`runner_config_callable_falls_back_to_inline_when_catalogue_write_fails`
  test) — relaxed the inline-fallback assertion from `!contains("\`load_skill\`")`
  to `!contains("catalogue of skills available to you")`. The base
  preamble now legitimately mentions `load_skill` as a tool name; the
  thing that must stay absent in inline fallback is the callable-mode
  catalogue header sentence.

### Added (per-group coding-skills toggle)

- **`crates/copperclaw-db/migrations/012_container_config_coding_enabled.sql`**
  — new `container_configs.coding_enabled INTEGER NOT NULL DEFAULT 0`
  column. Registered in `crates/copperclaw-db/src/migrate.rs`.
- **`crates/copperclaw-db/src/tables/container_configs.rs`** — added
  `ContainerConfig.coding_enabled: bool` and matching field on
  `UpsertContainerConfig`; `get` / `upsert` paths now read and write
  the new column. New narrow setter `set_coding_enabled(central, id, enabled)`
  does a single-column UPDATE.
- **`crates/copperclaw-host/src/container_manager.rs`** — new public
  constant `CODING_SKILL_NAMES = &["coding-task", "git-commit",
  "code-review", "testing"]`. `runner_config_for` builds an
  `exclude_names` filter that drops those four skills from the
  assembled prompt and `skills.json` catalogue when
  `coding_enabled == false`. The filter is plumbed through
  `select_callable_skills`, `build_skill_system_prompt`, and
  `assemble_system_prompt_with_catalogue`. Explicit selector lists
  are honoured as-is; the flag only caps the `SkillsSelector::All`
  default.
- **`crates/copperclaw-host/src/handlers/groups.rs`** — new
  `config_set_coding_enabled` handler bound to the
  `groups.config.set-coding-enabled` host-only socket method, plus
  `coding_enabled` surfaced in `container_config_to_json`.
- **`crates/copperclaw-host/src/handlers/mod.rs`**,
  **`crates/copperclaw-host/src/socket.rs`** — host-only entry and
  dispatch wiring for the new command.
- **`crates/copperclaw-cclaw/src/commands.rs`** — two new
  subcommands `cclaw groups enable-coding <id>` and
  `cclaw groups disable-coding <id>` that dispatch to
  `groups.config.set-coding-enabled` with `enabled: true|false`.
- **`crates/copperclaw-cclaw/src/lib.rs`** (`render_config_toml`) —
  current value is shown as a `# read-only: coding_enabled = …`
  line in the `cclaw groups config edit` buffer so operators can
  see the state when they open the TOML, with a hint pointing at
  the dedicated subcommands.
- **`README.md`** — replaced the "Per-group skill selection …
  is not yet exposed" paragraph with the new toggle instructions.

### Changed (default coding-skill loading)

- **Behaviour change for existing installs:** the four coding
  skills (`coding-task`, `git-commit`, `code-review`, `testing`)
  no longer load into every agent group automatically. After
  migration 012 applies, every existing group's `coding_enabled`
  defaults to 0, so coding skills stop loading by default. To
  restore the prior behaviour for a specific group, run
  `cclaw groups enable-coding <id>`.

### Added (Codex subprocess provider routed by build_provider)

- **`crates/copperclaw-runner/src/main.rs`** (`build_provider`) — added a
  `"codex"` arm that constructs a `CodexProvider` via
  `CodexProvider::new(binary_path, extra_args)`. Binary path resolves
  from `RunnerConfig::codex_binary`, then the runner's
  `COPPERCLAW_CODEX_BINARY` env var, then `/usr/local/bin/codex`. Args
  resolve from `RunnerConfig::codex_args`, then a comma-separated
  `COPPERCLAW_CODEX_ARGS`, then `["--json"]`. The function is now
  `pub(crate)` so the new `build_provider_tests` module can exercise
  it directly.
- **`crates/copperclaw-runner/src/config.rs`** — `RunnerConfigFile` and
  `RunnerConfig` grew `codex_binary: Option<String>` and
  `codex_args: Option<Vec<String>>`. `from_file_struct` carries them
  through; `provider` recognises `"codex"` as a known value (no more
  WARN-and-fall-back-to-anthropic). New unit tests:
  `provider_codex_passes_through`, `codex_binary_and_args_default_to_none`,
  `codex_binary_and_args_pass_through_from_file`,
  `codex_empty_args_round_trip`.
- **`crates/copperclaw-host/src/container_manager.rs`** —
  `RunnerConfigForFile` mirrors the new fields. `runner_config_for`
  sources them from the rotatable `forward_env` so an operator can
  edit `.env` + SIGHUP to swap binaries without restarting the host.
  `provider == "codex"` now also routes to the "no API key, no base
  URL" arm so the runner doesn't try to pull `ANTHROPIC_API_KEY` for
  a Codex session. `ROTATABLE_ENV_KEYS` learned the new keys so SIGHUP
  picks them up. New unit tests:
  `runner_config_propagates_codex_provider`,
  `runner_config_codex_omits_overrides_when_env_unset`.
- **`crates/copperclaw-host/src/boot.rs`** (`collect_forward_env`) —
  forwards `COPPERCLAW_CODEX_BINARY` and `COPPERCLAW_CODEX_ARGS` into the
  manager's initial `forward_env` at boot.
- **`crates/copperclaw-host/src/config.rs`** — env-var table in the
  module docstring lists the two new keys.
- **`README.md`** — Multiple-providers bullet now advertises the Codex
  subprocess bridge instead of disclaiming it. Configuration table
  lists `COPPERCLAW_CODEX_BINARY` and `COPPERCLAW_CODEX_ARGS`.

### Fixed (container_manager.rs — seven code-review findings)

- **`crates/copperclaw-host/src/container_manager.rs`** (runner_config_for)
  — Finding 1: switching `COPPERCLAW_SKILLS_MODE` from `callable` to
  `inline` between spawns no longer leaves a stale `skills.json` on
  disk for `load_skill` to read.
- **`crates/copperclaw-host/src/container_manager.rs`** (runner_config_for)
  — Finding 2: when the Callable-mode catalogue write fails, the
  prompt now falls back to Inline shape so the agent never sees a
  `load_skill` advert pointing at a missing file.
- **`crates/copperclaw-host/src/container_manager.rs`** (select_callable_skills,
  render_callable_skill_index) — Finding 3: new helper is the single
  source of truth shared by the in-prompt index and the on-disk
  catalogue, so the two cannot disagree about which skills exist.
- **`crates/copperclaw-host/src/container_manager.rs`** (build_spec memory
  mount) — Finding 7: when the per-group memory dir can't be created,
  a session-local `memory/UNAVAILABLE.md` marker is dropped so the
  agent inside the container learns its writes won't persist.
- **`crates/copperclaw-host/src/container_manager.rs`** (set_memory_dir_perms)
  — Finding 8: per-group memory dir is relaxed to `0o775` after
  creation so the operator can `rm` files the container's root user
  wrote into the bind without sudo.
- **`crates/copperclaw-host/src/container_manager.rs`** (read_project_briefing)
  — Finding 11: non-`NotFound` errors reading `COPPERCLAW.md` now surface
  as a `Briefing diagnostics` section in the assembled prompt, so the
  agent can mention the failure if asked.
- **`crates/copperclaw-host/src/container_manager.rs`** (build_skill_system_prompt,
  render_callable_skill_index) — Finding 13a: `skill.name` is now
  passed through `escape_attr` symmetrically with `skill.description`
  at both call sites; defence in depth against an unescaped `&` or `"`.

### Fixed (`create_agent` depth-cap correctness, persistence, poison handling)

- **`crates/copperclaw-modules/src/agent_to_agent.rs`** (~L475/~L536) —
  Finding 4: closed the TOCTOU race where two concurrent
  `create_agent` calls from the same parent could both pass the cap
  check and double-spawn at depth N+1. Hard cap is now re-checked
  under the `spawned` lock just before the cache insert.
- **`crates/copperclaw-modules/src/agent_to_agent.rs`** + new
  **`crates/copperclaw-db/migrations/011_agent_group_subagent_depth.sql`**
  — Finding 5: in-memory depth map reset on host restart, letting a
  depth-3 grandchild re-spawn fresh depth-1 children. Added
  `agent_groups.subagent_depth`; gate reads from DB on cache miss,
  writes through to DB on success.
- **`crates/copperclaw-modules/src/agent_to_agent.rs`** (~L478) —
  Finding 9: replaced `saturating_add(1)` with `checked_add(1)` so a
  parent at `u8::MAX` cannot keep passing the gate. Added a
  `MAX_SUBAGENT_DEPTH_CEILING = 16` clamp in `with_max_depth`.
- **`crates/copperclaw-modules/src/agent_to_agent.rs`** (~L475/~L536) —
  Finding 10: replaced inconsistent `.lock().unwrap()` on the
  `spawned` Mutex with `.lock().unwrap_or_else(std::sync::PoisonError::into_inner)`
  to match the workspace convention.
- **`crates/copperclaw-modules/src/agent_to_agent.rs`** (~L481) —
  Finding 12: orphan rejection path (depth-cap exceeded with no
  resolvable parent) previously returned silently. `warn!` is now
  unconditional so the failure is always auditable.

### Fixed (todo store: atomic writes + recovery from a corrupt file)

- **`crates/copperclaw-mcp/src/tools/todo.rs`** (`read_all` / `write_all`,
  ~line 102 onward) — `write_all` now writes to a sibling `<path>.tmp`
  and `rename`s into place, so a runner panic or SIGKILL mid-write
  leaves either the old or new file intact instead of a truncated
  half. `read_all` no longer hard-errors on a malformed file: it logs
  a warning, quarantines the file as `<path>.corrupt-<unix-nanos>`,
  and returns an empty store so the next mutator starts fresh. Four
  new tests cover the atomic-rename, quarantine, truncated-JSON, and
  add-after-recovery paths.

### Fixed (load_skill rendering + server test brittleness)

- **`crates/copperclaw-mcp/src/tools/load_skill.rs`** (render at ~L180,
  empty-catalogue branch at ~L150) and
  **`crates/copperclaw-mcp/src/server.rs`** (`lists_all_in_process_tools`
  test at ~L163) — extracted an `escape_attr` helper so the rendered
  `<skill name="...">` attribute is entity-encoded symmetrically with
  `description`; added a fast path that returns a clear "catalogue is
  empty" validation error instead of the misleading `(known: )` tail;
  replaced the brittle tail-name assertion with an equality check
  against `build_tool_set()`'s exact name sequence. Two new tests.

### Changed (subagent depth cap raised from 1 to configurable, default 3)

- **`crates/copperclaw-modules/src/agent_to_agent.rs`** — `create_agent`'s
  nesting gate now tracks per-group *depth* rather than a binary
  spawned-or-not flag. `HandlerDeps.spawned` is now
  `HashMap<AgentGroupId, u8>` and the gate computes the new child's
  depth as `parent_depth + 1`, rejecting when that would exceed the
  configured cap. New const `DEFAULT_MAX_SUBAGENT_DEPTH = 3` permits
  layered investigations (A delegates to B which delegates to C)
  without permitting unbounded fork-bombs. New `with_max_depth(u8)`
  builder on `CreateAgentModule` clamps values < 1 to 1. Three updated
  tests (depth-cap rejection at the new cap, intermediate-depth
  acceptance, historical depth=1 behaviour reproducible via
  `with_max_depth(1)`), plus a clamp test.

### Added (opt-in coding skill bundle)

- **`skills/coding-task/SKILL.md`** — disciplines for editing files,
  running tests, deciding when to comment, when to stop. The Copperclaw
  analog of Claude Code's "doing tasks" section, scoped to coding work.
- **`skills/git-commit/SKILL.md`** — staging, commit-message style, and
  the things to never do (amend pushed commits, `--no-verify`, force-
  push, `reset --hard` over uncommitted work).
- **`skills/code-review/SKILL.md`** — reading a diff, what to flag,
  what to ignore, how to summarise. Built on top of the existing
  `git_diff` tool.
- **`skills/testing/SKILL.md`** — finding the suite, interpreting
  failures, deciding when to add a test and when not to.

These are pure markdown files — they activate only when an operator
explicitly selects them via `SkillsSelector::Explicit(...)` on a
group's `container_config.skills`. The default messaging agent's
prompt is unchanged.

### Added (per-agent-group persistent memory mount)

- **`crates/copperclaw-host/src/container_manager.rs`** — `build_spec`
  now adds a second bind mount at `/data/memory/` backed by
  `<groups_dir>/<agent_group_id>/memory/` (created lazily). The mount
  is shared across every session of the same agent group, so memory
  files an agent writes in one chat are visible in the next. Disabled
  when `groups_dir` is unset. Two tests pin the present / absent
  cases.
- **`skills/agent-memory/SKILL.md`** — the auto-memory protocol from
  Claude Code adapted for Copperclaw: four entry types (user, feedback,
  project, reference), a `MEMORY.md` index, kebab-case slugs, and
  `[[name]]` cross-links. Agents read/write via the existing
  `read_file` / `write_file` tools — no new tool. Universal: every
  agent benefits from being able to remember the user across
  conversations.

### Added (todo tracker: per-session self-planning scratchpad)

- **`crates/copperclaw-mcp/src/tools/todo.rs`** — four new MCP tools
  (`todo_add`, `todo_list`, `todo_update`, `todo_delete`) backed by
  `/data/agent_todos.json` in the session dir. Universal (not coding-
  specific) — any agent juggling multi-step work can use the scratchpad
  to remember which steps are done, in-progress, or still pending.
  Items survive runner restarts within the same session but never bleed
  across sessions. Ten unit tests cover happy path, monotonic ids,
  empty-text/unknown-id validation, status transitions, and the
  unused-id error message shape.
- **`skills/todo-tracker/SKILL.md`** — documents the convention: one
  item per step, only one `in_progress` at a time, mark `completed`
  immediately, delete dead items rather than carrying them forward.
  Explicitly *not* a user-facing reminder system (that's `schedule_task`).

### Added (callable skills loader: index in prompt, bodies on demand)

- **`crates/copperclaw-host/src/container_manager.rs`** — new
  `SkillsMode` enum (`Inline` | `Callable`) on `ManagerConfig`.
  `Inline` (default) preserves today's behaviour — every selected
  skill's full SKILL.md body is dumped into the system prompt at spawn
  time. `Callable` emits only a compact `<skill name=… description=… />`
  index in the prompt and writes a per-session `skills.json` (one
  `{name, description, body}` per selected skill) next to `runner.json`.
- **`crates/copperclaw-host/src/config.rs`** — `HostConfig.skills_mode`
  parsed from `COPPERCLAW_SKILLS_MODE`. Unknown values fall back to
  `Inline` with a `WARN` so a typo never silently mutes skills.
- **`crates/copperclaw-mcp/src/tools/load_skill.rs`** — new `load_skill`
  MCP tool. Reads `/data/skills.json` and returns the named skill's
  body wrapped in the same `<skill>` envelope the inline-mode prompt
  uses, so the agent's experience is consistent across modes. Errors
  with an explanatory message when the catalogue is absent (i.e. the
  host is in inline mode and the bodies are already in the prompt).
- New tests:
  - 9 unit tests in `load_skill.rs` covering happy-path body
    retrieval, name-not-found errors with a known-skills hint, missing
    catalogue, malformed JSON, empty-name validation, description
    escaping.
  - 3 new manager-level tests pinning the callable-mode prompt shape,
    `skills.json` contents, the inline-mode no-write guarantee, and
    stale-catalogue cleanup when no skills are selected.
  - 3 config tests for `COPPERCLAW_SKILLS_MODE` default / parse / unknown.
- **Workspace tool inventory test** in `crates/copperclaw-mcp/src/server.rs`
  updated to expect `load_skill` as the new tail of the tool list.

### Added (universal system prompt: preamble, environment, project briefing)

- **`crates/copperclaw-host/src/container_manager.rs`** — every agent now
  receives a structured system prompt with three new sections prepended
  to the existing skill catalogue:
  1. A mode-agnostic `BASE_PREAMBLE` that establishes Copperclaw-agent
     identity, planning discipline, reversibility-aware action-taking,
     tool-selection preferences, and reply conciseness (incl. no-emojis).
     The text is deliberately *not* coding-specific so it applies to
     messaging, support, and any other workload equally.
  2. An `environment_block` carrying today's date, the session id, the
     agent-group id, the in-container working directory, and the
     assistant's display name when set.
  3. An optional project briefing read from `COPPERCLAW.md`. Two sources
     are checked, both optional: `<groups_dir>/<id>/COPPERCLAW.md` (per-
     group) and `<session_root>/COPPERCLAW.md` (per-session). When both
     exist the group briefing precedes the session briefing.
- **`runner_config_for`** now accepts an optional `session_root` and
  delegates prompt assembly to a new top-level `assemble_system_prompt`
  that stitches preamble → environment → briefing → skills.
- 13 new unit tests pin the preamble/env/briefing structure, ordering,
  empty-briefing behaviour, and the assistant-name codepath. The
  existing `runner_config_uses_skill_dir_when_configured` test now also
  asserts the preamble appears so a regression that strips it would
  surface immediately.

### Added (runner provider factory: native Ollama wiring)

- **`crates/copperclaw-runner/src/main.rs`** — replaced the hard-coded
  `AnthropicProvider::new(api_key)` with a `build_provider(&cfg, &env)`
  dispatch on `cfg.provider`. Recognises `"anthropic"` (default),
  `"ollama"` (native `/api/chat` NDJSON via `OllamaProvider::new`),
  `"ollama-shim"` (legacy Anthropic-shaped proxy via
  `OllamaProvider::shim`). Ollama paths read `OLLAMA_BASE_URL` from
  the container env (defaults to `http://localhost:11434`).
- **`crates/copperclaw-runner/src/config.rs`** — new `provider` field on
  `RunnerConfigFile` / `RunnerConfig` with `"claude"` alias for
  `"anthropic"` and a graceful fallback when the value is unknown.
  Five new unit tests pin the alias / fallback semantics.
- **`crates/copperclaw-host/src/container_manager.rs`** — the host's
  `runner_config_for` now emits `provider`, `api_key_env`, and
  `api_base_url` consistent with the chosen provider (Ollama native
  doesn't get `ANTHROPIC_API_KEY` injected; the rotatable
  `anthropic_base_url` doesn't leak into an Ollama runner). Two new
  meta-tests pin the per-provider config shape.
- **Forwarded env**: `OLLAMA_BASE_URL` joins the rotatable set so
  operators can configure it via the host `.env` and rotate via
  SIGHUP without restarting.

### Fixed (CreateAgent permission gate replaces always-allow)

- **`crates/copperclaw-modules/src/agent_to_agent.rs`** — type signature
  of `CreateAgentPermissionCheck` changed from `Fn() -> bool` to
  `Fn(&CreateAgentPermissionCtx) -> bool` so the check sees the
  parent's agent-group id, session id, and requested name. New
  `users_table_check(CentralDb)` factory denies by default and allows
  when (a) any user has been granted global `Role::Owner`/`Admin` in
  `user_roles`, or (b) the parent's scope has a granted Owner/Admin.
  DB read errors fail closed. Three new unit tests pin the deny /
  global-allow / scoped-allow paths.
- **`crates/copperclaw-host/src/boot.rs`** — `install_modules` now wires
  `create_agent_users_table_check(central)` in place of the
  `always_allow()` stub used during initial integration. A fresh
  install with no role grants denies every `create_agent` call until
  the operator grants Owner/Admin.

### Changed (skill body cap tightened to 4 KiB)

- **`crates/copperclaw-skills/tests/coverage.rs`** — `MAX_SKILL_BODY_BYTES`
  drops from 8 KiB to 4 KiB after a prose-cull pass on the nine
  previously-oversize skills (`explore`, `web-search`, `add-mcp-server`,
  `git`, `error-handling`, `web-fetch`, `messaging-context`,
  `customize`, `install-packages`). Adding back content that pushes a
  skill over the cap now means trimming elsewhere in that file, not
  raising the constant. All skills are under the new ceiling; the
  `skill_bodies_under_size_cap` test continues to pin it.

### Fixed (iMessage empty-body silent drop)

- **`crates/copperclaw-channels/imessage/src/adapter.rs`** — the
  `deliver` path used to return `Ok(None)` when the outbound message
  carried no text and no files, which the host's delivery loop
  interpreted as delivered-ok. Replaced with
  `Err(BadRequest("imessage deliver: empty body (no text, no files)"))`
  so the row lands in `dropped_messages` with a visible reason. The
  prior `deliver_empty_text_is_a_noop_when_no_files` test was renamed
  to `deliver_empty_body_is_bad_request_not_silent_drop` and now
  asserts the failure path.

### Added (Mattermost file uploads — two-step `/api/v4/files` + `posts.file_ids`)

- **`crates/copperclaw-channels/mattermost/src/api.rs`** — new
  `upload_file(channel_id, filename, bytes)` (multipart against
  `/api/v4/files`, returns the file id) and
  `create_post_with_files(...)` (POST `/api/v4/posts` with `file_ids`).
  `create_post` is now a thin wrapper.
- **`crates/copperclaw-channels/mattermost/src/adapter.rs`** — the
  `post` action now uploads files into the destination channel and
  attaches their ids on the message. Edit / reaction actions
  reject files with `BadRequest`. Three new tests cover the upload
  flow, the bad-request shape, and the empty `file_infos` path.

### Added (Teams + Google Chat attachments)

- **`crates/copperclaw-channels/teams/src/api.rs`** — `get_channel_files_folder`
  resolves the channel's SharePoint drive + folder ids;
  `upload_channel_file` PUTs bytes to
  `/drives/{drive}/items/{item}:/{filename}:/content`;
  `post_channel_message_with_attachments` includes the references on
  the new message and inlines `<attachment id="…">` markers in the
  HTML body. Chat (1:1 / group) attachments are explicitly rejected
  with `Unsupported` because Graph DM file upload requires delegated
  user-OneDrive auth that the bot's app-only token cannot reach. New
  unit tests cover both the happy-path channel upload and the chat
  rejection.
- **`crates/copperclaw-channels/gchat/src/api.rs`** — new
  `upload_attachment(space, filename, bytes)` (multipart against
  `/upload/v1/spaces/{space}/attachments:upload`, returns the
  `attachmentDataRef.resourceName`) and `send_text_with_attachments`
  (POSTs the message with `attachment[]` containing those names).
  Cards / edits / reactions reject files with `BadRequest`. The
  threaded-reply + attachments combination falls back to a top-level
  post with a `WARN` log because Chat's `messageReplyOption` doesn't
  accept attachments.

### Added (Signal daemon respawn)

- **`crates/copperclaw-channels/signal/src/rpc.rs`** — new
  `SignalSupervisor` wraps `Arc<JsonRpcClient>` behind a poll-based
  watchdog. When the underlying `signal-cli daemon` exits (writer or
  reader task finishes), the supervisor respawns the process with
  exponential backoff (500 ms → 30 s ceiling) and forwards
  notifications from each successive child through a shared mpsc so
  the adapter's notification loop sees the respawn as transparent.
  Adapter-facing trait surface (`RpcTransport`) is unchanged.
- **`crates/copperclaw-channels/signal/src/factory.rs`** — `init` now
  builds a `SignalSupervisor` instead of a bare `JsonRpcClient`.

### Added (Webex sha256 webhook signature with `SignatureAlgo::Auto`)

- **`crates/copperclaw-channels/webex/src/signature.rs`** — new
  `SignatureAlgo::Auto` variant. When configured, the verifier picks
  the concrete algorithm from the incoming signature's hex length
  (40 → sha1, 64 → sha256) before constant-time comparing. Lets
  operators on the Webex sha256 rollout configure `webhook_algo:
  "auto"` and survive the upstream transition without re-configuring.
  `compute_signature` with `Auto` panics (verifier-only).

### Added (X v2 media upload, opt-in)

- **`crates/copperclaw-channels/x/src/api.rs`** — `upload_media_v2`
  posts a multipart upload to `{api_base}/2/media/upload` and reads
  the media id from `data.id` (with a tolerant fallback to the
  top-level `media_id_string` shape some early v2 responses used).
- **`crates/copperclaw-channels/x/src/config.rs`** — new
  `media_api_version` field (`"v1"` default; `"v2"` opts in to the
  new endpoint). `XConfig::from_value` parses `v1`/`v2` (with
  `1`/`2`/`1.1` aliases) case-insensitively. `XAdapter::upload_files`
  dispatches on the configured version.

### Fixed (boot: install CreateAgentModule so create_agent action is no longer inert)

- **`crates/copperclaw-host/src/boot.rs`** — `install_modules` now constructs
  `CreateAgentModule::new(central, data_root, create_agent_always_allow())`
  and adds it to the install list alongside the legacy unit-struct
  `AgentToAgentModule`. Before this fix, Team CA's `CreateAgentModule`
  existed but was never installed; the agent's `create_agent` MCP tool
  emitted system rows that the delivery loop logged as "no handler;
  skipping" and silently marked delivered=ok. Caught by the new
  structural meta-test `every_runner_emit_has_a_host_handler`.
- **`crates/copperclaw-host/tests/action_handler_coverage.rs`** — also
  updated to mirror the production module list. Production and test
  module lists are now in lock-step; the test will fail loudly if
  either drifts.
- Follow-up (now landed): the `always_allow()` stub has been replaced
  with `create_agent_users_table_check(central)`. See the
  "CreateAgent permission gate replaces always-allow" entry above.

### Added (Test (structural): every runner-emitted action has a handler)

- **`crates/copperclaw-host/tests/action_handler_coverage.rs`** — new
  integration test file that ships four structural meta-tests sealing
  the bug class behind today's seven silently-inert subsystems
  (`ask_question` vs `ask_user_question`, `card` vs `send_card`,
  `SchedulingModule::install` no-op, `AgentToAgentModule` registering
  nothing, missing `edit`/`reaction` handlers, swallowed
  `install_packages`/`add_mcp_server` failures). All seven compiled,
  had passing unit tests on both sides, and shipped to production —
  nothing in CI cross-checked the runner's emit set against the
  host's handler set end-to-end.
  Tests:
  (1) `every_runner_emit_has_a_host_handler` enumerates every system
  action name the runner emits as `MessageKind::System`
  (`usage_report`, `edit`, `reaction`, `ask_user_question`,
  `send_card`, `create_agent`, `install_packages`, `add_mcp_server`,
  `schedule`) and asserts each one is either inline-handled in
  `DeliveryService::handle_system` or registered by a built-in
  module via `register_delivery_action`. The module set is captured
  by installing the same module list as
  `boot::install_modules` (`TypingModule`, `MountSecurityModule`,
  `PermissionsModule`, `ApprovalsModule`, `InteractiveModule`,
  `SchedulingModule`, `AgentToAgentModule`, `SelfModModule`) against
  a `MockModuleContext` and reading back `delivery_actions()`.
  (2) `runner_emit_set_matches_source` re-derives the runner emit
  set from `crates/copperclaw-runner/src/tools.rs` (`fn apply_*`
  bodies) and `crates/copperclaw-runner/src/run.rs`
  (`fn emit_usage_report` body) via a brace-matching parser +
  regex over `serde_json::json!({ "<name>": …`; asserts no drift
  from the hard-coded list in (1).
  (3) `host_handle_set_matches_inline_arms` scans
  `crates/copperclaw-host-delivery/src/service.rs` for every
  `if action.name == "…"` arm plus the typed `match action_name`
  block in `try_action_via_adapter`; asserts no drift.
  (4) `every_module_action_name_is_lowercase_snake` — every name
  registered against the dispatcher matches `^[a-z][a-z0-9_]*$`.
  On initial run, test (1) caught one extant gap: `create_agent`
  has a fully-implemented `CreateAgentModule` (added by team-CA)
  but `boot::install_modules` only installs `AgentToAgentModule`
  (the unit-struct interceptor sibling), so the `create_agent`
  delivery action is unwired in production. Tests (2)-(4) pass.
  Tracked as a follow-up: add `CreateAgentModule::new(…)` to the
  `install_modules` vec in `crates/copperclaw-host/src/boot.rs`.

### Added (skill ↔ tool coverage tests + `skills/README.md` conventions)

- **`crates/copperclaw-skills/tests/coverage.rs`** — new integration test
  file pinning the `tools ↔ skills` matrix. Nine tests:
  (1) every `skills/<dirname>/SKILL.md` has frontmatter `name:` equal
  to `<dirname>`; (2) every tool returned by
  `copperclaw_mcp::tools::build_tool_set` is mentioned in at least one
  skill, so the model always learns when to reach for it;
  (3) every backtick-quoted "looks like a tool" token in any skill
  body resolves to a real registry entry (catches typos and
  references to deprecated tools — uses a `VERB_PREFIXES` heuristic
  plus an explicit `NON_TOOL_TOKEN_ALLOWLIST` for schema-field
  tokens); (4) every skill description is at least 30 characters;
  (5) every skill body contains at least one WHEN-trigger word
  (`when`, `use this`, `reach for`, `if you need`, `prefer`,
  `before`, `after`) — lenient (allows up to one skill to lack a
  trigger, currently the meta-skill `discovering-tools`);
  (6) `SkillRegistry::scan` iterates skills in alphabetical order;
  (7) every `SKILL.md` body is under 8 KiB
  (TODO(team-skl): spec target is 4 KiB; bumped to 8 KiB until a
  cull pass on the long-form skills `explore`, `web-search`,
  `add-mcp-server`, etc.); (8) no skill body contains
  unprocessed `{{ }}` template markers or `<TODO>` / `[PLACEHOLDER]`
  WIP markers; (9) the reserved `tools:` frontmatter key, if
  present, lists only real registry tools (currently unused —
  documented in `skills/README.md`). All nine pass against the
  current `skills/` tree without any skill content changes.
- **`skills/README.md`** — new file documenting the conventions the
  coverage tests enforce: kebab-case directory naming, frontmatter
  shape, the WHEN-trigger requirement, the 8 KiB body cap, the
  `allowed-tools:` / reserved `tools:` distinction, and the two
  workflows that need to touch both sides (adding a new skill,
  renaming/deleting an MCP tool).

### Fixed (providers: native Ollama support that actually talks `/api/chat`)

- **`crates/copperclaw-providers/src/ollama.rs`** — replaced the
  Anthropic-Messages shim with a native `/api/chat` NDJSON adapter.
  The previous implementation always hit `<base_url>/v1/messages`, which
  vanilla `ollama serve` does not expose (`404`), so the path only
  worked against a LiteLLM-style proxy fronting Ollama. The native
  adapter now: streams `POST /api/chat` NDJSON frame-by-frame; emits
  `Activity` per content frame for liveness; reassembles
  `message.tool_calls[]` into `ToolStart` + `ToolCall` + `ToolEnd`;
  serialises tools in OpenAI's `{type:"function", function:{...}}`
  envelope; surfaces tool results as `tool` role messages with
  `tool_call_id`; maps `prompt_eval_count`/`eval_count` onto
  `ProviderEvent::Usage`. The shim path remains reachable via the new
  `OllamaProvider::shim(...)` constructor for operators with a
  proxy front-end.
- **`crates/copperclaw-providers/tests/ollama_conformance.rs`** — new,
  12 wiremock conformance tests covering every `ProviderEvent`
  emission path on the native code path (text, tool round-trip,
  streaming heartbeats, abort, usage, model passthrough, tool schema
  translation, tool-result history translation, system prompt
  placement, error classification, empty body, malformed JSON
  recovery).
- **`crates/copperclaw-providers/tests/ollama_live.rs`** — new,
  `#[ignore]`d live test against a real Ollama server. Reads
  `OLLAMA_HOST` (default `http://localhost:11434`) and `OLLAMA_MODEL`
  (default `llama3.1:8b`); run with
  `cargo test --ignored ollama_live -p copperclaw-providers`.
- **`crates/copperclaw-providers/tests/ollama_shim.rs`** — renamed from
  `ollama_sse.rs` and converted to drive `OllamaProvider::shim(...)` so
  the legacy facade path stays pinned against regressions.
- **`docs/providers/ollama.md`** — new audit document covering the
  gap matrix, wire-format notes, and follow-ups
  (`OllamaProvider` is not yet wired into the runner config —
  separate runner-side ticket).
- **`README.md`** — Ollama bullet under "Multiple providers" updated:
  native `/api/chat` is the default; the Anthropic shim remains
  available for proxy-fronted deployments.

### Added (Team CHN: channel adapter audit + edge-case tests)

- `docs/channels/` (NEW) — audit summary plus 21 per-channel reports.
  Confirms zero adapters have `todo!()` / `unimplemented!()` in the
  production deliver path; every adapter either calls the platform or
  returns a typed `AdapterError::Unsupported` / `BadRequest`. One
  MED-severity finding documented (imessage empty body returns silently,
  enshrined in an existing test). Each per-channel doc lists tested
  edges + deferred punch list with line-level pointers.
- `crates/copperclaw-channels/telegram/src/adapter.rs` — 3 new
  adapter-level edge tests: rate-limit retry-after,
  malformed-response-body → Transport, non-object content → BadRequest.
- `crates/copperclaw-channels/slack/src/adapter.rs` — 3 new
  adapter-level edge tests: empty text still posts, non-object content
  as empty text, 429 Retry-After → AdapterError::Rate.
- `crates/copperclaw-channels/discord/src/adapter.rs` — 3 new
  adapter-level edge tests: empty content object still posts,
  non-object content renders as JSON, 429 Retry-After → AdapterError::Rate.

### Fixed (scheduling: persist tasks and fire due ones from the sweep loop)

- **`crates/copperclaw-modules/src/scheduling.rs`** — `SchedulingModule::install`
  now registers a real `"schedule"` delivery action against the host's
  module context. Previously the module's `install` was a literal no-op,
  so every `schedule_task` / `list_tasks` / `cancel_task` / `pause_task` /
  `resume_task` / `update_task` call from the agent produced an outbound
  system row that the delivery loop logged as
  `"no handler for system action; skipping name=schedule"` and dropped
  on the floor. **Live-caught**: the agent reported it had scheduled a
  daily 9am dashboard for the user — and nothing was scheduled. The new
  `ScheduleHandler` drives a `TaskStore` trait (in-memory store for
  tests; the host wires a sqlite-backed `SqliteTaskStore`) and dispatches
  on the payload's `op` field.
- **`crates/copperclaw-db/migrations/010_tasks.sql`** — new `tasks` table
  on the central DB. Columns: `id` (server-generated `task_<uuid>`),
  `agent_group_id`, `session_id`, `name`, `prompt`, `when_spec`,
  `recurrence`, `next_fire`, `status`
  (`active`/`paused`/`cancelled`/`completed`), `created_at`, `updated_at`.
- **`crates/copperclaw-db/src/tables/tasks.rs`** — CRUD module for the
  new table: `insert`, `get`, `list_for_session`, `list_due`,
  `set_status`, `set_next_fire`, `update`.
- **`crates/copperclaw-host-sweep/src/checks/scheduling.rs`** — new sweep
  check called once per pass. For every `active` task with
  `next_fire <= now`, the check synthesises a `kind: task`, `on_wake: true`
  inbound row into the originating session's `inbound.db` and either
  re-arms (recurring tasks bump `next_fire` to the next occurrence) or
  transitions to `completed` (one-shot tasks clear `next_fire`). The
  existing `wake.rs` check then picks up the new pending row and walks
  the container back to `running`.
- **`crates/copperclaw-host-sweep/src/task_store.rs`** — the sqlite-backed
  `SqliteTaskStore` impl of `TaskStore`. Lives in the sweep crate so the
  modules crate stays decoupled from `copperclaw-db`.
- **`crates/copperclaw-host/src/boot.rs`** — boot now constructs
  `SqliteTaskStore::new(host_ctx.central().clone())` and passes it
  through `SchedulingModule::with_store(...)` so created tasks land in
  the same `tasks` table the sweep scans.
- **`crates/copperclaw-modules/src/context.rs`** — `DeliveryActionInput`
  gains `session_id: Option<SessionId>` and `DispatchTarget` derives
  `Default`. The host's delivery service populates both for system
  actions so the `ScheduleHandler` can identify the originating session.
  Existing handlers (`approval_card`, `ask_user_question`, `send_card`)
  ignore the new field.

### Added (modules: wire the `create_agent` delivery action)

- **`crates/copperclaw-modules/src/agent_to_agent.rs`** — the
  `AgentToAgentModule` now registers a `create_agent` delivery action
  via `register_delivery_action`. Previously the runner emitted the
  `{"create_agent": {...}}` system row but the host had no handler, so
  rows fell through to `no handler for system action; skipping
  name=create_agent` and silently dropped the request. The new
  `CreateAgentHandler` parses `{name, instructions, channel}`, gates on
  a configurable `CreateAgentPermissionCheck` closure (production wires
  this to a `users` / `user_roles` lookup; tests use `always_allow`),
  refuses requests originating from previously-spawned agent groups
  (max nesting = 1 to prevent fork-bombs), then `agent_groups::create`
  + `sessions::create` + (when `channel` is set) a synthetic
  `messaging_groups` + `messaging_group_agents` upsert. The container
  manager's reconcile loop picks up the new session on its next tick.
- **Parent notification** — after the central-DB mutations succeed,
  the handler writes a `kind=system` row to the *parent* session's
  `inbound.db` with content
  `{"create_agent_result": {"status": "created", "session_id": "...", "agent_group_id": "..."}}`
  so the calling agent learns the real ids on its next turn (the
  runner's `apply_create_agent` had returned a synthetic ack). Denied,
  rejected (nested), and invalid-payload requests surface a matching
  status row.
- **`crates/copperclaw-modules/src/lib.rs`** — re-exports
  `CreateAgentHandler`, `CreateAgentPermissionCheck`,
  `create_agent_always_allow`, `create_agent_always_deny` for host
  wiring + tests.
- **`crates/copperclaw-modules/Cargo.toml`** — adds `copperclaw-db` as a
  dependency (previously the modules crate avoided the dep by routing
  DB access through closures, but the create-agent flow's CRUD surface
  is too wide to plumb that way cleanly). `tempfile` added under
  `dev-dependencies` for the new tests.
- **Tests**: five new tests in `agent_to_agent.rs` —
  `create_agent_inserts_agent_group_and_session`,
  `create_agent_emits_result_to_parent_inbound`,
  `create_agent_with_channel_creates_wiring`,
  `create_agent_denied_when_permission_missing`,
  `create_agent_refuses_nesting`, plus
  `create_agent_invalid_payload_surfaces_back` and
  `install_registers_create_agent_action_when_deps_present`.

### Added (wire up agent `edit_message` / `add_reaction` end-to-end)

- **`crates/copperclaw-channels/core/src/adapter.rs`** — `ChannelAdapter`
  gains two default-`Unsupported` trait methods, `edit_message` and
  `add_reaction`, so adapters that don't expose those APIs fall
  through cleanly to the host's fallback path.
- **`crates/copperclaw-channels/telegram/src/adapter.rs`** plus
  **`crates/copperclaw-channels/telegram/src/api.rs`** — implements the
  trait against Telegram's `editMessageText` and `setMessageReaction`
  endpoints.
- **`crates/copperclaw-channels/slack/src/adapter.rs`** — implements the
  trait against Slack's `chat.update` and `reactions.add` (strips
  surrounding `:` from the emoji name before forwarding).
- **`crates/copperclaw-channels/discord/src/adapter.rs`** — implements
  the trait against Discord's `PATCH /channels/{id}/messages/{msg}`
  and `PUT /channels/{id}/messages/{msg}/reactions/{emoji}/@me`.
- **`crates/copperclaw-channels/core/src/testing.rs`** — `MockAdapter`
  records `edit_message` / `add_reaction` calls and exposes
  `set_edit_unsupported` / `set_reaction_unsupported` knobs so tests
  can drive the host's fallback path.
- **`crates/copperclaw-modules/src/interactive.rs`** — `InteractiveModule`
  now registers `edit` and `reaction` delivery-action handlers. They
  emit a synthetic chat message of the form `"(edit) <text>"` /
  `"(reaction: <emoji>)"`; the host invokes them only when the
  adapter call falls through.
- **`crates/copperclaw-host-delivery/src/service.rs`** — the
  registered-handler path now intercepts `action.name == "edit"` and
  `"reaction"`, resolves the original message's `platform_message_id`
  via the inbound `delivered` table (joined to `messages_out` by
  seq), and calls the typed adapter API. On `Unsupported`, missing
  external id, or malformed payload, the code falls through to the
  registered handler so the synthetic chat fallback gets dispatched
  through the normal delivery path. The existing hard-coded
  `usage_report` / `install_packages` / `add_mcp_server` paths are
  unchanged.
- **Why this fix matters:** before this change the runner emitted
  `system` rows with `{"edit": ...}` / `{"reaction": ...}` content
  but no handler existed, so the host logged "no handler; skipping"
  and the agent's "(edit / reaction)" tool calls were silent on the
  user-facing channel. Telegram, Slack, and Discord now do the right
  thing; other adapters (CLI, webhooks, etc.) get the fallback chat
  message automatically via the `Unsupported` default.

### Fixed (delivery: surface install_packages / add_mcp_server apply failures)

- **`crates/copperclaw-host-delivery/src/service.rs`** — the
  `install_packages` and `add_mcp_server` system-action handlers no
  longer mark a row `delivered.status="ok"` after the underlying
  `container_configs` update failed. On apply error the row is now
  recorded as `delivered.status="failed"` with the error message in
  the payload (so it surfaces in `cclaw dropped-messages outbound-list`),
  the failure is logged at `error!` (not `warn!`), and a
  `MessageKind::System` row carrying a `self_mod_error` envelope is
  written to the session's `inbound.db` so the agent learns its tool
  call failed and can adapt on the next turn. Without this, the
  agent would loop thinking its install succeeded while the next
  container spawn silently lacked the package.
- New metric counters
  `copperclaw_self_mod_failed_total{action}` and
  `copperclaw_self_mod_succeeded_total{action}` (`action` ∈
  `{install_packages, add_mcp_server}`) — fired on every self-mod
  apply outcome so operators can chart the failure rate.
- New env var `COPPERCLAW_SELFMOD_HARD_FAIL=1` flips failed applies
  into a non-retryable `DeliveryError::SystemAction` so the outer
  delivery loop records the row in `dropped-messages` instead of
  handling the failure inline. Default off; useful for tests + paranoid
  operators that want the message in the failed-deliveries view.
- **`crates/copperclaw-metrics/src/lib.rs`** — new
  `inc_self_mod_failed(action)` / `inc_self_mod_succeeded(action)`
  helpers + `SELF_MOD_FAILED_TOTAL` / `SELF_MOD_SUCCEEDED_TOTAL`
  name constants, following the existing pattern.

### Fixed (runner: route chat outbounds back to the originating channel)

- **`crates/copperclaw-runner/src/tools.rs`** and
  **`crates/copperclaw-runner/src/run.rs`** — when the model emits a
  reply (final assistant text or an explicit `send_message` /
  `send_file` with `to: None`), the `messages_out` row's
  `channel_type` / `platform_id` / `thread_id` / `in_reply_to`
  columns now carry the originating inbound's routing. Before this
  fix those columns were always written as `NULL`, so the host's
  delivery loop had nothing to dispatch by — the model replied
  correctly but the user saw silence. **Live-caught on Telegram**:
  every successful turn produced a chat outbound with empty routing
  and the user got nothing.
- **`crates/copperclaw-mcp/src/context.rs`** — the `ToolContext`
  trait gains `set_originating(...)` / `clear_originating()`
  methods with no-op default impls. The runner's `RunnerToolCtx`
  implements the real plumbing via a `Mutex<OriginatingRouting>`
  field that `run_loop` sets before each turn and clears after.
  Mock contexts and the subagent adapter inherit the no-op default.
- **`fixtures/{cli,discord,github,matrix,slack,telegram,webhooks}/*/expected/messages-out.jsonl`** —
  ten replay fixtures' chat-kind outbound rows updated to expect the
  populated routing columns (previously they pinned the bug by
  asserting `channel_type: null`). The `cli/budget-exhausted` fixture
  keeps `in_reply_to: null` because that reply is host-side, not
  runner-side.

### Fixed (rebuild.sh: don't let `copperclaw-setup --headless` wipe channel config from .env)

- **`rebuild.sh`** — the image-rebake step invokes the full
  `copperclaw-setup --headless` wizard, which rewrites `.env` from
  scratch with only the keys it knows about (`ANTHROPIC_API_KEY`,
  `COPPERCLAW_DATA_DIR`, `COPPERCLAW_DEFAULT_IMAGE_TAG`, etc.) — silently
  dropping channel-specific keys (`TELEGRAM_BOT_TOKEN`,
  `COPPERCLAW_CHANNELS`, `COPPERCLAW_CHANNELS_CONFIG`) and third-party
  provider keys (`TAVILY_API_KEY`, etc.). Caught live: a `./rebuild.sh`
  run silently disabled the Telegram channel by wiping its config.
  Real users would notice nothing — the host log would say
  "channels: cli, telegram" because the literal channel ENUM list
  survives, but the per-channel config and bot token would be gone
  and the Telegram polling would never start.
- The script now snapshots `.env` before invoking setup, runs setup,
  then re-appends any `KEY=VALUE` lines whose `KEY` is missing from
  the post-setup `.env`. Effectively makes the wizard additive for
  the rebuild use case. The proper long-term fix is to add an
  `copperclaw-setup image` subcommand that runs ONLY the image build
  without touching `.env` — filed for a follow-up.

### Fixed (recover from malformed tool_use JSON by feeding the parse error back to the model)

- **`crates/copperclaw-types/src/provider.rs`** — new
  `ProviderEvent::ToolInputParseError { tool_use_id, tool_name, raw_input, parse_error }`
  variant. Emitted by the provider when a `tool_use` content block's
  reassembled `input_json_delta` chunks fail to parse as JSON. Carries
  enough metadata for the runner to synthesise a corrective
  `tool_result` keyed by `tool_use_id`.
- **`crates/copperclaw-providers/src/anthropic.rs`** — on a `tool_use`
  input JSON parse failure (the live-caught `send_file` "EOF while
  parsing an object at line 1 column 37" case), the SSE pump now
  emits `ProviderEvent::ToolInputParseError` followed by
  `ProviderEvent::ToolEnd` instead of a terminal
  `ProviderEvent::Error`. The previous behaviour terminated the
  inbound with only the generic apology row reaching the user.
- **`crates/copperclaw-runner/src/run.rs`** — `pump_events` converts
  the new event into a synthetic `PendingToolCall` tagged with the
  parse error. `drive_turn` recognises these, skips the real tool
  invocation, and pushes a `HistoryMessage::Tool { is_error: true,
  content: "Your tool_use input JSON could not be parsed: <err>.
  Please re-issue this exact tool call with valid JSON." }` so the
  model self-corrects on the next turn (the Anthropic SDK's standard
  pattern). Hard-capped at 3 consecutive parse-error turns per
  inbound; on exhaustion the runner falls through to the existing
  terminal-failure / apology path. Real tool calls emitted in the
  same turn (e.g. a clean `shell` alongside a malformed `send_file`)
  still execute normally.
- **`crates/copperclaw-runner/src/subagent.rs`** — exhaustive-match arm
  added for the new variant. Subagent turns are single-shot, so the
  parse-error path bails the subagent turn (the parent runner is
  where the self-correction loop lives).
- **Tests** — four new tests in `crates/copperclaw-runner/src/run.rs`:
  `malformed_tool_use_recovers_after_one_retry`,
  `malformed_tool_use_gives_up_after_three_attempts`,
  `malformed_tool_use_other_tools_still_work`, and
  `tool_input_parse_error_event_serialization`. Workspace total goes
  from 4,898 → 4,902 passing.

### Added (delivery: plain-text fallback retry for formatting BadRequests)

- **`crates/copperclaw-channels/core/src/adapter.rs`** — new
  `ChannelAdapter::plain_text_fallback(&self, msg) -> Option<OutboundMessage>`
  trait method with a default impl that returns `None`. Adapters whose
  upstream platform has a known formatting-validation failure mode
  (Telegram `MarkdownV2`, Slack block-kit, Discord embeds) override this
  to return a downgraded copy of the outbound message — formatting
  metadata stripped, text body preserved and prepended with
  `"[reduced formatting] "` — that the channel will accept as plain
  text. Default-`None` means "no clean fallback known; fail fast", which
  preserves the previous behaviour for adapters that don't opt in
  (matrix, webhooks, github, etc.).
- **`crates/copperclaw-host-delivery/src/service.rs`** — `call_adapter` now
  inspects `AdapterError::BadRequest(msg)` for a formatting-error
  signature (`parse entities`, `rich text`, `blocks`, `block_kit`,
  `block kit`, `embed`, `embeds`, `format`, `formatting`; case-
  insensitive) via `is_formatting_bad_request`. When matched it calls
  `adapter.plain_text_fallback(message)` and re-issues `deliver` with
  the result. If the fallback succeeds the row is recorded as
  delivered, an info-level "delivered with reduced formatting" log
  line fires, and the new metric
  `copperclaw_delivery_formatting_fallback_total{channel_type}` is
  incremented. If the fallback fails (or the adapter has no
  fallback), the ORIGINAL `BadRequest` is surfaced and the existing
  terminal-failure path takes over — non-formatting BadRequests
  (e.g. "chat_id required") fail fast without a retry.
- **Per-channel `plain_text_fallback` impls** in:
  - `crates/copperclaw-channels/telegram/src/adapter.rs` — strips
    `parse_mode`, keeps `text`. Fixes the regression where the agent
    opting into `parse_mode=MarkdownV2` and emitting natural-language
    text with bare `!` / `.` / `-` / `(` / `)` / `[` / `]` would hit
    Telegram's 400 "can't parse entities" and the user got nothing.
  - `crates/copperclaw-channels/slack/src/adapter.rs` — strips
    `blocks`, keeps the `text` fallback string Slack already requires
    on `chat.postMessage`.
  - `crates/copperclaw-channels/discord/src/adapter.rs` — strips
    `embeds`, keeps `text`.
- **`crates/copperclaw-metrics/src/lib.rs`** — adds
  `DELIVERY_FORMATTING_FALLBACK_TOTAL` constant and
  `inc_delivery_formatting_fallback(channel_type)` helper, alongside
  the existing `inc_delivery_failed`. Surfaced in the metric-name
  prefix / ends-with-`_total` invariants so an operator scraping
  `/metrics` can alert on "delivered but downgraded".
- **`crates/copperclaw-channels/core/src/testing.rs`** — `MockAdapter`
  gains `enable_plain_text_fallback(bool)` and (under the hood) a
  FIFO queue for `fail_next_deliver` so a single test can preload
  multiple consecutive failures — required to exercise both the
  primary deliver AND the fallback retry failing on the same pass.
- Seven new tests pin the behaviour:
  - `plain_text_fallback_strips_parse_mode_for_telegram` /
    `plain_text_fallback_strips_blocks_for_slack` /
    `plain_text_fallback_strips_embeds_for_discord` — per-channel
    unit coverage of the stripping rules.
  - `plain_text_fallback_returns_none_when_already_plain` (telegram)
    — no formatting fields means no fallback.
  - `delivery_retries_with_plain_text_on_parse_entities_error` —
    row delivered after retry, fallback metric incremented.
  - `delivery_marks_failed_when_plain_text_fallback_also_rejected`
    — when both attempts fail, the original terminal-failure path
    runs.
  - `delivery_does_not_retry_on_other_bad_request` — a non-
    formatting BadRequest ("chat_id required") fails fast with no
    fallback attempt.

### Added (sweep: user-visible apology when an inbound is stuck)

- **`crates/copperclaw-host-sweep/src/checks/apology.rs`** — new sweep
  responsibility. On every 60s pass the sweep scans each active session's
  `inbound.db` for chat rows with `status='pending'` and `kind='chat'`
  whose `(now - timestamp) > APOLOGY_AFTER_SECS` (5 min, hard-coded), and
  writes a single user-visible apology chat row to the session's
  `outbound.db` so the delivery loop dispatches it back through the
  channel the inbound arrived on. Routes via the inbound's
  `(channel_type, platform_id, thread_id)` and stamps `in_reply_to` so
  the user sees the apology in the right place. The runner's own
  `emit_terminal_failure_apologies` path is unchanged — this fills the
  gap when the runner never even ran (container spawn broken, runner
  panic before any DB write, heartbeat stale with no recovery).
- **Dedupe via `tries=99` sentinel** — to avoid adding a new DB column,
  the check writes `tries=APOLOGY_TRIES_MARKER (=99)` on the inbound row
  after a successful apology emit. The host's regular retry path tops
  out at `MAX_TRIES=5`, so 99 is safely out-of-band. The query filter is
  `tries < 99`, so a second sweep skips the row.
- **`crates/copperclaw-host-sweep/src/spawn_tracker.rs`** — new in-memory
  `SpawnAttemptTracker` shared between the host's container manager and
  the sweep. The manager calls `record_failure(session_id)` on every
  failed `runtime.spawn(...)` and `record_success(session_id)` on a
  successful spawn. The sweep's apology check reads
  `is_exhausted(session_id)` (>= `SPAWN_FAIL_THRESHOLD = 3` attempts)
  combined with `container_status='stopped'` to fire the
  `reason=container_spawn_failed` branch — which emits the apology even
  for inbounds under the 5-min age threshold, because if the container
  can't come up at all the user shouldn't have to wait 5 min.
- **`crates/copperclaw-metrics/src/lib.rs`** — new counter
  `copperclaw_stuck_inbound_apology_total{agent_group_id, reason}` with
  reason ∈ {`pending_too_long`, `container_spawn_failed`}. Operators
  can alert on it spiking to detect a container that flat-out won't
  start (image corruption, OCI error, OOM at launch).
- **`crates/copperclaw-host/src/container_manager.rs`** — `maybe_spawn`
  now bumps the spawn-attempt tracker on every `runtime.spawn` failure
  and clears it on success. The shared `Arc<SpawnAttemptTracker>` is
  threaded through `with_spawn_tracker(...)` from `boot.rs`, where the
  same tracker is also handed to the sweep service.
- **`crates/copperclaw-host-sweep/src/lib.rs`** — exposes
  `APOLOGY_AFTER_SECS` (=300) and re-exports the new types
  (`ApologyEmit`, `ApologyReason`, `SpawnAttemptTracker`).
- **Tests** — five spec tests in `apology.rs`:
  `stuck_inbound_apology_emits_after_5min`,
  `apology_not_emitted_below_threshold`,
  `apology_only_emitted_once`,
  `container_spawn_failure_emits_apology`,
  `apology_routing_preserves_channel_fields`. Plus unit coverage of
  `SpawnAttemptTracker` and the missing-routing dedupe path.
- The sweep cadence stays at 60s; no new timer or DB schema change.
  Stuck-inbound scan is bounded to 50 rows per session per pass so a
  large outage backlog can't choke the loop.

### Added (boot-time image health check + host degraded mode)

- **`crates/copperclaw-host/src/image_health.rs`** — new module that
  inspects the configured `COPPERCLAW_DEFAULT_IMAGE_TAG` at boot
  before the container manager starts. Three checks:
  1. **Image exists locally** — `docker image inspect <tag>`. A
     missing image is what happens when an operator runs the host
     binaries (e.g. via systemd) without first running
     `./rebuild.sh` to refresh the session image. This is the
     bug-class the change closes.
  2. **Runner binary present + executable** — one-shot
     `docker run --rm --entrypoint /bin/ls <tag> -l /usr/local/bin/copperclaw-runner`
     bounded by a 5 s per-call timeout and `kill_on_drop(true)` so
     a wedged daemon can't monopolise boot.
  3. **Fingerprint compare** — reads the image's
     `copperclaw.fingerprint` label (set by `copperclaw-setup`) and
     compares it to the sha256 of the host's runner binary. A
     mismatch is a WARN, **not** a degrade — fingerprints can
     legitimately differ across architectures and build flavours,
     so we only flag the suspicion.
  The whole pipeline is bounded by an outer 10 s `tokio::time::timeout`.
- **`crates/copperclaw-host/src/boot.rs::run_boot_image_health_check`**
  wires the check into `run_host` between migrations and the
  container-manager spawn. On failure the host enters degraded mode
  via `image_health::enter_degraded_mode`: the metric gauge is set,
  a one-time `"The agent is temporarily degraded — the container
  image is missing or out of date. The operator has been notified."`
  apology row is written to every active session's `outbound.db`
  routed back through its most recent pending chat inbound's channel,
  and the container manager is flipped into refuse-spawn mode via
  the new `ContainerManager::set_degraded()`. The startup log line
  starts with `HOST DEGRADED:` so a quick log tail surfaces it.
- **`crates/copperclaw-host/src/container_manager.rs`** — new
  `ManagerError::HostDegraded` variant; `maybe_spawn` short-circuits
  with it when degraded; the reconcile loop swallows the error so
  the host log isn't spammed every tick.
- **`crates/copperclaw-metrics/src/lib.rs`** — new
  `copperclaw_degraded_state{reason}` Prometheus gauge with five label
  values: `image_not_found`, `runner_binary_missing`,
  `runner_binary_not_executable`, `health_check_timeout`,
  `health_check_failed`. Exposed via `set_degraded_state` /
  `clear_degraded_state` helpers.
- **Tests**: `image_health_passes_when_image_has_runner`,
  `image_health_fails_when_image_missing`,
  `image_health_fails_when_runner_binary_absent`,
  `image_health_warns_on_fingerprint_mismatch`,
  `degraded_mode_refuses_spawn`,
  `degraded_mode_emits_apology_to_pending_inbounds`, plus six more
  defensive cases (label-skip path, transport-error fallback,
  fingerprint-helper edge cases). Workspace tests: 4 898 → 4 910.

### Fixed (rebuild.sh: rebake session image so new runner reaches the agent)

- **`rebuild.sh`** — now also rebuilds the session container image
  (and pins the new sha256 tag in `.env`) after installing fresh
  binaries. Previously a code change to `copperclaw-runner` landed on
  disk but the agent inside the container kept running the old runner
  baked into the stale image, so new tools / new fixes never reached
  the live agent. Caught live: model kept hitting the `send_file`
  malformed-JSON tic on the old image's old runner, with no apology
  emit because that fix only existed in the on-disk-but-unbaked
  binary. The script now triggers `copperclaw-setup --headless` after
  install (with `image` cleared from `setup-state.json`'s completed
  list), reads the resulting image tag, and rewrites
  `COPPERCLAW_DEFAULT_IMAGE_TAG` so the next session spawn picks it up.
- **`rebuild.sh` install list** now includes `copperclaw-runner` so
  the binary the image step bakes in is current.
- **`CLAUDE.md`** — documents the new step in the "Local development
  loop" section.

### Changed (web_fetch: auto-convert HTML responses to markdown)

- **`crates/copperclaw-mcp/src/tools/computer_use.rs`** — `web_fetch`
  now detects HTML responses by Content-Type (`text/html`, including
  parametrised forms like `text/html; charset=utf-8`, plus
  `application/xhtml+xml`) and runs them through the pure-Rust `htmd`
  crate (a turndown.js port) before returning to the model. Markdown
  bodies are typically 5-10x smaller than the raw HTML, dramatically
  shrinking the model's input window for routine URL reads. The
  response gains three new fields when conversion fires —
  `content_type: "text/html → markdown"`, `raw_html_bytes`, and
  `markdown_bytes` — so the agent (and humans skimming traces) can
  tell at a glance what happened. Non-HTML responses (JSON, plain
  text, binary) are returned unchanged.
- **New `raw: true` opt-out** on the tool input — when the agent
  genuinely needs the original HTML (scraping `<meta>` tags, parsing
  embedded JSON-LD, etc.) it can pass `raw: true` and the body is
  returned untouched. Existing call sites without the field continue
  to work unchanged; the only behavioural difference is the body
  string content for HTML responses.
- **`skills/web-fetch/SKILL.md`** — documents the new default
  behaviour and the `raw` flag.
- **`crates/copperclaw-mcp/Cargo.toml`** — adds `htmd = "0.2"`. Pinned
  to 0.2 because 0.3+ require Rust 1.88's let-chains feature and the
  workspace pins 1.85. License is Apache-2.0, MIT-compatible.
- Four wiremock-backed tests pin the new behaviour:
  HTML-with-charset-param converts, plain JSON passes through, the
  `raw` flag suppresses conversion, and a Content-Type unit test
  covers the parser permutations.

### Changed (shell: persist working directory and env vars across calls)

- **`crates/copperclaw-mcp/src/tools/computer_use.rs`** — environment
  variables exported during a `shell` call now persist to subsequent
  `shell` calls in the same session, and `cd` carries forward
  between calls. Previously every call started in `/` with a fresh
  env, forcing the agent to thread `cwd` through every invocation
  and re-export anything it needed. The implementation sources a
  per-session state file (`/data/.shell_state`, where `/data` is the
  session's bind-mounted directory) before running the user's
  command, then captures the resulting `PWD` plus `export -p` and
  writes it back. Long agent workflows — clone a repo, `cd` into it,
  run a multi-call build — now feel like a normal interactive shell.
- **`reset: true` flag** on the tool input wipes the state file
  before running, so the agent can deliberately start clean (e.g.
  after a misconfigured env var).
- **Secret hygiene**: env vars matching `*_TOKEN`, `*_KEY`,
  `*_SECRET`, or starting with `ANTHROPIC_` are filtered out of the
  persisted snapshot so credentials don't bleed into the state
  file. They remain visible within the call that exported them.
- **`skills/shell/SKILL.md`** — documents the new persistence,
  reset, and secret-filtering rules.
- Six new tests pin: env-var persistence, cwd persistence, `reset`
  clears, `ANTHROPIC_*` filter, `_TOKEN`/`_KEY`/`_SECRET` filter,
  and the wrapped-command shape.

### Added (agent tool: `edit_file` for string-replacement edits)

- **`crates/copperclaw-mcp/src/tools/edit_file.rs`** — new in-process
  MCP tool that swaps an exact substring inside an existing file.
  Mirrors Claude Code's `Edit` semantics: `old_string` must appear
  exactly once unless `replace_all` is set, `old_string` must
  differ from `new_string`, and the path must already exist as a
  regular file. Writes go through a sibling temp file in the same
  directory with `fsync` + `rename(2)` so a crash mid-write leaves
  the original intact; the file's mode is restored onto the temp
  before the rename so permissions survive. Removes the token tax
  the agent was paying by re-emitting whole files via `write_file`
  for one-line tweaks.
- **`crates/copperclaw-mcp/src/tools/mod.rs`** — registers
  `edit_file` in `build_tool_set` (alphabetically within the
  computer-use group, before `read_file`). Tool count is now 21;
  the `tool_set_lists_every_in_process_tool` inventory test was
  updated to match.
- **`skills/edit-file/SKILL.md`** — tells the model to prefer
  `edit_file` over `write_file` for modifications, to `read_file`
  first to capture enough surrounding context for a unique match,
  and to reach for `replace_all` only on renames / refactors.
  (Directory uses kebab-case `edit-file` to match the skill
  registry's `[a-z0-9][a-z0-9-]{0,63}` rule; the underlying MCP
  tool is `edit_file`, snake_case like its peers.)
- **`README.md`** — bumps the "20 tools" copy to 21 and lists
  `edit_file` under computer-use.

### Added (agent tools: `grep` and `glob` for structured filesystem search)

- **`crates/copperclaw-mcp/src/tools/grep.rs`** — new in-process tool
  that regex-searches files under a path and returns structured
  `{path, line, text, context_before, context_after}` rows. Uses
  the `ignore` crate (the same one `ripgrep` uses) for `.gitignore`-
  aware traversal and the `regex` crate for matching. Default cap of
  100 results with a hard ceiling of 1000, per-line byte cap of 4 KiB
  (truncated on a UTF-8 char boundary with a `…[truncated]` marker),
  binary files skipped automatically by NUL-byte sniff, and
  `target/` / `node_modules/` / `.git/` skipped unconditionally
  on top of whatever `.gitignore` says. Optional flags: `glob`
  filename filter (e.g. `*.rs`), `case_insensitive`, `context_lines`
  (cap 20), and `no_ignore` to bypass `.gitignore`/`.ignore` for
  cases like log file search.
- **`crates/copperclaw-mcp/src/tools/glob.rs`** — companion tool that
  lists files under a path matching a gitignore-style glob. Uses
  `globset` for the pattern and the same `ignore`-walker for
  traversal. Default cap of 1000 results with a hard ceiling of
  10000. Returns sorted paths (workspace-relative when the search
  root was relative, absolute otherwise) so callers can snapshot
  the output reliably. No matches returns an empty array, not an
  error.
- **`skills/grep/SKILL.md`** and **`skills/glob/SKILL.md`** —
  auto-loaded skill docs telling the agent when to reach for these
  tools over `shell rg` / `shell find`. Both stress the
  structured-output win (no parsing) and explain the cap / ignore /
  binary-skip semantics.
- **Workspace `Cargo.toml`** — three new pinned workspace deps:
  `ignore = "0.4"`, `globset = "0.4"`, and `regex = "1"`.
- The new tools land in `build_tool_set()` alphabetically among the
  computer-use family, bringing the in-tree tool count from 20 to
  22. Existing schema-stability tests pass; the
  `tool_set_lists_every_in_process_tool` inventory test is updated.

### Added (agent tools: native git inspection via libgit2)

- **`crates/copperclaw-mcp/src/tools/git_status.rs`,
  `git_log.rs`, `git_diff.rs`, `git_blame.rs`** — four read-only
  git tools, backed by `git2` (libgit2 with the `vendored-libgit2`
  feature, so no host-side libgit2 install required). Output is
  structured JSON instead of `git ...` text the model has to
  parse:
  - `git_status` — branch, ahead/behind vs upstream, and per-file
    staged / unstaged / untracked lists with porcelain letter
    flags. Handles unborn HEAD (`git init`) and detached HEAD
    gracefully.
  - `git_log` — commit objects with `sha`/`short_sha`/`author`/
    `email`/RFC3339 `date`/`subject`/`body`/`files_changed`.
    Supports `ref`, `max_count` (default 20, cap 200), `since`
    (ISO date or RFC 3339), and a `files` pathspec filter.
  - `git_diff` — unified patch text plus a per-file
    additions/deletions summary. Working-tree mode when both
    `from` and `to` are omitted; ref-to-ref otherwise. `context`
    knob (default 3) and `max_bytes` cap (default 200 KiB, hard
    cap 1 MiB) with a `truncated` flag.
  - `git_blame` — per-line blame rows with short SHA / author /
    RFC 3339 date / line text. Range via `from_line`/`to_line`;
    out-of-bounds clamps to the file's actual size.
- **`crates/copperclaw-mcp/src/tools/git_common.rs`** — shared
  repository discovery, path resolution, libgit2 error wrapping,
  and short-OID / RFC 3339 helpers so the four tools render
  errors identically.
- **`crates/copperclaw-mcp/src/tools/mod.rs`** — registers all
  four entries in `build_tool_set()`. The crate's smoke test in
  `lib.rs` notes git tools test themselves (they need an on-disk
  repo the smoke harness doesn't stand up).
- **`skills/git/SKILL.md`** — one combined skill covering when
  to reach for each of the four tools, common patterns ("what
  changed in the last hour", "who wrote this function", "is the
  working tree clean"), and the explicit "these are read-only;
  hand mutations back to the operator" reminder.
- **`crates/copperclaw-mcp/Cargo.toml`** — pins `git2 = "0.19"`
  with `default-features = false, features = ["vendored-libgit2"]`
  so the build is self-contained (cmake + cc pulled in at
  compile time only; the resulting binary statically links
  libgit2). Workspace clippy stays clean at `-D warnings`; 23
  new unit tests cover every tool's happy path, validation
  errors, range clamping, truncation, empty-repo handling, and
  ref-not-found.

### Added (agent tools: `explore` — lightweight in-process subagent)

- **`crates/copperclaw-mcp/src/tools/explore.rs`** — new `explore` tool
  that opens a bounded LLM loop against the same upstream the parent
  runner uses (same provider, same model, same API key, same base
  URL) and returns a single summary string. Built for "go look at
  these files and tell me what's there" without the cost of
  `create_agent`'s full container spawn. Default budgets: 5 LLM
  turns, 50_000 cumulative input tokens, 60s wall-clock. Hard caps:
  10 turns, 200_000 tokens. Read-only tool allowlist by default
  (`grep`, `glob`, `read_file`, `web_fetch`); caller can pass an
  explicit `tools` array to widen. Nested `explore` (subagent calling
  `explore` from inside itself) is refused at validation. Tool count
  in `build_tool_set` goes from 20 to 21; the smoke test in
  `crates/copperclaw-mcp/src/lib.rs::smoke` and the order pin in
  `crates/copperclaw-mcp/src/server.rs::tests` are updated accordingly.
- **`crates/copperclaw-mcp/src/context.rs`** — adds `SubagentRequest`,
  `SubagentResult`, `SubagentToolCall` types, plus a new
  `ToolContext::spawn_subagent` trait method with a default impl that
  returns `ToolError::Context("subagent not supported in this
  context")`. `MockToolContext` records subagent calls and returns
  canned results so the `explore` tool's unit tests stay
  transport-free.
- **`crates/copperclaw-runner/src/subagent.rs`** — new module containing
  `run_inner_loop`, the slimmed-down sibling of `run::drive_turn`. It
  does not touch `outbound.db`, does not emit `send_message`, does
  not write `usage_report`, and filters the tool inventory to the
  caller's allowlist. Wall-clock + token-budget gates are polled
  cooperatively *between turns* so the partial last-assistant-text
  survives an overrun; the hard `tokio::time::timeout` lives in
  `explore.rs` as the outer fallback. Canonical exit summaries:
  `"explore stopped: max_turns reached"`, `"explore stopped: token
  budget exceeded"`, `"explore stopped: wall-clock timeout"`,
  `"explore stopped: provider error"`.
- **`crates/copperclaw-runner/src/tools.rs`** — `RunnerToolCtx` gains
  optional `SubagentRunnerDeps` (provider + tool_map + model + system
  prompt + per-turn max_tokens + provider deadline) wired in via a
  new `with_subagent(...)` builder method. `spawn_subagent` flips a
  re-entrancy guard so a subagent's own tool calls can write to
  `outbound.db` but can never recurse into another full subagent
  loop. The subagent's `ToolContext` is a fresh `SubagentCtxAdapter`
  whose `spawn_subagent` impl unconditionally refuses, giving us
  defense-in-depth against the nested case.
- **`crates/copperclaw-runner/src/main.rs`** — populates the
  `SubagentRunnerDeps` after building the tool map / provider /
  config, so the `explore` tool is fully wired the moment the runner
  starts.
- **`skills/explore/SKILL.md`** — usage guidance for the model:
  prefer `explore` for any question needing 3+ file reads or 2+
  search queries; pass a self-contained `task` (the subagent does
  not see the parent's history); keep the read-only default unless
  you have a concrete reason.
- **`README.md`** — Agent tools section bumped from 20 → 21 and the
  new tool documented.

### Fixed (runner: surface a reply when a turn fails terminally)

- **`crates/copperclaw-runner/src/run.rs`** — `finalize_messages` now
  emits a one-line chat outbound to the originating channel when an
  inbound is marked `failed`. Previously the user just saw the typing
  indicator clear with no reply, because all the host-side delivery
  code routes from `messages_out` rows and the runner emitted none on
  failure. Caught live on Telegram: model produced a malformed
  `send_file` tool_use JSON (`EOF while parsing an object at line 1
  column 37`), runner classified it terminal, inbound went to
  `status=failed`, and the user was left staring at silence.
  `emit_terminal_failure_apologies()` copies the inbound's routing
  (`channel_type` / `platform_id` / `thread_id`) into a Chat row with
  `in_reply_to = inbound.id` so the delivery loop dispatches the
  apology back through the same channel adapter. System / task / wake
  inbounds are skipped (no user on the other end). Pinned by
  `terminal_failure_emits_apology_to_originating_channel` —
  `fixtures/cli/provider-timeout` was updated to expect the new
  outbound row.

### Fixed (dev loop: skills now actually load)

- **`rebuild.sh`** — symlinks `<install_root>/data/skills` at the
  repo's `skills/` directory so dev edits to `SKILL.md` files land
  in the next session spawn without manual copying. Caught live:
  `COPPERCLAW_SKILLS_DIR` defaults to `<install_root>/data/skills`
  but setup never copied the repo's skills into that path. Result:
  the running session had an EMPTY system prompt (verified:
  `runner.json:system` was `""`), every skill we'd authored was
  invisible to the agent, and the identity skill in particular
  didn't fire when the user asked "what is Copperclaw?" — the model
  pulled from training data and described a tabletop RPG.
- **`CLAUDE.md`** — documents the symlink + the gotcha for the
  next contributor.

### Fixed (container rebuild: preserve runner binary)

- **`crates/copperclaw-host/src/container_manager.rs`** —
  `rebuild_image` now bases per-group image rebuilds on the install's
  `default_image_tag` (which has `/usr/local/bin/copperclaw-runner`
  baked in at setup time) instead of bare `debian:trixie-slim`. The
  rebuild Dockerfile only adds layers (apt / npm / labels); it never
  re-COPIES the runner binary. Caught live: agent on this box
  emitted `install_packages` for `git`/`nodejs`/`npm`, the host's
  M13 auto-apply flow triggered a rebuild against debian-slim, the
  resulting image had apt packages but no runner, and every
  subsequent `runc create` failed with `stat
  /usr/local/bin/copperclaw-runner: no such file or directory`. New
  `resolve_rebuild_base()` helper picks the default tag when set,
  falls back to `debian:trixie-slim` only when default is empty
  (tests). Two regression tests:
  `rebuild_base_prefers_default_image_tag` and
  `rebuild_base_falls_back_when_default_unset`.

### Added (skill: agent identity)

- **`skills/identity/SKILL.md`** — auto-loads into every agent's
  system prompt and teaches the agent that it's an Copperclaw agent.
  Previously the agent answered "who are you?" with the model's
  generic Claude-or-AI-assistant intro, denying any connection to
  Copperclaw (caught live: agent told a user "I'm not Copperclaw — I'm
  an AI assistant"). The skill names the system, describes the
  per-session container runtime + channel brokering, and includes
  three example phrasings to anchor the answer.

### Fixed (setup: telegram channel now ships fully wired)

- **`crates/copperclaw-setup/src/steps/quickstart_group.rs`** —
  `quickstart_group` now handles `first_channel = telegram` (previously
  only `cli`).  Closes the live gap I hit on this box: after the
  channel step persisted `TELEGRAM_BOT_TOKEN`, I still had to manually
  (a) add `COPPERCLAW_CHANNELS=cli,telegram` to `.env`, (b) add
  `COPPERCLAW_CHANNELS_CONFIG='{"telegram":{"bot_token":"...","mode":"long_poll"}}'`
  (single-quoted so dotenvy parses it), (c) `cclaw messaging-groups
  create --channel-type telegram --platform-id <chat_id>`, (d)
  `cclaw wirings create --mg ... --ag ... --engage pattern --pattern '.*'`,
  and (e) `cclaw approvals approve --channel telegram --identity <chat_id>`.
  All five now happen automatically when setup completes.
- New helper `bootstrap_telegram_install(db, cfg, name)` writes the
  channel-enable env vars + creates an agent group + (when the channel
  step captured `TELEGRAM_CHAT_ID`) creates the messaging-group,
  wiring, and sender approval. When no chat_id was captured the agent
  group + env vars still land so the runtime
  `unregistered_senders` flow can complete the wiring on first inbound.
- Three new tests:
  `bootstrap_telegram_install_writes_env_vars_and_db_rows_with_chat_id`
  pins the full-wire path; `..._without_chat_id_still_enables_channel`
  pins the minimal path; `..._errors_without_token` pins the
  channel-step-must-run-first contract.

### Fixed (runner: retry on transient stream errors)

- **`crates/copperclaw-providers/src/anthropic.rs`** — SSE
  transport/decode failures are now tagged `retryable: true` (was
  `false`). These almost always represent a dropped connection or
  malformed chunk mid-stream, not a fundamental upstream problem.
- **`crates/copperclaw-runner/src/run.rs`** — `run_llm_turn` now wraps
  `query + pump_events` in a second retry layer (in addition to the
  query-level retry Team Q added). When `pump_events` returns a
  failure tagged `retryable_failure=true` and there are attempts
  left, the whole call is re-issued with the same 250ms / 500ms / 1s
  exponential backoff and the same `MAX_PROVIDER_ATTEMPTS=3` cap.
  Closes the gap caught live with a Telegram message ("Where are you
  running") that produced a `usage_report` with `status=error`,
  `input_tokens=0`, and a `failed` inbound after OpenRouter dropped
  the SSE stream once. With the retry in place the second attempt
  succeeds and the agent replies. Two new tests:
  `retryable_stream_error_retries_then_succeeds` pins the new path;
  the existing `error_event_marks_inbound_failed` continues to cover
  the non-retryable terminal case.
- **`LlmTurnOutput.retryable_failure`** — new bool field carrying the
  classification through pump_events back to the caller.

### Fixed (telegram: plain-text default for outbound)

- **`crates/copperclaw-channels/telegram/src/adapter.rs`** — `DEFAULT_PARSE_MODE`
  flipped from `"MarkdownV2"` to `""`. The previous default unconditionally
  told Telegram to parse outbound text as MarkdownV2, but the agent generates
  natural-language replies that contain bare `!`, `.`, `-`, `(`, `)`, `[`,
  `]` etc. — every one of those is reserved in MarkdownV2 and Telegram
  rejects the send with HTTP 400 ("can't parse entities") unless the agent
  backslash-escapes them. Plain text now round-trips literally; the agent
  can still opt into a specific mode by setting `content.parse_mode =
  "MarkdownV2"` (or `Markdown` / `HTML`) on the outbound row. New regression
  test `deliver_text_omits_parse_mode_by_default` pins the contract.

### Removed (dead `pending_sender_approvals` module)

- **`crates/copperclaw-db/src/tables/pending_sender_approvals.rs`** and
  the `pending_sender_approvals` table from migration `001_initial.sql`
  are gone. The CRUD module shipped with full schema + insert/select +
  12 unit tests but no host code ever called it. The real
  sender-approval flow uses `unregistered_senders` (audit / dedup) and
  `users` (the approved-sender truth set): the router writes the
  unregistered row on every unknown-sender inbound, the approvals
  module's host-side notifier reads it for dedup before posting the
  in-channel "approve this sender?" prompt, and
  `cclaw approvals approve_sender` upserts into `users`. With no
  release yet on the `001_initial` schema the table is removed in
  place rather than via an additional drop migration. Doc strings in
  `crates/copperclaw-modules/src/{approvals.rs,context.rs}` and
  `skills/approvals/SKILL.md` updated to point at the real table.

### Added (runner: provider retry loop + per-call deadline)

- **`crates/copperclaw-runner/src/run.rs`** — `provider.query()` is now
  wrapped in an exponential-backoff retry loop with a per-attempt
  deadline. The new helper `query_with_retry()` honours
  `ProviderError::is_retryable()` (5xx, transport, overload retry; 4xx
  and `SessionInvalid` fail-fast), retries up to
  `MAX_PROVIDER_ATTEMPTS = 3` times with 250ms → 500ms → 1s backoffs,
  and wraps each attempt in `tokio::time::timeout(provider_deadline,
  ...)`. Terminal failures mark the inbound `status='failed'` via the
  existing `finalize_messages` path; the runner never panics.
- **`crates/copperclaw-runner/src/run.rs`** — new `provider_deadline`
  field on `RunnerDeps`, defaulting to
  `DEFAULT_PROVIDER_DEADLINE_MS = 60_000`. Configurable per-process via
  the new env var `COPPERCLAW_RUNNER_PROVIDER_DEADLINE_MS` (clamped to
  the `[30_000, 300_000]` ms range; out-of-range values warn and fall
  back to the default). `resolve_provider_deadline(env)` is re-exported
  from the crate root so the runner binary picks it up at startup.
- **`crates/copperclaw-providers/src/error.rs`** — new
  `ProviderError::DeadlineExceeded { deadline_ms, attempts }` variant
  emitted by the runner once all retries trip the per-call deadline.
  Non-retryable; carries the deadline and attempt count so log scrapers
  can spot flapping upstreams.
- **`crates/copperclaw-metrics/src/lib.rs`** — two new counters:
  `copperclaw_provider_retry_total{provider}` (fires once per retry
  decision) and `copperclaw_provider_deadline_total{provider}` (fires
  when the retry budget is exhausted by deadline trips).
- **`crates/copperclaw-host/tests/replay.rs`** — un-`#[ignore]`d
  `cli_provider_5xx_retry` and `cli_provider_timeout`; both pass
  against the new runner behaviour. The harness sets a short
  `provider_deadline` (200ms) so the timeout fixture finishes in well
  under a second.
- **`fixtures/cli/provider-timeout/manifest.json`** — updated to mount
  three `kind=timeout` mocks (one per retry attempt) and bumped
  `step_timeout_ms` to 10s to accommodate the worst-case retry budget.

### Added (budget-gate Prometheus counters)

- **`copperclaw_budget_exhausted_total{agent_group_id, gate}`** — fired by
  `ContainerManager::maybe_spawn` every time the budget or rate-limit
  gate refuses to spawn. `gate` is one of `daily_tokens`,
  `turns_per_minute`, `turns_per_hour`. Operators can now alert on
  "budget exhausted spike" with
  `sum by (agent_group_id, gate) (rate(copperclaw_budget_exhausted_total[15m])) > 0`
  instead of grepping logs.
- **`copperclaw_budget_exhausted_replies_total{agent_group_id}`** — fired
  when the in-channel "budget exhausted" notice is actually written to
  outbound (i.e. AFTER the per-group dedup window check).
- **`copperclaw_budget_exhausted_suppressed_total{agent_group_id}`** —
  fired when a refusal notice is suppressed by the per-group dedup
  window. Pair with the replies counter to see the user-visible
  notification rate independent of refusal volume.
- The three counters land on the existing `COPPERCLAW_METRICS_ADDR`
  endpoint automatically — no new opt-in. `docs/observability.md` and
  the README counter list were updated. New helpers
  `copperclaw_metrics::inc_budget_exhausted{,_reply,_suppressed}` and the
  `BUDGET_GATE_*` label constants are added without changing any
  existing public symbols in `copperclaw-metrics`.

### Added (replay-fixture coverage for tool-use loop)

- **`fixtures/cli/tool-use-shell/`** — new replay fixture that drives
  one CLI inbound (`run 'echo hello'`) through the runner's tool-use
  outer loop. Two Claude turns: turn 1 is a `tool_use` content block
  requesting the `shell` tool with `command: "echo hello"`; the runner
  executes real bash, feeds the `tool_result` back; turn 2 streams the
  final assistant text. Asserts the full inbound → router → runner →
  outbound → delivery pipeline still completes when the model uses a
  tool mid-turn. Backed by `cli_tool_use_shell` in
  `crates/copperclaw-host/tests/replay.rs`. No harness changes were
  needed: `mount_claude_turns` already dispenses pre-recorded turns
  sequentially across all LLM calls (not just one per inbound).

### Added (failure-mode replay fixtures)

- Three new fixtures under `fixtures/cli/` that exercise the runner's
  and host's failure modes deterministically:
  - **`empty-llm-response/`** — LLM returns a successful turn with no
    content blocks. Pins the `drive_turn` no-content branch: inbound
    completes, usage_report is still written, no chat outbound emitted.
    Active in `replay.rs`.
  - **`provider-5xx-retry/`** — first `/v1/messages` call returns 503,
    second succeeds. Documents the post-retry shape an eventual
    `provider.query()` retry loop should land. `#[ignore]`d in
    `replay.rs` until that retry exists.
  - **`provider-timeout/`** — provider hangs past the per-call budget.
    Documents the give-up-and-mark-failed shape an eventual runner-side
    deadline should land. `#[ignore]`d in `replay.rs` until that
    deadline exists.
- **`crates/copperclaw-host/tests/replay/fixture.rs`** — new optional
  `provider_responses` array on the fixture manifest. Each entry is one
  scripted response: `{"kind": "success", "file": "001-turn.json"}`,
  `{"kind": "error", "status": 503}`, or
  `{"kind": "timeout", "delay_ms": 60000}`. When absent, the harness
  keeps the legacy "i-th `claude/NNN-turn.json` for the i-th request"
  behaviour, so existing fixtures stay untouched.
- **`crates/copperclaw-host/tests/replay/harness.rs`** — honours the new
  field via `mount_provider_responses`, and now captures (instead of
  panicking on) per-turn `run_loop` errors so failure-mode fixtures
  can snapshot post-state even when the runner bails. Three new
  `#[tokio::test]` entries in `replay.rs`.

### Added (operational-gate replay fixtures)

- Three new replay fixtures exercise host gates that previously had no
  fixture coverage. Together they take the M11 acceptance gate from
  4,782 to 4,785 passing tests with the rest of the suite unchanged.
  - **`fixtures/cli/sender-not-approved/`** — drives the approvals
    sender-scope gate. An inbound from an unknown `cli:stranger`
    identity hits the gate, the router returns
    `RouteOutcome::Pending`, and the approvals module's new-pending
    notifier dispatches an in-channel "approve this sender?" notice
    through the delivery dispatcher. Asserts no `messages_in` /
    `messages_out` row was written.
  - **`fixtures/cli/budget-exhausted/`** — seeds `group_budgets`
    (`daily_token_cap = 100`) plus an `agent_turns` row for 200 tokens
    spent today. The container manager's budget gate refuses to spawn,
    writes the "budget exhausted" reply to `messages_out`, and the
    delivery loop fans it through cli. A second inbound exercises the
    per-agent-group dedup window — only one reply is posted within
    the hour.
  - **`fixtures/cli/scheduled-wake/`** — pre-seeds an `idle` session
    plus a `messages_in` row with `process_after` in the past and
    `kind = 'task'`. The harness runs a single
    `SweepService::run_once()` pass; the wake check transitions the
    session to `running`; the in-process runner serves a canned
    Claude reply; the delivery loop fans it out.
- **`crates/copperclaw-host/tests/replay/harness.rs`** — extends the
  replay harness with three small seams to drive the above:
  - `Manifest.gates: ["approvals" | "budget"]` opt-in. The harness
    installs `ApprovalsModule` (with a `users`-table persistent
    lookup and a notifier that dispatches through the delivery
    adapter) on the router's hook chain, or drives a cached
    `ContainerManager::tick()` instead of an in-process runner so
    the daily-token-cap gate fires + dedupes correctly across steps.
  - `Manifest.trigger_sweep: true` runs a `SweepService::run_once()`
    pass after seed but before any inbound events, then runs a turn
    + delivery pass for every woken session.
  - Optional `inbound.sql` file applied to every active session's
    `inbound.db` so fixtures can seed due-now `messages_in` rows
    without going through the router. `RouteOutcome::Pending` is now
    a non-fatal outcome for approvals-gated fixtures.

### Added (E2E chat round-trip integration test)

- **`crates/copperclaw-host/tests/e2e_chat.rs`** — boots
  `copperclaw_host::run_host` in-process against a tempdir install root,
  mounts a `wiremock` Anthropic-flavoured streaming stub, writes
  `"hello\n"` into the cli channel's real FIFO, and asserts the mocked
  reply (`"hi from the mock"`) appears in `<install_root>/chat.log`.
  The host's container manager is left disabled and an in-process
  runner driver (mirroring `replay/harness.rs`'s seam) processes
  inbound for each new session, so the test runs without Docker or
  network access. A second smaller test drives `cclaw chat
  --no-autostart` via `copperclaw_cclaw::run_cli` against a missing
  FIFO and asserts the friendly "run `copperclaw start`" hint. This
  pair is the gate that would have caught the FIFO-vs-stdin wiring
  bug that motivated M11.

### Added (setup wizard e2e harness)

- **End-to-end wizard integration test** at
  `crates/copperclaw-setup/tests/wizard_e2e.rs`. Drives the full step
  loop against a fresh `tempfile::tempdir` and asserts the install
  layout an operator would actually rely on: central DB migrated to
  `expected_central_schema_version()`, `.env` with the right keys at
  mode `0600`, `chat.fifo` is a FIFO, `chat.log` is a regular file at
  mode `0600`, `setup-state.json` records the completed steps, and the
  central DB has exactly one agent group + `(cli, stdin)` messaging
  group + wiring. Four scenarios: happy path, idempotent re-run,
  partial-failure recovery (auth step fails on a read-only data dir,
  then resumes after the lock is lifted), and downgrade refusal
  (manually bumping `schema_version` past the binary's expected count
  must surface a schema-mismatch error). Skips the container-image
  build and runs with `service_scope=print` so no real systemd /
  launchd units are touched.

### Changed (setup wizard schema-mismatch guard)

- **`central_db` step now refuses to run against a future schema.**
  Mirrors `copperclaw_host::boot::check_schema_version`: if the on-disk
  `schema_version` table reports more applied migrations than
  `expected_central_schema_version()`, the step returns an error
  rather than silently running migrations against a DB that was
  migrated by a newer binary. This protects operators who try to
  downgrade copperclaw without restoring from a backup.

### Added (install.sh integration test)

- **Containerised integration test for `install.sh`** at
  `tests/install/test_install_sh.sh`.  Spins up a clean Ubuntu 24.04
  container, mounts the repo read-only, and drives the installer
  through four scenarios: (1) missing-Docker clean-failure path,
  (2) full binary install via `cargo install --path` (opt-in via
  `COPPERCLAW_INSTALL_TEST_RUN_BUILD=1`; default-skipped because it
  adds ~5 minutes), (3) re-run idempotency — pre-existing binaries
  survive a dry-run re-invocation, (4) platform detection across all
  four supported triples plus an explicit `COPPERCLAW_RELEASE_TAG`.
  Default suite runtime: ~3 s after the image is cached.
- New CI job `install-sh` in `.github/workflows/ci.yml` runs the
  suite on `ubuntu-latest` and shellchecks both files, with a
  path-filter (`install.sh`, `tests/install/**`, the workflow
  itself) so the job is skipped on unrelated PRs.
- Three test-only escape hatches added to `install.sh`,
  default-off and silent unless explicitly set:
  `INSTALL_SH_SKIP_DOCKER_CHECK=1` skips the container-runtime
  check; `COPPERCLAW_INSTALL_DRY_RUN=1` prints the tarball URL the
  installer would fetch and exits 0; `COPPERCLAW_FORCE_TARGET=<triple>`
  overrides platform detection for the URL test.

### Added (replay fixture coverage — round 2)

- **Four new replay fixtures** under `fixtures/`, lifting in-tree
  coverage from 3 channel types to 7:
  `discord/inbound-message/` (Discord guild-channel message),
  `matrix/room-message/` (Matrix `m.room.message` `m.text`),
  `github/webhook-issue-comment/` (GitHub `issue_comment.created`),
  and `webhooks/generic-hmac/` (generic HMAC-signed webhook, e.g.
  Grafana / Stripe / Sentry style). Each runs through the existing
  in-process `ReplayHarness` in `crates/copperclaw-host/tests/replay.rs`
  via four new `#[tokio::test]` entries, exercising the inbound ->
  router -> runner -> outbound -> delivery pipeline for those channel
  types against the harness's per-channel-type `MockAdapter`s.

### Added (replay fixture coverage)

- **Three new replay fixtures** under `fixtures/`:
  `telegram/inbound-text-message/`, `slack/event-message/`, and
  `cli/multi-turn/`. Each runs through the existing in-process
  `ReplayHarness` in `crates/copperclaw-host/tests/replay.rs`. The
  telegram and slack fixtures exercise the inbound -> router ->
  runner -> outbound -> delivery pipeline for those channel types
  (against `MockAdapter`s pre-registered in the harness), and
  `cli/multi-turn` drives two inbound chat lines and two Claude turns
  through a single shared session to assert runner state continuity.
- **Harness now pre-registers a `MockAdapter` for each known channel
  type** (`cli`, `telegram`, `slack`, plus whatever the fixture
  manifest names if it falls outside that list) and aggregates
  `deliver()` calls across them. `expected/delivered.jsonl` rows now
  include a `channel_type` field so multi-channel fixtures can assert
  per-channel routing.
- **Harness test entry points are deduplicated** behind a single
  `run_fixture(channel, scenario)` helper. Adding a new fixture is
  now a one-line `#[tokio::test]` in `crates/copperclaw-host/tests/replay.rs`.

### Fixed (cli channel bridge)

- **`cclaw chat` now actually reaches the host.** The cli channel
  adapter previously read from the host process's own `tokio::io::stdin()`
  and wrote outbound replies to `tokio::io::stdout()` — so messages
  typed into `cclaw chat` (which wrote to `<install_root>/chat.fifo`)
  were never picked up, and replies were never appended to
  `<install_root>/chat.log` for the chat tailing loop to see. The
  adapter gains a FIFO/log mode: when `COPPERCLAW_CLI_FIFO` and/or
  `COPPERCLAW_CLI_LOG` are set (or defaulted from `COPPERCLAW_DATA_DIR`'s
  parent), the cli channel opens the FIFO with `O_RDWR | O_NONBLOCK`
  via `tokio::net::unix::pipe::Receiver` and appends outbound to the
  log, flushing each line. The `O_RDWR` open is the standard
  "reader is its own writer" trick that keeps the pipe alive across
  external-writer disconnects (Ctrl-D in one `cclaw chat` no longer
  EOFs the host's read side). With no paths configured the adapter
  still falls back to stdin/stdout for the developer REPL.
- **Setup wires the bridge by default.** `copperclaw-setup`'s
  `quickstart_group` step now also `mkfifo`s `chat.fifo` (0600),
  touches `chat.log` (0600), and writes `COPPERCLAW_CLI_FIFO` and
  `COPPERCLAW_CLI_LOG` lines into the install's `.env` so the host
  picks them up on next boot. Idempotent — re-running setup leaves
  an existing FIFO / log / env line alone.
- **Stray blank lines are no longer reified into `{"text":""}`
  inbound events.** The cli channel's read loop now skips empty
  lines, eliminating the spurious empty-message inbound that the
  original buggy stdin path produced when a terminal flushed a
  newline.

### Added (release automation)

- **Binary release workflow** at `.github/workflows/release.yml`.
  Triggered by `git push` of a `v*` tag (and manually via
  `workflow_dispatch` for smoke tests). Builds `copperclaw`, `cclaw`,
  and `copperclaw-setup` in parallel for four targets
  (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
  `x86_64-apple-darwin`, `aarch64-apple-darwin`), strips each
  binary, packages one `copperclaw-<target>.tar.gz` per target with
  binaries at the top level (the layout `install.sh` expects),
  generates a combined `SHA256SUMS`, extracts release notes from
  `CHANGELOG.md` for the tagged version, and publishes a GitHub
  Release with the tarballs + `SHA256SUMS` attached. Linux arm64
  is cross-compiled with the apt `gcc-aarch64-linux-gnu` linker;
  macOS x86_64 is cross-compiled on the `macos-14` arm64 runner.
  Co-exists with the container-image workflow so one tag push
  cuts both the binary release and the GHCR image.
- `install.sh`'s prebuilt-tarball strategy now actually resolves
  on tagged releases — no more silent fallback to `cargo install
  --git` for every install.

### Added (production hardening slice — three parallel-agent items)

- **Secret rotation via SIGHUP.** New `RotatableConfig` struct +
  `Arc<RwLock<...>>` on `ContainerManager` holds the rotatable
  surface (`ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`, web-search
  provider keys). `ContainerManager::reload_env(env_file)` parses
  the `.env` and updates the lock so subsequent container spawns
  pick up rotated values. SIGHUP handler wired in
  `wait_for_signal_or_sighup`; `run_host` gains an `env_file`
  parameter that the SIGHUP handler reads on each signal. New
  metric `copperclaw_secrets_rotated_total`. Running containers see
  rotated keys after idle-stop + respawn (default 5 min).
- **Webhooks TLS documentation.** New
  [`docs/webhooks-tls.md`](docs/webhooks-tls.md) covers the
  reverse-proxy patterns (Caddy / nginx / Cloudflare Tunnel) and
  explains why native rustls is deliberately not in 0.1.0.
- **Per-group LLM rate limits.** New columns
  `agent_turns_per_minute_cap` + `agent_turns_per_hour_cap` on
  `group_budgets` (migration `009_rate_limit_caps`). Container
  manager gates spawn on both windows in `maybe_spawn`; an
  in-channel reply explains the cap via the same outbound-write
  path the budget gate uses, dedup'd on a 1-minute window. New
  `cclaw budgets set --turns-per-minute N --turns-per-hour N`.
- **Versioned migrations.** New `expected_central_schema_version()`
  and `applied_central_schema_version()` helpers in
  `copperclaw-db::migrate`. Boot now refuses to start with
  `BootError::SchemaMismatch` (exit code 5) when the on-disk
  schema is newer than this binary expects (downgrade detection).
  New `cclaw schema-version` subcommand prints `{expected, applied,
  status}` as JSON.
- **`sessions/sessions/` path cleanup.** `HostConfig::sessions_root()`
  now returns `data_dir` directly; the double-`sessions/` layout
  is gone. New `migrate_sessions_layout()` runs at boot, moving
  contents from `data_dir/sessions/sessions/<ag>/<sess>/` up one
  level when present. Collisions log a warn and skip; the inner
  directory is only removed when all entries moved successfully.

### Added (onboarding polish slice)

- `cclaw doctor` — first-run / ongoing health probe. Walks the
  install end-to-end (host reachability, agent groups, wirings,
  active sessions, recent audit errors, dropped-message backlog,
  `ANTHROPIC_API_KEY` presence, web-search provider keys) and
  prints a per-row OK / WARN / FAIL with a `fix:` line on every
  non-OK row. Non-zero exit when any check is in FAIL so CI scripts
  can branch. `--json` for machine-readable output, `--no-ping` to
  skip the live LLM ping.
- Setup auto-bootstraps a default cli agent group + wiring. New
  `quickstart_group` step runs after `verify` and writes a
  `(cli, stdin)` messaging group + agent group + pattern-`.*`
  wiring directly to the central DB so `cclaw chat` works on the
  very first `copperclaw run`. Idempotent (skips when any agent group
  already exists). Opt out with `COPPERCLAW_SETUP_QUICKSTART=no` or
  decline the interactive prompt. Override the slug with
  `COPPERCLAW_SETUP_QUICKSTART_NAME`. The `first_chat` step's
  "what to do next" output flips to recommend `cclaw chat`
  directly when the bootstrap landed.
- Budget-exhausted reply to original sender. When the container
  manager's spawn gate refuses because today's tokens exceeded
  the group's `daily_token_cap`, the host now posts a one-line
  in-channel reply ("I have reached this agent's daily token
  budget. New requests will resume after &lt;next UTC midnight&gt;…") via
  the session's `outbound.db`. Dedupes per-group on a one-hour
  window so a chatty user gets one explanation, not ten. Skips
  silently when `session_routing` is empty.

### Added (M14 follow-up — web search)

- New `web_search` MCP tool, the 20th in-tree tool the agent can
  call. Closes the M14 follow-up gap: `web_fetch` could read a URL
  but the agent couldn't *find* one.
- Four provider backends in a single tool, normalised to one
  `{title, url, snippet, published?, score?}` result schema:
  - **Tavily** — agent-tuned default. `TAVILY_API_KEY`.
  - **Exa** — neural / semantic search with `text` snippets.
    `EXA_API_KEY`.
  - **Brave** — independent keyword index. `BRAVE_SEARCH_API_KEY`.
  - **SerpAPI** — Google / Bing / etc. wrapper. `SERPAPI_API_KEY`.
- Provider resolution: explicit `provider` arg → `COPPERCLAW_WEB_SEARCH_PROVIDER`
  env → auto-detect from configured keys in order
  `tavily, exa, brave, serpapi`. No keys configured surfaces a
  validation error naming all four env vars (errors over silent
  fallback).
- Host's `ContainerManager` now forwards
  `COPPERCLAW_WEB_SEARCH_PROVIDER` + the four provider keys into the
  session container at spawn via a new `forward_env` field, so the
  operator only configures keys once in the host's `.env`.
- New skill: `skills/web-search/SKILL.md` (auto-loaded into the
  system prompt under the existing
  `COPPERCLAW_SKILLS_DIR` mechanism).
- New doc: [`docs/web-search.md`](docs/web-search.md) — operator
  setup, provider trade-offs, egress allow-list interaction.

### Added (M14 — agent capability)

- `ProviderEvent::ToolCall` and a tool-use outer loop in the runner.
  The model now actually receives the schema for every in-tree tool
  and can call them per turn until it produces a turn without tool
  use (capped at 20 inner LLM rounds).
- Four computer-use tools wired through to the agent: `shell` (bash
  in container, 64 KiB output cap, 60 s default / 600 s ceiling),
  `read_file` (UTF-8 read, 1 MiB cap), `write_file` (create/append
  with auto-mkdir), `web_fetch` (HTTP GET/POST, 256 KiB body cap,
  30 s default / 120 s ceiling).
- Skill content auto-loaded into the agent's system prompt.
  `COPPERCLAW_SKILLS_DIR` points at the SKILL.md library, optional
  `COPPERCLAW_GROUPS_DIR` enables per-agent-group overrides under
  `<groups_dir>/<ag_uuid>/skills/`. Setup writes both env vars.
- New skills documenting the computer-use tools: `shell`,
  `read-file`, `write-file`, `web-fetch`.

### Added (M13 hardening — parallel-agent slice)

- **Image rebuild on `container_configs` change.** The manager
  fingerprints (`config_fingerprint` column) the rebuild-relevant
  fields and rebuilds + retags before the next spawn when they
  change. Rebuild failures log + emit
  `copperclaw_image_rebuild_failed_total` and fall back to the
  last-known-good image so the agent group is not blocked.
- **Container egress allow-list.** New
  `container_configs.egress_allow` (JSON array of host:port).
  Default empty == allow-all (default-allow + opt-in lockdown).
  Docker runtime translates to user-defined network policy; Apple
  Container runtime returns `RtError::Unsupported`. New
  `cclaw groups config set-egress-allow <id> --allow host:port ...`.
- **Per-group resource caps.** New
  `container_configs.resource_limits` JSON
  (`cpus` / `memory_mb` / `pids_limit`, all optional). Docker
  runtime applies via `--cpus` / `--memory` / `--pids-limit`. New
  `cclaw groups config set-resource-limits`.
- **Auto-applied `install_packages` / `add_mcp_server`.** The
  delivery loop now intercepts these system actions and writes
  directly to `container_configs.packages_apt` /
  `packages_npm` / `mcp_servers`. Combined with the rebuild
  fingerprint, the next spawn picks up the agent's tool calls
  automatically — no operator step required.
- **Central DB backup / restore.** `cclaw db backup <path>` runs
  a WAL checkpoint and atomically copies the file. `cclaw db
  restore <path>` always refuses with `host_running`; the
  operator-facing procedure is documented in
  `docs/db-backup.md` (stop host, copy file, restart).
- **Outbound dead-letter replay.** New
  `outbound_dropped_messages` table (migration `008_*`). Delivery
  failures that exhaust 3 retries land here.
  `cclaw dropped-messages outbound-list --since <window>` and
  `cclaw dropped-messages replay <id>` give the operator
  inspection / retry.
- **MCP server preset registry.** `cclaw mcp list-presets` shows
  the curated library (postgres, linear, github, notion,
  filesystem, browserbase). `cclaw mcp add <preset>
  --agent-group-id <id> --env K=V` writes the chosen preset into
  `container_configs.mcp_servers` (env values are redacted in the
  audit log).
- **Sender approval notifications in-channel.** When a new sender
  lands in `pending` for the first time, the host posts a plain-
  ASCII "approve?" notification to the agent group's primary
  messaging group. Dedup uses `unregistered_senders` so repeat
  senders don't re-spam.
- **Prometheus metrics endpoint.** Opt-in via
  `COPPERCLAW_METRICS_ADDR=127.0.0.1:9090` (bare port auto-prefixes
  to loopback). Counters:
  `copperclaw_messages_inbound_total{channel_type}`,
  `copperclaw_messages_outbound_total{channel_type}`,
  `copperclaw_containers_spawned_total`,
  `copperclaw_containers_crashed_total`,
  `copperclaw_delivery_failed_total{channel_type}`,
  `copperclaw_image_rebuild_failed_total`. Histograms:
  `copperclaw_llm_call_seconds`, `copperclaw_llm_tokens_input`,
  `copperclaw_llm_tokens_output`, `copperclaw_container_spawn_seconds`.
  New crate `copperclaw-metrics`.
- **Log rotation.** Opt-in via `COPPERCLAW_LOG_DIR=<path>`. Adds a
  daily-rotating file writer (`host.log.<YYYY-MM-DD>`) alongside
  the existing stderr writer. `COPPERCLAW_LOG` filter applies to
  both. Default stderr-only behaviour unchanged.
- **Audit-log env redaction.** The host's audit dispatch now masks
  values under any `env` block for `mcp.add` and
  `groups.config.set-mcp-servers` before serialising into
  `audit_log.args`. Keys are preserved; values become
  `<redacted>`.
- New docs: [`docs/container-config.md`](docs/container-config.md),
  [`docs/observability.md`](docs/observability.md),
  [`docs/db-backup.md`](docs/db-backup.md).

### Added

- One-command installer at `install.sh`: detects platform (Linux
  x86_64/aarch64, macOS arm64/x86_64), verifies Docker or Podman is
  reachable, then installs `copperclaw`, `cclaw`, and `copperclaw-setup`
  to `~/.local/bin` — preferring a prebuilt release tarball, falling
  back to `cargo install --git`, and finally `cargo install --path`
  when run inside a checkout. Re-running detects an existing install
  and offers upgrade/skip; setup state is resumed in place. Respects
  `NO_COLOR`, non-tty stdout, and quiets verbose output unless
  something fails.
- README "Install" section now leads with the one-liner; the
  longstanding `cargo build` instructions move under a "Manual install"
  subsection.
- One-terminal operator flow for the `copperclaw` binary: new
  `copperclaw start` (daemonize, write PID file, wait for admin socket
  ready), `copperclaw stop` (SIGTERM with SIGKILL escalation after a
  10s grace), `copperclaw status [--json]` (PID, uptime, paths, active
  session count; exits non-zero when not running for CI use), and
  `copperclaw logs [-f] [-n N]` (tail the host log). `copperclaw run`
  is preserved for foreground / service-managed deployments.
- `cclaw chat` now auto-starts the host via `copperclaw start` when
  the chat FIFO is missing; pass `--no-autostart` to keep the old
  "fail loudly" behaviour for scripted / CI use. Quick start
  collapses to `copperclaw start && cclaw chat` in one terminal.
- Interactive Telegram pairing wizard inside `copperclaw-setup`'s
  `channel` step. When the operator picks `telegram`, the wizard walks
  them through `@BotFather`, validates the token format
  (`^\d+:[A-Za-z0-9_-]+$`), verifies it via Telegram's `getMe`
  endpoint (10 s timeout, soft-fail on network errors), optionally
  polls `getUpdates` for ~60 s to capture the first chat id, and
  appends `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` to the data-dir
  `.env`. Headless mode is driven by
  `COPPERCLAW_SETUP_TELEGRAM_BOT_TOKEN` and
  `COPPERCLAW_SETUP_TELEGRAM_CHAT_ID`. Tokens are never logged — the
  audit messages use `<digits>:****<last-4>` redaction.
- `copperclaw-setup` `service_unit` step now installs and enables the
  generated systemd unit / launchd plist end-to-end rather than just
  writing it to disk. Operators pick a scope at the prompt
  (`system` / `user` / `print`) or via
  `COPPERCLAW_SETUP_SERVICE_SCOPE`; `COPPERCLAW_SETUP_SERVICE_ENABLE`
  controls whether `systemctl enable --now` / `launchctl bootstrap`
  fires. The step polls the admin socket for ~10s after enabling and
  prints a clear "service is running" / "didn't come up — check
  journalctl" line. `system` scope refuses to silently shell out to
  `sudo` and falls back to `user` when not root. Idempotent on re-
  run: identical bodies are detected and the step is skipped.
- `cclaw` with no subcommand now prints a one-shot operator dashboard
  (install root, agent groups, wirings, active sessions, recent audit
  + drop activity, 24h budget usage, and up to three heuristic
  next-step suggestions). Fans out to existing read-only handlers in
  parallel via `tokio::join!`; `--json` emits the same payload as a
  single object. When the host socket is unreachable the dashboard
  exits non-zero with a friendly "host not running" pointer.
- `cclaw groups config edit <id>` — opens the container config as
  TOML in `$EDITOR` (falls back to `$VISUAL`, then `vi`), diffs on
  save, and applies the changes via the existing `groups.config.*`
  socket commands. Supports `--dry-run` to preview the diff without
  committing. Read-only fields (`agent_group_id`, `updated_at`) are
  rendered as comments and ignored on save; TOML parse errors are
  re-rendered inline with a `(r)etry / (a)bort` prompt.
- Two guided-flow agent skills under `skills/`: `customize` (walks
  the user through model swaps, package/MCP installs, behavior
  prompt edits, and budget changes, routing host-only mutations to
  the operator with the exact `cclaw` command) and `debug` (pulls
  diagnostics reachable from inside the container and prints the
  `cclaw health` / `audit list` / `dropped-messages list` commands
  the operator must run to complete triage).
- Initial Rust workspace with 16 crates across the host, runner,
  providers, MCP server, modules, skills, container runtime, OneCLI
  gateway, cclaw admin client, and interactive setup.
- Central DB schema (`copperclaw.db`) with idempotent migrations under
  `crates/copperclaw-db/migrations/`. Per-session inbound and outbound DBs
  with attachment-safety helpers (`safe_attachment_name`,
  `extract_to_inbox`, `read_from_outbox`).
- Host pipeline: router (hook chain, fan-out, session resolution),
  delivery (active 1s + sweep 60s, exponential backoff, 3-attempt cap),
  and sweep (stuck detection, recurrence fanout, processing-ack reset).
- Container runtime trait with Docker (bollard) and Apple Container
  (CLI shell-out) backends. Image build with apt/npm package
  contributions per `container_configs` and sha256-fingerprinted tags.
- Provider trait + Anthropic HTTP-streaming impl with tool-use loop and
  context compaction. Subprocess provider variants for Codex and
  OpenCode. Ollama provider via the Anthropic-compatible base URL.
- MCP server with the 15-tool inventory documented in PLAN.md section 7.
- Channel registry with 17 in-tree channels: cli, telegram, slack,
  discord, resend, github, linear, webex, matrix, teams, gchat,
  whatsapp-cloud, signal, deltachat, emacs, x, plus the in-progress
  imessage/wechat/whatsapp crates landing as follow-ups.
- Modules: typing, mount-security, permissions, approvals, interactive,
  scheduling, agent-to-agent, self-mod.
- Skill discovery (frontmatter parse + per-group override) and
  symlink-based container materialisation; 17 authored skills under
  `skills/`.
- `copperclaw-cclaw` Unix-socket admin server inside the host plus the
  `cclaw` client binary; 41 distinct commands exported as
  `copperclaw_cclaw::ALL_COMMANDS`.
- `copperclaw-setup` interactive setup with `dialoguer`, systemd /
  launchd unit generators, headless env-var-driven mode, and the
  `--migrate-from` data-directory migrator.
- `copperclaw-onecli` HTTP credential gateway with full wiremock coverage
  for 401/404/409/429/5xx and `Retry-After` parsing.
- M11 documentation: `docs/cutover.md` for predecessor migration,
  `docs/replay-fixtures.md` describing the differential-testing
  harness, and `docs/release-checklist.md` for cutting tagged
  releases.
- Baseline CI workflow at `.github/workflows/ci.yml` (rustfmt, clippy,
  test on Linux + macOS, coverage gate at 85%).
- `container-image` GitHub Actions workflow that builds and publishes
  the session base image to GHCR (`ghcr.io/<repo>/session`) for every
  push to `main` (as `:edge`) and tagged release (as `:<semver>` and
  `:latest`), with multi-arch (linux/amd64, linux/arm64) buildx output,
  GHA build cache, and an `copperclaw.fingerprint` provenance label.
- Checked-in `container/Dockerfile` for the session base image, carrying
  an `COPPERCLAW_FINGERPRINT` build-arg stamped as an
  `copperclaw.fingerprint=<sha>` LABEL so pulled images can be verified
  against the locally-expected spec hash.
- `copperclaw-setup` `image` step now attempts a `docker pull` of the
  pre-built GHCR image before falling back to a local build. Pulls are
  verified by inspecting the image's `copperclaw.fingerprint` label;
  mismatches fall through to a local build with a clear "pulling
  failed, building locally" message. `COPPERCLAW_SETUP_NO_PULL=1` skips
  the pull attempt for air-gapped or reproducible-build use cases;
  `COPPERCLAW_SETUP_PULL_REGISTRY` overrides the registry slug for forks.

### Fixed

- Matrix `/sync` loop now respects cancellation while pushing inbound
  events, allowing the previously-ignored
  `sync_loop_pushes_events_and_persists_next_batch` test to run
  reliably without saturating the inbound mpsc.

### Known limitations

- Three M8 channels are noted in PLAN.md as the hardest of the set —
  imessage (macOS-local), wechat (Enterprise Work Weixin), and
  whatsapp (native Baileys port). Initial scaffolds are landing in
  follow-up commits; the whatsapp adapter ships behind a stubbed
  `CryptoBackend` until a real Signal-Protocol impl is wired in.
- Differential replay fixtures (M11) are designed in
  `docs/replay-fixtures.md` but the in-tree harness and captured
  fixtures are not yet committed.

[Unreleased]: https://github.com/phildougherty/copperclaw/compare/v0.0.0...HEAD
