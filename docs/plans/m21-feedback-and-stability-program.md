# M21 — Feedback and stability program

Goal: after M18/M19/M20, a user can text **"build me X"** on any channel and
watch a prototype get built, proven, shared — and built *well*. M21 makes the
platform underneath that experience trustworthy: every failure mode gets a
recovery path, every wait gets an honest signal, and every recovery is
visible to the person who has to operate the thing. Three themes, one per
wave:

1. **Nothing dies silently** — every background loop is supervised, every
   stuck/crashed/OOM'd session has an actuator and a backoff, and delivery
   retry state survives a host restart.
2. **The user is never in the dark** — cold spawns show life from message
   one, expired questions say so out loud, and recovery always comes with a
   note.
3. **Operators can see and trust it** — `cclaw doctor` detects everything
   Wave 1 recovers from, DB corruption is found instead of skipped, provider
   failover is live rather than spawn-time-only, and critical events can
   actually reach an operator.

Written 2026-07-16 from two subsystem audits of `main` at `3c91daf` — a
stability audit (gaps G1–G18: supervision, stuck detection, retry
persistence, OOM, DB integrity, doctor blind spots) and a user-feedback
audit (gaps F-cold through F-a2a: cold-spawn silence, silent question
expiry, dead-letter handling). Like M18/M19/M20 this document is written to
be executed by **parallel teams/agents, one task card per implementer**;
each card declares an exclusive scope and two cards in the same wave never
share a scope. Read the whole preamble before taking a card.

## Relationship to M20

M20 is **complete**: all 16 work items merged (`3c91daf`), the M1 metrics
rider swept, and the D1/D5 security-review pass recorded in the CHANGELOG.
M21 re-opens none of it. It builds on — and deliberately does **not**
rebuild — the surfaces the last three programs shipped:

  - the sweep apology machinery with its liveness gate
    (`crates/copperclaw-host-sweep/src/checks/apology.rs`);
  - the ErrorCard surface, including the delivery-failure card
    (`crates/copperclaw-host-delivery/src/service.rs:2617`);
  - the typing ticker (`crates/copperclaw-host/src/typing_ticker.rs`), the
    M18 Task HUD (`crates/copperclaw-runner/src/run/hud.rs`), progressive
    reveal, and the M19 blocker walls;
  - the approval-card expiry surfacing (fully wired — it is the pattern F2
    copies for `ask_user_question`).

Two long-deferred wishes are finally taken: **external-MCP connection
caching** (M17 B1b, deferred in M17 and again in M19) and a **runner
test-clock seam** (the M18 X2 known gap — the HUD StatusRows 60s leg has
been unfixturable since M18). Standing rejections are honored: no token
streaming, no periodic "still working" messages, no deploy-to-cloud, no
plugin registry.

## Rules for every implementing team

Identical to M18/M19/M20 — re-read `CLAUDE.md`, all of it applies:

1. `cargo fmt --all && cargo check --workspace && cargo clippy --workspace
   --all-targets -- -D warnings && cargo test --workspace --no-fail-fast`
   green before done. Baseline is M19's final gate (**7,473**) plus M20's
   additions — record the exact number at branch time. Gate = zero
   failures, not a fixed count; do not regress it.
2. The workspace forbids `unsafe_code`; clippy warnings are errors.
3. **CHANGELOG.md is the merge hotspot.** Every card adds its lines under
   `## [Unreleased]`, grouped `### Added/Changed/Fixed`. Append-only;
   expect conflicts; resolve by keeping both sides.
4. **No stubs in tree.** A card either ships its behavior complete behind
   its flag/default or does not merge.
5. **Secure-by-default.** Any outward-facing surface or default change
   requires a `security-review` pass recorded in the PR before merge. In
   this program that is **O4** (new opt-in alert destination — new config
   surface + new outbound path). **S2** and **F1** change default behavior
   (auto-restart of stuck sessions; a new default-on slow-spawn notice) —
   each records its default-change argument in the PR description.
6. **New DB state = a new numbered migration.** Next free is **029** —
   verify at branch time (S3 is the only card known to need one; it touches
   the *per-session* outbound schema — confirm per-session migrations share
   the central numbering before claiming 029). Never edit a released
   migration.
