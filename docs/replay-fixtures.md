# Replay fixture suite

A replay fixture is a captured platform interaction — webhook
bodies, gateway frames, REST responses — bundled with the
configuration that produced it. Replaying a fixture against a
freshly-spawned Copperclaw host should produce a byte-identical
outbound trace.

This is differential testing: the same input that hit production
yesterday should produce the same routed `InboundEvent`, the same
`messages_in` row, and (with a deterministic Claude stub) the same
container reply today.

The replay suite is the M11 acceptance gate. It catches regressions
that unit tests miss because they exercise individual layers; the
suite exercises the whole pipeline end-to-end with real platform
payloads.

---

## Fixture shape

A fixture is a directory:

```
fixtures/<channel>/<scenario>/
├── manifest.json             # fixture metadata + replay plan
├── central.sql               # central-DB seed (groups, users, wirings)
├── inbound/
│   ├── 001-message.json      # adapter-shaped inbound payload
│   └── ...
├── claude/
│   ├── 001-turn.json         # mock Claude response (or full SSE)
│   └── ...
└── expected/
    ├── inbound-events.jsonl  # one InboundEvent per line
    ├── messages-in.jsonl     # rows the router should have written
    ├── messages-out.jsonl    # rows the container should write
    └── delivered.jsonl       # outbound platform calls
```

The harness lives in `crates/copperclaw-host/tests/replay/`; the loader
is `fixture.rs::load`.

### `manifest.json`

```json
{
  "name": "telegram-inbound-text-message",
  "channel": "telegram",
  "description": "Plain-text DM, single Claude turn, no attachments.",
  "schema": 1,
  "replay": {
    "mode": "direct",
    "step_timeout_ms": 5000
  },
  "substitutions": {
    "[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}": "<UUID>",
    "\"timestamp\":\"[^\"]+\"": "\"timestamp\":\"<TS>\""
  }
}
```

Optional fields used by failure-mode fixtures:

- `adapter_caps`: `{ "<channel>": <usize> }` — override the per-channel
  `max_message_chars` cap the harness's wrapped `MockAdapter` reports
  (defaults: `telegram=4096`, `slack=40000`, `discord=2000`, etc.; see
  `default_cap_for` in `crates/copperclaw-host/tests/replay/harness.rs`).
  Used by the `*-long-message-split` fixtures to exercise the delivery
  loop's chat-text splitter.
- `pre_delivery_failures`: `[{"channel": "telegram", "kind": "rate",
  "retry_after": 7}]` — queue one or more `MockAdapter::fail_next_deliver`
  errors on the named channel before driving inbound. Accepted `kind`s:
  `"rate"`, `"transport"`, `"bad_request"`. Used by `rate-limited-retry`
  to pin the slice-1 contract where `bump_retry` honours
  `Rate { retry_after }` over the default exponential backoff.
- `redrive_after_ms`: `<u64>` — after each per-step delivery pass, sleep
  the given milliseconds and call `DeliveryService::process_session_once`
  a second time. Lets a fixture pin "row deferred on first tick,
  delivered on the second tick after the retry window elapses" without
  poking at `DeliveryService`'s private retry-state map.
- `runner_drain`: `true` — run the per-step runner in DRAIN mode
  instead of the default `max_turns = 1` mode: `run_loop` is raced
  against a watcher that resolves once the session's `messages_in` has
  no `pending` rows left, then the loop is cancelled. Needed by the
  `slash-clear` / `slash-compact` fixtures because the runner's
  slash-command sentinel handles a pure command batch synchronously
  and `continue`s WITHOUT counting a turn, so a `max_turns`-bounded
  loop would never return.

Two harness behaviours the slash-command fixtures rely on (M18 R1):

