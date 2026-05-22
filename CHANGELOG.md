# Changelog

All notable changes to Ironclaw are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added (Test (structural): every runner-emitted action has a handler)

- **`crates/ironclaw-host/tests/action_handler_coverage.rs`** — new
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
  set from `crates/ironclaw-runner/src/tools.rs` (`fn apply_*`
  bodies) and `crates/ironclaw-runner/src/run.rs`
  (`fn emit_usage_report` body) via a brace-matching parser +
  regex over `serde_json::json!({ "<name>": …`; asserts no drift
  from the hard-coded list in (1).
  (3) `host_handle_set_matches_inline_arms` scans
  `crates/ironclaw-host-delivery/src/service.rs` for every
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
  `install_modules` vec in `crates/ironclaw-host/src/boot.rs`.

### Added (skill ↔ tool coverage tests + `skills/README.md` conventions)

- **`crates/ironclaw-skills/tests/coverage.rs`** — new integration test
  file pinning the `tools ↔ skills` matrix. Nine tests:
  (1) every `skills/<dirname>/SKILL.md` has frontmatter `name:` equal
  to `<dirname>`; (2) every tool returned by
  `ironclaw_mcp::tools::build_tool_set` is mentioned in at least one
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

- **`crates/ironclaw-providers/src/ollama.rs`** — replaced the
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
- **`crates/ironclaw-providers/tests/ollama_conformance.rs`** — new,
  12 wiremock conformance tests covering every `ProviderEvent`
  emission path on the native code path (text, tool round-trip,
  streaming heartbeats, abort, usage, model passthrough, tool schema
  translation, tool-result history translation, system prompt
  placement, error classification, empty body, malformed JSON
  recovery).
- **`crates/ironclaw-providers/tests/ollama_live.rs`** — new,
  `#[ignore]`d live test against a real Ollama server. Reads
  `OLLAMA_HOST` (default `http://localhost:11434`) and `OLLAMA_MODEL`
  (default `llama3.1:8b`); run with
  `cargo test --ignored ollama_live -p ironclaw-providers`.
- **`crates/ironclaw-providers/tests/ollama_shim.rs`** — renamed from
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
- `crates/ironclaw-channels/telegram/src/adapter.rs` — 3 new
  adapter-level edge tests: rate-limit retry-after,
  malformed-response-body → Transport, non-object content → BadRequest.
- `crates/ironclaw-channels/slack/src/adapter.rs` — 3 new
  adapter-level edge tests: empty text still posts, non-object content
  as empty text, 429 Retry-After → AdapterError::Rate.
- `crates/ironclaw-channels/discord/src/adapter.rs` — 3 new
  adapter-level edge tests: empty content object still posts,
  non-object content renders as JSON, 429 Retry-After → AdapterError::Rate.

### Fixed (scheduling: persist tasks and fire due ones from the sweep loop)

- **`crates/ironclaw-modules/src/scheduling.rs`** — `SchedulingModule::install`
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
- **`crates/ironclaw-db/migrations/010_tasks.sql`** — new `tasks` table
  on the central DB. Columns: `id` (server-generated `task_<uuid>`),
  `agent_group_id`, `session_id`, `name`, `prompt`, `when_spec`,
  `recurrence`, `next_fire`, `status`
  (`active`/`paused`/`cancelled`/`completed`), `created_at`, `updated_at`.
- **`crates/ironclaw-db/src/tables/tasks.rs`** — CRUD module for the
  new table: `insert`, `get`, `list_for_session`, `list_due`,
  `set_status`, `set_next_fire`, `update`.
- **`crates/ironclaw-host-sweep/src/checks/scheduling.rs`** — new sweep
  check called once per pass. For every `active` task with
  `next_fire <= now`, the check synthesises a `kind: task`, `on_wake: true`
  inbound row into the originating session's `inbound.db` and either
  re-arms (recurring tasks bump `next_fire` to the next occurrence) or
  transitions to `completed` (one-shot tasks clear `next_fire`). The
  existing `wake.rs` check then picks up the new pending row and walks
  the container back to `running`.
- **`crates/ironclaw-host-sweep/src/task_store.rs`** — the sqlite-backed
  `SqliteTaskStore` impl of `TaskStore`. Lives in the sweep crate so the
  modules crate stays decoupled from `ironclaw-db`.
- **`crates/ironclaw-host/src/boot.rs`** — boot now constructs
  `SqliteTaskStore::new(host_ctx.central().clone())` and passes it
  through `SchedulingModule::with_store(...)` so created tasks land in
  the same `tasks` table the sweep scans.
- **`crates/ironclaw-modules/src/context.rs`** — `DeliveryActionInput`
  gains `session_id: Option<SessionId>` and `DispatchTarget` derives
  `Default`. The host's delivery service populates both for system
  actions so the `ScheduleHandler` can identify the originating session.
  Existing handlers (`approval_card`, `ask_user_question`, `send_card`)
  ignore the new field.

### Added (modules: wire the `create_agent` delivery action)

- **`crates/ironclaw-modules/src/agent_to_agent.rs`** — the
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
- **`crates/ironclaw-modules/src/lib.rs`** — re-exports
  `CreateAgentHandler`, `CreateAgentPermissionCheck`,
  `create_agent_always_allow`, `create_agent_always_deny` for host
  wiring + tests.
- **`crates/ironclaw-modules/Cargo.toml`** — adds `ironclaw-db` as a
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

- **`crates/ironclaw-channels/core/src/adapter.rs`** — `ChannelAdapter`
  gains two default-`Unsupported` trait methods, `edit_message` and
  `add_reaction`, so adapters that don't expose those APIs fall
  through cleanly to the host's fallback path.
- **`crates/ironclaw-channels/telegram/src/adapter.rs`** plus
  **`crates/ironclaw-channels/telegram/src/api.rs`** — implements the
  trait against Telegram's `editMessageText` and `setMessageReaction`
  endpoints.
- **`crates/ironclaw-channels/slack/src/adapter.rs`** — implements the
  trait against Slack's `chat.update` and `reactions.add` (strips
  surrounding `:` from the emoji name before forwarding).