7. **Fixtures before pipeline changes.** Any card that touches the
   inbound → router → runner → outbound → delivery pipeline adds or extends
   a replay fixture under `fixtures/<channel>/<scenario>/` first, so the
   diff catches regressions. The per-wave X-riders are the backstop, not a
   substitute.
8. File:line anchors in the cards are **orientation, not gospel** — they
   were verified at `3c91daf` and will drift as waves land. Re-find the
   code; do not patch blind.
9. One PR per card. Name it `M21 <ID>: <title>`.
10. Re-run the *integrated* gate after a multi-PR merge — a green
    per-branch gate does not prove the union is green or even fmt-clean.

## Scope / conflict map (lanes)

A lane is a set of files one team owns for the duration of its cards. Cards
within a lane are **sequential**; lanes run in **parallel**. Two cards in
the same wave never share a scope. `copperclaw-metrics` is a hotspot no
card touches except M1: every other card records its metric wishes in its
PR description and M1 sweeps them at the end.

| Lane | Owns | Cards |
|---|---|---|
| **H — host core** | `crates/copperclaw-host/src/{boot.rs, supervisor.rs (new), typing_ticker.rs, operator_alerts.rs (new), container_manager/**, handlers/**}` | S1 → S4 → S2 (H half) → F1 → F3 → O4 |
| **W — host sweep** | `crates/copperclaw-host-sweep/**` | S2 (W half) → F2 (W half) → O2 (+ one-string declared touch from O4) |
| **D — delivery** | `crates/copperclaw-host-delivery/**` + migration 029 in `crates/copperclaw-db/migrations/` (declared shared-crate touch) | S3 → S5 |
| **R — runner + providers** | `crates/copperclaw-runner/**`, `crates/copperclaw-providers/**` | S6 → O3 |
| **T — mcp + modules** | `crates/copperclaw-mcp/**`, `crates/copperclaw-modules/**` | F2 (T half) → F4 |
| **A — cclaw** | `crates/copperclaw-cclaw/**` | O1 |
| **X — program verification** | `fixtures/**`, replay-test registration | X-riders, one per wave |
| **M — metrics rider** | `crates/copperclaw-metrics` | M1 (single card, absolute last) |

Cross-lane coordinations, declared up front: **S2** spans W (detection →
actuator call) and H (the actuator impl + new `ReconcileAction`) as one
card with one implementer — the M20 D1 pattern for a two-crate seam.
**F2** spans W (new sweep check) and T (`modules/src/interactive.rs` expiry
semantics), same rule. **O4** is lane H with a one-string conditional
apology-copy touch in W, declared here. **O1** consumes host state via the
admin-socket status handler that **S1** lands — A blocks on H for that one
handler, nothing else. `boot.rs` is the lane-H hotspot: five H cards touch
it, strictly sequenced, never concurrent.

## Architecture decisions (made here — don't re-litigate)

**(a) The heartbeat stays "process liveness"; stuck recovery is
sweep-detected, manager-actuated.** The runner's `HeartbeatTicker` staying
fresh across tool dispatch (`crates/copperclaw-runner/src/run/tool_dispatch.rs:58`)
is *correct* — the runner process IS alive during a hung tool, and starving
the heartbeat would get long-but-legitimate tools killed by the crash path.
So the runner changes nothing. The missing piece is an actuator: the sweep
already detects stuck tools from per-session DB state
(`crates/copperclaw-host-sweep/src/checks/stuck.rs`) — S2 injects a
`StuckActuator` into `SweepService`, implemented by `ContainerManager`, so
detection finally drives a restart through the component that owns
container lifecycle. Rejected alternatives: a separate tool-progress file
(duplicates the per-session DB state the sweep already reads); sweep-side
direct docker kill (violates the manager's single-writer ownership of
container lifecycle).

**(b) Delivery retry state persists on the message row itself — migration
029.** `tries` + `not_before` columns on the per-session `messages_out`
schema; the in-memory `DashMap` becomes a write-through cache primed lazily
on first poll. Rejected: reusing the `delivered` table (terminal-state
records; wrong semantics for in-flight counters) and a new central-DB table
(retry state is per-message — it belongs beside the row, and the
per-session DBs already shard the write load).

