# M17 — Agentic capability + UX program (parallel-agent execution plan)

Comprehensive improvement program covering agentic capabilities, end-user UX,
CLI tooling, usability, and tool capabilities. Written 2026-07-09 from a
five-subsystem audit of the tree at branch `feat/runner-external-mcp`
(post-M16, pre-0.1.0).

This document is written to be executed by **parallel agents**, one task card
per agent. Each card declares an exclusive file scope; two cards in the same
wave never share a scope. Read this whole preamble before taking a card.

---

## Rules for every implementing agent

1. **Read `CLAUDE.md` first.** All of it applies. In particular:
   - `cargo fmt --all && cargo check --workspace && cargo clippy --workspace
     --all-targets -- -D warnings && cargo test --workspace --no-fail-fast`
     must pass before you declare done. Baseline ~6,660 tests; do not break it.
   - Workspace forbids `unsafe_code`. Clippy warnings are errors.
   - Every user-visible change gets a `CHANGELOG.md` line under
     `## [Unreleased]` (`### Added` / `### Changed` / `### Fixed`). The
     changelog is a **merge hotspot** — write your line as the last step and
     keep it to your task only.
   - Never edit a released migration. New DB state = new numbered migration in
     `crates/copperclaw-db/migrations/`. Next free number: check the directory
     (023/024 are taken by the external-MCP branch).
2. **No stubs in tree** (project tenet 1). If your card can't be finished
   whole, deliver a smaller whole thing, not a scaffold. A tool that is
   registered must work end-to-end.
3. **Secure-by-default** (tenet 2). New capability is opt-in; the default path
   stays bit-for-bit unchanged unless the card says otherwise. Anything that
   ingests external content must mark the turn untrusted
   (`mark_untrusted_context`); anything that mutates host state must write an
   audit row.
4. **File:line anchors in this doc are from the audit** — verify them before
   editing; the tree moves. Anchors are orientation, not gospel.
