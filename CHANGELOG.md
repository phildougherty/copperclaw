# Changelog

All notable changes to Ironclaw are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