**(c) Cold-start feedback = typing indicator during spawn + at most ONE
slow-path notice.** The typing ticker's `Running`-only gate widens to
sessions mid-spawn with pending inbound — a typing indicator is not a
message, so this respects the standing "no periodic new messages"
rejection. Separately, if a spawn crosses a slow threshold (~20s — first
image build/pull territory), the manager enqueues exactly one system notice
("Setting things up — this can take a minute or two"), once per spawn
attempt, never repeated. Rejected: pre-runner status *messages* by default
(chatty; brushes the periodic-message rejection) and a host-side fake HUD
(the HUD is runner-owned; two writers would fight).

**(d) Operator alerting = honest copy now, opt-in real push via existing
channels.** No new notification infrastructure. Wave 1 fixes the lying copy
("The operator has been notified" — today a log line and a counter,
`crates/copperclaw-host-sweep/src/checks/apology.rs:61`) to "If this keeps
happening, tell your operator — `cclaw doctor` will show what's wrong."
Wave 3 (O4) adds an **opt-in** operator-alert destination: a configured
channel target to which the host enqueues rate-limited system alert rows
through the *existing* delivery pipeline. When wired, the apology copy
conditionally upgrades back to "the operator has been notified" — and it is
finally true. Rejected: webhook/email push (new outward-facing infra);
always-on alerts (secure-by-default says no silent new outbound).