- Per inbound step, the runner only runs when the session has due
  `trigger = 1` work (`messages_in::count_due` — the same gate the
  container manager's spawn classifier applies). A `/stop` control row
  is written with `trigger = 0`, so a control-only step runs no turn.
- A `RouteOutcome::Answered` route (host-answered `/status`) skips the
  runner entirely and goes straight to the delivery pass — the router
  already wrote the synthesized reply to `messages_out`.

`replay.mode` is `direct` for the current harness — inbound payloads
are handed to the adapter's `MockAdapter::inject` rather than through
a webhook listener / gateway / poll / rpc front-door. (The
webhook / gateway / poll / rpc modes named in earlier design docs are
not implemented; the `manifest.toml` story was deferred behind an
unused dep, hence the `.json` filename.)

### `inbound/NNN-*.json`

Adapter-shaped inbound payload — the bytes the adapter would see
after its parser has lifted the raw HTTP / gateway frame into the
adapter's `InboundEvent` (or equivalent) struct. Fixtures hand this
directly to the adapter's mock instead of replaying transport.

The replay harness:

- Reads each `inbound/` file in name order.
- Constructs the request against the running host's HTTP listener
  (or, for gateway / poll channels, feeds frames through the
  abstracted transport trait the channel uses for testing).
- Waits up to `step_timeout_ms` for the host to write the next
  expected event, then advances to the next file.

### `claude/NNN-turn.json`

```json
{
  "events": [
    {"type": "message_start", "message": {"id":"msg_x","content":[]}},
    {"type": "content_block_delta", "delta":{"type":"text_delta","text":"hi"}},
    {"type": "message_stop", "stop_reason":"end_turn"}
  ]
}
```

A `wiremock`-fronted Claude stub serves these in order. One step =
one Claude turn. If the container makes a tool call, the fixture
includes a follow-up `claude/NNN+1-turn.json` with the tool-result
continuation.

Failure-mode fixtures can override the in-order default with an
explicit `provider_responses` plan in the manifest (see
`ProviderResponseSpec` in `crates/copperclaw-host/tests/replay/fixture.rs`
for the full key list: scripted errors, timeouts, per-call file
selection). Since M21 S6 any entry may also set `advance_clock_ms`:
when that scripted call is served, the harness advances the shared
runner `TestClock` (`copperclaw-runner/src/clock.rs`) injected into
every per-step runner's `RunnerDeps.clock`. This is how a fixture
makes time pass *inside* a single turn's tool loop, which the runner's
timed legs (e.g. the Task HUD's 60s status-row cadence and 150s
softening — see `fixtures/cli/status-row-heartbeat/`) need; the clock
is frozen apart from explicit advances, so elapsed renderings in
expected streams are exact. Tests driving the harness directly can
instead call `ReplayHarness::advance_clock` between steps.

Note the reach of that clock: it is injected into the RUNNER's
`RunnerDeps.clock` only. Host-side timers — the sweep cadence, the
S4 crash-loop backoff, the S1 supervisor backoff — run on tokio/wall
time and are NOT traversable from a fixture; they are pinned by
paused-clock crate tests instead. `fixtures/README-m21-wave1.md`
carries the full reachability map.

Since the M21 Wave-1 X-rider, tests driving the harness directly can
also call `ReplayHarness::restart_delivery()` — kill and recreate the
`DeliveryService` (fresh in-memory retry cache and a fresh
`MockAdapter` set) over the same central DB and per-session files, so
a test can pin restart-resume semantics (see
`fixtures/cli/delivery-retry-restart/`).

Since M21 F2 two more direct-drive seams exist:
`ReplayHarness::run_steps(start, end)` drives a contiguous subrange of
the fixture's inbound steps (`run()` delegates to it for the full
range), so a registered test can interleave imperative host-side work
— e.g. a `SweepService` pass — BETWEEN two fixture steps; and
`ReplayHarness::install_interactive_module(&module)` installs an
`InteractiveModule` against the harness delivery service (module
delivery-action registrations now forward to it, mirroring the host's
`HostContext`). See `fixtures/cli/question-expiry/` for both in use.

### `expected/*.jsonl`

After the replay completes, the harness diffs:

- `inbound-events.jsonl` against the router's audit trail.
- `messages-in.jsonl` against the per-session inbound DB rows
  (sorted by `seq`).
- `messages-out.jsonl` against the per-session outbound DB rows.
- `delivered.jsonl` against the channel adapter's `deliver()`
  invocations (captured by a tee adapter).

Diffs are reported as JSON-pointer paths so the failure surface is
specific: `messages-in[0].content.text` differs by 1 char.

---

## Harness layout

The replay harness lives in `crates/copperclaw-host/tests/replay/`
(or a dedicated `crates/copperclaw-replay/` if the surface grows). It
re-uses everything in the host's existing integration test surface:

- `CentralDb::open_in_memory()` for the central DB.
- An `in_memory_runtime::FakeRuntime` implementing
  `ContainerRuntime`, which spawns an in-process runner.
- The channels' `MockTransport` / `MockBridge` trait impls (every
  M8 channel ships one) — the harness wires the fixture's
  `inbound/` files into the appropriate mock.
- A `wiremock::MockServer` for the Anthropic stub, configured per
  fixture.

A fixture run is structurally:

```rust
let fixture = Fixture::load("telegram-text-reply")?;
let mut harness = ReplayHarness::new(fixture).await?;
harness.boot_host().await?;
harness.run().await?;          // drives every inbound/ step
let report = harness.compare().await?;
assert!(report.is_clean(), "{report}");
```

`ReplayHarness::run` drives the channel's mock transport, the
Claude stub, and waits for the host's `messages_in` / `messages_out`
to settle between steps. `compare()` produces a `DiffReport` with
zero or more `Mismatch { path, expected, actual }` entries.

---

## Authoring new fixtures

There is no automatic capture pipeline. (Earlier revisions of this doc
described a `COPPERCLAW_FIXTURE_CAPTURE` env var and a
`crates/copperclaw-host/src/fixture/redact.rs` redaction pass; neither
was ever implemented. Live capture-from-a-running-host remains a
deferred candidate — an M25 card per
`docs/plans/m24-debt-and-unlock-program.md` T2.) Fixtures are
hand-authored, with one mechanical assist for the `expected/` streams:

