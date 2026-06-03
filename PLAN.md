# Copperclaw — Implementation Plan

> **Audience.** This plan is written so that multiple agent teams can work
> in parallel on independent crates without stepping on each other. Each
> "Team" section is self-contained: it has scope, deliverables, the crate
> APIs it owns, the contracts other teams depend on it for, and acceptance
> criteria.

---

## Project tenets — "OpenBSD of claw agents"

Copperclaw's defining posture is the OpenBSD playbook applied to agent
runtimes. Every operational decision in M13 onwards is judged against
these invariants. They are not aspirations; a PR that violates one is
not landing.

1. **No stubs in tree.** A half-implemented adapter is worse than no
   adapter — it lies to the registry and fails at message time. If a
   crate can't be finished, it gets deleted (whatsapp native crate,
   M12). The "no stubs" rule is what made the workspace ~7600 LOC
   smaller and made the chat loop possible to debug.
2. **Secure-by-default, public-by-deliberate-act.** Every webhook
   binds `127.0.0.1` unless the operator explicitly chooses
   otherwise. The cli channel pre-approves only the literal local
   sender. The `.env` is `0o600`. The host writes tracing to
   stderr so log capture never contaminates the data path. Add
   capabilities, never default to them.
3. **One process, single binary.** `copperclaw` is the host;
   `copperclaw-runner` runs inside containers; `cclaw` is the admin
   client. No daemons-spawning-daemons. No optional foreground /
   background mode flags. Setup writes one `.env` and one
   service-unit and that's the deploy surface.
4. **Documentation is a deliverable.** Every crate's `lib.rs`
   doc-string explains what the crate does, what its inputs are,
   and what the error paths mean. Every command in `cclaw` has
   `--help` text written for an operator, not a developer. The
   bar is OpenSSH's man pages, not "auto-generated from docstrings".
5. **Conservative defaults.** Idle-stop timeout: minutes, not
   hours. Retry cap: 3, not infinity. Token / cost budget: opt-in
   capped, not unlimited. Rate limit: present even when low.
   Surprises always cost money.
6. **Audit everything that mutates.** Every cclaw socket call
   that writes — `groups.create`, `wirings.create`, etc. — lands
   in an `audit_log` table with caller, command, args, and
   result. Read paths excluded. The host can be forensically
   reconstructed from the log + the central DB snapshot.
7. **Reproducible builds.** Image fingerprints include the
   runner binary bytes. Same source → same sha-tag → same
   deployable artifact. No "latest" tags, no float, no
   yesterday's image silently running today.
8. **Pinned upstreams.** Workspace deps are version-pinned in
   `Cargo.toml`. `Cargo.lock` is checked in. CI runs
   `cargo deny` against an explicit allow-list of licenses + a
   block-list of yanked / unmaintained crates.
9. **Signed releases.** Every release tag is GPG-signed. The
   `release-checklist.md` includes the signing step; CI verifies
   the tag's signature before publishing artifacts.
10. **Errors over silent fallback.** A misconfigured channel
    fails loudly at boot — it does not silently disable itself.
    A bad webhook signature returns 401 — it does not silently
    accept. An unknown sender goes to a pending queue — it is
    not silently routed. Operators learn fast or not at all.

The 0.1.0 release ships when M13 is checked through and the
remaining items in M11 (replay-fixture harness, 0.1.0 tag) are
shipped. Until then the README's "candidate" status is the
honest one.

---

## Progress

Tick boxes as work completes. Each tick should reference the commit or
file paths that landed the change.

**Workspace snapshot** (single source of truth — update only here per slice):

| Metric | Value |
| - | - |
| `cargo test --workspace` | 4634 passing / 0 failing / 4 ignored |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| Channel crates | 21 |
| In-tree tools the agent can call | 20 (15 messaging/scheduling/self-mod + 4 computer-use + 1 multi-provider web search) |
| Skill docs | 22 |
| Latest milestone | Replay-fixture harness — M11 acceptance gate (in-process harness + first cli/text-reply fixture) |

Older M-section "totals" lines are historical snapshots and will drift —
trust the table above.

### M0 — Workspace skeleton + T1 types (gate)
- [x] Create `/home/phil/dev/copperclaw/` and `git init`
- [x] Write workspace `Cargo.toml`, `rust-toolchain.toml`, `.gitignore`
- [x] Scaffold all crate directories
- [x] Write every crate's `Cargo.toml`
- [x] Copy `LICENSE`
- [x] Write `copperclaw-types` lib — modules `id`, `channel`, `message`, `routing`, `session`, `provider`, `approval`, `schedule`
- [x] `cargo build -p copperclaw-types` passes
- [x] Serde round-trip tests (15 unit tests, all passing)
- [x] `cargo build --workspace` passes (all scaffolds compile)

### M1 — T2 copperclaw-db (gate for everything else)

Infrastructure (done):
- [x] `migrations/001_initial.sql` — consolidated central schema
- [x] `migrations/002_session_inbound.sql`
- [x] `migrations/003_session_outbound.sql`
- [x] Migration runner (`migrate.rs`) with `MigrationSet::{Central, SessionInbound, SessionOutbound}`
- [x] `CentralDb` pool wrapper (WAL + foreign keys on)
- [x] `SessionPaths` + `open_inbound` / `open_outbound` / `open_inbound_ro_no_mmap`
- [x] Attachment safety helpers (`safe_attachment_name`, `extract_to_inbox`, `read_from_outbox`)
- [x] Cross-mount visibility integration test (3 tests in `tests/cross_mount_visibility.rs`)
- [x] `cargo test -p copperclaw-db` passes (57 tests)

Exemplar table modules (done — establishes the pattern for other teams):
- [x] `tables/agent_groups.rs` — full CRUD with 9 tests
- [x] `tables/sessions.rs` — find_for_agent, mark_running/idle/stopped, list_active/list_running, 8 tests
- [x] `tables/messages_in.rs` — host-side writes (even seq), get_pending, count_due, mark_completed/failed, 8 tests
- [x] `tables/messages_out.rs` — container-side writes (odd seq), list_due, get, 5 tests

Central DB table modules (delivered by parallel teams, each following the `agent_groups` pattern):
- [x] `tables/messaging_groups.rs` — 11 tests (list, get, get_by_platform, get_with_agent_count, upsert, delete, mark_denied)
- [x] `tables/messaging_group_agents.rs` — 15 tests (list_for_mg, list_for_ag, get, upsert, delete)
- [x] `tables/users.rs` — 9 tests (list, get, get_by_identity, upsert; deterministic `UserId` via UUIDv5)
- [x] `tables/user_roles.rs` — 14 tests (list_for_user, list_for_scope, grant, revoke; `Role` enum)
- [x] `tables/agent_group_members.rs` — 10 tests (list, add, remove)
- [x] `tables/user_dms.rs` — 9 tests (get, upsert, list)
- [x] `tables/pending_questions.rs` — 6 tests (insert, get, delete)
- [x] `tables/pending_approvals.rs` — 12 tests (list, get, upsert, update_status, delete; `ApprovalStatus`)
- [x] `tables/pending_sender_approvals.rs` — 8 tests (list, get, upsert, delete)
- [x] `tables/pending_channel_approvals.rs` — 7 tests (list, get, upsert, delete)
- [x] `tables/agent_destinations.rs` — 11 tests (list, get, add, remove, lookup_by_target)
- [x] `tables/unregistered_senders.rs` — 10 tests (upsert, list, get)
- [x] `tables/dropped_messages.rs` — 7 tests (insert, list)
- [x] `tables/container_configs.rs` — 28 tests (get, upsert, get/set_skills, get/set_mcp_servers, add/remove_package_apt, add/remove_package_npm; `CliScope`, `SkillsSelector`)

Per-session DB modules:
- [x] `tables/delivered.rs` — 9 tests (insert, get_delivered_ids, list)
- [x] `tables/destinations.rs` — 10 tests (replace_all in transaction, list, get)
- [x] `tables/session_routing.rs` — 5 tests (read, write — single-row table)
- [x] `tables/processing_ack.rs` — 14 tests (insert, update_status, get_all, get, delete; `ProcessingStatus`)
- [x] `tables/session_state.rs` — 7 tests (get, set, delete, list)
- [x] `tables/container_state.rs` — 9 tests (get, set, clear_tool — single-row table)

M1 totals (this slice): 211 new table-module tests + 57 infrastructure tests = **272 passing tests in `copperclaw-db`**.
Workspace totals: **290 passing tests** (272 db + 3 db integration + 15 types). Clippy clean on `--all-targets`.

### M2 — T4 container-rt, T8 skills, T11 onecli (parallel after M1)
- [x] T4 `ContainerRuntime` trait + `DockerRuntime` (bollard) + `AppleContainerRuntime` (CLI shell-out) — 70 tests, 2 ignored (daemon-bound)
- [x] T4 image build (`apt-packages`, `npm-packages` per `container_configs`) — sha256-fingerprinted tags; inline USTAR tar writer for the build context
- [x] T8 skill discovery (frontmatter parse, per-group override) — 59 tests; name mismatch is an error, missing named-skill is a skip
- [x] T8 skill materializer (symlinks into container mount) — idempotent; rejects path-escape; refuses to clobber non-symlink entries
- [x] T11 OneCLI HTTP client (`ensure_agent`, `apply_container_config`, approvals) — 84 tests; full wiremock coverage for 401/404/409/429/5xx + `Retry-After` parsing

### M3 — T5 runner/providers/mcp, T6 channels core+cli, T7 modules (parallel)
- [x] T5 provider trait + Anthropic HTTP streaming + tool-use loop — `AgentProvider`/`AgentQuery`; `AnthropicProvider` over SSE; 49 tests (35 unit + 14 wiremock)
- [x] T5 context compaction (configurable strategy) — window 200k, margin 8k, `chars/4` token estimate, archives pre-compaction transcript to `outbox/_compactions/<ts>.md`
- [x] T5 MCP server with all 15 tools (see § 7) — `ToolContext` trait + `OutboundToolEffect` sum type; 95 tests
- [x] T5 MCP client (stdio for external servers); HTTP-SSE is wired but gated behind rmcp's `transport-sse-client` feature (returns `McpError::Protocol` stub until enabled)
- [x] T5 container poll loop + formatter + destinations — 84 tests; system-kind JSON envelopes documented for host integration; state resume via `session_state`
- [x] T6 `core` crate: trait + registry — 41 tests; reusable `MockAdapter`/`MockFactory` in a public `testing` module
- [x] T6 `cli` crate: stdin/stdout channel — 26 tests; pluggable reader/writer for tests, stdio bound by default
- [x] T7 modules: typing, mount-security, permissions, approvals, interactive, scheduling, agent-to-agent, self-mod — 152 tests (151 unit + 1 integration); `ModuleContext` trait defines the host hook surface ahead of M4

M2+M3 totals (this slice): 460 new tests across 9 crates; full workspace **~950 passing tests**. Clippy clean on `cargo clippy --workspace --all-targets -- -D warnings`.

### M4 — T3 host (integrates M2+M3)
- [x] Host `main` boot sequence (signals, migrations, runtime check, orphan cleanup) — `copperclaw {run,migrate,version}`; graceful SIGINT/SIGTERM with 30s shutdown deadline
- [x] Router (hook chain, fan-out, session resolution) — 58 tests; session_mode policy: Shared / PerThread / AgentShared all implemented; 500ms debouncer + in-flight re-entry guard
- [x] Delivery (active 1s, sweep 60s, re-entry guard, retries) — 71 tests; exponential backoff `5_000 * 2^(tries-1)` capped at `ABSOLUTE_CEILING_MS`; 3-attempt cap then marks failed
- [x] Sweep (stuck detection, recurrence fanout, processing-ack reset) — 61 tests; injectable `Clock` for deterministic time; series_id correlation; emits `SweepReport` for the host's container manager
- [x] `copperclaw run` boots successfully and idles cleanly — verified via `boot::tests::run_host_boots_with_noop_runtime_and_idles`; live runtime not exercised in this slice (no daemon available)

### M5 — T9 cclaw
- [x] Unix-socket server inside host — newline-delimited JSON at `data/cclaw.sock` mode `0o600`; cancellation token shutdown
- [x] `cclaw` binary client — 87 tests; lib + bin; pluggable `CallTransport`
- [x] All resource subcommands (see § A2) — 41 distinct command strings exported as `copperclaw_cclaw::ALL_COMMANDS`; every handler maps to `copperclaw-db` table fns
- [x] CLI-scope enforcement for agent callers — `HOST_ONLY_COMMANDS` set; agents calling mutation cmds get `permission_denied`

M4+M5 totals (this slice): 446 new tests across 5 crates (router 58 + delivery 71 + sweep 61 + cclaw 87 + host 169). Full workspace **1396 passing tests, 3 ignored, 0 failures**. Clippy clean on `cargo clippy --workspace --all-targets -- -D warnings`.