**(e) Crash-loop policy: per-session exponential backoff, in-memory; OOM is
its own class.** CrashRestart backoff 5s → 15s → 60s → 300s cap, reset
after 10 minutes healthy; state lives on `ContainerManager` and a host
restart resets it (acceptable — boot's recovery path re-baselines
everything anyway). Exit 137 / `State.OOMKilled` classifies as `OomKill`
with its own metric and restart reason; after 3 OOMs in the window the user
gets one ErrorCard ("this task keeps running out of memory — an operator
can raise the limit") instead of an infinite silent kill loop. Image
pull/build failure becomes a distinct spawn-failure reason feeding the
existing `SpawnAttemptTracker` → apology path.

**(f) DB integrity: rotating `PRAGMA quick_check`, quarantine via sidecar
marker, no migration.** The sweep runs `quick_check` on the central DB at
boot + daily, and on a rotating subset of per-session DBs each pass. A
corrupt per-session DB gets a `quarantined` sidecar file next to it (no
migration needed; survives restarts; trivially visible to doctor); the
session is excluded from sweeps with ONE escalating log + metric instead of
today's silent per-pass skip. Rejected: full `integrity_check` every pass
(too costly at fleet size) and a sessions-table column (migration 030 for
state that is really about a file on disk).

**(g) Supervision = a JoinSet supervisor with restart-on-panic.** A new
`crates/copperclaw-host/src/supervisor.rs` owns every background loop
(inbound consumer, active + sweep delivery loops, sweep loop, typing
ticker, todo watcher): panics and unexpected exits are caught, logged at
ERROR, and restarted with the decision-(e) backoff curve; exceeding the cap
sets a degraded flag and (once O4 lands) fires an operator alert. Loops
keep their own internal error handling — the supervisor catches only the
class that today silently kills a subsystem until process exit
(`crates/copperclaw-host/src/boot.rs:903-919`).

---

## Wave 1 — "Nothing dies silently"

The trust backbone. Everything else in the program assumes these hold.

### S1. Supervise all host background loops — P0, M — lane H

**Problem.** Every host loop is a bare `tokio::spawn` whose `JoinHandle` is
only awaited at shutdown (`crates/copperclaw-host/src/boot.rs:903-919`). A
panic in the delivery loop, sweep loop, inbound consumer, or typing ticker
silently kills that subsystem for the remaining life of the process —
indistinguishable from idle, invisible to logs-at-a-glance, metrics, and
doctor. This is the single largest silent-death surface in the host.

**Change.** Per decision (g): new `crates/copperclaw-host/src/supervisor.rs`
— a `JoinSet`-based supervisor with named tasks, restart-on-panic with the
decision-(e) backoff curve, and a degraded flag on cap exhaustion. `boot.rs`
registers every background loop through it. Add a small admin-socket status
handler (`handlers/`) exposing per-loop liveness + restart counts — O1
reads it; O4 later hooks the permanent-failure event. Record liveness-gauge
and restart-counter wishes for M1.

**Acceptance.** Unit: a task that panics is restarted with backoff; a task
that exceeds the cap flips the degraded flag. Integration: panic-inject the
sweep loop in a test host — sweeping resumes within one backoff step; the
status handler reports the restart. Shutdown semantics byte-identical: all
loops still drain on SIGTERM in the same order.

### S2. Stuck-tool actuator: detected stuck sessions get restarted — P0, L — lane W (detection → act) + lane H (actuator), after S1 + S4

Default-change argument recorded in the PR (sessions that today wedge
forever will now be restarted automatically).

**Problem.** Stuck tools are detected
(`crates/copperclaw-host-sweep/src/checks/stuck.rs:42-63` — past the
declared timeout floor at 60s, unconditionally past the 30-minute ceiling)
but `SweepReport.stuck_sessions` is only logged
(`crates/copperclaw-host-sweep/src/service.rs:287-297`). Meanwhile the
runner keeps the heartbeat fresh during every tool call
(`crates/copperclaw-runner/src/run/tool_dispatch.rs:58`), so
`ContainerManager::classify` never fires `CrashRestart` for a hung tool.
Net: a hung tool is *detected but never recovered* — the session wedges
until an operator intervenes. The user gets the 300s apology and then
nothing, forever.

**Change.** Per decision (a): a `StuckActuator` trait injected into
`SweepService` at boot, implemented by `ContainerManager` as a new
`ReconcileAction::StuckRestart`
(`crates/copperclaw-host/src/container_manager/classify.rs`), fired only
past `ABSOLUTE_CEILING_MS` — the 60s claim threshold stays observe-only.
The restart rides the existing crash-restart apology machinery so the user
hears "I hit a snag and restarted; some progress may have been lost" rather
than silence. The runner is untouched. This card also lands the
decision-(d) honest apology copy fix in
`crates/copperclaw-host-sweep/src/checks/apology.rs:61-62`.

**Acceptance.** Unit: actuator fires only past the ceiling; claim-threshold
detections still report-only. Integration (mock runtime): a tool hung past
the ceiling → `StuckRestart` issued within one sweep interval → apology row
written once (deduped) → next inbound processes normally. The apology copy
no longer claims operator notification anywhere until O4 wires it.

### S3. Persist delivery retry state — migration 029 — P0, M — lane D

**Problem.** `RetryState` lives only in an in-memory `DashMap`
(`crates/copperclaw-host-delivery/src/service.rs:186,310`). A host restart
wipes all attempt counters and backoff windows, so `MAX_DELIVERY_ATTEMPTS=3`
is really "3 attempts per host lifetime": a poisoned outbound row retries
unboundedly across restarts, and a row mid-backoff loses its `not_before`
and fires immediately on boot.

**Change.** Per decision (b): migration **029** adds `tries` and
`not_before` to the per-session `messages_out` schema (verify numbering per
Rule 6). The DashMap becomes a write-through cache primed lazily on first
poll of each session. Exhaustion dead-letters exactly once across restarts
via the existing dropped-messages path and the existing delivery-failure
ErrorCard (`service.rs:2617`) — no new user surface.

**Acceptance.** Unit: counters round-trip through the row; cache priming
honors persisted `not_before`. Integration: kill and restart the delivery
service mid-retry → attempts resume at the persisted count → exhaustion
produces exactly one `delivered{status="failed"}` row and one ErrorCard.
Fresh-install and migrated-install schemas agree (`PRAGMA` diff test, the
copperclaw-db pattern).

### S4. OOM and crash-loop classification with backoff — P0, M — lane H, after S1

**Problem.** Exit 137 / `State.OOMKilled` is indistinguishable from a
generic crash in `container_manager/classify.rs`, and `CrashRestart` has no
backoff — an OOM-looping session crash-restarts in a tight loop forever,
with no OOM metric, no signal that `memory_mb` is too low, and no user
message beyond the generic restart apology each time. Image pull/build
failures are likewise not a modeled spawn-failure class.

**Change.** Per decision (e): inspect the container at `CrashRestart`
capture and classify `OomKill` distinctly; apply per-session exponential
backoff (5s → 15s → 60s → 300s, reset after 10 min healthy) to all crash
restarts; after 3 OOMs in the window emit one ErrorCard telling the user
the task keeps running out of memory and an operator can raise the limit.
Model image pull/build failure as a distinct spawn-failure reason in
`container_manager/spawn.rs` feeding the existing `SpawnAttemptTracker` →
apology path. Record OOM/backoff metric wishes for M1.

**Acceptance.** Unit: exit-137 classification; backoff curve and healthy
reset; OOM threshold → single ErrorCard (deduped). Integration (mock
runtime): a crash-looping container is respawned at increasing intervals,
not hot-looped; the OOM card appears exactly once per episode. Generic
crash behavior for non-OOM causes byte-identical apart from the backoff.

### S5. Bound the forever-pending outbound rows — P1, S — lane D, after S3

**Problem.** Rows whose channel has no live adapter are counted "deferred"
and left pending with no ceiling
(`crates/copperclaw-host-delivery/src/service.rs:682-686`). A
permanently-unconfigured or removed channel accumulates unbounded pending
outbound that nothing will ever drain and nothing reports.

**Change.** A deferral counter and age ceiling (24h) for pending rows with
no adapter: on expiry, mark them failed into the dropped-messages path with
reason `no_adapter`, recoverable by the existing
`cclaw dropped-messages replay`. Surface the backlog count through the S1
status handler so O1 can check it. No new user-facing surface — these rows
by definition have no deliverable channel.

**Acceptance.** Unit: ceiling math; rows below the ceiling untouched.
Integration: an unconfigured channel's rows expire into
`outbound_dropped_messages` with reason `no_adapter` and replay cleanly
once the adapter exists.

### S6. Test-clock seam for the runner's timed surfaces — P2, S — lane R

**Problem.** The HUD StatusRows 60s leg has been unfixturable since M18
(the X2 known gap: it needs a real 60s wall-clock wait), and M21 adds more
time-driven behavior (backoffs, TTLs, spawn thresholds) that fixtures
cannot traverse deterministically.

**Change.** A minimal injectable `Clock` seam in
`crates/copperclaw-runner/src/run/hud.rs` and the run-loop timing call
sites that feed it, defaulting to real time; the replay harness gains a
way to advance it so X-riders can pin the 60s status-row leg and later
timed legs deterministically. No behavior change at default.

**Acceptance.** The previously-uncoverable StatusRows 60s first-fire and
the 150s softening are pinned by a deterministic test. All existing timing
behavior byte-identical under the real clock.

### X-rider (Wave 1). Recovery fixtures — P1, S — lane X

Lock the wave's behavior into deterministic tests: hung-tool →
`StuckRestart` → single apology sequence (mock runtime); delivery retry
counters surviving a service restart with exactly-once dead-lettering; OOM
classification + backoff at the unit-harness level; loop-panic →
supervisor-restart integration test. Register in the workspace replay
test set. Anything a fixture cannot reach without the S6 seam is
documented, not skipped silently.

---

## Wave 2 — "The user is never in the dark"

The felt wave. F1 is the marquee change: the first message to a fresh
session shows life immediately instead of dead air.

### F1. Cold-start feedback: typing from message one, one slow-spawn notice — P0, M — lane H, after S4

Default-change argument recorded in the PR (a new default-on notice on the
slow-spawn path).

**Problem.** A first message to a fresh session gets zero feedback for the
entire container spawn — image check/build, boot, runner handshake. The
typing ticker only fires for `Running` sessions with pending inbound
(`crates/copperclaw-host/src/typing_ticker.rs`), so on a slow spawn the
user stares at silence, and on a failing spawn the first signal is the
300-second sweep apology. This is the worst first impression the platform
makes.

**Change.** Per decision (c): widen the ticker's gate to sessions that are
mid-spawn with pending inbound, so typing shows within one tick of the
message landing; `container_manager/spawn.rs` enqueues exactly one
"Setting things up — this can take a minute or two" system notice when a
spawn attempt crosses ~20s, once per attempt, never periodic. Spawn-phase
duration histogram recorded as an M1 wish.

**Acceptance.** Integration (mock adapter): inbound to a stopped session →
`set_typing` observed before the runner is up; a spawn held past the
threshold produces exactly one notice, a fast spawn produces zero; a
failing spawn still ends in the existing apology. Typing on `Running`
sessions byte-identical.

### F2. Expire `ask_user_question` out loud — P0, M — lane W (sweep check) + lane T (`modules/src/interactive.rs`)

**Problem.** `InteractiveModule::sweep_expired()` exists
(`crates/copperclaw-modules/src/interactive.rs`) but no sweep loop calls it
— contrast `pending_approvals::sweep_expired`, wired into four handlers. An
unanswered question (default TTL 24h) simply evaporates: the card sits
there, the user is never told the answer window lapsed, and the waiting
agent's question is never resolved. The polished approval-expiry path shows
exactly what this should look like.

**Change.** A new sweep check invoking the module's expiry. Each expired
question emits one terminal user-facing note ("this question expired — just
reply and I'll pick it up") and unblocks the waiting agent with a synthetic
no-answer result, mirroring the approval-expiry pattern (card stamped
terminal, no live buttons left behind).

**Acceptance.** Unit: expiry selection honors the TTL; already-answered
questions untouched. Integration: an unanswered question past TTL → one
expiry note delivered, the pending row terminal, the agent's next turn sees
the no-answer result. Fixture added (Rule 7) covering ask → expire → reply
resumes normally.

### F3. Host-restart recovery notice — P1, S — lane H, after F1

**Problem.** Boot-time recovery resets `running` sessions to `stopped`
(`crates/copperclaw-host/src/boot.rs`, orphan cleanup + reset step) but —
unlike the live `CrashRestart` path — emits no apology. A user whose turn
was in flight when the host restarted watches the agent silently drop the
turn; the inbound is re-queued only after the sweep's processing-ack reset,
up to 60s later, with no explanation ever.

**Change.** After the boot reset, sessions with pending inbound get the
same recovery notice the live crash path emits (reuse the crash-restart
apology text machinery, deduped, liveness-gated so a clean idle restart
never fires it).

**Acceptance.** Integration: host restarted with a turn in flight → the
affected session's user receives exactly one recovery notice and the
re-queued inbound processes; restart with no in-flight work produces zero
notices.

### F4. External-MCP connection caching — P2, M — lane T, after F2

**Problem.** Every external-MCP tool call pays a fresh connection setup —
~1–2s per call (M17 B1b, deferred in M17 and again in M19). Users feel it
as unexplained per-step latency in exactly the long multi-tool tasks where
waiting already hurts.

**Change.** Per-session connection reuse in the `copperclaw-mcp` client
keyed by server config, with idle reaping and reconnect-on-broken-pipe.
Failure behavior identical to today: a dead cached connection retries once
fresh before erroring. Cache hit/miss recorded as an M1 wish.

**Acceptance.** Unit: cache keying, idle reap, broken-pipe retry-once.
Integration (stub MCP server): N sequential calls open one connection; a
server restart mid-session degrades to reconnect, not error. First-call
behavior byte-identical.

### X-rider (Wave 2). Feedback fixtures — P2, S — lane X, uses S6's clock seam

Fixtures: spawn-phase typing + single slow-spawn notice (clock-advanced);
question expiry → terminal note → unblocked agent; restart-recovery notice
exactly-once. Register alongside the Wave-1 set.

---

## Wave 3 — "Operators can see and trust it"

### O1. `cclaw doctor` learns everything Wave 1 recovers from — P0, M — lane A, after O2 (reads its quarantine markers)

**Problem.** Doctor's check framework
(`crates/copperclaw-cclaw/src/lib.rs:853`) has blind spots for precisely
the failure modes this program addresses: container-runtime reachability,
host degraded mode, stuck/heartbeat-stale session counts, provider-chain
health — including the inert "no chain configured" default
(`crates/copperclaw-providers/src/failover.rs:28-32`) — background-loop
liveness, dead-letter/`no_adapter` backlog, and DB integrity. An operator
running doctor against a host with a dead delivery loop gets an all-green
report today.

**Change.** New check rows for each blind spot, every FAIL with a `fix:`
line (house rule). Loop liveness and the degraded flag read via S1's
admin-socket status handler; integrity reads O2's sidecar markers;
dead-letter counts get a "replay with `cclaw dropped-messages replay`" fix
line (auto-replay stays rejected); the empty failover chain is a WARN with
the config pointer, not a FAIL.

**Acceptance.** Each new check has a healthy and a failing unit case; the
failing case's `fix:` line names a real command or config key. Existing
check output byte-identical.

### O2. Find corruption instead of skipping it — P1, M — lane W, after F2

**Problem.** No `PRAGMA` integrity check exists anywhere in the workspace.
A corrupt per-session DB makes every per-session sweep check log-and-swallow
that session on every pass, forever — no metric, no escalation, no operator
signal. The session is silently dead.

**Change.** Per decision (f): rotating `quick_check` — central DB at boot
and daily, a rotating subset of per-session DBs each pass. Corruption
writes a `quarantined` sidecar next to the DB, excludes the session from
sweeps with one escalating log line + metric wish (instead of a silent
per-pass skip), and fires an O4 alert when wired. `cclaw` gains no new verb
— doctor (O1) surfaces it.

**Acceptance.** Unit: rotation covers every session within N passes; a
deliberately-corrupted fixture DB is detected, quarantined, and excluded;
healthy DBs pay one `quick_check` per rotation slot, not per pass.
Quarantine survives a host restart (it's a file).

### O3. Make the failover chain live, not spawn-time-only — P1, M — lane R, after S6

**Problem.** Provider health is applied only when a container spawns: the
host hydrates the health map, selects, and writes `runner.json` once. A
primary that dies mid-session burns per-turn retries against it until the
next respawn; a primary that *recovers* is never restored mid-session. The
in-container chain and the host's health view are two disconnected brains.

**Change.** The runner re-consults the chain's health state at
provider-call construction, using the cooldown/re-probe semantics already
in `crates/copperclaw-providers/src/failover.rs`, and records
failures/successes as it goes. The host-side spawn snapshot
(`container_manager/provider_failover.rs`) is unchanged. Transitions logged
with the existing "switched to <provider>" user note; transition counts
recorded as an M1 wish.

**Acceptance.** Unit (mock providers): mid-session primary death → next
call selects the fallback without a respawn; primary recovery after
re-probe window → restored. Single-provider (empty chain) behavior
byte-identical.

### O4. Opt-in operator alert destination — P1, L — lane H, after F3 (+ declared one-string W touch)

Security review recorded in the PR (new config surface + new outbound
path).

**Problem.** Nothing pushes to an operator. The apology copy's old claim of
notification was a log line and a counter (made honest in S2); loop death,
crash loops, quarantine, and spawn-failure streaks are all pull-only —
visible exactly when an operator happens to run doctor or watch metrics.

**Change.** Per decision (d): new
`crates/copperclaw-host/src/operator_alerts.rs` — an opt-in configured
channel destination (group-config/env) to which the host enqueues
rate-limited, deduped system alert rows through the existing delivery
pipeline. Call sites: supervisor permanent failure (S1), crash-loop/OOM
threshold (S4), quarantine (O2), spawn-failure streaks. When configured,
the apology copy conditionally restores "the operator has been notified"
(the one-string W touch).

**Acceptance.** Unit: rate limiting and dedup (one alert per episode, not
per sweep pass); disabled by default with zero new outbound. Integration:
a loop permanent-failure with the destination configured → exactly one
alert row delivered to the configured channel; without configuration →
nothing but the log + metric. Security review recorded before merge.

### X-rider (Wave 3). Operator-surface fixtures — P2, S — lane X

Fixtures/tests: doctor rows for each new check in healthy + failing states;
quarantine sidecar → doctor FAIL → sweep exclusion; mid-session failover
transition (mock provider); alert enqueue → delivery for a loop-death
event, and silence when unconfigured.

---

## M1. Metrics rider — P2, M — lane M, absolute last

No card except this one edits `copperclaw-metrics`. Sweep the PR-recorded
wishes: loop liveness gauges + restart counts (S1); stuck restarts by
reason (S2); retry resumes + dead-letters by reason including `no_adapter`
(S3/S5); OOM kills + backoff level (S4); spawn-duration histogram +
slow-spawn notices (F1); question expiries (F2); recovery notices (F3);
MCP connection cache hit/miss (F4); integrity outcomes + quarantines (O2);
failover transitions (O3); operator alerts by severity (O4); and the
long-wished `sweep_last_run_timestamp` gauge. Update
`docs/observability.md` with the new series and recommended alerts.

## Wave summary

| Wave | Theme | Cards | Parallel lanes |
|---|---|---|---|
| 1 | Nothing dies silently | S1→S4→S2 (H/W); S3→S5 (D); S6 (R); X-rider | H is the long pole; D and R fully parallel |
| 2 | The user is never in the dark | F1→F3 (H); F2 (W+T); F4 (T); X-rider | H, W+T parallel; F4 after F2 in T |
| 3 | Operators can see and trust it | O2 (W); O1 (A, after O2); O3 (R); O4 (H); X-rider | W→A sequenced; R and H parallel |
| last | Metrics | M1 | single card |

Critical path to the felt outcome: **S1 + S2 + S3** are the trust backbone;
**F1** is the marquee felt change; **O1** is what makes an operator believe
the rest. **S6** and **F4** are slippable (P2 — nothing depends on them
except X-rider convenience and latency polish).

## Program-level acceptance

1. **Chaos smoke.** Panic-inject each background loop → auto-restart within
   backoff; the status handler and doctor show the event; permanent failure
   degrades loudly instead of silently.
2. **Hung-tool smoke.** A tool sleeping past the ceiling → container
   restarted within the ceiling + one sweep interval; the user receives the
   recovery apology once; the next message works.
3. **Restart resilience.** Host restarted mid-retry → counters resume and
   exhaustion dead-letters exactly once; users with in-flight turns get the
   recovery notice exactly once.
4. **Cold-spawn UX.** First message to a fresh session shows typing within
   one tick; a forced slow image build produces exactly one notice, never
   two; a failed spawn still ends in the apology.
5. **Doctor detects the matrix.** Runtime down, corrupt DB, dead loop,
   stuck session, empty failover chain — each a FAIL/WARN row with a
   working `fix:` line.
6. The integrated gate is green (zero failures) after every multi-PR merge;
   the security review is recorded for O4; default-change arguments are
   recorded for S2 and F1.

## Deferred / rejected (don't re-litigate)

- **Inbound reaction steering parity** (17 of 21 channels don't parse
  reactions) and **read receipts** on `ChannelAdapter` — real gaps, but
  wide multi-adapter fan-out for modest payoff; typing + HUD already signal
  receipt. Scoped out of M21 by decision; a channel-parity milestone can
  take them together.
- **Relaying agent-to-agent failures to upstream humans** — silent-by-design
  stands; the sweep already routes an Agent-kind apology upward. Revisit
  with an a2a-focused milestone.
- **Auto-replay of dead-lettered messages** — stale-delivery hazard;
  manual `cclaw dropped-messages replay` + O1 visibility is the posture.
- **Runner-side per-tool process kill** — S2's container restart is the
  reliable actuator; finer grain is demand-pull if restarts prove too
  blunt.
- **Persisting crash-loop backoff across host restarts** and **a migration
  for quarantine state** — restart re-baselines everything anyway; the
  sidecar file suffices.
- **Webhook / email operator push** — O4's channel reuse covers the need
  without a new outward-facing surface.
- **Full `integrity_check` every sweep pass** — rotating `quick_check`
  only; cost scales with fleet size.
- **Replay-fixture capture tooling (`COPPERCLAW_FIXTURE_CAPTURE`)** —
  deferred again; S6's clock seam is the fixture investment this program
  takes.
- **Token streaming, periodic "still working" messages, deploy-to-cloud,
  runtime plugin registry** — standing non-goals, unchanged from
  M17/M18/M19/M20.
