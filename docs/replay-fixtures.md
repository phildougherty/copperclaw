# Replay fixture suite

A replay fixture is a captured platform interaction — webhook
bodies, gateway frames, REST responses — bundled with the
configuration that produced it. Replaying a fixture against a
freshly-spawned Ironclaw host should produce a byte-identical
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

The harness lives in `crates/ironclaw-host/tests/replay/`; the loader
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
  `default_cap_for` in `crates/ironclaw-host/tests/replay/harness.rs`).
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

The replay harness lives in `crates/ironclaw-host/tests/replay/`
(or a dedicated `crates/ironclaw-replay/` if the surface grows). It
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

## Capturing new fixtures

When a real platform interaction reveals a bug, capture it for the
replay suite:

1. **Record.** Run the host with `IRONCLAW_FIXTURE_CAPTURE=<dir>` —
   the channel adapter and router will tee inbound bodies, the
   Anthropic provider will tee the SSE stream, and the database
   layer will tee writes. The directory ends up with the same shape
   as a hand-authored fixture.
2. **Redact.** `target/fixture-capture/...` will contain bearer
   tokens, signing secrets, and personal text. The redaction pass
   lives at `crates/ironclaw-host/src/fixture/redact.rs` but is not
   wired to a CLI subcommand — write a small one-shot Rust binary
   or shell script that walks the capture dir and feeds each file
   through `redact::redact_bytes` (see the unit tests in that file
   for usage). Manual review is still required.
3. **Stabilise.** Add substitutions to `manifest.toml` for any
   field that varies between recordings (timestamps, generated
   ids, server-side message ids).
4. **Bisect to minimum.** Drop steps until the bug still
   reproduces. Smaller fixtures fail faster and survive refactors.
5. **Commit.** Land in `fixtures/<channel>/<scenario>/` with a
   one-paragraph README explaining what the fixture asserts and
   what bug (if any) it captured. CI re-plays every fixture on
   every PR.

---

## Conventions

- One fixture per behaviour, not per platform. A telegram fixture
  that asserts "long-poll resume after restart" and a separate one
  for "webhook with media" both pull their weight.
- Fixtures are **not** mocks — they are captured reality plus a
  redaction pass. Hand-authored fixtures are allowed only when no
  real recording is available (typically for error paths, e.g. a
  429 with a specific `Retry-After`).
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
  and skill mount are covered by `ironclaw-container-rt`,
  `ironclaw-skills`, and the runner integration tests. Replay
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