M6+M7+M9+M10 totals (this slice): 575 new tests across 6 deliverables (setup 175 + telegram 120 + slack 101 + discord 134 + providers +45 = 94 total + 17 skills authored). Full workspace **1971 passing tests, 3 ignored, 0 failures**. Clippy clean.

### M6 — T10 setup
- [x] Interactive setup (`dialoguer`) — 13 step modules; `Prompt` trait with `Interactive` / `EnvBacked` / `Scripted` impls
- [x] systemd unit / launchd plist generators — `units.rs` snapshot-tested
- [x] Headless mode (env-var driven) — `COPPERCLAW_SETUP_*` env-var surface
- [x] Optional data-directory migrator — `--migrate-from <path>` copies `copperclaw.db` and re-runs migrations
- 175 tests in `copperclaw-setup`. Stubs: image-build step requires live runtime (skipped without); CLI-agent step only checks PATH.

### M7 — First three real channels (T6 parallel)
- [x] T6 telegram (long-poll + webhook) — 162 tests; both ingress modes; multipart `sendDocument`; inbound `document` / `photo` (largest variant) / `audio` / `video` / `voice` / `video_note` / `sticker` downloaded via `getFile` + the file endpoint and written under `data_dir/inbox/<msg_id>/<filename>` (path + metadata in `content.attachment`); `attachment_download` (default `true`) toggles back to the metadata-only `MessageKind::System` fallback; `max_attachment_bytes` (default 20 MB — Bot API hard cap) demotes oversized files / `getFile` failures to `System` with `reason` + captured error
- [x] T6 slack (events API + Web API) — 101 tests; HMAC-SHA256 signature verification (hand-rolled); files v2 upload flow; 256-entry event_id LRU
- [x] T6 discord (slim gateway + REST) — 134 tests; tokio-tungstenite gateway client; pure codec/lifecycle parsers fully unit-tested; intent constant noted as `38_401` (PLAN's `33_281` was arithmetically off)

### M8 — Remaining channels (T6 parallel batch)

Batch 1 (REST/webhook):
- [x] resend (Resend.com email; send-only) — 114 tests, 2082 LOC; REST `POST /emails`, base64 attachments, no ingress (Resend has no user-reply surface)
- [x] github (issue/PR/review comments + webhook) — 167 tests, 3366 LOC; HMAC-SHA256 webhook with `X-GitHub-Delivery` LRU dedup, `platform_id = "{owner}/{repo}#{number}"`, 8-slug emoji map, `X-RateLimit-Remaining: 0` → `Rate`
- [x] linear (GraphQL + webhook) — 153 tests, 3403 LOC; HMAC-SHA256 webhook, `commentCreate`/`commentUpdate`/`reactionCreate` mutations, GraphQL `errors[]` lifted to `Auth`/`Rate`/`BadRequest`
- [x] webex (REST + webhook) — 181 tests, 4291 LOC; HMAC-SHA1 (or SHA256) webhook, body fetch via `GET /messages/{id}` because Webex omits text from webhooks, `person:` platform_id prefix routes DMs via `toPersonId`, beta `POST /reactions` 404/501 → `Unsupported`

Batch 2 (REST/webhook + long-poll):
- [x] matrix (Client-Server REST + `/sync` long-poll) — 146 tests + 1 ignored, 3719 LOC; persists `next_batch` to `data_dir/next_batch.txt`; `m.text`/`m.file`/`m.image`/`m.audio`/`m.video` events; alias resolution cached; threads via `m.relates_to`; one test ignored (`sync_loop_pushes_events_and_persists_next_batch`) — mock would saturate the 8-cap mpsc; fix is to use `up_to_n_times` on the mock or `select!` around the inbound send
- [x] teams (MS Graph REST + change-notifications webhook) — 156 tests, 3791 LOC; validation handshake (`?validationToken=…` → `text/plain` 200); `clientState` constant-time-compare; `team/{T}/channel/{C}` and `chat/{C}` platform_id shapes; 6-reaction map (`like`/`heart`/`laugh`/`surprised`/`sad`/`angry`); HTML→text fallback
- [x] gchat (Google Chat REST + HTTP push) — 134 tests, 3127 LOC; v1 simplification: `?token=<client_token>` instead of JWT verification (documented); `MESSAGE`+`CARD_CLICKED` events; emoji shortcode → numeric Unicode codepoint table (no literal emoji in source); `cardsV2` for cards
- [x] whatsapp-cloud (Meta Cloud API) — 142 tests, 3475 LOC; `hub.verify_token` handshake + `X-Hub-Signature-256` HMAC; `<phone_number_id>:<wa_id>` platform_id; `text`/`image`/`document`/`audio`/`video`/`button`/`interactive` events; `set_typing` = `mark_read` of last message; edit → `Unsupported`; reactions via `type:"reaction"`

Batch 3 (subprocess RPC + REST/poll):
- [x] signal (signal-cli daemon, stdio JSON-RPC) — 144 tests, 3466 LOC; `RpcTransport` trait abstracts the subprocess (no test spawns `signal-cli`); `user:<e164>` + `group:<base64>` platform_id; JSON-RPC error codes -1 → Auth, -3 → Rate; safe-attachment-name validation; system actions: edit / reaction / delete supported
- [x] deltachat (`deltachat-rpc-server` stdio JSON-RPC) — 165 tests; `RpcTransport` + `MockTransport` (no test spawns the server); `account/<id>/chat/<id>` platform_id; chat-type → `is_group` mapping; `edit` → `Unsupported`; outbound attachments written to `data_dir/outgoing/`; inbound attachments go through `download_full_msg` when `download_state != "Done"`, then the blob is `stat` + `open`-verified and the resolved on-disk path surfaced under `content.attachment.bytes_path` (with `size` / `mime`); `attachment_download` (default `true`), `blob_dir` (optional override for shared-blob deployments), and `max_attachment_bytes` (default 50 MiB) gate the behaviour, with oversized files / `download_full_msg` / `stat` / `open` failures demoted to `MessageKind::System` with `reason` + captured error; `add_account` called eagerly when configured `account_id = 0` and no accounts exist
- [x] emacs (emacsclient subprocess) — 117 tests, 2336 LOC; `EmacsClient` trait abstracts the spawn (real `EmacsClientCli` only invoked when intentionally testing a missing-binary case); `${BUFFER_JSON}` / `${TEXT_JSON}` template substitution; defaults `(copperclaw-pop-inbound)` + `(copperclaw-deliver ${BUFFER_JSON} ${TEXT_JSON})`; files + `edit` + `reaction` → `Unsupported`; stderr matching `can't find socket` → `Auth`
- [x] x (Twitter/X v2 DMs + v1.1 media upload + `/2/dm_events` polling) — 141 tests, 3224 LOC; `user:<id>` + `conversation:<id>` platform_id; since-id persisted to `data_dir/x_dm_since_id.txt`; 429 priority order `x-rate-limit-reset` → `Retry-After` → 60s fallback; media_category inferred from filename extension; `set_typing` no-op; `edit`/`reaction` → `Unsupported`

- [x] All 15 registered in host `build_registry` — single test asserts every in-tree factory is present (cli + 3 M7 + 4 batch 1 + 4 batch 2 + 4 batch 3 + wechat + imessage + whatsapp = 19)

- [x] whatsapp (native Baileys port — websocket + e2e crypto, skeleton) — 308 tests, 6389 LOC; ships the full transport + framing + protocol-state-machine stack above a pluggable `CryptoBackend` trait, but real e2e crypto is **deferred** behind `StubBackend` (every primitive returns `CryptoError::NotImplemented`) so `deliver` / `set_typing` / edit / reaction return `AdapterError::Unsupported` until a `libsignal-protocol`-backed `CryptoBackend` is dropped in; `[flags][u24 length][payload]` frame codec; WhatsApp binary-XML codec with a small token table (`iq` ping/pong round-trips); Noise XX handshake state machine (`Initial -> SentE -> ReceivedSE -> Done`/`Failed`); atomic-write JSON keystore with corruption rotation; `WsTransport` trait + `tokio-tungstenite` real impl + `MockTransport`; `user:<wa_id>` + `group:<jid>` platform_id; full lifecycle (connect / heartbeat / cancellation / reconnect-backoff) tested against the mock
  - whatsapp `CryptoBackend` now has a real `DalekBackend` default (X25519 / HKDF-SHA256 / AES-256-GCM / Ed25519 via `x25519-dalek` + `hkdf` + `aes-gcm` + `ed25519-dalek`); 80 new tests exercising RFC 7748 § 5.2 X25519, RFC 5869 A.1/A.2/A.3 HKDF, and RFC 8032 § 7.1 Ed25519 vectors; outbound `deliver` is still gated on the Signal Protocol session-state machinery (X3DH + Double Ratchet + sender keys + envelope construction), which sits above the primitives and remains unwritten — the adapter surfaces a distinct "pipeline not yet implemented above the CryptoBackend" message in that branch
- [x] imessage (local macOS Messages.app — osascript + sqlite chat.db) — 193 tests, 3565 LOC; `IMessageBridge` trait abstracts both `osascript` and `sqlite3` (no test invokes either binary); Cocoa-epoch nanos auto-detected vs. seconds and converted to `DateTime<Utc>`; hand-rolled `applescript_escape` rejects null + C0 controls (except `\n`/`\r`/`\t`) and escapes `"` / `\`; `handle:<email-or-phone>` + `chat:<chat-guid>` platform_id shapes; high-water `ROWID` persisted to `data_dir/imessage_since_rowid.txt`; `edit` / `reaction` system actions → `Unsupported` (AppleScript tapback surface unreliable); `set_typing` no-op; chat.db unreachable (Full Disk Access) → `Auth`
- [x] wechat (WeChat Work / Work Weixin — REST + webhook) — 196 tests, 4390 LOC; hand-rolled `WXBizMsgCrypt` (AES-256-CBC + PKCS7) + SHA1-over-sorted-concat signature (not HMAC); `user:` / `party:` / `tag:` platform_id prefixes for `touser` / `toparty` / `totag`; cached `gettoken` via `TokenStore` with one-shot retry on `errcode 42001`; `text` / `image` / `voice` / `video` / `file` / `event` inbound types; `edit` and `reaction` system actions → `Unsupported`; corp_id checked inside decrypted payload; consumer WeChat deliberately out of scope (no documented API)

M8 totals (3 slices + tail): 2433 new tests across 15 channels (batch 1: 114 + 167 + 153 + 181 = 615; batch 2: 146 + 156 + 134 + 142 = 578; batch 3: 144 + 141 + 117 + 141 = 543; tail: wechat 196 + imessage 193 + whatsapp 308 = 697). Workspace **4406 passing tests, 4 ignored, 0 failures**. Clippy clean on `cargo clippy --workspace --all-targets -- -D warnings`.

### M9 — Provider variants (T5 parallel)
- [x] codex provider (subprocess + JSON protocol) — thin wrapper over shared `SubprocessProvider`
- [x] opencode provider (same shape, different binary) — `PushPolicy::Accept` by default
- [x] ollama provider (Anthropic-compatible base URL) — built on `AnthropicProvider::with_base_url`, default model `llama3.1:8b`; no changes to `anthropic.rs`
- 94 tests in `copperclaw-providers` (was 49). JSON-Lines bridge protocol documented for subprocess providers.

### M10 — Skill content
- [x] Author copperclaw-native skill content for each capability under `skills/` — 17 skills authored (send-message, send-file, edit-message, add-reaction, send-card, ask-user-question, schedule-task, install-packages, add-mcp-server, create-agent, messaging-context, destinations, approvals, typing-indicator, cli-channel, discovering-tools, error-handling)
- [x] Document the "adding a channel" workflow — `docs/adding-a-channel.md` (500 lines): traits, crate layout, inbound/outbound mapping, error mapping, testing strategy, host wiring, PR checklist

### M11 — Differential testing + release
- [x] Cutover docs — `docs/cutover.md` (preflight → quiesce → snapshot → migrate → verify → switch ingress → first-hour watch → rollback)
- [x] Replay-fixture suite design — `docs/replay-fixtures.md` (fixture layout, harness internals, capture/redact workflow)
- [x] Release-checklist + CI — `docs/release-checklist.md`; `.github/workflows/ci.yml` runs fmt + clippy + test on Linux/macOS with an 85% coverage gate; `CHANGELOG.md` seeded with the Keep-a-Changelog `[Unreleased]` section
- [x] Replay-fixture harness implementation + first captured fixture (the M11 acceptance gate). In-process harness lives under `crates/copperclaw-host/tests/replay/` (`fixture.rs` + `diff.rs` + `harness.rs`); first fixture lives under `fixtures/cli/text-reply/`. Drives `Router::route` → in-process `run_loop` (with a wiremock-served Anthropic SSE stub) → `DeliveryService::process_session_once` → `MockAdapter::deliveries()`, then diffs the four captured JSONL streams (inbound-events / messages-in / messages-out / delivered) against `expected/*.jsonl` with manifest-driven regex substitutions for non-deterministic UUIDs and timestamps.
- [ ] Release 0.1.0 (cut the tag, bump `workspace.package.version`, publish release notes from CHANGELOG)

### M11 — Post-M10 hardening (this slice)

These items closed open stubs and added coverage the earlier milestones had explicitly deferred. Each ships as part of the 0.1.0 candidate.

- [x] **Matrix `/sync` cancellation fix** — wrapped the inbound `mpsc::send` in `select!` against the shutdown token so the loop exits cleanly even when the channel is saturated. Re-enabled `sync_loop_pushes_events_and_persists_next_batch`; matrix now ships 147 passing tests with no `#[ignore]` markers.
- [x] **MCP HTTP-SSE transport** — enabled rmcp's `transport-sse-client` + `reqwest` features and replaced the `connect_http_sse` placeholder. Static headers (auth bearer, etc.) thread through via a pre-loaded `reqwest::Client`. 15 client tests including malformed-header rejection, invalid-URL classification, and a wiremock check that wrong `content-type` lands as `Transport`.
- [x] **`groups.restart` wired** — replaced the `{"queued": true}` stub with a `sessions::list_for_agent_group` (new) + `mark_container_stopped` loop. Returns `{agent_group_id, sessions_marked_stopped, sessions}`; sessions already in `stopped` state are counted as no-change. 5 new handler tests + 2 new DB tests cover the happy path, the no-sessions case, the unknown-group not-found, and cross-group isolation.
- [x] **WhatsApp `DalekBackend`** — replaced `StubBackend` as the default `CryptoBackend` with a real Curve25519 + Ed25519 + HKDF-SHA256 + AES-256-GCM impl on `x25519-dalek` + `ed25519-dalek` + `hkdf` + `aes-gcm`. 80 new tests including RFC 7748 § 5.2, RFC 5869 A.1/A.2/A.3, and RFC 8032 § 7.1 vectors. Outbound `deliver` is still gated on the Signal Protocol session state above the primitives; the adapter surfaces a distinct error string for that branch so the gap is testable.
- [x] **Telegram + Deltachat attachment downloads** — flipped both channels from `MessageKind::System` placeholders to real downloads. Telegram adds `get_file` + binary stream + `attachment_download` (default true) / `max_attachment_bytes` (default 20 MB) config knobs (+42 tests → 162). Deltachat calls `download_full_msg` when `download_state != "Done"`, then `stat` + `open`-verifies the blob (with optional `blob_dir` override) and surfaces the resolved path under `content.attachment.bytes_path` (+24 tests → 165). Oversized / auth-failure / network-failure paths demote to `System` with a captured reason.

**Hardening totals (this slice):** ~165 new tests across 6 deliverables. Workspace **4564 passing tests, 4 ignored, 0 failures**. Clippy clean on `cargo clippy --workspace --all-targets -- -D warnings`.

### M12 — Local-chat slice (this slice; closes the round-trip)

The end-to-end chat path was never wired in M0–M11: every layer
shipped tested in isolation but nothing connected `inbound.db` to a
running container that called the provider. M12 closes that gap and
the cosmetic follow-ups it surfaced.

- [x] **Native whatsapp crate removed.** The previous `StubBackend`
  → `DalekBackend` migration left an adapter whose `deliver` still
  returned `Unsupported` until the Signal-Protocol session layer
  above the primitives was built. Per the project invariant of
  "no stubs in tree", the whole crate (~7.6k LOC, ~384 tests) was
  removed. `whatsapp-cloud` (Meta Cloud Business API) remains as
  the supported WhatsApp path.
- [x] **OpenRouter / Anthropic-compatible providers.** Added
  `api_base_url` to `RunnerConfigFile`/`RunnerConfig`,
  `ANTHROPIC_BASE_URL` env-var passthrough in
  `AnthropicProvider::with_base_url`, and a trailing-`/v1` strip
  so users can paste `https://openrouter.ai/api/v1` verbatim.
- [x] **Setup writes a complete `.env`.** Auth step now persists
  `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`,
  `COPPERCLAW_DATA_DIR`, `CCLAW_SOCKET`, and the
  `COPPERCLAW_DEFAULT_IMAGE_TAG` from the image build step. Host
  auto-loads `.env` from the platform install root before falling
  back to CWD-dotenv, so `copperclaw run` works from any cwd.
- [x] **Container manager** — new `copperclaw_host::container_manager`
  module. Polls `sessions` every 1s; for any row with
  `container_status='stopped'` and `messages_in.count_due > 0`,
  writes a `runner.json` (mirrored `RunnerConfigFile` schema) into
  the session dir and calls `runtime.spawn` with the right env /
  bind / labels / entrypoint. 6 unit tests + a `NoopRuntime`
  `spawn_calls()` introspection helper.
- [x] **Image bakes the runner binary.** Setup's image step finds
  `copperclaw-runner` next to its own exe (or via
  `COPPERCLAW_RUNNER_BIN` / `PATH`) and copies it into the image at
  `/usr/local/bin/copperclaw-runner`. Image fingerprint includes
  the binary bytes, so re-cargo-build → re-image-build is
  automatic.
- [x] **Router seeds `session_routing`.** On session create the
  router now writes `(channel_type, platform_id, thread_id)` from
  the inbound event into the per-session `session_routing` table.
  Without this the delivery loop marked every outbound `NoRoute`
  because the runner emits text with no explicit destination.
- [x] **`ApprovalsModule` pre-seeded with `cli:local`.** The cli
  channel's only sender is the operator running `copperclaw run`,
  not a remote identity that warrants approval. Boot now
  constructs `ApprovalsModule::with_initial_approved` with the
  `cli:local` identity baked in; everything else still flows
  through the unregistered-sender gate.
- [x] **Stale `container_status=running` reset on boot.** After
  `cleanup_orphans` removes leftover containers from a prior run,
  the host now walks `sessions::list_running` and resets each to
  `stopped` so the manager respawns them. Without this, sessions
  that were running when the host died sat un-handled forever.
- [x] **Runner opens `inbound.db` RW.** The poll loop calls
  `messages_in::mark_completed` (an UPDATE) after every turn, but
  the runner was opening inbound via the RO `open_inbound_ro_no_mmap`
  helper, so the update silently no-op'd and the same message was
  reprocessed every poll. Added a sibling `open_inbound_rw_no_mmap`
  (same no-mmap WAL avoidance, RW) and switched the runner's
  `main.rs` to use it.

**Verified live** against OpenRouter from this host:

    > What's the capital of France? One word only.
    agent> Paris

    > Reply with just a haiku about containers.
    agent> Boxes hold the world,
           Isolated, yet deployed—
           Code sails everywhere.

Each prompt produced exactly one reply; the inbound rows were
marked `completed`; the same session container served every turn.

**Slice totals:** 4365 passing, 0 failing. Clippy clean. The drop
from 4564 → 4365 is the whatsapp crate removal; the rest of the
workspace gained ~185 tests across the manager + provider + setup
changes.

### M13 — Operational hardening

Most of M13 shipped over an autonomous slice. The remaining
items moved to the "Top 10 next" list at the end of M14.
Container lifecycle, observability (audit log + `cclaw health`),
cost/safety (token accounting + per-group budgets), sender
approval (persistent users-table-backed), and setup polish
(OpenRouter shortcut, `cclaw chat` REPL) are all live.

#### Container lifecycle

- [x] **Idle-stop**. ContainerManager now reconciles four states
  (Stopped/Idle/Running with a fifth WakeFromIdle transition);
  Running + last_active > idle_timeout_secs (default 300s) →
  runtime.stop + container_status=idle. Idle + pending inbound →
  Stopped → next tick respawns.
- [x] **Crash-restart**. Heartbeat-stale detection in the manager
  itself (DEFAULT_HEARTBEAT_STALE_SECS=60). Stale → runtime.stop
  best-effort + mark Stopped for respawn. Sweep's report becomes
  observational; the manager owns the action path so the loop is
  one tick, not "sweep → manager → next tick".
- [x] **Sweep ↔ manager wiring**. Resolved by the above —
  manager reads heartbeat mtime directly, no IPC needed.
- [ ] **Per-group resource caps**. `ContainerSpec` doesn't carry
  cpu / memory / pids / network limits. Add them, plumb through
  `container_configs.resource_limits` JSON, and apply at spawn.
- [ ] **Image rebuild on `container_config` change**. When a user
  updates `container_configs.packages_apt` or
  `container_configs.skills`, the manager should rebuild that
  group's image before the next spawn rather than reusing the
  default sha-tagged image.

#### Observability

- [x] **`cclaw health`**. New composite command. Sequential
  round-trips to `sessions.list` (all + active filter) and
  `audit.list`; reports session count, breakdown by
  container_status (running/idle/stopped), and last 5 audit
  entries. `--json` flag for a structured payload a future
  `/healthz` HTTP endpoint can reuse verbatim.
- [ ] **Prometheus metrics endpoint**. Counters for
  `messages_inbound_total`, `messages_outbound_total`,
  `containers_spawned_total`, `containers_crashed_total`,
  `delivery_failed_total`; histograms for
  `llm_call_seconds`, `llm_tokens_input`, `llm_tokens_output`,
  `container_spawn_seconds`. Optional bind, default off.
- [ ] **Log rotation**. `chat.log` and `host.log` grow unbounded
  when copperclaw is run as a long-lived daemon; setup's systemd /
  launchd units don't currently wire up logrotate or
  size-capping. Either document the recommended logrotate config
  or use `tracing-appender::rolling`.
- [x] **Audit log of cclaw socket actions**. Migration
  `004_audit_log.sql`, `tables::audit_log` module, dispatch
  hook in `copperclaw-host::socket` writes one row per mutation
  with caller / command / args (truncated at 4KiB) / result /
  error_code / error_message / latency_ms. Read paths
  excluded. New `cclaw audit list --since <window> --limit N`
  subcommand renders the table.

#### Cost and safety

- [x] **Token accounting**. New `ProviderEvent::Usage` variant
  emitted by AnthropicProvider from `message_start.usage` and
  `message_delta.usage`. Runner accumulates per-turn (taking
  the latest cumulative value rather than summing increments)
  and writes a `MessageKind::System` row keyed `usage_report`
  at end-of-turn. Host's delivery loop intercepts that action
  and writes a row into the new `agent_turns` table
  (migration `005_agent_turns.sql`). New
  `cclaw usage --since <window>` rolls up per-group totals.
- [x] **Per-group budgets**. New `group_budgets` table
  (`daily_token_cap`, `daily_cost_cap`; cost reserved) +
  `cclaw budgets list` / `cclaw budgets set --agent-group-id <id>
  --daily-tokens N`. Manager's `is_over_budget` check runs before
  every spawn, summing today's `agent_turns` (UTC midnight day
  boundary). Over-budget refusal is soft: the inbound row stays
  pending until the operator raises the cap or the day rolls
  over. The "budget exhausted" deliverable-to-original-sender
  variant is still open.
- [ ] **Rate limiting**. Per-group max LLM calls per minute /
  hour. Manager throttles spawn rate; runner backs off on 429
  with `Retry-After` (already wired) but the host-side cap is
  missing.

#### Sender approval

- [x] **`cclaw approvals approve`** + DB-backed gate. New
  subcommand `cclaw approvals approve --channel <ct> --identity
  <id> --display-name <name?>` upserts the central `users` row.
  ApprovalsModule grows an optional `SenderLookup` closure the
  host wires to `users::get_by_identity` — the gate checks the
  in-memory pre-approved set first, then the central DB. cli/local
  stays pre-seeded; everything else flows through the DB.
- [x] **Persist `known_senders` across restarts**. Source-of-
  truth is now the central `users` table; the in-memory list
  is just the cli/local pre-approval. Restarts no longer drop
  approvals.
- [ ] **Approval notifications**. When a sender lands in
  `pending` state, post a deliverable "approve?" prompt through
  the original messaging group so the operator can decide
  in-channel.

#### Reliability

- [ ] **Outbound dead-letter**. After 3 retries the delivery
  loop marks an outbound row `failed` and moves on. Need a
  `dropped_messages` flow (table already exists) so an operator
  can replay or inspect failures.
- [ ] **Central DB backup / restore**. `cclaw db backup <path>`
  + `cclaw db restore <path>`. The central DB is single-file
  SQLite; a backup is a copy under a held WAL checkpoint.
- [ ] **Versioned migrations**. `copperclaw migrate` already
  runs the central migrations but there's no
  schema-version table the host can check on boot to refuse a
  downgrade or warn about an in-progress upgrade.

#### Security

- [ ] **Webhooks TLS termination story**. Every webhook channel
  binds plain HTTP on `127.0.0.1` by default. Production
  deployments need either native TLS (rustls) or documented
  reverse-proxy patterns (Caddy / nginx / Cloudflare Tunnel)
  per channel. Pick one and ship it.
- [ ] **Secret rotation without restart**. Rotating
  `ANTHROPIC_API_KEY` today requires restarting the host so the
  spawned containers pick up the new env. A SIGHUP handler that
  rereads `.env` and re-emits to running containers is the
  minimum viable change.
- [ ] **Container egress allow-list**. Spawned containers can
  reach the whole internet via the host's network. Add a
  `container_configs.egress_allow` list (set of host:port pairs)
  the manager translates into `--add-host` and an in-container
  iptables / nftables rule, or use Docker's network policy
  flags.

#### Setup polish

- [x] **OpenRouter as a first-class provider in setup**. The
  auth step's base-URL prompt accepts friendly shortcuts:
  `openrouter`/`or` expands to the OpenRouter API base, blank
  /`anthropic`/`default` uses the upstream API, anything else
  passes through (trimmed). `OPENROUTER_BASE_URL` is a
  `pub const` so headless installs can refer to it without
  hard-coding.
- [x] **systemd `Restart=on-failure`** in the generated unit.
  Already in `units::render_systemd` since the unit template
  was first written — kept as [x] for the audit trail.
- [x] **`cclaw chat` shell**. New `cclaw chat` interactive REPL
  opens the install's `chat.fifo` for writing and tails
  `chat.log` from EOF for new replies. Pure file I/O — no
  socket call. Path defaults match the setup-produced layout
  on Linux/macOS; `--fifo` / `--log` overrides for non-default
  installs. Replaces the documented "echo into FIFO + tail log"
  pattern.

#### Bugs to clean up

- [ ] **Double `sessions/sessions/` path layout**. Host code uses
  `FsSessionRoot::new(cfg.sessions_root())` which produces
  `data_dir/sessions/sessions/<ag>/<session>` — fine but weird.
  Pick one (likely just `data_dir/sessions/<ag>/<session>`) and
  migrate. Container manager already has a comment explaining the
  workaround.
- [ ] **`base_url_does_not_strip_v1_in_the_middle_of_the_path`
  edge case**. Provider strips a trailing `/v1` but a user
  base URL like `https://corp.example/v1` followed by tenant
  segments wouldn't trip the heuristic. Document the rule or
  switch to an explicit `--api-base-no-suffix-append` mode.
- [ ] **Heartbeat-stale detection is logged but not acted on**.
  `sweep_report.heartbeat_stale > 0` is a clear signal a runner
  has died — wire it through to the manager (see "Crash-restart"
  above).
- [ ] **PLAN.md test counts diverge from `cargo test --workspace`**.
  Snapshots in M-section progress lines are written-once and
  drift fast. Move the count to a single line near the top that's
  the only spot that needs an update per slice.

### M14 — Agent capability (tool wiring + computer-use)

Before this slice the agent could only generate text. The runner
passed `tools: Vec::new()` to the provider, and even with a
`ToolDef` list the per-turn loop never called handlers — `ToolStart`
/ `ToolEnd` were telemetry only.

- [x] **`ProviderEvent::ToolCall`**. New variant carries
  `(id, name, input)` parsed from a `tool_use` content block.
  Anthropic SSE pump rewritten around an `SseState` struct that
  tracks the in-flight tool_use block, captures the `id` at
  `content_block_start`, accumulates `input_json_delta` chunks,
  parses to `Value` at `content_block_stop`.
- [x] **Tool-use loop in the runner**. `drive_turn` refactored
  into an outer tool-use loop and an inner `run_llm_turn`.
  Outer loop runs LLM → execute tools → LLM → … until the
  model produces a turn with no tool_use blocks (cap:
  `max_tool_turns`, default 20). Each tool's result becomes a
  `HistoryMessage::Tool` so the next turn sees what happened.
  Each LLM round emits its own `usage_report`, so `agent_turns`
  records each call separately.
- [x] **15 messaging / scheduling / self-mod tools wired**.
  `main.rs` builds the `tool_map` from
  `copperclaw_mcp::build_tool_set()` once at startup; the same
  pass produces the `ToolDef` list for the provider. The
  model now actually receives the schemas for `send_message`,
  `send_file`, `edit_message`, `add_reaction`,
  `ask_user_question`, `send_card`, `create_agent`,
  `install_packages`, `add_mcp_server`, `schedule_task`,
  `list_tasks`, `cancel_task`, `pause_task`, `resume_task`,
  `update_task`.
- [x] **4 computer-use tools added**
  (`crates/copperclaw-mcp/src/tools/computer_use.rs`):
  * `shell(command, cwd?, timeout_secs?)` — bash inside the
    container. Returns stdout/stderr/exit/elapsed. 64 KiB
    output cap, 60 s default timeout, 600 s ceiling.
  * `read_file(path)` — UTF-8 read with 1 MiB cap and
    char-boundary-safe truncation.
  * `write_file(path, content, create_parents?, append?)`
    — create or append; auto-mkdir-p.
  * `web_fetch(url, method?, body?, timeout_secs?)` — HTTP
    GET/POST. 256 KiB body cap, 30 s default, 120 s ceiling.
  Each ships with unit tests; full computer-use family is 11
  new tests.
- [x] **Heartbeat actually bumps mtime**. `touch_heartbeat`
  was opening but not writing; Linux only updates mtime on
  actual writes. Now writes a single byte per tick. Also,
  manager removes the heartbeat file at spawn so the new
  container starts with a clean slate.
- [x] **Base image bumped to `debian:trixie-slim`**
  (glibc 2.41) from `debian:12-slim` (glibc 2.36). Symptoms
  of the mismatch: container start crashed with
  `version GLIBC_2.39 not found` when the runner was built
  against any reasonably-modern host glibc.
- [x] **Default model bumped to `claude-sonnet-4-6`** from
  `claude-sonnet-4-5`. Six call sites updated in lockstep.

**Verified live** (OpenRouter, local cli channel via
`cclaw chat`):

    > Use the shell tool to run: whoami && uname -a && pwd.
    > Report the output exactly.
    agent> The output shows:
      - User: root
      - System: Linux ed8a01f860ad 6.17.0-14-generic …
      - Current directory: /

The bash command actually executed inside the session
container; nothing in that output came from training data.

**Slice totals**: 4416 passing, 0 failing. Clippy clean.

### M14 follow-up — Skill content auto-loaded into system prompt

The agent shipped M14 with 19 tools wired through but a vanilla one-
liner for a system prompt. The runner received the tool schemas but
not the per-skill prose explaining when to reach for each one. This
slice closes that gap.

- [x] **`copperclaw-skills` exposes a body reader**. New
  `frontmatter::skip_frontmatter(&str) -> &str` and
  `registry::read_skill_body(&Skill) -> Result<String, _>` return
  the markdown body with the YAML frontmatter stripped (BOM + CRLF
  aware). Lets the host inline a SKILL.md without re-parsing.
- [x] **`HostConfig` learns `skills_dir` + `groups_dir`**. Two new
  env-var fields (`COPPERCLAW_SKILLS_DIR`, `COPPERCLAW_GROUPS_DIR`) both
  optional and unset by default. Empty strings collapse to `None` so
  an unset variable means "skip skill injection".
- [x] **`ContainerManager::runner_config_for` populates `system`**.
  At spawn time the manager scans the global skills dir (optionally
  with a per-group override under `<groups_dir>/<ag_uuid>/skills/`),
  filters through the group's `SkillsSelector` (defaulting to
  `All` when no `container_config` exists), and concatenates each
  skill's body inside an `<skill name="…" description="…">` block
  preceded by a short framing sentence. Read or parse failures for
  individual skills are logged and skipped — the spawn still
  succeeds with the remaining content.
- [x] **`copperclaw-setup` writes both env vars**. `EnvFileSpec` gains
  `skills_dir` + `groups_dir`; `AuthStep` defaults them to
  `<data_dir>/skills` and `<data_dir>/groups` respectively (env-var
  override takes precedence). Empty paths omit the line so existing
  installs that haven't re-run setup stay unchanged.

**Slice totals**: 4436 passing, 0 failing. Clippy clean. +20 tests
across `copperclaw-skills` (+7), `copperclaw-host` (+12), and
`copperclaw-setup` (+1).

### M13 hardening — parallel-agent Top 10 sweep

Four worktree-isolated agents working in parallel landed nine of the
ten "next" items in a single slice (`web_search` deliberately skipped
per operator direction). Their outputs were merged sequentially after
fixing a duplicate migration number (Agent A and Agent B both
allocated `007_*`; Agent B's was renumbered to `008_*`) and a small
cross-agent integration gap (`UpsertContainerConfig` grew three new
fields under Agent A that Agent B's `mcp::add` path didn't know
about).

**Agent A — Container lifecycle hardening**

- [x] **Image rebuild on `container_config` change** (Top 10 #1).
  New `container_configs.config_fingerprint` sha256 column over the
  rebuild-relevant fields (`packages_apt`, `packages_npm`, `skills`,
  `mcp_servers`); manager diffs it pre-spawn and triggers a rebuild
  via the M2 image-build machinery, persisting the new tag onto
  `image_tag`. Closes the loop on `install_packages` /
  `add_mcp_server` so the agent's own tools take effect on the
  next spawn.
- [x] **Container egress allow-list** (Top 10 #6). New
  `container_configs.egress_allow` (JSON array of `host:port` pairs).
  Default empty == allow-all (default-allow + opt-in lockdown). Docker
  runtime applies via `--add-host` + a user-defined network; Apple
  runtime surfaces as `Unsupported`. Plumbed through `ContainerSpec`.
- [x] **Per-group resource caps** (Top 10 #8). New
  `container_configs.resource_limits` JSON (`cpus`, `memory_mb`,
  `pids_limit`; all optional). Docker runtime translates to
  `--cpus`, `--memory`, `--pids-limit`. Apple runtime: best-effort
  fall-through. New `cclaw groups config set-resource-limits` /
  `set-egress-allow` subcommands.

Migration `007_container_config_extensions.sql` adds the three
columns. Audit log captures the new mutation commands.

**Agent B — Operator cclaw commands**

- [x] **Central DB backup / restore** (Top 10 #5). `cclaw db backup
  <path>` runs `PRAGMA wal_checkpoint(TRUNCATE)` then writes a
  temp file + atomic `rename`. `cclaw db restore <path>` refuses
  with `host_running` when the host is up (the central DB has an
  open writer; you can't safely swap it). Both commands route
  through the cclaw socket and write `audit_log` rows; both are in
  `HOST_ONLY_COMMANDS`.
- [x] **Outbound dead-letter replay** (Top 10 #7). New
  `outbound_dropped_messages` table (migration `008_*` after
  renumbering — see merge note above). Delivery loop writes a
  dead-letter row when an outbound exhausts its retries. New
  commands `cclaw dropped-messages outbound-list --since <window>
  --limit N` and `cclaw dropped-messages replay <id>`. `replay`
  fetches the dead-letter, opens the per-session `outbound.db`,
  inserts a fresh `WriteOutbound`, and deletes the dead-letter row
  so the delivery loop picks it up again. `parse_since` accepts
  ISO-8601 or relative shorthands (`24h`, `30m`, `7d`).
- [x] **MCP server registry + first-class cclaw subcommand**
  (Top 10 #3). Curated preset library (`postgres`, `linear`,
  `github`, `notion`, `filesystem`, `browserbase`) shipped as a
  static `McpPreset` const array. `cclaw mcp list-presets`
  (no-socket; pure local catalog) and `cclaw mcp add <preset>
  --agent-group-id <id> [--env KEY=VAL]...` merge the chosen
  preset into `container_configs.mcp_servers` (JSON object keyed
  by server name). Audited mutation.

**Agent C — Sender approval notifications in-channel**

- [x] **Approval notifications** (Top 10 #2). `ApprovalsModule`
  grew `with_new_pending_notifier`; the boot wiring builds a
  closure that (a) checks `unregistered_senders` to de-dup, (b)
  resolves the agent group's primary messaging group, and (c)
  dispatches a plain-ASCII notification via the existing
  `DeliveryDispatcher`. Notification text: `Unknown sender pending
  approval. Run: cclaw approvals approve --channel <ct> --identity
  <id>` plus a small structured block (channel / identity /
  display name / first contact). Silently no-ops when the group
  has no messaging group attached. `pending_sender_approvals::
  upsert` was updated to return `(row, newly_inserted)` and a new
  `exists_for` helper added; both wired but the dedup actually
  uses `unregistered_senders` which is the right source of truth.

**Agent D — Observability**

- [x] **Prometheus metrics endpoint** (Top 10 #9). New
  `copperclaw-metrics` crate. Opt-in via `COPPERCLAW_METRICS_ADDR=
  127.0.0.1:9090` (bare port auto-prefixed to `127.0.0.1:`); off
  by default per the conservative-defaults tenet. Counters:
  `copperclaw_messages_inbound_total{channel_type}`,
  `copperclaw_messages_outbound_total{channel_type}`,
  `copperclaw_containers_spawned_total`,
  `copperclaw_containers_crashed_total`,
  `copperclaw_delivery_failed_total{channel_type}`. Histograms:
  `copperclaw_llm_call_seconds`, `copperclaw_llm_tokens_input`,
  `copperclaw_llm_tokens_output`, `copperclaw_container_spawn_seconds`.
  Hand-rolled `tokio::net::TcpListener` accept-loop, no axum/warp.
  Bumps live in router (inbound after DB write), delivery (outbound
  on success + final-failure), container manager (spawn success +
  CrashRestart), and runner (LLM call timing/tokens after
  `run_llm_turn`).
- [x] **Log rotation** (Top 10 #10). `tracing-appender::rolling`
  daily file writer wired into the host's main, opt-in via
  `COPPERCLAW_LOG_DIR=<path>` (default off; stderr-only behaviour
  unchanged). Pinned versions: `tracing-appender = "0.2"`,
  `metrics = "0.24"`, `metrics-exporter-prometheus = "0.16"`.

**Slice totals**: 4436 → 4540 passing (+104 tests). 0 failing.
Clippy clean. New crate: `copperclaw-metrics`. New migrations:
`007_container_config_extensions`, `008_outbound_dropped_messages`.
New cclaw subcommands: `db backup/restore`, `dropped-messages
outbound-list/replay`, `mcp list-presets/add`, `groups config
set-resource-limits/set-egress-allow`.

### Post-merge hardening (docs + edge cases + gap fixes)

- [x] **Audit-log env redaction.** `cclaw mcp add postgres --env
  DATABASE_URL=postgres://u:p@h/d` used to persist the connection
  string in `audit_log.args`. Added a per-command redactor in
  `socket.rs` that masks values under any `env` block for
  `mcp.add` and `groups.config.set-mcp-servers` while keeping the
  keys. 4 new tests.
- [x] **Image-rebuild failure fallback.** A bad apt name used to
  block the whole agent group indefinitely. The manager now falls
  back to the last-known-good image, emits
  `copperclaw_image_rebuild_failed_total`, and keeps the
  fingerprint unchanged so the rebuild retries on the next spawn
  when the operator fixes the config.
- [x] **`install_packages` / `add_mcp_server` were silent no-ops.**
  The tools emitted system rows that no handler picked up. The
  delivery loop now intercepts them inline (mirroring the
  `usage_report` pattern), merges them into `container_configs`,
  and the next spawn rebuilds. 7 new tests.
- [x] **Documentation:** `docs/observability.md`,
  `docs/db-backup.md`, `docs/container-config.md`. Updated
  README + CHANGELOG, refreshed `install-packages` /
  `add-mcp-server` skills to reflect automatic rebuild semantics.
  New skill docs for the four computer-use tools (`shell`,
  `read-file`, `write-file`, `web-fetch`).

### M14 follow-up — `web_search` tool

The agent could fetch URLs but couldn't find them. This slice adds a
multi-provider search tool that auto-routes based on which API key
the operator has configured.

- [x] **`web_search` tool** with four provider backends:
  - **Tavily** (`TAVILY_API_KEY`) — agent-tuned default.
  - **Exa** (`EXA_API_KEY`) — neural / semantic search.
  - **Brave** (`BRAVE_SEARCH_API_KEY`) — keyword-friendly
    independent index.
  - **`SerpAPI`** (`SERPAPI_API_KEY`) — Google/Bing/etc. wrapper.
- [x] **Normalised output shape** across all four providers
  (`{title, url, snippet, published?, score?}`) so the agent's
  downstream behaviour is provider-agnostic.
- [x] **Provider resolution**: explicit `provider` arg →
  `COPPERCLAW_WEB_SEARCH_PROVIDER` env → auto-detect from configured
  keys in the order `tavily, exa, brave, serpapi`. No keys → loud
  validation error naming all four env vars.
- [x] **Per-call result cap** (1–25, default 10) and per-result
  snippet cap (4 KiB, UTF-8-safe truncation with ellipsis).
- [x] **Host env-forwarding**: new `ManagerConfig.forward_env`
  field; `boot.rs::collect_forward_env` pushes the five
  `web_search` env vars into every spawned container so the
  operator only sets them once in `.env`. Empty values are
  skipped so an unset var doesn't leak into the container env.
- [x] **`skills/web-search/SKILL.md`** + operator-facing
  [`docs/web-search.md`](docs/web-search.md).

**Slice totals**: 4540 → 4576 passing (+36 tests: 25 in the
web_search module, 11 in host edge-case / delivery-apply / config
ensure paths). 0 failing. Clippy clean.

### Onboarding polish slice

Smoothing the gap between "I ran `copperclaw-setup`" and "my agent is
talking back to me". Three changes, each closing a distinct first-
run footgun.

- [x] **`cclaw doctor`** — composite first-run diagnostic.
  Sequence: host reachability via `groups.list` → agent group
  count → wiring count → active session count → recent audit
  errors (1h, top 3 by `result=error`) → dropped-message
  backlog → `ANTHROPIC_API_KEY` presence in this shell's env →
  web-search provider keys (informational). Each check renders
  `[OK   |WARN |FAIL ]` + name + detail, and non-OK rows print a
  `fix:` line the operator can copy-paste. `--json` emits
  `{ "status": "ok|fail", "checks": [...] }` for CI. Non-zero
  exit when any FAIL is present.
- [x] **Setup auto-bootstraps a default cli group + wiring.**
  New `quickstart_group` step (between `verify` and
  `first_chat`) writes a `(cli, stdin)` messaging group, an
  agent group named `first`, and a pattern-`.*` wiring with
  `session_mode = Shared`. Idempotent against any pre-existing
  agent group. The `first_chat` step's "what to do next" output
  flips to recommend `cclaw chat` directly when the bootstrap
  landed, falling back to the `cclaw quickstart cli` instruction
  when the operator declined. Opt out via
  `COPPERCLAW_SETUP_QUICKSTART=no`; rename the slug via
  `COPPERCLAW_SETUP_QUICKSTART_NAME`.
- [x] **Budget-exhausted reply to original sender.** When the
  container manager's spawn gate refuses because today's tokens
  exceeded `group_budgets.daily_token_cap`, the host now posts
  a one-line in-channel reply via the session's `outbound.db`
  (routed through `session_routing` so it lands back on the
  user's channel). Dedup per-agent-group on a 1 h window via a
  process-local `last_budget_notice` map so a chatty user gets
  one explanation, not ten. Silent skip when
  `session_routing` is empty (no inbound to anchor the routing
  yet).

**Slice totals**: 4576 → 4597 passing (+21 tests: 7 doctor + 7
quickstart-group + 4 budget-reply + 3 misc fix-ups). 0 failing.
Clippy clean.

### Production hardening slice (3 parallel agents)

Three worktree-isolated agents landed in one slice. After-merge:
- One renumbering (agent B's migration 007 → 009 to avoid
  collision with 007/008 from prior slices).
- Two of three notification text fixes during merge (agent B's
  per-hour text said "Per-minute"; agent B's helper wrote to
  inbound DB while main's pattern writes to outbound — unified
  via a new `post_cap_reply` helper that both paths share).
- Agent A's hand-rolled `copperclaw-metrics` collided with the
  real crate landed in observability — counter ported across
  via `inc_secrets_rotated()`.

- [x] **Secret rotation via SIGHUP** (`PLAN.md` was Next #1).
  `RotatableConfig` struct (anthropic_api_key, anthropic_base_url,
  forward_env) held behind an `Arc<RwLock<...>>` on
  `ContainerManager`. `reload_env(env_file)` parses `.env`
  without touching the process env (hand-rolled `parse_dotenv_content`
  handles quotes / comments / `export` prefixes) and swaps the
  lock. `wait_for_signal_or_sighup` loops on SIGHUP, calling
  `mgr.reload_env(env_file.as_deref())`. `run_host` gained an
  `env_file: Option<PathBuf>` parameter the main binary fills
  from `cli.env_file`. New metric
  `copperclaw_secrets_rotated_total`. Running containers see the
  rotated keys after idle-stop + respawn (default 5 min).

- [x] **Webhooks TLS termination doc** (was Next #2). New
  `docs/webhooks-tls.md` (204 lines) covers Caddy / nginx /
  Cloudflare Tunnel patterns, the bind-address override, and the
  design rationale for not bundling rustls in 0.1.0.

- [x] **Host-side rate limiting** (was Next #3). Two new columns
  on `group_budgets` (`agent_turns_per_minute_cap`,
  `agent_turns_per_hour_cap`) via migration `009_rate_limit_caps`.
  New `agent_turns::turns_since` count helper. Container manager
  gate fires in `maybe_spawn` after the budget gate: checks both
  windows, posts an in-channel reply on the outbound path (same
  `post_cap_reply` helper as the budget gate) with a 1-minute
  dedup window. `cclaw budgets set` extended with
  `--turns-per-minute` and `--turns-per-hour` flags.

- [x] **Versioned migrations** (was Next #5).
  `expected_central_schema_version()` (= `CENTRAL.len()`) and
  `applied_central_schema_version()` helpers in
  `copperclaw-db::migrate`. `BootError::SchemaMismatch` (exit code
  5) fires when applied > expected (downgrade detection).
  `cclaw schema-version` subcommand returns
  `{ "expected": N, "applied": M, "status": "ok|pending|future" }`.

- [x] **Double `sessions/sessions/` path cleanup** (was Next #6).
  `HostConfig::sessions_root()` returns `data_dir` directly;
  the layout flattens to `data_dir/sessions/<ag>/<sess>/`. New
  `migrate_sessions_layout()` runs at boot, moving contents from
  the old nested path up one level. Collision-safe: logs + skips
  when the destination exists, only removes the inner directory
  when all entries moved successfully.

**Slice totals**: 4597 → 4633 passing (+36 tests across the three
agents and the merge resolution). 0 failing. Clippy clean.

### Replay-fixture harness slice (M11 acceptance gate)

The plan's "Next #1" closed in this slice. The harness is
deliberately scoped down from the design doc's capture-and-redact
pipeline to the smallest thing that proves the differential test
loop: load a hand-authored fixture, drive the host's internal
services end-to-end against a wiremock-served Anthropic stub, and
diff the four captured streams against `expected/*.jsonl` with
substitution-aware comparison.

- [x] `crates/copperclaw-host/tests/replay/fixture.rs` — load
  `manifest.json` + `central.sql` + `inbound/*` + `claude/*` +
  `expected/*.jsonl` from a fixture directory. JSON manifest for
  v1 (TOML deferred behind an unused dep).
- [x] `crates/copperclaw-host/tests/replay/diff.rs` — substitution
  table (regex → replacement, applied to the serialized form of
  both sides before reparse), `walk`-based JSON-pointer-path
  mismatch reporter.
- [x] `crates/copperclaw-host/tests/replay/harness.rs` —
  `ReplayHarness` driving `Router::route` against an in-memory
  `CentralDb`, an in-process runner via `run_loop` with
  `max_turns = Some(1)` against `wiremock`-served Anthropic SSE,
  and `DeliveryService::process_session_once` against a
  `MockAdapter`. Per-fixture cleanup via `tempfile::TempDir`. Skips
  the full `run_host` boot dance (no channel mpsc consumer, no
  socket server, no signal handling, no container manager) — the
  harness is a structural-not-faithful reproduction of the host.
- [x] `fixtures/cli/text-reply/` — first fixture. Single-line cli
  inbound, one Claude turn, one outbound chat row, one delivery.
  Manifest substitutions normalise UUIDs (`[0-9a-f]{8}-...{12}`)
  and three timestamp shapes (`timestamp` / `started_at` /
  `ended_at`).
- [x] `crates/copperclaw-host/tests/replay.rs` — integration test
  entry. One `#[tokio::test]` loads the cli fixture and asserts
  `harness.compare().is_clean()`.

**Slice totals**: 4633 → 4634 passing (+1 integration test;
harness internals are exercised through it). 0 failing. Clippy
clean. New dev-deps in `copperclaw-host`: `wiremock`, `regex`,
`copperclaw-runner` / `-providers` / `-mcp` as workspace path deps.

---

## Next things to address

The five highest-impact items from the prior list landed in
earlier slices; the M11 acceptance gate landed in this slice. The
remaining items, ranked:

1. **Release 0.1.0**. Cut the tag, bump
   `workspace.package.version`, publish release notes from
   `CHANGELOG.md`. With the replay harness landed the candidate
   status in README can flip to a real release. _(M11)_

2. **Grow the fixture suite.** The first cli fixture proves the
   harness works end-to-end, but the suite has real value only
   when more channels and more behaviours are captured. Highest-
   leverage next captures: a telegram webhook with a
   `getFile`-backed attachment download, a slack
   `app_mention` with HMAC signature, and a runner tool-use loop
   (one `shell` round-trip).

---

## 0. Context

Copperclaw is a multi-channel Claude-agent runtime:

- The host orchestrates per-session Linux containers; one container per
  session.
- All host ↔ container IPC is **SQLite-on-bind-mount**: each session has
  an `inbound.db` (host writes, container reads) and an `outbound.db`
  (container writes, host reads). A central `copperclaw.db` holds identity,
  wiring, sessions, and configs.
- Channels (Telegram, Slack, Discord, …) feed the router; the router
  resolves a session and writes `messages_in`; the container's poll loop
  calls Claude and writes `messages_out`; the host's delivery loop
  delivers via the channel adapter.

Goal of this plan: define crate boundaries, contracts, milestones, and
team assignments so a swarm of agents can build copperclaw in parallel.

The Anthropic Agent SDK and MCP SDK both have viable Rust counterparts.
`rmcp` is the official MCP Rust SDK (used here). The agent loop itself
is hand-rolled against the Anthropic HTTP API, which is cleaner than
depending on a third-party Rust Agent SDK and gives us full control
over compaction and tool orchestration.

---

## 1. Architecture

```
                     ┌─────────────────┐
                     │ External channel│ (Telegram, Slack, …)
                     └────────┬────────┘
                              │ webhook / gateway
                       ┌──────▼──────┐
                       │  Channel    │ (one trait, many impls)
                       │  adapter    │
                       └──────┬──────┘
                              │ ChannelSetup::on_inbound_event(...)
                       ┌──────▼──────┐
                       │   Router    │ (resolve session, fan-out)
                       └──────┬──────┘
              writes to       │
                       ┌──────▼──────┐
                       │ inbound.db  │ ◄── session-scoped SQLite
                       └──────┬──────┘     (journal_mode=DELETE)
                              │ (Docker bind-mount, RO inside container)
              ╔═══════════════▼═══════════════╗
              ║ Container (1 per session)     ║
              ║  poll loop ── Anthropic API   ║
              ║         │     (HTTP stream)   ║
              ║         └─── rmcp client      ║
              ║                │              ║
              ║         ┌──────▼──────┐       ║
              ║         │ outbound.db │       ║
              ║         └──────┬──────┘       ║
              ╚════════════════│══════════════╝
                               │ host-poll
                       ┌───────▼──────┐
                       │   Delivery   │ (polls outbound, dispatches)
                       └───────┬──────┘
                               │ ChannelAdapter::deliver(...)
                       ┌───────▼──────┐
                       │ External user│
                       └──────────────┘

Background loops on host:
  - Active delivery poll (1s, running sessions only)
  - Sweep delivery poll (60s, all active sessions)
  - Host sweep (60s — stuck detection, recurrence, heartbeat sync)
  - CLI Unix socket server (listener for `cclaw`)
```

**Three invariants that must not change:**
1. `inbound.db` uses `journal_mode=DELETE` (WAL's mmap doesn't propagate
   across the Docker bind-mount — silent data loss otherwise).
2. Each SQLite file has exactly one writer process.
3. Sessions are durable; containers are ephemeral. State lives in DBs +
   filesystem, never in process memory beyond debounce/inflight maps.

---

## 2. Cargo workspace layout

```
copperclaw/
├── Cargo.toml                          # workspace + shared deps
├── rust-toolchain.toml                 # pin edition 2024 / rust 1.85+
├── PLAN.md                             # this document
├── README.md
├── LICENSE
├── .github/workflows/                  # ci.yml (fmt+clippy+test)
├── crates/
│   ├── copperclaw-types/                 # T1 — shared types, no I/O
│   ├── copperclaw-db/                    # T2 — central DB + per-session DB
│   ├── copperclaw-host/                  # T3 — binary: orchestrator
│   ├── copperclaw-host-router/           # T3 — inbound router
│   ├── copperclaw-host-delivery/         # T3 — outbound delivery loop
│   ├── copperclaw-host-sweep/            # T3 — 60s maintenance loop
│   ├── copperclaw-container-rt/          # T4 — Docker + Apple Container
│   ├── copperclaw-runner/                # T5 — binary: container-side agent runner
│   ├── copperclaw-providers/             # T5 — Claude/Codex/OpenCode/Ollama trait + impls
│   ├── copperclaw-mcp/                   # T5 — MCP server + tool impls
│   ├── copperclaw-channels/              # T6 — Channel trait + per-channel crates
│   │   ├── core/                       # trait + registry
│   │   ├── cli/                        # stdin/stdout adapter
│   │   └── …                           # added per channel
│   ├── copperclaw-modules/               # T7 — typing, permissions, approvals, scheduling, …
│   ├── copperclaw-skills/                # T8 — skill discovery/validation/mount
│   ├── copperclaw-cclaw/                   # T9 — binary: admin CLI + socket server
│   ├── copperclaw-setup/                 # T10 — binary: interactive setup
│   └── copperclaw-onecli/                # T11 — OneCLI credential gateway
├── container/
│   ├── Dockerfile                      # Debian-slim + Chromium + static runner
│   ├── build.sh
│   └── entrypoint.sh                   # tini + copperclaw-runner
├── data/                               # runtime (gitignored): copperclaw.db, sessions/, logs/
├── groups/                             # per-agent workspaces (created at runtime)
├── skills/                             # copperclaw skill content (authored under T10)
├── docs/                               # architecture docs
├── config-examples/
├── launchd/                            # macOS plist template
└── systemd/                            # Linux user unit template
```

---

## 3. Shared types contract (`copperclaw-types`)

This crate is **the contract surface** between all other crates. It has
zero I/O dependencies and must compile fast. Every team consumes it.
**Owner: T1.** Other teams MUST NOT add new types here without T1 review.

```rust
// crates/copperclaw-types/src/lib.rs

pub mod id;
pub mod session;
pub mod message;
pub mod routing;
pub mod channel;
pub mod provider;
pub mod approval;
pub mod schedule;
```

**Required type stubs** (see source for full definitions):

```rust
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct AgentGroupId(pub Uuid);
pub struct MessagingGroupId(pub Uuid);
pub struct SessionId(pub Uuid);
pub struct UserId(pub Uuid);
pub struct MessageId(pub Uuid);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelType(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundEvent { /* … */ }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage { /* … */ }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage { /* … */ }

