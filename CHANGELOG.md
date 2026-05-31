# Changelog

All notable changes to Copperclaw are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