- **`crates/ironclaw-channels/discord/src/adapter.rs`** — implements
  the trait against Discord's `PATCH /channels/{id}/messages/{msg}`
  and `PUT /channels/{id}/messages/{msg}/reactions/{emoji}/@me`.
- **`crates/ironclaw-channels/core/src/testing.rs`** — `MockAdapter`
  records `edit_message` / `add_reaction` calls and exposes
  `set_edit_unsupported` / `set_reaction_unsupported` knobs so tests
  can drive the host's fallback path.
- **`crates/ironclaw-modules/src/interactive.rs`** — `InteractiveModule`
  now registers `edit` and `reaction` delivery-action handlers. They
  emit a synthetic chat message of the form `"(edit) <text>"` /
  `"(reaction: <emoji>)"`; the host invokes them only when the
  adapter call falls through.
- **`crates/ironclaw-host-delivery/src/service.rs`** — the
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

- **`crates/ironclaw-host-delivery/src/service.rs`** — the
  `install_packages` and `add_mcp_server` system-action handlers no
  longer mark a row `delivered.status="ok"` after the underlying
  `container_configs` update failed. On apply error the row is now
  recorded as `delivered.status="failed"` with the error message in
  the payload (so it surfaces in `iclaw dropped-messages outbound-list`),
  the failure is logged at `error!` (not `warn!`), and a
  `MessageKind::System` row carrying a `self_mod_error` envelope is
  written to the session's `inbound.db` so the agent learns its tool
  call failed and can adapt on the next turn. Without this, the
  agent would loop thinking its install succeeded while the next
  container spawn silently lacked the package.
- New metric counters
  `ironclaw_self_mod_failed_total{action}` and
  `ironclaw_self_mod_succeeded_total{action}` (`action` ∈
  `{install_packages, add_mcp_server}`) — fired on every self-mod
  apply outcome so operators can chart the failure rate.
- New env var `IRONCLAW_SELFMOD_HARD_FAIL=1` flips failed applies
  into a non-retryable `DeliveryError::SystemAction` so the outer
  delivery loop records the row in `dropped-messages` instead of
  handling the failure inline. Default off; useful for tests + paranoid
  operators that want the message in the failed-deliveries view.
- **`crates/ironclaw-metrics/src/lib.rs`** — new
  `inc_self_mod_failed(action)` / `inc_self_mod_succeeded(action)`
  helpers + `SELF_MOD_FAILED_TOTAL` / `SELF_MOD_SUCCEEDED_TOTAL`
  name constants, following the existing pattern.

### Fixed (runner: route chat outbounds back to the originating channel)

- **`crates/ironclaw-runner/src/tools.rs`** and
  **`crates/ironclaw-runner/src/run.rs`** — when the model emits a
  reply (final assistant text or an explicit `send_message` /
  `send_file` with `to: None`), the `messages_out` row's
  `channel_type` / `platform_id` / `thread_id` / `in_reply_to`
  columns now carry the originating inbound's routing. Before this
  fix those columns were always written as `NULL`, so the host's
  delivery loop had nothing to dispatch by — the model replied
  correctly but the user saw silence. **Live-caught on Telegram**:
  every successful turn produced a chat outbound with empty routing
  and the user got nothing.
- **`crates/ironclaw-mcp/src/context.rs`** — the `ToolContext`
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

### Fixed (rebuild.sh: don't let `ironclaw-setup --headless` wipe channel config from .env)

- **`rebuild.sh`** — the image-rebake step invokes the full
  `ironclaw-setup --headless` wizard, which rewrites `.env` from
  scratch with only the keys it knows about (`ANTHROPIC_API_KEY`,
  `IRONCLAW_DATA_DIR`, `IRONCLAW_DEFAULT_IMAGE_TAG`, etc.) — silently
  dropping channel-specific keys (`TELEGRAM_BOT_TOKEN`,
  `IRONCLAW_CHANNELS`, `IRONCLAW_CHANNELS_CONFIG`) and third-party
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
  `ironclaw-setup image` subcommand that runs ONLY the image build
  without touching `.env` — filed for a follow-up.

### Fixed (recover from malformed tool_use JSON by feeding the parse error back to the model)

- **`crates/ironclaw-types/src/provider.rs`** — new
  `ProviderEvent::ToolInputParseError { tool_use_id, tool_name, raw_input, parse_error }`
  variant. Emitted by the provider when a `tool_use` content block's
  reassembled `input_json_delta` chunks fail to parse as JSON. Carries
  enough metadata for the runner to synthesise a corrective
  `tool_result` keyed by `tool_use_id`.