pub enum MessageKind { Chat, Task, Webhook, System, Agent }
pub enum SessionMode { Shared, PerThread, AgentShared }
pub enum ContainerStatus { Idle, Running, Stopped }
pub enum EngageMode { Pattern, Mention, MentionSticky }
```

**Rule**: when in doubt, add the field to the type rather than a sidecar
JSON blob. Typed boundaries beat JSON blobs.

---

## 4. Database schemas (authoritative)

DB ownership and schemas are fixed for the first port. **Owner: T2.**

### 4.1 Central DB `data/copperclaw.db`

| Table | PK | Unique | FKs | Indexes |
|---|---|---|---|---|
| `agent_groups` | `id` | `folder` | — | — |
| `messaging_groups` | `id` | `(channel_type, platform_id)` | — | — |
| `messaging_group_agents` | `id` | `(messaging_group_id, agent_group_id)` | both | — |
| `users` | `id` | — | — | — |
| `user_roles` | `(user_id, role, agent_group_id)` | — | `users` | `idx_user_roles_scope(agent_group_id, role)` |
| `agent_group_members` | `(user_id, agent_group_id)` | — | both | — |
| `user_dms` | `(user_id, channel_type)` | — | both | — |
| `sessions` | `id` | — | both | `idx_sessions_agent_group`, `idx_sessions_lookup` |
| `pending_questions` | `question_id` | — | `sessions` | — |
| `unregistered_senders` | `(channel_type, platform_id)` | — | — | `idx_unregistered_senders_last_seen` |
| `pending_sender_approvals` | `id` | `(messaging_group_id, sender_identity)` | mg, ag, users | `idx_pending_sender_approvals_mg` |
| `pending_channel_approvals` | `messaging_group_id` | — | both | — |
| `container_configs` | `agent_group_id` | — | `agent_groups CASCADE` | — |
| `dropped_messages` | `id` | — | — | — |

### 4.2 Per-session inbound DB

`data/sessions/<agent_group_id>/<session_id>/inbound.db`

**Host writes, container reads.** `PRAGMA journal_mode=DELETE` is
mandatory.

```sql
CREATE TABLE messages_in (
  id           TEXT PRIMARY KEY,
  seq          INTEGER NOT NULL UNIQUE,
  kind         TEXT NOT NULL,
  timestamp    TEXT NOT NULL,
  status       TEXT NOT NULL DEFAULT 'pending',
  process_after TEXT,
  recurrence   TEXT,
  series_id    TEXT,
  tries        INTEGER NOT NULL DEFAULT 0,
  trigger      INTEGER NOT NULL DEFAULT 1,
  platform_id  TEXT,
  channel_type TEXT,
  thread_id    TEXT,
  content      TEXT NOT NULL,
  source_session_id TEXT,
  on_wake      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_messages_in_series ON messages_in(series_id);

CREATE TABLE delivered (
  message_out_id      TEXT PRIMARY KEY,
  platform_message_id TEXT,
  status              TEXT NOT NULL,
  delivered_at        TEXT NOT NULL
);

CREATE TABLE destinations (
  name           TEXT PRIMARY KEY,
  display_name   TEXT NOT NULL,
  type           TEXT NOT NULL,
  channel_type   TEXT,
  platform_id    TEXT,
  agent_group_id TEXT
);

CREATE TABLE session_routing (
  id            INTEGER PRIMARY KEY CHECK (id = 1),
  channel_type  TEXT,
  platform_id   TEXT,
  thread_id     TEXT
);
```

### 4.3 Per-session outbound DB

**Container writes, host reads.**

```sql
CREATE TABLE messages_out (
  id           TEXT PRIMARY KEY,
  seq          INTEGER NOT NULL UNIQUE,
  in_reply_to  TEXT,
  timestamp    TEXT NOT NULL,
  deliver_after TEXT,
  recurrence   TEXT,
  kind         TEXT NOT NULL,
  platform_id  TEXT,
  channel_type TEXT,
  thread_id    TEXT,
  content      TEXT NOT NULL
);

CREATE TABLE processing_ack (
  message_id     TEXT PRIMARY KEY,
  status         TEXT NOT NULL,
  status_changed TEXT NOT NULL
);

CREATE TABLE session_state (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE container_state (
  id                       INTEGER PRIMARY KEY CHECK (id = 1),
  current_tool             TEXT,
  tool_declared_timeout_ms INTEGER,
  tool_started_at          TEXT,
  updated_at               TEXT
);
```

### 4.4 Session folder layout

```
data/sessions/<agent_group_id>/<session_id>/
├── inbound.db
├── outbound.db
├── .heartbeat                 # container touches mtime; host stat()s
├── inbox/<msg_id>/<filename>  # host-extracted attachment files
└── outbox/<msg_id>/<filename> # container-written attachment files
```

**Attachment safety contract** (T2 implements; everyone consumes):
- `safe_attachment_name()` rejects `..`, slashes, leading dots, len>255.
- `extract_to_inbox()` uses `O_EXCL | O_NOFOLLOW`, refuses symlinks at
  any path component, asserts `realpath(out)` ⊂ session inbox root.
- Mirror for `read_from_outbox()`.

---

## 5. Wire contracts between subsystems

### 5.1 Channel adapter trait

```rust
#[async_trait]
pub trait ChannelAdapter: Send + Sync {
    fn channel_type(&self) -> &ChannelType;
    fn supports_threads(&self) -> bool { false }

    async fn subscribe(&self, platform_id: &str, thread_id: Option<&str>) -> Result<(), AdapterError> { Ok(()) }
    async fn set_typing(&self, platform_id: &str, thread_id: Option<&str>) -> Result<(), AdapterError> { Ok(()) }
    async fn deliver(&self, platform_id: &str, thread_id: Option<&str>, message: &OutboundMessage) -> Result<Option<String>, AdapterError>;
    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> { Ok(None) }
}

#[async_trait]
pub trait ChannelFactory: Send + Sync {
    fn channel_type(&self) -> ChannelType;
    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError>;
    async fn shutdown(&self) -> Result<(), AdapterError> { Ok(()) }
    fn container_contribution(&self) -> ContainerContribution { ContainerContribution::default() }
}
```

### 5.2 Provider trait

```rust
#[async_trait]
pub trait AgentProvider: Send + Sync {
    fn supports_native_slash_commands(&self) -> bool { false }
    async fn query(&self, input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError>;
    fn is_session_invalid(&self, err: &ProviderError) -> bool { false }
}

#[async_trait]
pub trait AgentQuery: Send {
    async fn push(&mut self, message: String) -> Result<(), ProviderError>;
    async fn end(&mut self) -> Result<(), ProviderError>;
    async fn next_event(&mut self) -> Option<ProviderEvent>;
    async fn abort(&mut self);
}

pub enum ProviderEvent {
    Init { continuation: String },
    Result { text: Option<String> },
    Error { message: String, retryable: bool },
    Progress { message: String },
    Activity,
    ToolStart { name: String, declared_timeout_ms: Option<u64> },
    ToolEnd,
}
```

### 5.3 Module hook surface

```rust
pub struct HostContext { /* … */ }
impl HostContext {
    pub fn set_sender_resolver(&self, f: Arc<dyn Fn(&InboundEvent) -> Option<UserId> + Send + Sync>);
    pub fn set_access_gate(&self, f: Arc<dyn Fn(GateCtx) -> GateDecision + Send + Sync>);
    pub fn set_sender_scope_gate(&self, f: ...);
    pub fn set_message_interceptor(&self, f: ...);
    pub fn set_channel_request_gate(&self, f: ...);
    pub fn register_delivery_action(&self, name: &str, h: Arc<dyn DeliveryActionHandler>);
    pub fn on_delivery_adapter_ready(&self, cb: Arc<dyn Fn(&dyn DeliveryDispatcher) + Send + Sync>);
}
```

### 5.4 `cclaw` Unix-socket wire protocol

JSON, newline-delimited, request-response, half-close per request.
Lives at `data/cclaw.sock` (mode `0o600`).

```rust
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Request {
    Call { id: String, command: String, args: serde_json::Value, caller: Caller },
}

#[derive(Serialize, Deserialize)]
pub enum Caller {
    Host,
    Agent { session_id: SessionId, agent_group_id: AgentGroupId, messaging_group_id: Option<MessagingGroupId> },
}

#[derive(Serialize, Deserialize)]
pub enum Response {
    Ok { id: String, data: serde_json::Value },
    Err { id: String, error: ErrorPayload },
}
```

Agent caller comes through the **session DBs** (container-side `cclaw`
writes a `cli_request` system message; container poll loop forwards to
host; host writes the response back), not through the socket.

### 5.5 Container-runner spawn contract

```rust
#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    async fn ensure_running(&self) -> Result<(), RtError>;
    async fn cleanup_orphans(&self, install_slug: &str) -> Result<(), RtError>;
    async fn spawn(&self, spec: ContainerSpec) -> Result<ContainerHandle, RtError>;
    async fn stop(&self, name: &str, grace: Duration) -> Result<(), RtError>;
    async fn build_image(&self, spec: ImageBuildSpec) -> Result<String, RtError>;
}

pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub labels: HashMap<String, String>,
    pub env: Vec<(String, String)>,
    pub mounts: Vec<Mount>,
    pub entrypoint: Vec<String>,
    pub user: Option<String>,
    pub extra_hosts: Vec<(String, String)>,
}
```

Backends: `DockerRuntime` (via `bollard`) and `AppleContainerRuntime`
(spawn `container` CLI; same trait).

---

## 6. Team assignments

Each team is a parallel work stream. **Dependencies are explicit** — a
team can begin once its dependencies' contracts are merged, even if
those teams aren't done implementing.

### T1 — `copperclaw-types` (gate; serial)

Shared types + serde + enum kinds. No I/O. Acceptance: all types in
§ 3 compile; serde round-trip tests; no tokio/reqwest/rusqlite deps.

### T2 — `copperclaw-db`

Depends on T1. Central DB + per-session DB layer, migrations,
attachment safety. Acceptance: migrations apply cleanly; per-table
CRUD round-trip; property test for seq parity; cross-mount visibility
test.

### T3 — host + host-router + host-delivery + host-sweep

Depends on T1, T2, T4, T5, T6, T7. Host orchestrator with the boot
sequence, router, delivery loops, and sweep.

**Constants**: `ACTIVE_POLL_MS=1000`, `SWEEP_POLL_MS=60_000`,
`ABSOLUTE_CEILING_MS=1_800_000`, `CLAIM_STUCK_MS=60_000`,
`MAX_DELIVERY_ATTEMPTS=3`, `MAX_TRIES=5`, `BACKOFF_BASE_MS=5_000`.

### T4 — `copperclaw-container-rt`

Depends on T1. Docker + Apple Container; image build.

### T5 — runner + providers + mcp

Depends on T1, T2. The entire container-side process.

**Poll loop constants**: `POLL_INTERVAL_MS=1000`,
`ACTIVE_POLL_INTERVAL_MS=500`.

**Disallowed built-in tools (host owns these)**: `CronCreate`,
`CronDelete`, `CronList`, `ScheduleWakeup`, `AskUserQuestion`,
`EnterPlanMode`, `ExitPlanMode`, `EnterWorktree`, `ExitWorktree`.

**Compaction**: when input tokens approach
`model_input_window - safety_margin`, summarize the oldest half of the
transcript via a separate `messages.create` call, archive the
pre-compaction transcript to `outbox/_compactions/<ts>.md`, replace the
summarized chunk with a `compact_boundary` synthetic user message,
continue.

**Session resume**: persist `(message_history, continuation_seq)` to
`outbound.session_state` after every turn; reload on container startup.

### T6 — `copperclaw-channels/*`

Depends on T1, T3 trait. Channels for M7: telegram, slack, discord.
Channels for M8: whatsapp, whatsapp-cloud, signal, deltachat, imessage,
teams, matrix, gchat, webex, linear, github, resend, wechat, emacs, x.

**Pattern contract**: each channel crate exposes
`pub struct <Name>Factory; impl ChannelFactory for <Name>Factory`, and a
`register(reg: &mut ChannelRegistry)` function. No other public API.

### T7 — `copperclaw-modules`

Depends on T1, T2, T3. Sub-modules: typing, mount-security, permissions,
approvals, interactive, scheduling, agent-to-agent, self-mod.

### T8 — `copperclaw-skills`

Depends on T1. Skill discovery (frontmatter parse), per-group override,
container materialization via symlinks.

### T9 — `copperclaw-cclaw`

Depends on T1, T2, T3. See § A2 for subcommand inventory.

### T10 — `copperclaw-setup`

Depends on most other crates. Interactive setup; environment check,
container build, OneCLI init, auth, mounts, service unit, cli-agent,
timezone, channel, verify, first-chat.

### T11 — `copperclaw-onecli`

Depends on T1. OneCLI HTTP client.

---

## 7. MCP tool inventory (T5 owns)

| Tool | Module | Schema |
|---|---|---|
| `send_message` | `core` | `{ to?: string, text: string }` |
| `send_file` | `core` | `{ to?: string, filename: string, data: base64, text?: string }` |
| `edit_message` | `core` | `{ message_id: int_seq, text: string }` |
| `add_reaction` | `core` | `{ message_id: int_seq, emoji: string }` |
| `ask_user_question` | `interactive` | `{ title: string, options: [string], to?: string }` |
| `send_card` | `interactive` | `{ to?: string, card: object }` |
| `create_agent` | `agents` | `{ name: string, instructions: string, channel?: string }` |
| `install_packages` | `self-mod` | `{ apt?: [string], npm?: [string], reason: string }` |
| `add_mcp_server` | `self-mod` | `{ name: string, transport: object, reason: string }` |
| `schedule_task` | `scheduling` | `{ name, when, prompt, recurrence? }` |
| `list_tasks` | `scheduling` | `{}` |
| `cancel_task` | `scheduling` | `{ id }` |
| `pause_task` | `scheduling` | `{ id }` |
| `resume_task` | `scheduling` | `{ id }` |
| `update_task` | `scheduling` | `{ id, prompt?, when?, recurrence? }` |

---

## 8. Milestone sequencing

```
M0  Workspace skeleton + T1 types               (gate)
M1  T2 db                                       (gate for everything else)
M2  T4 container-rt                ┐
    T8 skills                      ├─ parallel after M1
    T11 onecli                     ┘
M3  T5 runner+providers+mcp        ┐
    T6 channels/core + T6 cli      ├─ parallel
    T7 modules                     ┘
M4  T3 host                        — assembles M2/M3
M5  T9 cclaw
M6  T10 setup
M7  T6 telegram + T6 slack + T6 discord  (parallel)
M8  T6 batch: remaining channels         (parallel teams per channel)
M9  T5 codex/opencode/ollama provider variants
M10 Skill content
M11 Differential test + release
```

---

## 9. Verification

### Test-coverage standard (applies to every crate)

**Every public function, type variant, error path, SQL query, trait impl,
and HTTP endpoint must have a test.** A module that ships without coverage
is incomplete work — the step is not done until the tests are in place.

- Per-crate: `cargo build`, `cargo clippy -- -D warnings`, `cargo test`.
- Coverage gate: `cargo llvm-cov --workspace --fail-under-lines 85`
  (CI-enforced).
- DB code: round-trip every CRUD path against a fresh in-memory or
  `tempfile`-backed database.
- Async loops: use `tokio::time::pause()` + advance to drive time-based
  branches; do not depend on real sleeps.
- HTTP clients: snapshot tests against `wiremock` fixtures.
- Trait abstractions with multiple impls: write a generic test suite that
  runs once per backend.
- T1/T2 types: serde round-trip property tests for every public struct
  and enum variant.
- T3 host: integration tests with stubbed channel + stubbed container.
- T5 runner: integration tests against a mock Anthropic server
  (`wiremock`); end-to-end test with a real Claude API key gated by
  `COPPERCLAW_E2E=1`.
- T6 channels: replay tests against captured platform fixtures.

### Cross-crate (M4 onward)
- End-to-end "CLI channel" test: spawn host + container, type a line in
  the CLI channel, see a Claude response.
- Skill mount test: verify expected skills appear in container.
- Sweep test: artificially stall a container, observe kill+restart.

---

## 10. Risks and mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| **Channel platform APIs underestimated.** Each is bespoke. | High | High | Budget ~1 team-week per channel beyond the first three. |
| **Anthropic streaming/tool-use loop bugs.** | High | High | Snapshot tests against a mock server before live API. |
| **Cross-mount SQLite visibility regression.** | Medium | High | Dedicated cross-mount visibility test; enforce `journal_mode=DELETE` in code. |
| **Compaction divergence.** | Medium | Medium | Configurable strategy; ship "summarize oldest half" in M3, refine later. |
| **`rmcp` API churn.** | Low | Medium | Pin minor version; vendor or fork if needed. |
| **`bollard` Docker API drift.** | Low | Medium | Pin engine API version 1.43+. |
| **OneCLI is a moving target.** | Medium | Medium | Wrap behind T11's trait so we can stub or swap. |

---

## Appendix

### A1 — DB function inventory (T2 exports)

Central DB:
```
agent_groups: list, get, get_by_folder, create, update, delete
messaging_groups: list, get, get_by_platform, get_with_agent_count, upsert, delete
messaging_group_agents: list_for_mg, list_for_ag, get, upsert, delete
users: list, get, get_by_identity, upsert
user_roles: list_for_user, list_for_scope, grant, revoke
agent_group_members: list, add, remove
user_dms: get, upsert, list
sessions: find_for_agent, find_by_agent_group, create, get, update_status,
          mark_running, mark_idle, mark_stopped, touch_last_active,
          list_active, list_running
pending_questions: insert, get, delete
container_configs: get, upsert, update_field, get_skills, set_skills,
                   get_mcp_servers, set_mcp_servers
unregistered_senders: upsert, list
pending_sender_approvals: list, get, upsert, delete
pending_channel_approvals: list, get, upsert, delete
dropped_messages: insert
```

Inbound session DB:
```
messages_in: insert, get_pending(first_poll, limit), count_due,
             mark_completed, mark_failed, retry_with_backoff, get_by_series
delivered: insert, get_delivered_ids
destinations: replace_all, list
session_routing: read, write
```

Outbound session DB:
```
messages_out: insert(seq=next_odd), list_due, get_by_id
processing_ack: get_all, insert, update_status
session_state: get, set
container_state: get, set
```

### A2 — `cclaw` subcommand inventory (T9 implements)

```
cclaw groups list
cclaw groups get <id>
cclaw groups create --folder <f> --name <n> [--provider <p>]
cclaw groups update <id> [--name <n>] [--provider <p>]
cclaw groups delete <id>
cclaw groups restart <id>
cclaw groups config get <id>
cclaw groups config update <id> --field <k>=<v>
cclaw groups config add-mcp-server <id> --json '<config>'
cclaw groups config remove-mcp-server <id> --name <name>
cclaw groups config add-package <id> --apt <pkg> | --npm <pkg>
cclaw groups config remove-package <id> --apt <pkg> | --npm <pkg>

cclaw messaging-groups list
cclaw messaging-groups get <id>
cclaw messaging-groups create --channel-type <t> --platform-id <p> [--name <n>] [--is-group]
cclaw messaging-groups update <id> [...]
cclaw messaging-groups delete <id>

cclaw wirings list
cclaw wirings get <id>
cclaw wirings create --mg <id> --ag <id> --engage <pattern|mention|mention-sticky> [--pattern <re>] [--sender-scope <all|known>] [--session-mode <shared|per-thread|agent-shared>] [--priority <n>]
cclaw wirings update <id> [...]
cclaw wirings delete <id>

cclaw users list
cclaw users get <id>
cclaw users create --identity <channel:handle> [--display-name <n>]
cclaw users update <id> [...]

cclaw roles list
cclaw roles grant <user> <role> [--agent-group <id>]
cclaw roles revoke <user> <role> [--agent-group <id>]

cclaw members list <agent-group>
cclaw members add <agent-group> <user>
cclaw members remove <agent-group> <user>

cclaw destinations list <agent-group>
cclaw destinations add <agent-group> --name <n> --type <channel|agent> [...]
cclaw destinations remove <agent-group> --name <n>

cclaw sessions list [--agent-group <id>] [--status <s>]
cclaw sessions get <id>

cclaw user-dms list
cclaw dropped-messages list [--since <ts>]
cclaw approvals list
cclaw approvals get <id>
```

Output formats: human table by default; `--json` for machine output.

---


## Implementation invariants

Load-bearing properties of this codebase that are convention rather than enforced by types or DB constraints. Any new write path must respect them; any change here is a breaking change.

- **Seq parity is convention, not constraint.** Host writes even `seq` to `messages_in`; container writes odd `seq` to `messages_out`. The DB does not enforce parity — two helper functions do. New write paths must preserve parity or two writers will collide on edits.
- **Heartbeat liveness uses file mtime, not DB queries.** The host `stat()`s `<session>/.heartbeat`; the container touches it. Switching to a DB-backed liveness would re-introduce the cross-mount visibility hazard `journal_mode=DELETE` exists to prevent.
- **Destinations are dual-written.** Approvals + group-membership mutations land in both the central DB and the per-session DB inside the same router call. Don't lazy-sync — stale destinations are how cross-channel messages get sent to the wrong place.
- **Wake messages bypass debounce.** The `on_wake = 1` column on `messages_in` is processed once at container boot regardless of recurrence state; it is how the host hands over in-flight context to a freshly-spawned container without racing the poll loop.
- **Per-install service slug.** systemd / launchd unit names are SHA1-suffixed by project root so a developer can run two installs side-by-side without one stomping the other's `cclaw.sock`.
- **Single-writer per SQLite file.** Inbound DB: host writes only. Outbound DB: container writes only. Central DB: host process only. Every test that opens a writer outside the owning process must use `open_inbound_ro_no_mmap` or equivalent.

## Future work

Items deliberately deferred from 0.1.0; tracked here so they don't get rediscovered.

- **Scheduled tasks table.** A first-class `scheduled_tasks` table for recurring agent jobs (independent of the per-message `recurrence` column). The MCP `schedule_task` tool currently writes into `messages_in` with a recurrence and a `process_after`; a dedicated table would let us list/cancel without scanning the message log.
- **WhatsApp Signal Protocol session state.** The Curve25519 / Ed25519 / HKDF / AES-GCM primitives in `crates/copperclaw-channels/whatsapp/src/crypto/dalek.rs` are RFC-tested and ready. What sits above them — X3DH key agreement, the Double Ratchet, Sender Keys for group chat, the WA wire-envelope construction — is the next-contributor task. Adapter `deliver()` surfaces a distinct error message so the gap is testable.
- **Docker Sandbox runtime backend.** A third `ContainerRuntime` impl using a micro-VM (`firecracker`, `cloud-hypervisor`, or Apple's `Virtualization.framework`) for installations that want a stronger isolation boundary than a Docker container.
- **Replay-fixture capture tooling.** The harness reads hand-authored or captured fixtures (see `docs/replay-fixtures.md` and `fixtures/cli/text-reply/`). The capture-time tooling described in the design doc — `COPPERCLAW_FIXTURE_CAPTURE=<dir>` taps in the channel adapter, router, provider, and DB layer, plus an `copperclaw fixture redact <dir>` pass — is still unwritten. Until it lands new fixtures must be hand-authored.


## Sign-off

Updates to the plan happen in-tree from this point on. Whenever a step
in **Progress** is completed, tick the box and reference the artifact
that landed it.

---

## M16 — OpenClaw parity + security-hardening roadmap (2026-06-02)

Derived from a multi-agent gap analysis of OpenClaw (formerly ClawdBot /
Moltbot) against the Copperclaw tree. The defining bet: Copperclaw covers
~70% of OpenClaw's capability with a deliberately smaller surface. We close
the gap by *first* making the controls we already advertise actually run,
*then* adding capability in an order where each new attack surface lands on
controls that bound its blast radius. We do **not** claim "strictly safer at
every step" — that is false once a browser and vector memory ship. Every
phase is tagged **[REDUCES surface]** or **[EXPANDS surface]**.

### Inert controls found in tree (fix before any new capability)

Three advertised controls do not actually run today (verified file:line):

- **Egress allow-list is a no-op.** `container-rt/src/spec.rs:179` claims
  iptables enforcement; `docker.rs` wires only `extra_hosts` — no
  `NetworkMode`/netns/nftables, no DNS handling. Containers get the default
  bridge = unrestricted outbound.
- **Provider key is leaked.** `host/src/container_manager/spawn.rs:481`
  forwards `ANTHROPIC_API_KEY` as a plain container env var; one injected
  `printenv` exfiltrates the master key.
- **Per-skill `allowed-tools` is parsed, never enforced.** Read only in
  tests; the sole live gate is the static 9-entry `disallowed.rs` list.
- Plus: `web_fetch`/`web_search` (`mcp/src/tools/computer_use.rs`,
  `web_search.rs`) have no SSRF guard and no redirect policy (loopback /
  `169.254.169.254` / RFC1918 reachable); `modules/src/permissions.rs:194`
  returns `Defer` (open-fail) on an unknown op even with the module
  installed; `drive_turn.rs:108` caps consecutive tool turns but has no
  identical/ping-pong loop detection; `mount_security.rs` symlink check is
  TOCTOU.

### Phases

- **Phase 0a — Egress + DNS default-deny + SSRF [REDUCES]** (M-L, 3-4wk).
  Internal no-default-route network; only model endpoint + allow-list +
  filtering resolver reachable; pin `resolv.conf` to a filtering resolver;
  shared SSRF guard for `web_fetch`/`web_search`/browser with per-hop
  redirect re-check. Auto-inject the model endpoint into every group's
  implicit allow-list (migration is a 100%-outage risk); ship opt-in in N,
  flip default in N+1, gate with `cclaw groups egress migrate`. Files:
  `container-rt/src/{docker.rs,spec.rs}`, new `egress.rs`, `spawn.rs`, new
  `mcp/src/tools/net_guard.rs`. v1 allow-lists the direct model endpoint
  (not the not-yet-built broker) so it ships before 0b.
- **Phase 0b — Credential broker [REDUCES]** (M-L, 3-4wk, single-owner).
  Stop forwarding the key (`spawn.rs:481`); host-side loopback model proxy;
  per-session, group-scoped, TTL-bounded, revocable token; per-group budget
  at the broker. Honest caveat: brokering stops key *theft*, not *misuse* —
  the token is still a live in-box credential and exfil survives inside
  model prompts. We narrow exfil to model-proxy + approved recipients; we
  never say "severs."
- **Phase 0c — Default-deny fallbacks + log redaction [REDUCES]** (S-M).
  `permissions.rs:195` unparseable op -> Deny + audit; host default-deny
  when no permissions module installed; secret redaction in runner/host
  logs; `0600` on session dirs/.env. **Phase 0 gate is mechanical**: doctor
  FAILs / `rebuild.sh` refuses to bake an image if any Phase-3+ capability
  is on while egress is allow-all or the broker is down.
- **Phase 1 — Layered tool authorization + loop breakers [REDUCES]** (M).
  Make `allowed_tools` real: dispatch-time positive allow-list vs (group,
  sender role, active skill); profiles minimal/messaging/coding/full;
  repeated-call circuit breakers. Un-gated; run parallel with Phase 0.
  Files: `runner/src/disallowed.rs`->`policy.rs`, `skills/src/registry.rs`,
  `modules/src/permissions.rs`, `run/drive_turn.rs`.
- **Phase 2 — Approval lifecycle + DM pairing + mention gating [REDUCES]**
  (M, parallel w/ 1). Approval TTL + revocation + decision-audit; 8-char
  1hr rate-limited pairing codes; per-group `requireMention` + regex +
  implicit-mention-on-quote. Files: `modules/src/approvals.rs`, new
  migration, `host/src/handlers/approvals.rs`, channel adapters, `cclaw`.
- **Phase 3 — Searchable memory + provenance [EXPANDS]** (L+M, gated by
  0,1,2). Per-group `sqlite-vec`+FTS5 store; embeddings via broker.
  Provenance is necessarily coarse (taint does not propagate through an
  LLM): any turn containing untrusted-provenance content requires fresh
  approval for credentialed external actions; autonomous/heartbeat turns
  may search but not act (read-then-propose).
- **Phase 4 — Provider fallback chains + multi-key [neutral]** (M). Home =
  `copperclaw-providers` directly (not gated on broker). New migration for
  per-group provider profiles + health state; per-channel pinning.
- **Phase 5 — Browser tool [EXPANDS — max trifecta risk]**. 5a (committed,
  gated by 0+1): headless render + screenshot + read-only DOM in a
  dedicated child container under gVisor/microVM, off the broker trust
  domain, reusing the 0a SSRF guard. 5b (stretch, gated by 0+1+3):
  interactive click/type + browser-writes-memory; demand-pull only.
- **Phase 6 — Operability + MCP + supply-chain integrity [REDUCES]** (S-M).
  `cclaw security audit [--fix]`; per-server MCP tool filtering + host-side
  oauth store; constrain `install_packages` subprocesses (deny broker
  token); TOCTOU symlink fix (`mount_security.rs`); signed/attested image +
  runner binary; heartbeat condition-check-in only.
- **Phase 7 — More channels [neutral]** — demand-pull only. Ranked SMS,
  IRC, Twitch, Feishu, then the long tail. Keep WhatsApp on the official
  Cloud API; never Baileys QR.

### How we stay MORE secure than OpenClaw (with honest residuals)

- **Containers for all** (vs OpenClaw `sandbox.mode=off`) — residual: shared
  kernel; a runtime escape defeats it (hence microVM for the browser).
- **Default-deny egress + DNS** — residual: exfil survives through *allowed*
  channels (model prompts, approved recipients). "Narrows," not "severs."
- **Credential brokering** — residual: token is a live in-box credential;
  stops theft, not misuse.
- **No in-process plugins** — residual: moves third-party code from host to
  container trust (install_packages / MCP), does not eliminate it.
- **Injection defenses** — residual: no control makes an LLM injection-proof;
  the trifecta is bounded, never eliminated.

### Won't chase (deliberate non-goals)

Runtime plugin/ClawHub registry (imports the supply-chain breach class we
avoid by construction); Baileys QR WhatsApp; telephony + device-node
companion apps; voice wake / always-listening; Canvas/A2UI; any
`sandbox.mode=off`; full interactive browser (5b is demand-pull).

### Sequencing

| Phase | Surface delta | Effort | Gated by |
|---|---|---|---|
| 0a egress+DNS+SSRF | REDUCES | M-L | — |
| 0b credential broker | REDUCES | M-L | 0a |
| 0c default-deny + redaction | REDUCES | S-M | — |
| 1 tool auth + loop breakers | REDUCES | M | — (parallel w/ 0) |
| 2 approvals + pairing + mentions | REDUCES | M | — (parallel w/ 1) |
| 3 searchable memory + provenance | EXPANDS | L+M | 0,1,2 |
| 4 provider fallback/multi-key | neutral | M | — |
| 5a browser read-only | EXPANDS | committed | 0,1 |
| 5b browser interactive | EXPANDS | stretch | 0,1,3 |
| 6 security-audit/MCP/attestation | REDUCES | S-M | — |
| 7 more channels | neutral | demand-pull | — |

Note: OpenClaw threat-intel figures (CVE dates, "~341 malicious skills",
"~7.1% leak secrets") came from the research dossier and are **unverified**;
do not cite externally without confirmation.