5. **Hotspots to serialize** (do not touch outside your card's scope):
   - `crates/copperclaw-metrics/src/lib.rs` — many crates touch it. If your
     card adds a metric, add ONLY your metric and its accessor.
   - `crates/copperclaw-host-delivery/src/service.rs` (~6,200 lines) — three
     cards touch it across waves; they are sequenced, never concurrent.
   - `CHANGELOG.md` — append-only, one line per card, expect merge conflicts
     and resolve by keeping both lines.
6. **Fixtures before pipeline changes** (project convention): if your card
   touches inbound → router → runner → outbound → delivery flow, add or extend
   a replay fixture under `fixtures/<channel>/<scenario>/` first.
7. **Tests**: every card lists required tests. Unit tests live next to the
   code; e2e/replay tests in `crates/copperclaw-host/tests/`. Mock providers
   and adapters already exist — mirror how neighboring tests do it.

## Scope / conflict map

| Scope key | Files | Cards |
|---|---|---|
| RUNNER | `crates/copperclaw-runner/src/**` | A1, A2, A4, A7 |
| MCP-TOOLS | `crates/copperclaw-mcp/src/tools/**` | A5, B4, B5, B6 |
| MCP-EXT | `crates/copperclaw-mcp/src/external.rs`, `crates/copperclaw-runner/src/run/external_mcp.rs` | B1a, B1b |
| BROWSER | `crates/copperclaw-browser/**`, `crates/copperclaw-mcp/src/tools/browser_render.rs` | B2 |
| DELIVERY | `crates/copperclaw-host-delivery/src/**` | C1, C2 (sequenced), A3 (wave 2) |
| ROUTER | `crates/copperclaw-host-router/src/**` | C4 |
| CM | `crates/copperclaw-host/src/container_manager/**` | C3 |
| ADAPTER-<name> | `crates/copperclaw-channels/<name>/**` | C5 (one card per adapter) |
| CHANNELS-CORE | `crates/copperclaw-channels/core/src/**` | C6 |
| CCLAW | `crates/copperclaw-cclaw/src/**` | D1, D2, D3, D4, D5, D6, D7, D8 (sequenced within wave — see wave notes) |
| SETUP | `crates/copperclaw-setup/src/**` | E2, D8c |
| HOST-MISC | `crates/copperclaw-host/src/` (non-CM) | D8b, E4 |
| DB | `crates/copperclaw-db/**` | migrations ride with their owning card |

The runner + providers are one tightly-coupled scope: **at most one RUNNER
card in flight at a time.** Same for CCLAW (single crate, shared
`commands.rs`/`lib.rs`) — CCLAW cards within a wave run sequentially or on one
agent.

---

## Wave 0 — prerequisite (not a card)

Land `feat/runner-external-mcp` to main. Everything below assumes it merged:
the external-MCP plumbing (migrations 023/024, `mcp_calls.rs`,
`external.rs`, `external_mcp.rs`, `drain_mcp_calls`) is the base for B1a/B1b,
and its policy changes (`mcp__*` treated as credentialed-external) are the
base for every provenance note below.

---

## Wave 1 — "the agent feels alive"

Five cards, five disjoint scopes, all parallel-safe.

### A1. Parallel tool execution — P0, S — scope: RUNNER

**Problem.** Tool calls collected from a stream execute strictly sequentially
in a `for` loop (`crates/copperclaw-runner/src/run/drive_turn.rs:442`), while
the system prompt explicitly tells the model to issue independent calls in
parallel. Multi-tool turns pay full serial latency.

**Change.** Execute the batch concurrently (`futures::future::join_all` over
`invoke_tool`), then append results to history **in the original call order**
so transcripts stay deterministic and tool_use/tool_result pairing is
preserved. Keep these sequential-only behaviors intact:
- The `ToolLoopGuard` fingerprint check (`drive_turn.rs:208,325`) runs per
  call before dispatch — evaluate it against the batch before spawning.
- `HeartbeatTicker` must stay alive across the whole batch.
- `persist_mid_message` (`drive_turn.rs:107,487`) runs once after the batch,
  as today.
- Tools that mutate shared shell state (`shell` persists cwd/env via
  `/data/.shell_state`) must not interleave: if a batch contains more than one
  `shell` call, run the `shell` calls in order relative to each other
  (concurrent with everything else). Same guard for the edit family targeting
  the same path (`edit_file`/`multi_edit`/`apply_patch`/`write_file`): group
  by path, serialize within a group.

**Acceptance.**
- A turn with N independent `read_file` calls completes in ~max, not ~sum
  (assert with a slow mock tool in a unit test).
- Two `shell` calls in one batch observe each other's cwd changes in order.
- Existing drive_turn tests green; transcript ordering byte-identical for a
  single-tool turn.

**Tests.** Unit tests in drive_turn: concurrency (timing with mock tools),
ordering, shell serialization, per-path edit serialization, loop-guard trip on
a duplicate-heavy batch.

### C1. Continuous typing signal — P0, S — scope: DELIVERY

**Problem.** `set_typing` fires once, immediately before delivering a chat row
(`crates/copperclaw-host-delivery/src/service.rs:1261-1266`). During a long
tool run the user sees nothing. Only telegram/discord/matrix/signal(/slack
partial) implement typing at all.

**Change.** On each active-loop tick (1s, `service.rs:34`), for every session
that is `Running` with an in-flight turn (pending `processing_ack` row or
fresh heartbeat + undelivered inbound), re-fire `set_typing` best-effort,
throttled per platform (telegram's action lasts ~5s — re-ping every 4s, keep a
per-session last-ping timestamp in memory; do not add DB state). Stop when
the chat row delivers or the turn ends. Must be best-effort: adapter errors
logged at debug, never affect delivery.

**Acceptance.** With a mock adapter recording `set_typing` calls, a simulated
60s turn produces periodic typing calls (throttled), and zero calls once the
reply lands. No behavior change for adapters whose `set_typing` is the no-op
default.

**Tests.** Delivery-service unit test with mock adapter + fake clock;
replay fixture unaffected (typing is not persisted).

### C3. Event-driven wake for idle sessions — P0, M — scope: CM (+ ROUTER hook, read-only touch)

**Problem.** A message to an idle/stopped session waits for the reconcile poll
to notice pending inbound; worst-case wake latency is bounded by sweep cadence
(60s, `SWEEP_POLL_MS`) for sessions the 1s loop isn't watching.

**Change.** Router and container manager live in the same host process. Add a
`tokio::sync::Notify` (or an mpsc of session ids) owned by
`ContainerManager`; the router signals it after inserting a `messages_in` row
(`crates/copperclaw-host-router/src/route.rs` insert path). On signal, the
container manager runs an immediate `tick()` for that session (or a global
tick if per-session is invasive). Polling stays as the fallback — the notify
is an accelerator, not a replacement (crash-safe by construction).

**Acceptance.** e2e: message to a stopped session spawns a container within
~1 poll interval, not a sweep interval. No spawn storms: notify coalesces
(Notify semantics) and `classify()` remains the single decision point.

**Tests.** e2e in `crates/copperclaw-host/tests/` with the replay harness:
stopped session + inbound → spawn latency assertion. Unit: notify coalescing.

### D1. cclaw color + TTY awareness — P0, S — scope: CCLAW

**Problem.** Zero color infrastructure in cclaw (no style crate in its
`Cargo.toml`). doctor OK/WARN/FAIL, table headers, remote errors are
monochrome; there is no `IsTerminal` check client-side.

**Change.** Add a minimal style layer (e.g. `anstyle` or `owo-colors`,
workspace-dependency it) gated on `std::io::IsTerminal` AND absence of
`NO_COLOR`, plus `--no-color` global flag. Apply to: doctor levels (green
OK / yellow WARN / red FAIL, `lib.rs:698-1032`), `fix:` hint lines (cyan),
table headers (`output.rs:82-119`, bold), `remote error:` lines (red,
`lib.rs:222`), dashboard section headers (`lib.rs:1298`). `--json` output must
remain byte-identical — never style JSON.

**Acceptance.** `cclaw doctor | cat` produces no ANSI codes. `NO_COLOR=1`
produces none. TTY run is colored. All existing output-format tests green.

**Tests.** Unit tests on the style-decision function; snapshot test that
piped output is ANSI-free.

### D2. Fix `sessions get`, add `sessions tail` — P0, M — scope: CCLAW (+ read-only host handler)

**Problem.** `cclaw sessions get` help text claims it returns "last few
inbound/outbound rows" (`commands.rs:936-940`) but the host handler returns
only the session row (`sessions.rs:27-31`). The real diagnosis flow (CLAUDE.md
"agent isn't replying", steps 6a/6b) requires hand-opening two SQLite files.

**Change.**
1. Make `sessions get` honest: return the session row PLUS the last N (10)
   `messages_in` and `messages_out` rows (kind, status, ts, content preview
   ~120 chars) by opening the per-session DBs read-only host-side (the host
   already knows the session dir).
2. Add `cclaw sessions tail <id> [--follow]`: one-shot prints the merged,
   time-ordered recent rows from both DBs; `--follow` polls 1s and prints new
   rows with direction markers (`<-` inbound, `->` outbound, `--` breadcrumb/
   status kinds). Read-only; callable while the session runs (SQLite WAL
   handles the concurrent reader).

**Acceptance.** `sessions get` output matches its help text. `tail --follow`
against a live replay-harness session shows rows appearing in order. Content
previews pass through the existing redaction shape checks (no raw secrets).

**Tests.** Host-side handler unit tests with seeded per-session DBs; e2e in
the replay harness asserting tail output.

### Wave 1 rider: A1+C1 metric — scope: METRICS (single tiny card, run last)

After A1 and C1 land: add `copperclaw_tool_batch_size` histogram (runner
usage_report already carries per-turn tool counts) and
`copperclaw_typing_pings_total{channel_type}`. One agent, metrics crate only,
plus the two one-line call sites. Keeps the hotspot serialized.

---

## Wave 2 — control (cancel, steer, progressive replies)

A2 and C4 are two ends of one feature; land C4 (router/UX end) first or
together — A2 consumes the rows C4 produces. A3 depends on A2's turn-state
plumbing only loosely (can run parallel to C4, after A2).

### C4. End-user slash commands — P1, M — scope: ROUTER

**Problem.** No `/cancel`, `/help`, `/reset`, `/status` anywhere. Telegram
`bot_command` entities are parsed for mention detection only
(`telegram/src/ingress/mod.rs:628`). Users cannot stop or inspect an agent
from chat.

**Change.** A command layer in the router **before** mention gating
(`route.rs:193`), applied to messages whose text starts with `/` from a
resolved (non-guest) sender:
- `/help` — synthesized outbound reply listing commands; never reaches the
  agent.
- `/status` — synthesized reply: session state, heartbeat age, current todo
  list snapshot if available.
- `/reset` — writes the existing clear-history sentinel (same mechanism as the
  `clear_history` tool) for the session; confirms via synthesized reply.
- `/cancel` — inserts a `messages_in` row with a new `kind = 'control'` and
  `content = {"op":"cancel"}` (migration NOT needed if `kind` is TEXT —
  verify; otherwise new migration). A2 consumes it. Confirm via reply.
Unknown `/commands` fall through to the agent unchanged (people type `/s` in
prose). Group chats: command must also pass the mention gate to avoid
hijacking shared channels — commands in groups require mention.

**Acceptance.** Replay fixture: `/cancel` mid-long-turn produces a control row
and a confirmation without waking a second turn. `/help` works on cli channel.
Non-command messages byte-identical through the router (fixture diff clean).

**Tests.** Router unit tests per command; new replay fixture
`fixtures/cli/slash-commands/`.

### A2. Mid-turn interruption + steering — P0, M — scope: RUNNER

**Problem.** `run_loop` polls inbound only between turns
(`crates/copperclaw-runner/src/run/mod.rs:583`); a human message queues behind
a long build; nothing can stop an in-flight turn.

**Change.** Between tool turns inside `drive_turn` (the natural cooperative
point, after each batch + `persist_mid_message`), peek `inbound.db`:
- New human Chat row → inject into the transcript as a user message annotated
  as an interjection, and continue the turn (the model decides whether to
  pivot). Mark the row processed so `run_loop` doesn't double-handle it.
- `control{op:cancel}` row (from C4) → stop the turn cleanly: persist state,
  emit a short "stopped — here's where things stand" reply, mark inbound rows
  appropriately, return to the poll loop.
Autonomous turns: interjection injection applies; cancel applies. Do NOT
check mid-provider-call — only between turns (keeps the provider stream
logic untouched).

**Acceptance.** Replay/e2e: cancel row lands during a multi-tool turn → turn
ends within one tool-turn boundary, session reusable immediately after.
Interjection appears in transcript in order. No double-processing (the batch
pickup in `run_loop` `mod.rs:583-595` must skip rows consumed mid-turn).

**Tests.** Runner unit tests with mock provider (multi-turn script);
e2e fixture pairing with C4's.

### A3. Progressive reply delivery — P0, M — scope: RUNNER (emit side) then DELIVERY (edit path — sequenced after C1/C2 merge)

**Problem.** Assistant text is emitted once at turn end
(`drive_turn.rs:402`). Long answers feel dead even when generation is done in
stages.

**Change.** Do NOT build token streaming through the SQLite transport.
Instead: when accumulated assistant text crosses a threshold (~400 chars) and
the turn is still running (more tool calls pending), emit it as an early
`send_message`; on subsequent flushes, grow it via the existing `edit_message`
machinery (`adapter.rs:403`; breadcrumb edit path precedent
`service.rs:959-969`). Rich adapters (telegram/slack/discord/matrix/webex)
edit in place; others receive it as sequential parts (the existing chunking
fallback shape). Final flush replaces/completes the message. Feature-gate via
runner config (`progressive_replies`, default ON for rich channels — the
channel's `edit_message` capability is known host-side; plumb a capability
flag into `runner.json`).

**Acceptance.** On a mock rich adapter: a long multi-tool turn produces one
message edited K times, ending byte-equal to the single-shot reply text. On a
bare adapter: behavior unchanged (single final message) unless parts mode is
explicitly enabled. Mid-split resume (`service.rs:184-202`) still correct.

**Tests.** Runner unit (flush thresholds, final-equals-accumulated); delivery
unit (edit path); replay fixture on cli (unchanged output — cli is bare).

### D3 + D4 + E1 — CCLAW wave-2 batch (one agent, sequential) — scope: CCLAW

- **D3. `--watch`** — P1, S. `-w/--watch [secs]` on dashboard, health,
  sessions list: clear-screen + re-render loop, Ctrl-C exits. TTY-only
  (error otherwise).
- **D4. Interactive pickers** — P1, M. Where a positional id is required and
  stdin is a TTY and the arg is omitted: fetch the list, present a numbered
  picker (dialoguer, already used by setup). Also: accept unambiguous id
  prefixes — resolve client-side by listing then matching (no wire change).
- **E1. `cclaw sessions clear <id>`** — P1, S. "Clear history, keep files."
  Host-side handler writes the same clear-history sentinel the
  `clear_history` tool uses into the session dir (verify sentinel name in
  `crates/copperclaw-mcp/src/tools/clear_history.rs:27`); if the container is
  stopped, the sentinel applies at next spawn. Audit-log the mutation.

**Acceptance.** Picker never triggers when piped. `sessions clear` leaves
`/data` files intact and empties transcript on next turn (e2e). Watch mode
redraws.

---

## Wave 3 — capability

### B2. Live browser driver — P0, L — scope: BROWSER

**Problem.** Everything around `browser_render` is implemented and tested
(SSRF preflight + per-redirect re-guard `driver.rs:97`, taint, locked-down
child-container spec `container.rs`, opt-in gates) except the driver itself:
`BrowserDriver` is a trait with only a mock; live calls return
`ToolError::Internal` "driver not provisioned"
(`crates/copperclaw-mcp/src/tools/browser_render.rs:290-295`). The most
prominent advertised-but-nonfunctional feature in the tree.

**Change.** Implement the real driver behind the existing trait: headless
Chromium via CDP inside the already-specified hardened child container
(`COPPERCLAW_BROWSER_IMAGE`). Deliver exactly Phase 5a modes: `screenshot`
(PNG to host-side path — existing contract returns a path, not bytes),
`dom_text`, `aria_snapshot`. Navigation timeout, page-size caps, and
single-navigation-per-call (no session reuse — one render, one container or
one pooled instance if the pool is strictly per-session and torn down on
idle). Respect every existing gate: `COPPERCLAW_BROWSER_ENABLED=1` opt-in,
egress narrowed to nav target `host:port`, no broker token in the child, taint
on result. Interactive input (click/type/scroll) stays out of scope (5b,
explicit non-goal).

CDP client: prefer a thin hand-rolled CDP-over-WebSocket layer or a minimal
pinned crate (tenet 8 — pinned upstreams; check licensing + maintenance
before adding a heavy dependency like chromiumoxide; document the choice in
the PR).

**Acceptance.** With the browser image present and the env gate on:
`browser_render` of a local fixture HTTP server returns a real screenshot
file, real dom_text, real aria snapshot. With the gate off: unchanged refusal.
All existing security tests green unmodified. An integration test tagged
`#[ignore]` (needs Docker + image) exercises the live path; CI-safe tests
mock at the CDP boundary.

### A4. Hot in-session provider failover — P1, M — scope: RUNNER

**Problem.** `FallbackChain` (`crates/copperclaw-providers/src/failover.rs`)
is consumed only at spawn by the host
(`container_manager/provider_failover.rs`); a mid-session 429/529/outage is
survived only by respawn.

**Change.** Host writes the resolved fallback chain (ordered
provider/model/key entries — capability tokens where the broker is on) into
`runner.json` at spawn. In `query_with_retry`
(`run/provider_call.rs:474`), after `MAX_PROVIDER_ATTEMPTS` exhaust on the
current entry with a retryable/degrade-class error, step to the next chain
entry (rebuild the provider client, log, emit the failure reason in the
usage_report exactly as today so the host health fold stays authoritative for
next spawn). Prompt-cache note: switching providers invalidates the cache
prefix — acceptable; do not try to preserve it. Chain position resets per
turn (primary is always retried first next turn).

**Acceptance.** Unit: scripted provider that 529s → runner completes the turn
on the second chain entry; usage_report carries the degrade reason. Chain
absent from runner.json → behavior byte-identical to today.

### A5. Writable + semantic memory — P1, M — scope: MCP-TOOLS (+ one migration)

**Problem.** Agent memory is read-only (`tools/memory.rs` has only
`memory_search`/`memory_get`); embeddings columns exist (migration 021) but
every call passes `&[]` — retrieval is FTS-only.

**Change.**
1. `memory_write` (key, body, optional tags) and `memory_delete` (key) tools.
   Provenance: entries written during a tainted turn are stored
   `provenance='untrusted'` (schema already carries it); untrusted hits
   already taint future turns — this closes the loop rather than opening a
   hole. Both tools are in the `Messaging`+ tool profiles, blocked for Guest
   senders (mutating floor, `policy.rs:295` — verify the mutating list picks
   them up).
2. Embeddings: add an optional embedding hook — when the group's provider
   chain includes an Anthropic-family or OpenAI-compatible endpoint with an
   embedding model configured (`container_configs` gets an
   `embedding_model` nullable column — new migration), populate vectors on
   write and use cosine re-rank on search (linear scan is fine; store dim as
   today). No embedding model configured → pure FTS, exactly today's
   behavior.
3. `memory_search` gains a `k` re-rank note in its description; no schema
   change to the tool input.

**Acceptance.** Write→search→get round-trip; taint propagation test (write
during tainted turn → hit marked untrusted → next-turn credentialed action
blocked without approval); FTS-only path unchanged when no embedding model.

### B1a. External-MCP image passthrough — P1, S — scope: MCP-EXT (+ DELIVERY drain site)

**Problem.** Image content blocks in remote MCP results are dropped with an
`<image>` placeholder (`run/external_mcp.rs:127-135`).

**Change.** Host-side (`drain_mcp_calls` / `render_mcp_content`,
`copperclaw-host-delivery/src/service.rs:631,240`): decode image blocks,
write to `<session_dir>/mcp_images/<request_id>_<n>.png` (size cap 5 MB,
matching `view_image`'s refuse threshold), and replace the placeholder with
the container-visible path (`/data/mcp_images/...`) plus a hint to use
`view_image`. Runner side: nothing to change if paths ride in the text
result. GC the images with the session.

**Acceptance.** Mock MCP server returning an image block → file exists,
result text carries the path, `view_image` on it succeeds; oversize image →
placeholder with an explanatory note, no file.

### B1b. External-MCP connection cache — P2, S — scope: MCP-EXT

**Problem.** Connect-per-call (`external.rs` `connect_filtered`) adds ~1-2s
per external tool call; flagged as a future optimization in the branch.

**Change.** Host-side keyed cache `(group_id, server_name) → live connection`
with idle TTL (60s) and drop-on-error. Filters re-applied per call (config may
change). Stdio children reaped on TTL expiry. Cache is an optimization only —
any cache failure falls back to fresh connect.

**Acceptance.** Two sequential calls to the same server reuse one connection
(assert with a counting mock transport); config filter change between calls is
respected; TTL reaps the child process.

### D5 + D6 — CCLAW wave-3 batch (one agent) — scope: CCLAW

- **D5. Chat REPL upgrade** — P1, M. Replace raw `stdin.read_line`
  (`lib.rs:269-395`) with `rustyline`: history (persisted to
  `~/.local/share/copperclaw/chat_history`), line editing, multi-line paste.
  Add speaker labels (`you>` / `agent>`) and dim-styled connection banners
  (reuse D1's style layer). Transport (FIFO + log tail) unchanged.
- **D6. `cclaw groups briefing edit <group-id>`** — P1, S. `$EDITOR`
  round-trip for `<groups_dir>/<id>/COPPERCLAW.md`, mirroring
  `groups config edit` (`lib.rs:1550`). Create-if-missing with a commented
  template. Audit-log the mutation. (Closes a named vaporware item.)

### E2. Slack + Discord setup wizard steps — P1, M — scope: SETUP

**Problem.** Setup accepts slack/discord at the channel prompt then dead-ends
to docs (`steps/first_chat.rs:80`, `channel.rs`); only telegram is wired
end-to-end.

**Change.** Interactive (+ env-backed headless) steps for both, modeled on the
telegram step: prompt for bot token (+ Slack app/signing secret as its adapter
requires — read the adapter's config struct for the authoritative field list),
validate with a cheap API call (`auth.test` / `users/@me`), create
messaging-group + wiring, honor idempotency (existing state → skip). Update
`first_chat` to stop dead-ending for these two.

**Acceptance.** Headless run with `COPPERCLAW_SETUP_*` vars for each channel
creates working wiring rows; re-run is a no-op; wizard with an invalid token
fails the step with a clear message, not late.

---

## Wave 4 — depth and floor-raising

### E6. Fixture capture tooling — P2, L — scope: HOST-MISC (fixture module) — **do before C5**

**Problem.** Only 7/21 channels have replay fixtures; capture tooling
(`COPPERCLAW_FIXTURE_CAPTURE`, `copperclaw fixture redact`) is design-only
(`docs/plans/vaporware-followups.md:16-35`). C5's adapter edits would land
unguarded without this.

**Change.** Implement per the vaporware doc: capture pipeline gated by
`COPPERCLAW_FIXTURE_CAPTURE=<dir>` recording the pipeline byte-stream, and
`copperclaw fixture redact <dir>` applying redaction passes (bearer tokens,
signing secrets, personal text — one unit test per rule). Then capture
fixtures for the C5 target channels (signal, whatsapp-cloud, mattermost) —
mock-transport captures are acceptable where live credentials aren't
available.

### C5. Raise the mid-tier adapter floor — P1, M each — scopes: ADAPTER-signal, ADAPTER-whatsapp-cloud, ADAPTER-mattermost (three parallel cards)

**Problem.** Exactly {telegram, slack, discord, teams, gchat, webex, matrix}
have the rich progress stack; everywhere else each breadcrumb is a new chat
line (spam) and formatting errors hard-fail (`plain_text_fallback` exists in
only 4 adapters).

**Change (per adapter, one agent each — adapters are disjoint crates).**
Implement where the platform API allows: `deliver_breadcrumb` with
edit-in-place (`existing_message_id`), `set_typing` (signal has it already —
verify; mattermost via `user_typing` websocket or skip with a comment),
`plain_text_fallback` for formatting-bad-request recovery, `deliver_todo_list`
edit-in-place. Platform can't do it → keep the trait default and note why in
the adapter's module doc (tenet: honest docs over stubs).

**Acceptance (each).** Replay fixture (from E6) shows breadcrumb edits, not
line spam; formatting-failure test falls back to plain text.

### C6. Shared markdown renderer — P2, M — scope: CHANNELS-CORE (sequenced after C5 merges)

**Problem.** Each rich adapter converts markdown to its dialect independently
(Telegram MarkdownV2, Slack mrkdwn, Discord); no shared renderer in core;
drift-prone, and bare adapters get raw markdown.

**Change.** Core AST layer (pulldown-cmark, pinned) with per-dialect emitters:
`to_plain`, `to_telegram_md2`, `to_slack_mrkdwn`, `to_discord`. Migrate
telegram first (it has the most escaping bugs historically — check its
formatting tests), then slack/discord in follow-up PRs. Bare adapters get
`to_plain` (strips syntax cleanly) instead of raw markdown.

**Acceptance.** Existing per-adapter formatting tests pass against the shared
emitters (port them into core); byte-diff on fixtures for the migrated
adapters reviewed intentionally (escaping fixes are allowed, silent semantics
changes are not).

### A6. Subagent fan-out + write-capable delegation — P1, L — scope: RUNNER (+ MCP-TOOLS subagent files)

**Problem.** `explore` is bounded, read-only, non-recursive
(`subagent.rs:1-345`); `create_agent` is a heavyweight sibling container.
Nothing in between; no parallel fan-out.

**Change.**
1. Allow N concurrent `explore` calls in one batch (composes with A1;
   subagent token budgets deduct from the parent's `max_task_tokens`
   atomically).
2. New `delegate` tool: an in-process subagent with the write tool family
   enabled, **scoped to a git worktree** it creates (the prompt already
   documents worktree discipline for `create_agent`); returns a summary +
   worktree path; parent merges. Depth stays 1 (no recursion — keep the
   `nested` refusal). Policy: `delegate` sits in Coding profile and inherits
   the parent turn's taint.

**Acceptance.** Fan-out of 3 explores runs concurrently within budget;
delegate produces a commit in a worktree without touching the parent tree;
budget exhaustion mid-fanout aborts cleanly and reports per-child usage.

### A7. Self-verification pass — P1, M — scope: RUNNER (after A6 merges)

**Problem.** No verify/critic step; the `todo_update` evidence contract
demands verification the agent has no first-class support for.

**Change.** Opt-in per group (`container_configs.verify_mode` nullable —
migration): for Coding-profile tasks, before the final reply the runner runs a
verify turn — re-read `git diff`, run the group's configured check command
(new `check_command` config field) via `shell`, and a Low-effort provider
critique of the diff vs. the task. Failures feed back as one additional fix
turn (bounded: verify runs at most twice). Off by default.

**Acceptance.** Mock-provider e2e: verify catches a scripted failing check and
triggers exactly one fix turn; `verify_mode` unset → zero behavior change.

### E3. Dollar cost accounting — P2, M — scope: HOST-MISC (budgets) + CCLAW display (two sequenced PRs)

**Problem.** `group_budgets.daily_cost_cap` reserved-unused since migration
006; only token budgets exist; `cclaw usage` shows tokens only.

**Change.** Static per-model price table (TOML shipped in-tree, operator
override via config dir), cost computed from `agent_turns` at read time (no
schema change to turns), dollar column in `cclaw usage`, and enforcement of
`daily_cost_cap` at the same request-time gate as the token cap
(`container_manager/budgets.rs`). Unknown model → cost null, never guessed
(tenet 10). Metric: `copperclaw_turn_cost_dollars` counter (metrics-hotspot
rider rules apply).

### D7 + D8 — CCLAW/host polish batch (one agent) — scopes: CCLAW, HOST-MISC, SETUP

- **D7. Guided restore + config export** — P2, M. `cclaw db restore`
  orchestrates stop → restore → start (today it refuses,
  `commands.rs:339-347`); `cclaw export`/`import` for groups + wirings +
  messaging-groups + budgets as TOML (also patches the cutover gap).
- **D8a.** Fill sparse `--help` on messaging-groups / wirings / users / roles /
  members / destinations (tenet 4).
- **D8b.** `copperclaw logs --since <dur> --level <lvl> --grep <pat>`
  (`daemon.rs:482`).
- **D8c.** Setup: wire the already-imported-but-unused `indicatif` spinners
  for image pull/build; run `cclaw doctor` at the end of the verify step and
  print its summary.
- **D8d.** Resolve the `copperclaw status` / `cclaw status` verb collision:
  rename cclaw's to `cclaw overview` with a deprecation alias.

### E4. `unknown_sender_policy`: implement or remove — P2, S — scope: HOST-MISC (approvals)

**Problem.** The field is stored but advisory-only; the sender gate holds all
non-`users` senders unconditionally (CHANGELOG's own caveat). Advisory config
that does nothing violates tenet 10.

**Change.** Branch the `SenderScopeGate` on it (`allow`/`hold`/`deny` — read
the enum from the schema) with `hold` as the default matching today's
behavior; or, if the decision is to drop it, remove the column via a new
migration and scrub the docs. Decide in the PR; default recommendation:
implement `deny` and `hold`, treat `allow` as invalid for now (secure by
default).

### E5. Observability riders — P2, S — scope: METRICS (single card, run last in wave)

`copperclaw_turn_duration_seconds` histogram, `copperclaw_tainted_turns_total`
counter, plus (post-E3) the cost counter. One agent, metrics crate + one-line
call sites. Add a `docs/observability.md` with a starter Grafana dashboard
JSON.

---

## Deferred / rejected (so agents don't re-litigate)

- **Token-level streaming over the SQLite transport** — rejected; A3's
  edit-in-place approximation captures most of the perceived value without an
  IPC redesign.
- **Interactive browser (click/type/scroll)** — Phase 5b, explicit PLAN.md
  non-goal for now.
- **LSP bridge / pre-post-edit hooks** — stays deferred per
  `vaporware-followups.md`; B4 (tree-sitter/ctags `symbols` tool) is the
  cheap alternative if demand appears. Not scheduled in a wave.
- **B5 (web_fetch pagination), B6 (multi-provider search fan-out), C7 (voice
  STT), C8 (reaction approvals), A8 (structured output), A9 (tokenizer-exact
  budgeting)** — real but demand-pull; take them as follow-ups only after
  their wave's P0/P1 cards are merged.
- **Mutating git tools** — actively not pursued (existing decision).

## Wave summary

| Wave | Cards | Parallelism |
|---|---|---|
| 0 | merge `feat/runner-external-mcp` | — |
| 1 | A1, C1, C3, D1, D2 (+ metrics rider) | 5 agents |
| 2 | C4 → A2 → A3; D3+D4+E1 batch | 2 lanes |
| 3 | B2, A4, A5, B1a, B1b, D5+D6 batch, E2 | up to 6 agents (A4/A5 sequenced on RUNNER? A5 is MCP-TOOLS — parallel OK; A4 alone on RUNNER) |
| 4 | E6 → C5×3 → C6; A6 → A7; E3, D7+D8 batch, E4, E5 rider | 4+ lanes |

Every card: branch off main, PR per card, changelog line last, full check
suite green before review.