- **`crates/ironclaw-providers/src/anthropic.rs`** — on a `tool_use`
  input JSON parse failure (the live-caught `send_file` "EOF while
  parsing an object at line 1 column 37" case), the SSE pump now
  emits `ProviderEvent::ToolInputParseError` followed by
  `ProviderEvent::ToolEnd` instead of a terminal
  `ProviderEvent::Error`. The previous behaviour terminated the
  inbound with only the generic apology row reaching the user.
- **`crates/ironclaw-runner/src/run.rs`** — `pump_events` converts
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
- **`crates/ironclaw-runner/src/subagent.rs`** — exhaustive-match arm
  added for the new variant. Subagent turns are single-shot, so the
  parse-error path bails the subagent turn (the parent runner is
  where the self-correction loop lives).
- **Tests** — four new tests in `crates/ironclaw-runner/src/run.rs`:
  `malformed_tool_use_recovers_after_one_retry`,
  `malformed_tool_use_gives_up_after_three_attempts`,
  `malformed_tool_use_other_tools_still_work`, and
  `tool_input_parse_error_event_serialization`. Workspace total goes
  from 4,898 → 4,902 passing.

### Added (delivery: plain-text fallback retry for formatting BadRequests)

- **`crates/ironclaw-channels/core/src/adapter.rs`** — new
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
- **`crates/ironclaw-host-delivery/src/service.rs`** — `call_adapter` now
  inspects `AdapterError::BadRequest(msg)` for a formatting-error
  signature (`parse entities`, `rich text`, `blocks`, `block_kit`,
  `block kit`, `embed`, `embeds`, `format`, `formatting`; case-
  insensitive) via `is_formatting_bad_request`. When matched it calls
  `adapter.plain_text_fallback(message)` and re-issues `deliver` with
  the result. If the fallback succeeds the row is recorded as
  delivered, an info-level "delivered with reduced formatting" log
  line fires, and the new metric
  `ironclaw_delivery_formatting_fallback_total{channel_type}` is
  incremented. If the fallback fails (or the adapter has no
  fallback), the ORIGINAL `BadRequest` is surfaced and the existing
  terminal-failure path takes over — non-formatting BadRequests
  (e.g. "chat_id required") fail fast without a retry.
- **Per-channel `plain_text_fallback` impls** in:
  - `crates/ironclaw-channels/telegram/src/adapter.rs` — strips
    `parse_mode`, keeps `text`. Fixes the regression where the agent
    opting into `parse_mode=MarkdownV2` and emitting natural-language
    text with bare `!` / `.` / `-` / `(` / `)` / `[` / `]` would hit
    Telegram's 400 "can't parse entities" and the user got nothing.
  - `crates/ironclaw-channels/slack/src/adapter.rs` — strips
    `blocks`, keeps the `text` fallback string Slack already requires
    on `chat.postMessage`.
  - `crates/ironclaw-channels/discord/src/adapter.rs` — strips
    `embeds`, keeps `text`.
- **`crates/ironclaw-metrics/src/lib.rs`** — adds
  `DELIVERY_FORMATTING_FALLBACK_TOTAL` constant and
  `inc_delivery_formatting_fallback(channel_type)` helper, alongside
  the existing `inc_delivery_failed`. Surfaced in the metric-name
  prefix / ends-with-`_total` invariants so an operator scraping
  `/metrics` can alert on "delivered but downgraded".
- **`crates/ironclaw-channels/core/src/testing.rs`** — `MockAdapter`
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

- **`crates/ironclaw-host-sweep/src/checks/apology.rs`** — new sweep
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
- **`crates/ironclaw-host-sweep/src/spawn_tracker.rs`** — new in-memory
  `SpawnAttemptTracker` shared between the host's container manager and
  the sweep. The manager calls `record_failure(session_id)` on every
  failed `runtime.spawn(...)` and `record_success(session_id)` on a
  successful spawn. The sweep's apology check reads
  `is_exhausted(session_id)` (>= `SPAWN_FAIL_THRESHOLD = 3` attempts)
  combined with `container_status='stopped'` to fire the
  `reason=container_spawn_failed` branch — which emits the apology even
  for inbounds under the 5-min age threshold, because if the container
  can't come up at all the user shouldn't have to wait 5 min.
- **`crates/ironclaw-metrics/src/lib.rs`** — new counter
  `ironclaw_stuck_inbound_apology_total{agent_group_id, reason}` with
  reason ∈ {`pending_too_long`, `container_spawn_failed`}. Operators
  can alert on it spiking to detect a container that flat-out won't
  start (image corruption, OCI error, OOM at launch).
- **`crates/ironclaw-host/src/container_manager.rs`** — `maybe_spawn`
  now bumps the spawn-attempt tracker on every `runtime.spawn` failure
  and clears it on success. The shared `Arc<SpawnAttemptTracker>` is
  threaded through `with_spawn_tracker(...)` from `boot.rs`, where the
  same tracker is also handed to the sweep service.
- **`crates/ironclaw-host-sweep/src/lib.rs`** — exposes
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

- **`crates/ironclaw-host/src/image_health.rs`** — new module that
  inspects the configured `IRONCLAW_DEFAULT_IMAGE_TAG` at boot
  before the container manager starts. Three checks:
  1. **Image exists locally** — `docker image inspect <tag>`. A
     missing image is what happens when an operator runs the host
     binaries (e.g. via systemd) without first running
     `./rebuild.sh` to refresh the session image. This is the
     bug-class the change closes.
  2. **Runner binary present + executable** — one-shot
     `docker run --rm --entrypoint /bin/ls <tag> -l /usr/local/bin/ironclaw-runner`
     bounded by a 5 s per-call timeout and `kill_on_drop(true)` so
     a wedged daemon can't monopolise boot.
  3. **Fingerprint compare** — reads the image's
     `ironclaw.fingerprint` label (set by `ironclaw-setup`) and
     compares it to the sha256 of the host's runner binary. A
     mismatch is a WARN, **not** a degrade — fingerprints can
     legitimately differ across architectures and build flavours,
     so we only flag the suspicion.
  The whole pipeline is bounded by an outer 10 s `tokio::time::timeout`.
- **`crates/ironclaw-host/src/boot.rs::run_boot_image_health_check`**
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
- **`crates/ironclaw-host/src/container_manager.rs`** — new
  `ManagerError::HostDegraded` variant; `maybe_spawn` short-circuits
  with it when degraded; the reconcile loop swallows the error so
  the host log isn't spammed every tick.
- **`crates/ironclaw-metrics/src/lib.rs`** — new
  `ironclaw_degraded_state{reason}` Prometheus gauge with five label
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
  binaries. Previously a code change to `ironclaw-runner` landed on
  disk but the agent inside the container kept running the old runner
  baked into the stale image, so new tools / new fixes never reached
  the live agent. Caught live: model kept hitting the `send_file`
  malformed-JSON tic on the old image's old runner, with no apology
  emit because that fix only existed in the on-disk-but-unbaked
  binary. The script now triggers `ironclaw-setup --headless` after
  install (with `image` cleared from `setup-state.json`'s completed
  list), reads the resulting image tag, and rewrites
  `IRONCLAW_DEFAULT_IMAGE_TAG` so the next session spawn picks it up.
- **`rebuild.sh` install list** now includes `ironclaw-runner` so
  the binary the image step bakes in is current.
- **`CLAUDE.md`** — documents the new step in the "Local development
  loop" section.

### Changed (web_fetch: auto-convert HTML responses to markdown)

- **`crates/ironclaw-mcp/src/tools/computer_use.rs`** — `web_fetch`
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
- **`crates/ironclaw-mcp/Cargo.toml`** — adds `htmd = "0.2"`. Pinned
  to 0.2 because 0.3+ require Rust 1.88's let-chains feature and the
  workspace pins 1.85. License is Apache-2.0, MIT-compatible.
- Four wiremock-backed tests pin the new behaviour:
  HTML-with-charset-param converts, plain JSON passes through, the
  `raw` flag suppresses conversion, and a Content-Type unit test
  covers the parser permutations.

### Changed (shell: persist working directory and env vars across calls)

- **`crates/ironclaw-mcp/src/tools/computer_use.rs`** — environment
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

- **`crates/ironclaw-mcp/src/tools/edit_file.rs`** — new in-process
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
- **`crates/ironclaw-mcp/src/tools/mod.rs`** — registers
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

- **`crates/ironclaw-mcp/src/tools/grep.rs`** — new in-process tool
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
- **`crates/ironclaw-mcp/src/tools/glob.rs`** — companion tool that
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

- **`crates/ironclaw-mcp/src/tools/git_status.rs`,
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
- **`crates/ironclaw-mcp/src/tools/git_common.rs`** — shared
  repository discovery, path resolution, libgit2 error wrapping,
  and short-OID / RFC 3339 helpers so the four tools render
  errors identically.
- **`crates/ironclaw-mcp/src/tools/mod.rs`** — registers all
  four entries in `build_tool_set()`. The crate's smoke test in
  `lib.rs` notes git tools test themselves (they need an on-disk
  repo the smoke harness doesn't stand up).
- **`skills/git/SKILL.md`** — one combined skill covering when
  to reach for each of the four tools, common patterns ("what
  changed in the last hour", "who wrote this function", "is the
  working tree clean"), and the explicit "these are read-only;
  hand mutations back to the operator" reminder.
- **`crates/ironclaw-mcp/Cargo.toml`** — pins `git2 = "0.19"`
  with `default-features = false, features = ["vendored-libgit2"]`
  so the build is self-contained (cmake + cc pulled in at
  compile time only; the resulting binary statically links
  libgit2). Workspace clippy stays clean at `-D warnings`; 23
  new unit tests cover every tool's happy path, validation
  errors, range clamping, truncation, empty-repo handling, and
  ref-not-found.

### Added (agent tools: `explore` — lightweight in-process subagent)

- **`crates/ironclaw-mcp/src/tools/explore.rs`** — new `explore` tool
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
  `crates/ironclaw-mcp/src/lib.rs::smoke` and the order pin in
  `crates/ironclaw-mcp/src/server.rs::tests` are updated accordingly.
- **`crates/ironclaw-mcp/src/context.rs`** — adds `SubagentRequest`,
  `SubagentResult`, `SubagentToolCall` types, plus a new
  `ToolContext::spawn_subagent` trait method with a default impl that
  returns `ToolError::Context("subagent not supported in this
  context")`. `MockToolContext` records subagent calls and returns
  canned results so the `explore` tool's unit tests stay
  transport-free.
- **`crates/ironclaw-runner/src/subagent.rs`** — new module containing
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
- **`crates/ironclaw-runner/src/tools.rs`** — `RunnerToolCtx` gains
  optional `SubagentRunnerDeps` (provider + tool_map + model + system
  prompt + per-turn max_tokens + provider deadline) wired in via a
  new `with_subagent(...)` builder method. `spawn_subagent` flips a
  re-entrancy guard so a subagent's own tool calls can write to
  `outbound.db` but can never recurse into another full subagent
  loop. The subagent's `ToolContext` is a fresh `SubagentCtxAdapter`
  whose `spawn_subagent` impl unconditionally refuses, giving us
  defense-in-depth against the nested case.
- **`crates/ironclaw-runner/src/main.rs`** — populates the
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

- **`crates/ironclaw-runner/src/run.rs`** — `finalize_messages` now
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
  `IRONCLAW_SKILLS_DIR` defaults to `<install_root>/data/skills`
  but setup never copied the repo's skills into that path. Result:
  the running session had an EMPTY system prompt (verified:
  `runner.json:system` was `""`), every skill we'd authored was
  invisible to the agent, and the identity skill in particular
  didn't fire when the user asked "what is Ironclaw?" — the model
  pulled from training data and described a tabletop RPG.
- **`CLAUDE.md`** — documents the symlink + the gotcha for the
  next contributor.

### Fixed (container rebuild: preserve runner binary)

- **`crates/ironclaw-host/src/container_manager.rs`** —
  `rebuild_image` now bases per-group image rebuilds on the install's
  `default_image_tag` (which has `/usr/local/bin/ironclaw-runner`
  baked in at setup time) instead of bare `debian:trixie-slim`. The
  rebuild Dockerfile only adds layers (apt / npm / labels); it never
  re-COPIES the runner binary. Caught live: agent on this box
  emitted `install_packages` for `git`/`nodejs`/`npm`, the host's
  M13 auto-apply flow triggered a rebuild against debian-slim, the
  resulting image had apt packages but no runner, and every
  subsequent `runc create` failed with `stat
  /usr/local/bin/ironclaw-runner: no such file or directory`. New
  `resolve_rebuild_base()` helper picks the default tag when set,
  falls back to `debian:trixie-slim` only when default is empty
  (tests). Two regression tests:
  `rebuild_base_prefers_default_image_tag` and
  `rebuild_base_falls_back_when_default_unset`.

### Added (skill: agent identity)

- **`skills/identity/SKILL.md`** — auto-loads into every agent's
  system prompt and teaches the agent that it's an Ironclaw agent.
  Previously the agent answered "who are you?" with the model's
  generic Claude-or-AI-assistant intro, denying any connection to
  Ironclaw (caught live: agent told a user "I'm not Ironclaw — I'm
  an AI assistant"). The skill names the system, describes the
  per-session container runtime + channel brokering, and includes
  three example phrasings to anchor the answer.

### Fixed (setup: telegram channel now ships fully wired)

- **`crates/ironclaw-setup/src/steps/quickstart_group.rs`** —
  `quickstart_group` now handles `first_channel = telegram` (previously
  only `cli`).  Closes the live gap I hit on this box: after the
  channel step persisted `TELEGRAM_BOT_TOKEN`, I still had to manually
  (a) add `IRONCLAW_CHANNELS=cli,telegram` to `.env`, (b) add
  `IRONCLAW_CHANNELS_CONFIG='{"telegram":{"bot_token":"...","mode":"long_poll"}}'`
  (single-quoted so dotenvy parses it), (c) `iclaw messaging-groups
  create --channel-type telegram --platform-id <chat_id>`, (d)
  `iclaw wirings create --mg ... --ag ... --engage pattern --pattern '.*'`,
  and (e) `iclaw approvals approve --channel telegram --identity <chat_id>`.
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

- **`crates/ironclaw-providers/src/anthropic.rs`** — SSE
  transport/decode failures are now tagged `retryable: true` (was
  `false`). These almost always represent a dropped connection or
  malformed chunk mid-stream, not a fundamental upstream problem.
- **`crates/ironclaw-runner/src/run.rs`** — `run_llm_turn` now wraps
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

- **`crates/ironclaw-channels/telegram/src/adapter.rs`** — `DEFAULT_PARSE_MODE`
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

- **`crates/ironclaw-db/src/tables/pending_sender_approvals.rs`** and
  the `pending_sender_approvals` table from migration `001_initial.sql`
  are gone. The CRUD module shipped with full schema + insert/select +
  12 unit tests but no host code ever called it. The real
  sender-approval flow uses `unregistered_senders` (audit / dedup) and
  `users` (the approved-sender truth set): the router writes the
  unregistered row on every unknown-sender inbound, the approvals
  module's host-side notifier reads it for dedup before posting the
  in-channel "approve this sender?" prompt, and
  `iclaw approvals approve_sender` upserts into `users`. With no
  release yet on the `001_initial` schema the table is removed in
  place rather than via an additional drop migration. Doc strings in
  `crates/ironclaw-modules/src/{approvals.rs,context.rs}` and
  `skills/approvals/SKILL.md` updated to point at the real table.

### Added (runner: provider retry loop + per-call deadline)

- **`crates/ironclaw-runner/src/run.rs`** — `provider.query()` is now
  wrapped in an exponential-backoff retry loop with a per-attempt
  deadline. The new helper `query_with_retry()` honours
  `ProviderError::is_retryable()` (5xx, transport, overload retry; 4xx
  and `SessionInvalid` fail-fast), retries up to
  `MAX_PROVIDER_ATTEMPTS = 3` times with 250ms → 500ms → 1s backoffs,
  and wraps each attempt in `tokio::time::timeout(provider_deadline,
  ...)`. Terminal failures mark the inbound `status='failed'` via the
  existing `finalize_messages` path; the runner never panics.
- **`crates/ironclaw-runner/src/run.rs`** — new `provider_deadline`
  field on `RunnerDeps`, defaulting to
  `DEFAULT_PROVIDER_DEADLINE_MS = 60_000`. Configurable per-process via
  the new env var `IRONCLAW_RUNNER_PROVIDER_DEADLINE_MS` (clamped to
  the `[30_000, 300_000]` ms range; out-of-range values warn and fall
  back to the default). `resolve_provider_deadline(env)` is re-exported
  from the crate root so the runner binary picks it up at startup.
- **`crates/ironclaw-providers/src/error.rs`** — new
  `ProviderError::DeadlineExceeded { deadline_ms, attempts }` variant
  emitted by the runner once all retries trip the per-call deadline.
  Non-retryable; carries the deadline and attempt count so log scrapers
  can spot flapping upstreams.
- **`crates/ironclaw-metrics/src/lib.rs`** — two new counters:
  `ironclaw_provider_retry_total{provider}` (fires once per retry
  decision) and `ironclaw_provider_deadline_total{provider}` (fires
  when the retry budget is exhausted by deadline trips).
- **`crates/ironclaw-host/tests/replay.rs`** — un-`#[ignore]`d
  `cli_provider_5xx_retry` and `cli_provider_timeout`; both pass
  against the new runner behaviour. The harness sets a short
  `provider_deadline` (200ms) so the timeout fixture finishes in well
  under a second.
- **`fixtures/cli/provider-timeout/manifest.json`** — updated to mount
  three `kind=timeout` mocks (one per retry attempt) and bumped
  `step_timeout_ms` to 10s to accommodate the worst-case retry budget.

### Added (budget-gate Prometheus counters)

- **`ironclaw_budget_exhausted_total{agent_group_id, gate}`** — fired by
  `ContainerManager::maybe_spawn` every time the budget or rate-limit
  gate refuses to spawn. `gate` is one of `daily_tokens`,
  `turns_per_minute`, `turns_per_hour`. Operators can now alert on
  "budget exhausted spike" with
  `sum by (agent_group_id, gate) (rate(ironclaw_budget_exhausted_total[15m])) > 0`
  instead of grepping logs.
- **`ironclaw_budget_exhausted_replies_total{agent_group_id}`** — fired
  when the in-channel "budget exhausted" notice is actually written to
  outbound (i.e. AFTER the per-group dedup window check).
- **`ironclaw_budget_exhausted_suppressed_total{agent_group_id}`** —
  fired when a refusal notice is suppressed by the per-group dedup
  window. Pair with the replies counter to see the user-visible
  notification rate independent of refusal volume.
- The three counters land on the existing `IRONCLAW_METRICS_ADDR`
  endpoint automatically — no new opt-in. `docs/observability.md` and
  the README counter list were updated. New helpers
  `ironclaw_metrics::inc_budget_exhausted{,_reply,_suppressed}` and the
  `BUDGET_GATE_*` label constants are added without changing any
  existing public symbols in `ironclaw-metrics`.

### Added (replay-fixture coverage for tool-use loop)

- **`fixtures/cli/tool-use-shell/`** — new replay fixture that drives
  one CLI inbound (`run 'echo hello'`) through the runner's tool-use
  outer loop. Two Claude turns: turn 1 is a `tool_use` content block
  requesting the `shell` tool with `command: "echo hello"`; the runner
  executes real bash, feeds the `tool_result` back; turn 2 streams the
  final assistant text. Asserts the full inbound → router → runner →
  outbound → delivery pipeline still completes when the model uses a
  tool mid-turn. Backed by `cli_tool_use_shell` in
  `crates/ironclaw-host/tests/replay.rs`. No harness changes were
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
- **`crates/ironclaw-host/tests/replay/fixture.rs`** — new optional
  `provider_responses` array on the fixture manifest. Each entry is one
  scripted response: `{"kind": "success", "file": "001-turn.json"}`,
  `{"kind": "error", "status": 503}`, or
  `{"kind": "timeout", "delay_ms": 60000}`. When absent, the harness
  keeps the legacy "i-th `claude/NNN-turn.json` for the i-th request"
  behaviour, so existing fixtures stay untouched.
- **`crates/ironclaw-host/tests/replay/harness.rs`** — honours the new
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
- **`crates/ironclaw-host/tests/replay/harness.rs`** — extends the
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

- **`crates/ironclaw-host/tests/e2e_chat.rs`** — boots
  `ironclaw_host::run_host` in-process against a tempdir install root,
  mounts a `wiremock` Anthropic-flavoured streaming stub, writes
  `"hello\n"` into the cli channel's real FIFO, and asserts the mocked
  reply (`"hi from the mock"`) appears in `<install_root>/chat.log`.
  The host's container manager is left disabled and an in-process
  runner driver (mirroring `replay/harness.rs`'s seam) processes
  inbound for each new session, so the test runs without Docker or
  network access. A second smaller test drives `iclaw chat
  --no-autostart` via `ironclaw_iclaw::run_cli` against a missing
  FIFO and asserts the friendly "run `ironclaw start`" hint. This
  pair is the gate that would have caught the FIFO-vs-stdin wiring
  bug that motivated M11.

### Added (setup wizard e2e harness)

- **End-to-end wizard integration test** at
  `crates/ironclaw-setup/tests/wizard_e2e.rs`. Drives the full step
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
  Mirrors `ironclaw_host::boot::check_schema_version`: if the on-disk
  `schema_version` table reports more applied migrations than
  `expected_central_schema_version()`, the step returns an error
  rather than silently running migrations against a DB that was
  migrated by a newer binary. This protects operators who try to
  downgrade ironclaw without restoring from a backup.

### Added (install.sh integration test)

- **Containerised integration test for `install.sh`** at
  `tests/install/test_install_sh.sh`.  Spins up a clean Ubuntu 24.04
  container, mounts the repo read-only, and drives the installer
  through four scenarios: (1) missing-Docker clean-failure path,
  (2) full binary install via `cargo install --path` (opt-in via
  `IRONCLAW_INSTALL_TEST_RUN_BUILD=1`; default-skipped because it
  adds ~5 minutes), (3) re-run idempotency — pre-existing binaries
  survive a dry-run re-invocation, (4) platform detection across all
  four supported triples plus an explicit `IRONCLAW_RELEASE_TAG`.
  Default suite runtime: ~3 s after the image is cached.
- New CI job `install-sh` in `.github/workflows/ci.yml` runs the
  suite on `ubuntu-latest` and shellchecks both files, with a
  path-filter (`install.sh`, `tests/install/**`, the workflow
  itself) so the job is skipped on unrelated PRs.
- Three test-only escape hatches added to `install.sh`,
  default-off and silent unless explicitly set:
  `INSTALL_SH_SKIP_DOCKER_CHECK=1` skips the container-runtime
  check; `IRONCLAW_INSTALL_DRY_RUN=1` prints the tarball URL the
  installer would fetch and exits 0; `IRONCLAW_FORCE_TARGET=<triple>`
  overrides platform detection for the URL test.

### Added (replay fixture coverage — round 2)

- **Four new replay fixtures** under `fixtures/`, lifting in-tree
  coverage from 3 channel types to 7:
  `discord/inbound-message/` (Discord guild-channel message),
  `matrix/room-message/` (Matrix `m.room.message` `m.text`),
  `github/webhook-issue-comment/` (GitHub `issue_comment.created`),
  and `webhooks/generic-hmac/` (generic HMAC-signed webhook, e.g.
  Grafana / Stripe / Sentry style). Each runs through the existing
  in-process `ReplayHarness` in `crates/ironclaw-host/tests/replay.rs`
  via four new `#[tokio::test]` entries, exercising the inbound ->
  router -> runner -> outbound -> delivery pipeline for those channel
  types against the harness's per-channel-type `MockAdapter`s.

### Added (replay fixture coverage)

- **Three new replay fixtures** under `fixtures/`:
  `telegram/inbound-text-message/`, `slack/event-message/`, and
  `cli/multi-turn/`. Each runs through the existing in-process
  `ReplayHarness` in `crates/ironclaw-host/tests/replay.rs`. The
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
  now a one-line `#[tokio::test]` in `crates/ironclaw-host/tests/replay.rs`.

### Fixed (cli channel bridge)

- **`iclaw chat` now actually reaches the host.** The cli channel
  adapter previously read from the host process's own `tokio::io::stdin()`
  and wrote outbound replies to `tokio::io::stdout()` — so messages
  typed into `iclaw chat` (which wrote to `<install_root>/chat.fifo`)
  were never picked up, and replies were never appended to
  `<install_root>/chat.log` for the chat tailing loop to see. The
  adapter gains a FIFO/log mode: when `IRONCLAW_CLI_FIFO` and/or
  `IRONCLAW_CLI_LOG` are set (or defaulted from `IRONCLAW_DATA_DIR`'s
  parent), the cli channel opens the FIFO with `O_RDWR | O_NONBLOCK`
  via `tokio::net::unix::pipe::Receiver` and appends outbound to the
  log, flushing each line. The `O_RDWR` open is the standard
  "reader is its own writer" trick that keeps the pipe alive across
  external-writer disconnects (Ctrl-D in one `iclaw chat` no longer
  EOFs the host's read side). With no paths configured the adapter
  still falls back to stdin/stdout for the developer REPL.
- **Setup wires the bridge by default.** `ironclaw-setup`'s
  `quickstart_group` step now also `mkfifo`s `chat.fifo` (0600),
  touches `chat.log` (0600), and writes `IRONCLAW_CLI_FIFO` and
  `IRONCLAW_CLI_LOG` lines into the install's `.env` so the host
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
  `workflow_dispatch` for smoke tests). Builds `ironclaw`, `iclaw`,
  and `ironclaw-setup` in parallel for four targets
  (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
  `x86_64-apple-darwin`, `aarch64-apple-darwin`), strips each
  binary, packages one `ironclaw-<target>.tar.gz` per target with
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
  metric `ironclaw_secrets_rotated_total`. Running containers see
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
  `iclaw budgets set --turns-per-minute N --turns-per-hour N`.
- **Versioned migrations.** New `expected_central_schema_version()`
  and `applied_central_schema_version()` helpers in
  `ironclaw-db::migrate`. Boot now refuses to start with
  `BootError::SchemaMismatch` (exit code 5) when the on-disk
  schema is newer than this binary expects (downgrade detection).
  New `iclaw schema-version` subcommand prints `{expected, applied,
  status}` as JSON.
- **`sessions/sessions/` path cleanup.** `HostConfig::sessions_root()`
  now returns `data_dir` directly; the double-`sessions/` layout
  is gone. New `migrate_sessions_layout()` runs at boot, moving
  contents from `data_dir/sessions/sessions/<ag>/<sess>/` up one
  level when present. Collisions log a warn and skip; the inner
  directory is only removed when all entries moved successfully.

### Added (onboarding polish slice)

- `iclaw doctor` — first-run / ongoing health probe. Walks the
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
  wiring directly to the central DB so `iclaw chat` works on the
  very first `ironclaw run`. Idempotent (skips when any agent group
  already exists). Opt out with `IRONCLAW_SETUP_QUICKSTART=no` or
  decline the interactive prompt. Override the slug with
  `IRONCLAW_SETUP_QUICKSTART_NAME`. The `first_chat` step's
  "what to do next" output flips to recommend `iclaw chat`
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
- Provider resolution: explicit `provider` arg → `IRONCLAW_WEB_SEARCH_PROVIDER`
  env → auto-detect from configured keys in order
  `tavily, exa, brave, serpapi`. No keys configured surfaces a
  validation error naming all four env vars (errors over silent
  fallback).
- Host's `ContainerManager` now forwards
  `IRONCLAW_WEB_SEARCH_PROVIDER` + the four provider keys into the
  session container at spawn via a new `forward_env` field, so the
  operator only configures keys once in the host's `.env`.
- New skill: `skills/web-search/SKILL.md` (auto-loaded into the
  system prompt under the existing
  `IRONCLAW_SKILLS_DIR` mechanism).
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
  `IRONCLAW_SKILLS_DIR` points at the SKILL.md library, optional
  `IRONCLAW_GROUPS_DIR` enables per-agent-group overrides under
  `<groups_dir>/<ag_uuid>/skills/`. Setup writes both env vars.
- New skills documenting the computer-use tools: `shell`,
  `read-file`, `write-file`, `web-fetch`.

### Added (M13 hardening — parallel-agent slice)

- **Image rebuild on `container_configs` change.** The manager
  fingerprints (`config_fingerprint` column) the rebuild-relevant
  fields and rebuilds + retags before the next spawn when they
  change. Rebuild failures log + emit
  `ironclaw_image_rebuild_failed_total` and fall back to the
  last-known-good image so the agent group is not blocked.
- **Container egress allow-list.** New
  `container_configs.egress_allow` (JSON array of host:port).
  Default empty == allow-all (default-allow + opt-in lockdown).
  Docker runtime translates to user-defined network policy; Apple
  Container runtime returns `RtError::Unsupported`. New
  `iclaw groups config set-egress-allow <id> --allow host:port ...`.
- **Per-group resource caps.** New
  `container_configs.resource_limits` JSON
  (`cpus` / `memory_mb` / `pids_limit`, all optional). Docker
  runtime applies via `--cpus` / `--memory` / `--pids-limit`. New
  `iclaw groups config set-resource-limits`.
- **Auto-applied `install_packages` / `add_mcp_server`.** The
  delivery loop now intercepts these system actions and writes
  directly to `container_configs.packages_apt` /
  `packages_npm` / `mcp_servers`. Combined with the rebuild
  fingerprint, the next spawn picks up the agent's tool calls
  automatically — no operator step required.
- **Central DB backup / restore.** `iclaw db backup <path>` runs
  a WAL checkpoint and atomically copies the file. `iclaw db
  restore <path>` always refuses with `host_running`; the
  operator-facing procedure is documented in
  `docs/db-backup.md` (stop host, copy file, restart).
- **Outbound dead-letter replay.** New
  `outbound_dropped_messages` table (migration `008_*`). Delivery
  failures that exhaust 3 retries land here.
  `iclaw dropped-messages outbound-list --since <window>` and
  `iclaw dropped-messages replay <id>` give the operator
  inspection / retry.
- **MCP server preset registry.** `iclaw mcp list-presets` shows
  the curated library (postgres, linear, github, notion,
  filesystem, browserbase). `iclaw mcp add <preset>
  --agent-group-id <id> --env K=V` writes the chosen preset into
  `container_configs.mcp_servers` (env values are redacted in the
  audit log).
- **Sender approval notifications in-channel.** When a new sender
  lands in `pending` for the first time, the host posts a plain-
  ASCII "approve?" notification to the agent group's primary
  messaging group. Dedup uses `unregistered_senders` so repeat
  senders don't re-spam.
- **Prometheus metrics endpoint.** Opt-in via
  `IRONCLAW_METRICS_ADDR=127.0.0.1:9090` (bare port auto-prefixes
  to loopback). Counters:
  `ironclaw_messages_inbound_total{channel_type}`,
  `ironclaw_messages_outbound_total{channel_type}`,
  `ironclaw_containers_spawned_total`,
  `ironclaw_containers_crashed_total`,
  `ironclaw_delivery_failed_total{channel_type}`,
  `ironclaw_image_rebuild_failed_total`. Histograms:
  `ironclaw_llm_call_seconds`, `ironclaw_llm_tokens_input`,
  `ironclaw_llm_tokens_output`, `ironclaw_container_spawn_seconds`.
  New crate `ironclaw-metrics`.
- **Log rotation.** Opt-in via `IRONCLAW_LOG_DIR=<path>`. Adds a
  daily-rotating file writer (`host.log.<YYYY-MM-DD>`) alongside
  the existing stderr writer. `IRONCLAW_LOG` filter applies to
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
  reachable, then installs `ironclaw`, `iclaw`, and `ironclaw-setup`
  to `~/.local/bin` — preferring a prebuilt release tarball, falling
  back to `cargo install --git`, and finally `cargo install --path`
  when run inside a checkout. Re-running detects an existing install
  and offers upgrade/skip; setup state is resumed in place. Respects
  `NO_COLOR`, non-tty stdout, and quiets verbose output unless
  something fails.
- README "Install" section now leads with the one-liner; the
  longstanding `cargo build` instructions move under a "Manual install"
  subsection.
- One-terminal operator flow for the `ironclaw` binary: new
  `ironclaw start` (daemonize, write PID file, wait for admin socket
  ready), `ironclaw stop` (SIGTERM with SIGKILL escalation after a
  10s grace), `ironclaw status [--json]` (PID, uptime, paths, active
  session count; exits non-zero when not running for CI use), and
  `ironclaw logs [-f] [-n N]` (tail the host log). `ironclaw run`
  is preserved for foreground / service-managed deployments.
- `iclaw chat` now auto-starts the host via `ironclaw start` when
  the chat FIFO is missing; pass `--no-autostart` to keep the old
  "fail loudly" behaviour for scripted / CI use. Quick start
  collapses to `ironclaw start && iclaw chat` in one terminal.
- Interactive Telegram pairing wizard inside `ironclaw-setup`'s
  `channel` step. When the operator picks `telegram`, the wizard walks
  them through `@BotFather`, validates the token format
  (`^\d+:[A-Za-z0-9_-]+$`), verifies it via Telegram's `getMe`
  endpoint (10 s timeout, soft-fail on network errors), optionally
  polls `getUpdates` for ~60 s to capture the first chat id, and
  appends `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` to the data-dir
  `.env`. Headless mode is driven by
  `IRONCLAW_SETUP_TELEGRAM_BOT_TOKEN` and
  `IRONCLAW_SETUP_TELEGRAM_CHAT_ID`. Tokens are never logged — the
  audit messages use `<digits>:****<last-4>` redaction.
- `ironclaw-setup` `service_unit` step now installs and enables the
  generated systemd unit / launchd plist end-to-end rather than just
  writing it to disk. Operators pick a scope at the prompt
  (`system` / `user` / `print`) or via
  `IRONCLAW_SETUP_SERVICE_SCOPE`; `IRONCLAW_SETUP_SERVICE_ENABLE`
  controls whether `systemctl enable --now` / `launchctl bootstrap`
  fires. The step polls the admin socket for ~10s after enabling and
  prints a clear "service is running" / "didn't come up — check
  journalctl" line. `system` scope refuses to silently shell out to
  `sudo` and falls back to `user` when not root. Idempotent on re-
  run: identical bodies are detected and the step is skipped.
- `iclaw` with no subcommand now prints a one-shot operator dashboard
  (install root, agent groups, wirings, active sessions, recent audit
  + drop activity, 24h budget usage, and up to three heuristic
  next-step suggestions). Fans out to existing read-only handlers in
  parallel via `tokio::join!`; `--json` emits the same payload as a
  single object. When the host socket is unreachable the dashboard
  exits non-zero with a friendly "host not running" pointer.
- `iclaw groups config edit <id>` — opens the container config as
  TOML in `$EDITOR` (falls back to `$VISUAL`, then `vi`), diffs on
  save, and applies the changes via the existing `groups.config.*`
  socket commands. Supports `--dry-run` to preview the diff without
  committing. Read-only fields (`agent_group_id`, `updated_at`) are
  rendered as comments and ignored on save; TOML parse errors are
  re-rendered inline with a `(r)etry / (a)bort` prompt.
- Two guided-flow agent skills under `skills/`: `customize` (walks
  the user through model swaps, package/MCP installs, behavior
  prompt edits, and budget changes, routing host-only mutations to
  the operator with the exact `iclaw` command) and `debug` (pulls
  diagnostics reachable from inside the container and prints the
  `iclaw health` / `audit list` / `dropped-messages list` commands
  the operator must run to complete triage).
- Initial Rust workspace with 16 crates across the host, runner,
  providers, MCP server, modules, skills, container runtime, OneCLI
  gateway, iclaw admin client, and interactive setup.
- Central DB schema (`ironclaw.db`) with idempotent migrations under
  `crates/ironclaw-db/migrations/`. Per-session inbound and outbound DBs
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
- `ironclaw-iclaw` Unix-socket admin server inside the host plus the
  `iclaw` client binary; 41 distinct commands exported as
  `ironclaw_iclaw::ALL_COMMANDS`.
- `ironclaw-setup` interactive setup with `dialoguer`, systemd /
  launchd unit generators, headless env-var-driven mode, and the
  `--migrate-from` data-directory migrator.
- `ironclaw-onecli` HTTP credential gateway with full wiremock coverage
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
  GHA build cache, and an `ironclaw.fingerprint` provenance label.
- Checked-in `container/Dockerfile` for the session base image, carrying
  an `IRONCLAW_FINGERPRINT` build-arg stamped as an
  `ironclaw.fingerprint=<sha>` LABEL so pulled images can be verified
  against the locally-expected spec hash.
- `ironclaw-setup` `image` step now attempts a `docker pull` of the
  pre-built GHCR image before falling back to a local build. Pulls are
  verified by inspecting the image's `ironclaw.fingerprint` label;
  mismatches fall through to a local build with a clear "pulling
  failed, building locally" message. `IRONCLAW_SETUP_NO_PULL=1` skips
  the pull attempt for air-gapped or reproducible-build use cases;
  `IRONCLAW_SETUP_PULL_REGISTRY` overrides the registry slug for forks.

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

[Unreleased]: https://github.com/phildougherty/ironclaw/compare/v0.0.0...HEAD