1. **Author the inputs by hand.** Create
   `fixtures/<channel>/<scenario>/` with `manifest.json`,
   `central.sql`, `inbound/NNN-*.json`, and `claude/NNN-turn.json` —
   copy a neighbouring fixture for the same channel as a skeleton.
   Base payload shapes on real platform payloads where you have them
   (a webhook body from logs, an SSE trace), redacted and trimmed by
   hand.
2. **Register a test** in `crates/copperclaw-host/tests/replay.rs`.
   Fixtures only run through their registered test — there is no
   directory auto-discovery.
3. **Generate the expected streams from a real harness run.** Rather
   than hand-guessing `expected/*.jsonl`, most registered tests carry
   a generate path guarded by a per-card env var: when the var is set,
   the test runs the fixture through the harness and calls
   `ReplayHarness::dump_expected_jsonl()` instead of asserting the
   diff. The dump prints each captured stream as JSONL fenced with
   `===DUMP <stream>===` / `===END <stream>===` markers, with the
   manifest's substitutions already applied (so lines carry the
   `<UUID>` / `<TS>` placeholders the diff expects); slice each block
   into the matching `expected/<stream>.jsonl`. For example:

   ```
   COPPERCLAW_M22W0_GENERATE=1 cargo test -p copperclaw-host \
       --test replay cli_tool_use_shell -- --nocapture
   ```

   The guard vars are per card-family, not global. As of M24 the
   registered ones are `COPPERCLAW_X2_GENERATE`,
   `COPPERCLAW_XR1_GENERATE`, `COPPERCLAW_XR2_GENERATE`,
   `COPPERCLAW_XR3_GENERATE`, `COPPERCLAW_CX_GENERATE`,
   `COPPERCLAW_M21F2_GENERATE`, `COPPERCLAW_M21X1_GENERATE`,
   `COPPERCLAW_M21X2_GENERATE`, `COPPERCLAW_M21X3_GENERATE`,
   `COPPERCLAW_M22W0_GENERATE`, and `COPPERCLAW_M23W3_GENERATE` —
   grep `_GENERATE` in `tests/replay.rs` for the current list and
   which tests each one gates. When registering a new fixture's test,
   add the same guard shape so its streams can be regenerated after
   intentional pipeline changes.
4. **Stabilise.** Add `substitutions` to `manifest.json` for any field
   that varies between runs (timestamps, generated ids). Never
   substitute a field the fixture is asserting on.
5. **Review the generated streams.** The dump is captured output, not
   an oracle — read the JSONL and confirm it shows the behaviour the
   fixture exists to pin before committing it as expected.
6. **Bisect to minimum.** Drop steps until the behaviour still
   reproduces. Smaller fixtures fail faster and survive refactors.
7. **Commit.** Land in `fixtures/<channel>/<scenario>/` with a
   one-paragraph README explaining what the fixture asserts and
   what bug (if any) it captured. CI re-plays every fixture on
   every PR.

---

## Conventions

- One fixture per behaviour, not per platform. A telegram fixture
  that asserts "long-poll resume after restart" and a separate one
  for "webhook with media" both pull their weight.
- Fixtures are hand-authored today, but they should mirror captured
  reality: base payload shapes on real platform payloads (redacted by
  hand) wherever a recording exists, and reserve invented shapes for
  paths reality can't produce on demand (typically error paths, e.g.
  a 429 with a specific `Retry-After`).
- The Claude stub is allowed to be hand-authored. The container's
  Claude calls are not platform-level reality; they are responses
  to whatever the fixture sets up, and tightly-controlled stubs are
  more readable than recordings.
- Substitutions are evaluated before diffing. Never substitute
  fields the test is asserting on — that masks regressions.
- Fixtures live in-tree. They are part of the test suite, not test
  data downloaded at CI time.

---

## What the suite does **not** cover

- **Container build correctness.** Image-build, package install,
  and skill mount are covered by `copperclaw-container-rt`,
  `copperclaw-skills`, and the runner integration tests. Replay
  fixtures assume the container is up.
- **OneCLI authentication.** Replays run with `Caller::Host` and a
  fake CLI scope; OneCLI's gateway is tested independently.
- **Real network.** The harness never opens an outbound socket.
  Every transport goes through a trait impl whose test variant is
  in-process.
- **Webhook authentication / signature rejection paths.** The harness
  drives `replay.mode = "direct"`, pushing already-parsed `InboundEvent`s
  at the router. The HMAC / bearer-token checks live in the channel
  adapter's webhook handler (e.g. `telegram::ingress::webhook`,
  `whatsapp-cloud::events::router`), which the harness skips entirely.
  These paths have their own unit tests in the channel crate; pinning
  "401 on bad `X-Telegram-Bot-Api-Secret-Token`" through a replay
  fixture would require a new `replay.mode = "webhook"` variant that
  wires up the adapter's HTTP listener — not implemented today.

When a regression surfaces that the suite does not catch, the right
response is to capture a new fixture, not to bend the harness.
