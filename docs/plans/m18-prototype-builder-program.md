# M18 — Autonomous prototype-builder program (parallel-team execution plan)

Goal: a user texts **"build me X"** from Telegram / Slack / Discord and, without
touching a terminal, watches the agent plan, build, and *prove* a working
prototype — then opens it from a link, sees a screenshot in the thread, and can
steer or stop the run at any point. Written 2026-07-15 from a three-subsystem
audit of `main` (runner/tools, channels/delivery, container/preview/skills).

This document is written to be executed by **parallel teams (or agents), one
task card per implementer**. Each card declares an exclusive scope; two cards
in the same wave never share a scope. Read the whole preamble before taking a
card.

## Relationship to M17 (`docs/plans/m17-agentic-ux-program.md`)

M17 Wave 1 shipped (A1 parallel tools, C1 typing ticker, C3 event wake, D1/D2
cclaw, plus the preview proxy). M18 **absorbs the still-open M17 cards** that
serve the prototype-builder goal and re-sequences them; where M18 amends a
card, the amendment here wins. Absorbed: **C4, A2, A3 (re-scoped), A4, A6,
A7 (amended by R3), B2, C5, C6**. M17 cards NOT absorbed (D3-D8, E1-E6
cclaw/setup/ops polish) remain valid in M17 and conflict with nothing here
except where noted. Do not implement an absorbed card from the M17 text alone
— read its M18 card first.

## Rules for every implementing team

1. **Read `CLAUDE.md` first.** All of it applies:
   - `cargo fmt --all && cargo check --workspace && cargo clippy --workspace
     --all-targets -- -D warnings && cargo test --workspace --no-fail-fast`
     green before done. Baseline ~6,660 tests; do not break it.
   - Workspace forbids `unsafe_code`; clippy warnings are errors.
   - Every user-visible change gets a `CHANGELOG.md` line under
     `## [Unreleased]`. The changelog is a merge hotspot — write your line
     last and keep it to your card only.
   - Never edit a released migration. New DB state = new numbered migration
     in `crates/copperclaw-db/migrations/` (next free: **026** — verify at
     branch time).
2. **No stubs in tree.** If a card can't be finished whole, deliver a smaller
   whole thing. A registered tool works end-to-end.
3. **Secure-by-default.** New capability is opt-in unless the card explicitly
   changes a default (only H1 does). External content marks the turn
   untrusted (`mark_untrusted_context`); host-state mutations write audit
   rows.
4. **File:line anchors are from the 2026-07-15 audit** — orientation, not
   gospel; verify before editing.
5. **Fixtures before pipeline changes.** Any card touching
   inbound → router → runner → outbound → delivery adds/extends a replay
   fixture under `fixtures/` per `docs/replay-fixtures.md`.
6. **PR per card, branch off `main`, one card per branch.**

## Execution status (updated 2026-07-16, start of fifth session)

Wave 1 is **complete and merged**. Wave 2: R2 merged (#31), **R3 merged
(#32)** — the verification gate is live on `main`. The "R3 status" section
below is now historical; its "Next session" checklist is done except the
live hand-verify smoke test, which the PR explicitly shipped without
(tracked as a follow-up, see "Program-level acceptance"). Next free
migration is **027**. A fresh session should take the next unblocked card
per the wave summary — R3 no longer blocks anything. The operator has
directed merges of agent-authored PRs to `main` each time so far
(2026-07-15/16); merges use merge commits (house style). CHANGELOG
keep-both conflicts between card branches are the norm — resolve by
keeping both entries, then merge.

Two housekeeping traps that bit this session, worth checking early in any
fresh session: (1) local `main` can silently drift behind `origin/main` by
many commits (it was 21 behind at one point) if a prior session's PRs merged
on GitHub without a local `git pull` after — always `git fetch && git log
HEAD..origin/main --oneline` before trusting local `main`. (2) A prior
session's "agent in flight on branch X" claim in this doc turned out to be
false for both R2 and C3 — the branches existed but held zero card-specific
commits. Don't trust an "in flight" claim without checking `git log
main..<branch> --oneline` yourself first.

### Card status

| Card | Status | PR |
|---|---|---|
| R0 | Merged | #27 |
| H1 | Merged | #30 |
| R1 | Merged | #29 |
| C1 | Merged | #24 |
| C2 | Merged | #28 |
| T1 | Merged | #25 |
| P1 | Merged | #26 |
| R2 | Merged | #31 |
| R3 | **Merged.** Verification gate is live on `main`. Live hand-verify smoke test was NOT run before merge (PR body left it unchecked) — still owed, see program-level acceptance. | #32 |
| C3 | **Merged.** Recovered an earlier session's uncommitted WIP (checkpointed as `c97a620`), verified it carefully rather than trusting it, fixed two clippy issues (`route_impl`'s `#[allow(clippy::unused_async)]`, an underscore-prefixed test field that was actually in use), added the missing e2e replay fixture (`fixtures/telegram/inbound-document-attachment/`) plus a file-readability test, and confirmed the read-only touch on `container_manager/spawn.rs` needed no changes. Full workspace `cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace --no-fail-fast` all green (6,977 passed, 0 failed). Unblocks C4a/C4b. | #33 |
| X1 | **Merged.** `fixtures/cli/prototype-golden/` — scripted mock-provider e2e for the golden path, real end-to-end pipeline exercise. Two pieces explicitly NOT covered, each root-caused precisely in the PR/fixture README: the R3 verify-gate/todo mechanic (`/data` is hardcoded in `verify_gate.rs`/`todo.rs` with only a `#[cfg(test)]`-gated override invisible to the `copperclaw-host` integration-test binary, and `/data` is a real unwritable root-owned path on any host running the suite — the minimal un-gating fix was attempted and reverted after security review correctly flagged it as a capability weakening needing explicit sign-off) and the H1 live Task HUD (`cli` isn't edit-capable, so `Behavior` is always `StatusRows`, which needs 60s real wall-clock to fire once and has no finalize arm at all). Follow-up worth a dedicated card: an unconditional (non-`#[cfg(test)]`) env-var override for the `/data` root, mirroring the shell tool's `COPPERCLAW_SHELL_STATE_FILE` precedent, would let a future fixture close both gaps. Full workspace check suite green (6,964 passed, 0 failed). | #34 |
| P2 | **PR open.** Skills refresh — five coding skills (`coding-task`, `testing`, `debug`, `preview`, `send-file`) now teach the R3 verify contract (`.copperclaw/verify` + completion gate, quoting R3's actual refusal wording), T1's `shell tail_bytes` + paged `read_file`, and the artifact-delivery close. Copy only, no `crates/**` changes. P3's fuller `send_card` ritual is noted as forthcoming (P3 unimplemented), not taught. Skills coverage validation 9/9 green in isolation; full gate green. | #36 |
| All others | Not started | — |

### R3 status (read this first if you're picking up R3)

**Where things stand:** `git checkout m18/r3-verification-gate` (pushed
through `b53ce57`; `e7c65fa` — part 2/2 — is committed locally on top and
needs `git push`). The gate itself is fully implemented and tested:

- `crates/copperclaw-mcp/src/tools/verify_gate.rs` (new): marker-file
  dirty-tracking primitives (`project_root_of`, `mark_dirty`, `is_dirty`,
  `clear_dirty`, `record_verify_failure`, `fix_cycles`, `last_failure`,
  `recorded_verify_command`, `scan_dirty_projects`, `FIX_CYCLE_CAP = 2`).
  21 unit tests, including a from-scratch battery for `project_root_of`
  (the trickiest function, per the design note that was already worked
  out below — still worth reading if you're touching this file).
- Hooked into `write_file` / `edit_file` / `multi_edit` / `apply_patch`
  (mark dirty on success) and `shell` (verify-run match clears/records;
  any other command with a resolvable `cwd` conservatively marks dirty;
  timeout and `background` paths also mark dirty conservatively since
  their exit status isn't trustworthy).
- `todo.rs`'s `update::handle` runs the gate after the existing
  evidence-≥40-chars check: refuses with a structured message naming the
  dirty project + recorded verify command + cycles remaining; after
  `FIX_CYCLE_CAP` is burned, auto-transitions to a new `TodoStatus::Blocked`
  (+ `blocked_reason` field, `#[serde(default)]` for old stores) and
  returns success instead of refusing forever.
- **Design decision worth flagging to reviewers:** `Blocked` was added
  only to the local storage-side `TodoStatus` enum in `copperclaw-mcp`,
  NOT to the portable wire schema (`copperclaw_channels_core::TodoItemStatus`,
  which 8 channel-adapter crates exhaustively match on). Rendering maps
  `Blocked -> TodoItemStatus::InProgress` for the chip UI ("not done" is
  still accurate); the agent itself sees the real `blocked` status +
  `blocked_reason` via `todo_list` / `todo_update`'s direct JSON response.
  This avoided a fan-out across every adapter crate for a card that lives
  in lane T's directory (`copperclaw-mcp/src/tools/**`) but was executed
  as part of lane R's R3 card — flag if a reviewer wants the wire schema
  extended properly later (would need coordination with lane C, since C5's
  shared markdown renderer is due to land there anyway).
- `ToolContext` gained `verify_gate_enabled()` / `check_command_override()`
  (default `true` / `None`); `RunnerToolCtx::with_verify_gate(...)` wires
  them from `RunnerConfig` in `main.rs`. `RunnerDeps` also carries
  `verify_gate` / `check_command_override` fields for parity/test
  scaffolding (same convention as `hud_mode`/`policy`) even though the
  live enforcement path is entirely through `ToolContext` — the mcp tool
  handlers never see `RunnerDeps`.
- Tests: 21 in `verify_gate.rs`, ~17 gate-focused across
  `todo.rs`/`computer_use.rs`/`edit_file.rs`/`multi_edit.rs`/`apply_patch.rs`
  (refusal shapes, fix-cycle→blocked transition, `verify_gate=off`
  byte-stable path, `check_command_override` precedence, parallel-batch
  edits to different projects marking independently without racing,
  timeout/background conservative marking). All test files that reach
  `verify_gate`'s shared data-root override share one lock
  (`verify_gate::data_root_test_lock()`) so parallel `cargo test` runs
  can't leak one test's override into another's assertions — this cost
  a real (now-fixed) flake during this session, worth preserving the
  pattern if you add a fourth call site.
- Not done: the "build-verify-loop" e2e replay fixture (flagged as a
  stretch goal in the card — "don't let it block merging") and the
  program's live hand-verify smoke test.

**Next session, in order:**
1. `git push` the `m18/r3-verification-gate` branch (currently 1 commit
   ahead of origin locally).
2. Open the PR.
3. Hand-verify per "Acceptance to hand-verify once built" below (needs
   either a scripted mock-provider e2e or a manual run against a real
   session per the program-level acceptance smoke test) — not done this
   session, flag in the PR description as an open item if skipped.
4. Once merged: next free migration is unchanged at **027** (026 stays
   taken by this card). Unblocks C4a+C4b (after C3, itself still
   unstarted), X1, P2 — see "Wave summary" below.

The original part-1 design note is preserved below for anyone re-deriving
or auditing the approach; the "Where things stand" bullets above are now
authoritative for what's actually in the tree.

**Done (part 1/2):**
- Migration `026_container_config_verify_gate.sql`: `container_configs`
  gets `check_command TEXT` (nullable, per-group verify-command override)
  and `verify_gate INTEGER` (nullable; `NULL`/unset = gate ON (default), `0`
  = off — there is no explicit "1" state, see the migration's doc comment).
  Registered in `crates/copperclaw-db/src/migrate.rs`'s `CENTRAL` list.
- `crates/copperclaw-db/src/tables/container_configs.rs`: `ContainerConfig`
  / `UpsertContainerConfig` gained `check_command: Option<String>` and
  `verify_gate: bool`; `row_to_container_config`, `get`, `upsert` updated;
  narrow setters `set_check_command` / `set_verify_gate` added (mirroring
  `set_preview_enabled`/`set_preview_bind`); tests added. Both structs
  needed `#[allow(clippy::struct_excessive_bools)]` (4 bools now:
  `coding_enabled`/`surface_thinking`/`preview_enabled`/`verify_gate` — a
  legitimate flag-bag, not a state-machine candidate; don't fight the lint
  with a refactor here).
- `crates/copperclaw-host/src/container_manager/runner_config.rs`:
  `RunnerConfigForFile` gained `check_command: Option<String>` and
  `verify_gate: Option<bool>` (only emits `Some(false)`, skip-if-default
  like `surface_thinking`/`tool_profile` — both stay OUTSIDE
  `compute_fingerprint`, runner-config-only like `tool_profile`).
  `runner_config_for` resolves both from `cc` with no env fallback (unlike
  `hud_mode`/`temperature` — these are per-group only, no host-wide knob).
- `crates/copperclaw-host/src/handlers/groups.rs`'s `config_update` (the
  `cclaw groups config update --field key=value` handler) gained
  `"check_command"` / `"verify_gate"` match arms, so operators can already
  set both from the CLI ahead of the runner honoring them.
- `crates/copperclaw-modules/src/agent_to_agent/create_agent.rs`'s
  `inherit_parent_container_config` copies both fields to a spawned child
  agent (same verification contract as the parent).
- A pile of mechanical `UpsertContainerConfig{...}` / `ContainerConfig{...}`
  struct-literal call sites across `copperclaw-host`, `copperclaw-host-
  delivery`, and `copperclaw-modules` (test fixtures + a few production
  default-row constructors) got the two new fields (`check_command: None`,
  `verify_gate: true`) added so the workspace compiles. If you add a THIRD
  new `container_configs` field later, expect the same fan-out — `grep -rn
  "UpsertContainerConfig {" --include=*.rs` / `"ContainerConfig {"` to find
  them all.

**Not done (part 2/2 — the actual gate). Concrete design, work it in this
order:**

1. **`copperclaw-runner` config plumbing.** `crates/copperclaw-runner/src/
   config.rs`: parse `check_command`/`verify_gate` out of `runner.json`
   into the runner's own config struct (mirror how `hud_mode`/
   `surface_thinking` are already parsed there). `RunnerDeps` (`run/
   mod.rs`) gets two new fields: `verify_gate: bool` (default `true`) and
   `check_command_override: Option<String>`. Wire both through `main.rs`
   and `RunnerDeps::minimal` (test default: gate on, no override — tests
   opt out explicitly, same convention as `hud_mode`/`policy`).

2. **`ToolContext` capability (`crates/copperclaw-mcp/src/context.rs`).**
   Add two default-impl trait methods, same shape as `is_context_tainted`
   / `is_autonomous_turn`:
   ```rust
   fn verify_gate_enabled(&self) -> bool { true }
   fn check_command_override(&self) -> Option<String> { None }
   ```
   `RunnerToolCtx` (in `copperclaw-runner`, the real impl) overrides both
   from `RunnerDeps`. `MockToolCtx` (the `copperclaw-mcp` test double)
   keeps the defaults unless a test overrides them explicitly.

3. **New module: `crates/copperclaw-mcp/src/tools/verify_gate.rs`** — the
   file-marker-based dirty-tracking primitives (mirrors `todo.rs`'s own
   `todo_path()` / `#[cfg(test)]` override pattern for testability). All
   state lives under `<project_root>/.copperclaw/`:
   - `verify` — the recorded command, one line (already spec'd; the agent
     writes this itself per P1's prompt guidance — this module only
     *reads* it).
   - `dirty` — presence = dirty; content irrelevant (empty file is fine).
   - `fix_cycles` — plain integer text, absent = `0`.
   - `last_failure` — tail text of the most recent failed verify run.

   Functions needed (all `async`, best-effort — I/O errors log+swallow,
   they must never abort a tool call):
   - `fn project_root_of(path: &str) -> Option<PathBuf>` — `/data/<name>/
     ...` → `/data/<name>`; `/data/<name>` itself → `/data/<name>`; a bare
     top-level `/data/<file>` (no subdirectory) or anything outside
     `/data` → `None` (no project, no gate). **This is the trickiest
     function to get right — write its unit tests first.**
   - `async fn mark_dirty(project_root: &Path)` — creates the `dirty`
     marker AND resets `fix_cycles` to `0`. Resetting on every fresh edit
     is deliberate: a new edit after a bad stretch deserves a fresh
     2-strike budget, not a permanently doomed todo.
   - `async fn is_dirty(project_root: &Path) -> bool`
   - `async fn clear_dirty(project_root: &Path)` — successful verify run:
     removes the `dirty` marker, resets `fix_cycles` to `0`.
   - `async fn record_verify_failure(project_root: &Path, tail: &str) ->
     u32` — increments `fix_cycles`, writes `last_failure`, returns the
     new count. Does NOT touch `dirty` (a failed verify leaves the
     project dirty — nothing to clear).
   - `async fn fix_cycles(project_root: &Path) -> u32`
   - `async fn last_failure(project_root: &Path) -> Option<String>`
   - `async fn recorded_verify_command(project_root: &Path, override_cmd:
     Option<&str>) -> Option<String>` — `override_cmd` (from
     `ctx.check_command_override()`) wins when `Some`; else read+trim
     `verify`, `None` if missing/empty.
   - `const FIX_CYCLE_CAP: u32 = 2;`

4. **Hook into `computer_use.rs`.** The edit-family list already exists as
   `EDIT_FAMILY_TOOLS` in `copperclaw-runner/src/run/drive_turn.rs` — that
   one's for R2's serialization concern, not directly reusable from
   `copperclaw-mcp` (wrong crate/purpose), but mirror its tool-name list.
   - `write_file` / `edit_file` / `multi_edit` / `apply_patch` handlers:
     after a **successful** write, if `project_root_of(path)` resolves,
     call `verify_gate::mark_dirty` (best-effort, ignore errors — don't
     let a marker-file write failure fail the actual edit).
   - `shell` handler (`computer_use.rs`'s `shell::handle`): after the
     command completes, only when `input.cwd` is **explicitly `Some`**
     (documented limitation — a `cwd`-less call isn't attributed to any
     project; models pass `cwd` explicitly for project-scoped commands in
     practice) and `project_root_of(cwd)` resolves:
     - If `input.command.trim()` equals
       `recorded_verify_command(project_root, ctx.check_command_override())`
       — this IS the verify run: exit code `0` → `clear_dirty`; nonzero →
       `record_verify_failure(project_root, tail_of(stdout+stderr))`.
     - Otherwise (any other command) → `mark_dirty` (conservative: it
       could have written files, e.g. `npm install` touching
       `package-lock.json`).

5. **The gate itself — `crates/copperclaw-mcp/src/tools/todo.rs`,
   `update::handle`.** After the existing evidence-≥40-chars anti-
   fabrication check (unchanged, still required) and only when
   `ctx.verify_gate_enabled()`:
   - Add a `TodoStatus::Blocked` variant (+ carry the failure text —
     reuse the existing `evidence` field's slot or add one; check how
     `TodoItem` serializes to `todo_list()`'s tool-facing JSON and update
     that too).
   - **Design simplification (deliberate, matches the single-project-
     per-session golden path the whole M18 program targets — flag if you
     find a reason to generalize):** todos are NOT linked to a specific
     project directory in the store. The gate check is session-wide: scan
     `/data/*/.copperclaw/dirty` (glob one level under `/data`) for ANY
     dirty project, not "the project this todo belongs to". So:
     - No project dirty → proceed exactly as today (evidence-only). This
       is also how "no git repo / no verify file" degrades per the card
       spec — a pure-chat group never touches `/data/<name>/...`, so
       nothing is ever marked dirty, so the strict gate never engages.
     - Some project dirty, its `fix_cycles < FIX_CYCLE_CAP` → refuse
       (`ToolError::Validation`) naming the project, the recorded verify
       command (or "none recorded — write one to `.copperclaw/verify`"
       if `recorded_verify_command` is `None`), and cycles-remaining.
     - Some project dirty, its `fix_cycles >= FIX_CYCLE_CAP` (the model
       already burned its two fix attempts and is STILL dirty) → instead
       of refusing again, auto-transition status to `Blocked` with
       `last_failure` attached, and return **success** (not an error) —
       "never silently completed" but also never permanently stuck
       refusing forever.
   - `verify_gate_enabled() == false` → skip all of the above, byte-
     identical to pre-R3 behaviour.

6. **Tests.** Runner/mcp unit tests per the card's "Tests" line: dirty
   tracking (including the "parallel batch edits" case R2's
   `execute_tool_batch` already proves can happen — two edits to
   different files in one batch should both mark their respective
   projects dirty without racing), gate refusal shapes, the fix-cycle→
   blocked transition, `verify_gate=off` byte-stable behaviour. Use
   `deps_with_mocks`-style test scaffolding (see R2's tests in
   `drive_turn.rs` for the pattern) or `copperclaw-mcp`'s own
   `MockToolCtx`. An e2e replay fixture ("build-verify-loop" on cli, per
   the card) is a stretch goal — if the harness limitation R2 hit
   (documented above) also blocks this, note it the same way and don't
   let it block merging.

7. **Acceptance to hand-verify once built:** mock-provider e2e — scripted
   turn edits a file, attempts `todo_update completed` → refused; runs a
   failing verify command → refused with stderr attached; runs a passing
   verify → allowed. `verify_gate=off` → today's behaviour, byte-stable.

**Correction (2026-07-15, later session):** the prior session's claim that
R2 and C3 had agents "in flight" on `m18/r2-mid-turn-steering` /
`m18/c3-inbound-file-contract` was stale — both branches, on inspection,
contained zero card-specific commits (only merge-catchups of already-merged
cards). Whatever ran on them didn't land any work. R2 was re-implemented
from scratch this session on a fresh branch (old branch left alone,
untouched, in case it holds context worth recovering later). C3 is still
genuinely unstarted.

R3 is code-complete on its branch and just needs a PR + merge (see "R3
status" above); R4 (next in lane R, after R3) can start once R3 merges.
Once R3 merges: C4a + C4b (after C3 merges — C3 itself is still fully open
and unstarted), X1 (after R3), P2 (after R3). Wave 3's V1, V3, and G1 have
no unmerged prerequisites and can start any time lanes are free — a
reasonable pick if R3's PR is out for review and you'd rather not wait on it.

R2 shipped without its "e2e fixture pairing with R1's" — the shared replay
harness drives one `inbound/NNN-*.json` step fully (including its whole
multi-turn tool loop) before the next step is injected, so it structurally
can't express a row landing *mid*-drive of another inbound. Covered instead
by runner unit tests (`crates/copperclaw-runner/src/run/drive_turn.rs`).
Fixturing the real race needs a harness change — lane X (X1) scope, flagged
there, not blocking R2.

One flaky test observed (not a regression): a single `copperclaw-modules`
lib test failed once on 2026-07-15 and passed on 4 consecutive reruns —
rerun before treating a lone modules failure as real. Also observed this
session: `copperclaw-skills --test coverage` (all 9 tests) fails
consistently under a full `cargo test --workspace` run but passes clean in
isolation — looks like resource contention from full-workspace parallel
test execution (the test resolves its fixture path via compile-time
`CARGO_MANIFEST_DIR`, so it isn't a CWD/env race); unrelated to any R2
file. Worth a look if it keeps showing up.

Also observed during R3 part 2/2: `copperclaw_mcp::tools::artifact_path::
tests::returns_host_path_from_discovery_file` failed once under a full
`cargo test --workspace` run, passed clean in 5/5 reruns and in isolation.
Root cause: `artifact_path.rs`'s two tests (`returns_host_path_from_
discovery_file`, `error_when_discovery_file_missing`) both mutate a shared
global-static test override (`HOST_PATH_FILE_TEST_OVERRIDE`) with no
`Mutex`-based serialization between them (unlike `todo.rs`'s
`todo_env_lock` / `verify_gate.rs`'s `data_root_test_lock` pattern) — a
pre-existing latent race, not something R3 introduced, just more likely to
surface under full-workspace parallel load. Cheap fix for whoever's next
in that file: add a `static LOCK: OnceLock<Mutex<()>>` guard around both
tests, same shape as `todo_env_lock()`.

### Facts later cards need (learned during Wave 1 — trust these over the audit anchors)

- **R2:** the control-row contract is documented in
  `crates/copperclaw-host-router/src/commands.rs` (new in R1). `/stop` writes
  `kind=system`, `trigger=0`, `content.control.op=stop`; the row **stays
  `pending`** for R2 to consume (no `inbound_wake` notify). Aliases `/cancel`,
  `/reset`, `/new` normalize to the runner sentinels. The HUD hook for
  "steering noted" is `TaskHud::add_note()` in
  `crates/copperclaw-runner/src/run/hud.rs` (also the R5 "switched provider"
  hook).
- **R3:** migration 026 is now TAKEN (`container_configs.check_command` /
  `.verify_gate`, added by R3 part 1/2 — see the "R3 status" section
  above). Next free migration is **027**.
- **R3 (from R2):** the mid-turn steering check lives in
  `drive_turn.rs`'s `check_mid_turn_steering`, called right after
  `hud.on_batch_end` on every batch iteration. In the end R3 did NOT need
  a `drive_turn.rs` seam at all — dirty-tracking and the completion gate
  live entirely tool-handler-side (`copperclaw-mcp/src/tools/{computer_use,
  edit_file,multi_edit,apply_patch,todo}.rs`, via two new `ToolContext`
  methods), since the natural trigger points (a successful edit-family
  call, a `todo_update`) are already tool calls with `&dyn ToolContext` in
  hand — no need to thread state through the runner's turn loop. Leaving
  this note for whoever reads it looking for a `drive_turn.rs` hook that
  isn't there. `messages_in::get_new_since` / `max_seq` are still the
  `copperclaw-db` primitives if something else needs to peek inbound
  mid-turn.
- **C4a/C4b:** follow the contract C3 defines; C3's PR body will carry a
  "Note for C4a/C4b implementers".
- **C5:** the fence logic to absorb lives in
  `crates/copperclaw-host-delivery/src/fence.rs` (self-contained,
  char-indexed, dependency-free, built for this migration). Also fix two
  stale doc comments in `copperclaw-host-delivery` that reference emit
  functions H1 removed (flagged in PR #30).
- **HUD capability plumbing (H1):** rich-vs-bare is decided by the new static
  `copperclaw_channels_core::capabilities` module (`supports_message_edit`:
  telegram/slack/discord/matrix/webex; `typing_indicator_visible` mirrors the
  Slack rule). C1's dynamic `ChannelAdapter::typing_indicator_visible` also
  exists on the trait.
- **Replay fixtures** are registered explicitly in
  `crates/copperclaw-host/tests/replay.rs` — a fixture dir without a
  registration there is dead data. Replies >30 lines bypass the splitter via
  the runner's collapsible expander (`EXPANDER_LINE_THRESHOLD`,
  `copperclaw-runner/src/tools.rs:1482`) — keep splitter fixtures under it.
- **T1 correction:** `read_file` already had `offset`/`limit`/`mode` on main;
  T1 added `total_lines` (lines mode only), `shell tail_bytes` (clamped to
  the 32 KiB cap), and truncation hints. Card prose elsewhere assuming "no
  paging existed" is stale.
- **R0 follow-ups:** comment-only references to the deleted floor remain in
  `copperclaw-host/src/container_manager/runner_config.rs:78` and
  `copperclaw-db/src/tables/container_configs.rs:155` — clean up
  opportunistically from the owning lanes.
- **Test baseline** is now ~6,860+ (CLAUDE.md's ~6,660 is stale; the suite
  grew with each card). Gate = zero failures, not a fixed count.
- **Process:** each merged PR's description records "Metrics wishes (for
  M1)" — M1 must sweep PR #24-#30 descriptions (and later ones) when it runs.

## Scope / conflict map (lanes)

A lane is a set of files one team owns for the duration of its cards. Cards
within a lane are **sequential**; lanes run in **parallel**.

| Lane | Owns | Cards |
|---|---|---|
| **R — Runner core** | `crates/copperclaw-runner/src/**` (except `policy.rs` where noted) | R0, H1, R2, R3, R4, R5, R6, R7 |
| **T — Tool surface** | `crates/copperclaw-mcp/src/tools/**` | T1, E1 |
| **P — Prompt + skills** | `crates/copperclaw-host/src/container_manager/prompt.rs`, `skills/**` | P1, P2, P3 |
| **C — Channels + delivery** | `crates/copperclaw-channels/**`, `crates/copperclaw-host-delivery/**`, `crates/copperclaw-host-router/**` | C1, C2, C3, C4a, C4b, R1(router), C5 |
| **V — Preview + browser** | `crates/copperclaw-host/src/preview.rs`, `crates/copperclaw-modules/src/preview.rs`, `crates/copperclaw-browser/**` | V1, V2, V3, V4, V5 |
| **G — Approvals** | `crates/copperclaw-modules/src/approvals.rs`, `crates/copperclaw-host/src/handlers/**` (approvals), approval routing glue | G1 |
| **E — Environment** | `crates/copperclaw-setup/src/steps/image.rs`, `crates/copperclaw-container-rt/**` | E2 |
| **X — Program verification** | `fixtures/**`, new e2e harness files only | X1 |
| **M — Metrics rider** | `crates/copperclaw-metrics` | M1 (single card, last) |

Cross-lane touches are declared on the card ("+ read-only touch" or "one
function in lane Y, coordinate"). `copperclaw-metrics` is a hotspot: **no card
except M1 edits it**; cards record wanted metrics in their PR description and
M1 sweeps them up.

---

## Wave 1 — "see it and steer it"

The user must never stare at a bare typing bubble for minutes, and must be
able to redirect or stop a run from chat. Six cards, five run in parallel.

### R0. Make the tool-policy floor real — P1, S — lane R (`policy.rs` only)

**Problem.** The host-owned `DISALLOWED_TOOLS` floor lists Claude-Code
PascalCase names (`EnterPlanMode`, `AskUserQuestion`, …) that never match the
snake_case in-container tool names, so the floor enforces nothing
(`crates/copperclaw-runner/src/policy.rs`). The `ToolProfile` allow-lists
(Minimal/Messaging/Coding/Full) are the only real gate and default to `Full`.

**Change.** Replace the dead names with the actual tool names the floor is
meant to pin (or delete the floor and document that profiles are the
mechanism — pick one, don't keep a decorative list). Add a test that every
name in the floor and in each profile exists in `build_tool_set()`
(`crates/copperclaw-mcp/src/tools/mod.rs:76`) so drift fails CI.

**Acceptance.** A tool named in the floor is refused at dispatch under every
profile. The name-drift test fails if a profile references a nonexistent tool.

### H1. Task HUD — one self-editing status message, ON by default — P0, M — lane R (+ contract addition in `channels/core`, coordinate with lane C)

**Problem.** During a multi-minute run the default feedback is a typing
bubble every 4s. Three overlapping mechanisms exist and none is on: per-tool
breadcrumbs gated behind `COPPERCLAW_TOOL_BREADCRUMBS=1`
(`runner/src/run/provider_call.rs:356-363`), a 60s "still working" status row
(`drive_turn.rs:24,535`), and the pinned todo checklist. The user cannot tell
a working agent from a hung one.

**Change.** One **HUD message per inbound task**, posted at first tool call
and edited in place after every tool batch (and at least every 30s):
current todo step (`agent_todos.json`), last tool + Running/Done, cumulative
tool count, elapsed time. Final edit collapses it to a one-line "done in
M:SS, N steps" (or removes it, config choice `hud_mode = full|final|off`,
default `full`). Implementation reuses the breadcrumb edit path
(`finish_tool_breadcrumb`, `drive_turn.rs:737`) and `edit_message`
(`channels/core/src/adapter.rs:403`); the 60s status row and env-gated
breadcrumbs are **removed**, not left as a fourth mechanism. Adapters without
`edit_message` get the old behavior (periodic status rows). Do not touch the
todo-pin path.

**Acceptance.** Mock rich adapter: a 10-tool scripted turn produces exactly
one HUD message edited ≥10 times, then finalized; a bare adapter produces
periodic rows; `hud_mode=off` restores today's byte-identical fixture output.
Replay fixtures updated for cli/telegram/slack/discord.

**Tests.** Runner unit (edit cadence, finalization); fixture diffs.

### R1 (= M17-C4). End-user slash commands — P0, M — lane C (router)

As specified in M17-C4: `/stop`, `/status`, `/compact`, `/clear` parsed
router-side into `control{op}` rows that bypass the mention gate
(`host-router/src/mention.rs:153-162` already whitelists command payloads).
`/compact` and `/clear` already have runner-side sentinels
(`runner/src/run/mod.rs:1006`) — wire, don't reinvent. `/status` answers from
host state without waking the runner. Unknown `/x` falls through to the agent
as text.

**Acceptance.** Fixture per command on cli + telegram. `/stop` row lands in
`inbound.db` marked control even while a turn is in flight.

### R2 (= M17-A2). Mid-turn interruption + steering — P0, M — lane R, after H1 merges; requires R1

As specified in M17-A2, unchanged in substance: between tool batches in
`drive_turn` (after `persist_mid_message`, `drive_turn.rs:108`), peek
`inbound.db`; a control `stop` row ends the turn cleanly with a "stopped —
here's where things stand" reply; a human Chat row is injected into the
transcript as an interjection so "actually use SQLite" lands within one
tool-batch boundary, not after 150 turns. Consumed rows are marked so
`run_loop` (`run/mod.rs:744-767`) never double-processes.

**Acceptance/Tests.** As M17-A2, plus: interjection updates the H1 HUD
("steering noted"). Fixture pairs with R1's.

### C1. Slack presence fallback — P0, S — lane C (ADAPTER-slack)

**Problem.** Slack typing uses `assistant.threads.setStatus`, which only
renders inside assistant threads; in a normal channel/DM the keepalive is a
silent no-op (`channels/slack/src/adapter.rs:118-132`) — Slack users get *no*
signal during long runs.

**Change.** When `set_typing` targets a non-assistant-thread conversation,
degrade gracefully: no-op remains correct once H1 ships (the HUD is the
signal), so the card is: detect the no-op case and report a capability flag
the host can read, so the H1 HUD forces `hud_mode=full` + tighter edit cadence
on Slack non-assistant surfaces. No posted-then-deleted "working…" spam.

**Acceptance.** Unit: assistant-thread → setStatus called; channel/DM →
capability flag false, no API call. HUD cadence test picks up the flag.

### C2. Fence-aware message splitter — P1, S — lane C (delivery)

**Problem.** `split_text_into_chunks` / `find_cut`
(`host-delivery/src/service.rs:2576,2609`) cuts on paragraph/sentence/char
and can land mid-code-fence; a split code block renders as garbage in
Telegram/Discord — the most visible "janky" signal for a coding agent.

**Change.** Track fence state during the cut scan; never cut inside a fence
if a pre-fence cut exists within the limit; otherwise close the fence at the
cut (```` ``` ````) and reopen with the same info string on the next chunk.
Same for Telegram HTML `<pre>` blocks.

**Acceptance.** Unit table: fenced block longer than the cap emits chunks
that each parse as balanced fences; existing splitter tests unchanged.
Fixture: telegram long-code-reply.

### T1. Paged reads + bounded shell output controls — P0, S — lane T

**Problem.** `read_file` truncates at 128 KiB with no offset/limit
(`copperclaw-mcp/src/tools/computer_use.rs:47`); `shell` caps stdout/stderr
at 32 KiB/stream (`:34`). A failing build log gets cut exactly where the
error is; the model's only recourse is re-running with `grep`/`tail` if it
thinks of it.

**Change.** `read_file`: add optional `offset` (line) + `limit` (lines),
result reports `total_lines` and whether truncated, description documents the
paging idiom. `shell`: add optional `tail_bytes` (keep the *last* N bytes
instead of the first — default stays head-truncate for compatibility), and on
truncation append a one-line hint naming the full log path for backgrounded
jobs (`/data/.jobs/…`).

**Acceptance.** Unit: paged read of a 1M-line file returns the requested
window + honest metadata; `tail_bytes` returns the end of a large stream.
Tool schemas stay hand-written per `make_tool` convention (`mod.rs:169`).

### P1. Promote core coding discipline into the base prompt — P0, S — lane P

**Problem.** The rules that make builds succeed (git init first, commit per
increment, verify before claiming done, artifact delivery via
`send_file`/`artifact_path`/preview) live in the `coding-task` skill, loaded
only if the model remembers `load_skill("coding-task")` — a repeatedly-cited
failure mode, worst on small local models. The base preamble's
anti-fake-completion rule (`container_manager/prompt.rs:117`) only requires
files to *exist*.

**Change.** When the group's tool profile is `Coding` or `Full`, inline a
~25-line condensed coding block into `BASE_PREAMBLE` (git-repo-per-project,
commit per working increment, run the project's check before marking a code
todo complete, always end with an artifact-delivery step). Keep the long-form
skill for depth; the inline block points at it. Preserve prompt-cache
stability: the block is static per spawn, never per-turn.

**Acceptance.** Prompt snapshot tests per profile; cache-stability test
(`runner/src/run/prompt.rs:379` precedent) still passes; Messaging/Minimal
profiles get zero new bytes.

---

## Wave 2 — "code that provably runs"

### R3 (amends M17-A7). Verification gate — P0, L — lane R, after R2

**Problem.** Nothing runs a build or test after edits. "Verified" is
aspirational prose; the model can mark todos complete on vibes. This is the
single biggest gap for autonomous prototyping. M17-A7 proposed an opt-in
`verify_mode` critique turn; that under-shoots — verification should be the
default contract for code work, not an opt-in extra.

**Change.** Three parts:
1. **Verify command per project.** On first edit inside `/data/<project>`,
   the agent records the check command in `/data/<project>/.copperclaw/verify`
   (one shell line: `npm test`, `cargo check`, `python -m pytest`, or a smoke
   `curl` for servers). Prompt guidance in P1 covers discovery; a
   `container_configs.check_command` (migration 026, nullable) overrides per
   group.
2. **Completion gate.** The runner tracks "dirty since last verify" per
   project dir (any edit-family or shell call with cwd inside it).
   `todo_update(status=completed)` on a todo whose project dir is dirty is
   refused with a structured error telling the model to run the verify
   command first — same enforcement pattern as the heredoc rejection
   (`computer_use.rs:139`). A successful `shell` run of the recorded command
   (exit 0) clears dirty.
3. **Bounded fix loop.** A failing verify feeds stderr back (T1's
   `tail_bytes` makes this useful); at most two verify-fix cycles per todo,
   then the todo is marked `blocked` with the failure attached — never
   silently completed.

Groups with no git repo / no verify file: gate degrades to today's
files-exist check, logged once per session. Escape hatch:
`verify_gate = off` in container config for pure-chat groups.

**Acceptance.** Mock-provider e2e: scripted turn edits a file, attempts
`todo_update completed` → refused; runs failing verify → refused with stderr;
runs passing verify → allowed. `verify_gate=off` → today's behavior,
byte-stable fixtures.

**Tests.** Runner unit (dirty tracking incl. parallel batch edits, gate
refusal shapes), e2e fixture "build-verify-loop" on cli channel.

### R4. Compaction that survives long builds — P1, M — lane R (`compaction.rs`), after R3

**Problem.** Token estimation is 4 chars/token (`compaction.rs:129`), the
soft target is 40k (`compaction.rs:51`), and summarization is asked to keep
only "decisions/open questions/identifiers" — a 90-minute build gets its
mid-task detail summarized away, and the naive estimate makes the trigger
fire early on code-dense transcripts.

**Change.** (a) Real tokenizer estimate (tiktoken-rs `cl100k_base` or the
Anthropic-published approximation; benchmark, pick one, document error
bounds). (b) Coding-profile soft target raised (e.g. 80k on 200k windows),
still config-clamped. (c) A structured **project-facts header** (project
path, verify command, branch, key decisions — sourced from R3 state + todo
list) that is pinned verbatim through every compaction instead of trusted to
the summarizer.

**Acceptance.** Compaction unit suite green with new estimator; a synthetic
long-build transcript retains the facts header verbatim after 3 compactions;
`pair_safe_pivot` behavior unchanged.

### R5 (= M17-A4). Hot in-session provider failover — P1, M — lane R, after R4

As M17-A4: consult the persisted `FallbackChain` health state on mid-turn
provider errors and retry the *current* LLM call against the next healthy
entry instead of failing the inbound with an apology. A 20-minute build must
not die at minute 18 because one gateway hiccuped. Acceptance/tests per
M17-A4, plus: the H1 HUD notes "switched provider" so the user isn't confused
by a style change.

### C3. Inbound-file contract: session-local materialization — P0, M — lane C (channels/core + router; + read-only touch on `container_manager/spawn.rs` to confirm mounts)

**Problem.** Telegram downloads attachments to the **channel's**
`data_dir/inbox/<msg_id>/` (`channels/telegram/src/ingress/mod.rs:568`) and
puts that *host* path in `content.attachment` — but the container mounts the
*session* dir (`spawn.rs:631`), so non-image files are unreachable by the
agent that was just told about them. Images only work because they ride
base64 (`runner/src/formatter.rs:110-127`). "Here's the CSV / spec / sketch,
build around it" is a core prototype request and its bytes never arrive.

**Change.** Define the contract in `channels/core`: adapters stage downloads
to a temp path; the **router**, which knows the resolved session, moves the
file into `<session_dir>/inbox/<msg_id>/<safe_name>` at route time and
rewrites `content.attachment.path` to the *container-visible* path
(`/data/inbox/…`). Telegram migrates to the contract; size caps and
too_large/download_failed system rows keep their current shape
(`ingress/mod.rs:213-233`).

**Acceptance.** e2e: telegram document fixture → file readable at
`/data/inbox/...` from a runner test; attachment path in `messages_in` is the
container path; oversized file still yields the `too_large` system row.

### C4a. Slack inbound files — P0, S — lane C (ADAPTER-slack), after C3
### C4b. Discord inbound files — P0, S — lane C (ADAPTER-discord), after C3

**Problem.** Slack ignores inbound files entirely (no `url_private` fetch in
`slack/src/events/router.rs`); Discord forwards attachment URLs without
downloading (`discord/src/events.rs:80-88`). Two of the three flagship
channels can't receive a file.

**Change.** Fetch (Slack: `url_private` with bot-token auth header; Discord:
CDN URL), enforce the same `max_attachment_bytes` config + system-row
taxonomy as Telegram, stage per the C3 contract. Small images additionally
inline `data_base64` for vision parity with Telegram
(`inline_image_base64` precedent, `telegram/src/ingress/mod.rs:479`).

**Acceptance.** Per-adapter unit + one fixture each mirroring the C3 telegram
fixture. Download failure → `download_failed` system row, never a silent drop.

### P2. Skills refresh for the verify + delivery contract — P1, S — lane P, after R3 lands

Update `skills/coding-task`, `skills/testing`, `skills/debug`,
`skills/preview`, `skills/send-file` to teach: the `.copperclaw/verify` file,
the completion gate and its error shape, `tail_bytes` for build logs, paged
`read_file`, and the Wave-3 demo ritual (P3) as the mandatory final step.
Skills are copy — no code — but they are the agent's manual; stale skills
would actively teach the pre-M18 workflow.

**Acceptance.** Skill lint passes (`copperclaw-skills` validation); no skill
references a tool parameter that doesn't exist (grep-able check).

### X1. Golden-path program fixture — P0, M — lane X, starts when R3 + H1 merge

**The program's acceptance test.** A scripted mock-provider e2e: inbound
"build me a tiny HTTP todo app" on the cli channel → agent creates
`/data/todo-app` as a git repo, edits files, hits the verify gate, passes it,
exposes a (mock-brokered) preview, sends the P3 ritual card, HUD finalizes.
Byte-stable expected output committed under `fixtures/cli/prototype-golden/`.
Every later card that changes this path updates the fixture **in its own PR**
— the fixture is the regression tripwire for the whole program.

---

## Wave 3 — "the demo moment"

### V1. WebSocket pass-through in the preview proxy — P0, M — lane V

**Problem.** The preview proxy 501s WebSocket upgrades
(`host/src/preview.rs`; skill tells agents to poll). Vite dev servers, live
reload, and anything realtime — i.e., the modern prototypes users will
actually request — degrade or break.

**Change.** Handle the upgrade in the axum proxy (`axum::extract::ws` on the
proxy side + `tokio-tungstenite` client to `container_ip:port`), bridging
frames both ways; cookie gate applies to the upgrade request exactly as to
HTTP. Idle-reaper accounting treats an open WS as activity. Update
`skills/preview/SKILL.md` (remove the polling caveat) — coordinate the skill
edit with lane P (single-file touch, sequence after P2).

**Acceptance.** Integration test: echo WS server in a "container" (local
listener), client connects through the proxy with the cookie, echoes
round-trip; without cookie → 403 before upgrade. HTTP paths byte-identical.

### V2. One-tap preview enablement + auto re-expose — P1, S — lane V, after G1

**Problem.** Preview is off per group until the operator runs a `cclaw`
command in a terminal — on their phone, the demo moment dies at
`PreviewError::Disabled`. Expired 30-min tokens 403 with no recovery.

**Change.** Keep secure-by-default (tenet 3): preview stays opt-in, but the
`Disabled` error card becomes a G1 approval card — the operator taps
**Enable previews for this group** in chat; resolution flips
`preview_enabled` with the same audit row the cclaw path writes. Expired
token: the tokened `GET /__preview/<token>` for a *known but expired* preview
re-mints (re-exposes the same session:port if the container is still up)
instead of 403, once per token.

**Acceptance.** e2e: disabled group → agent expose attempt → approval card →
approve → retry succeeds. Expired-link unit: one re-mint, second reuse 403s.

### V3 (= M17-B2). Live browser driver — P0, L — lane V (browser crate), parallel with V1

As M17-B2, unchanged: implement the concrete Chromium/CDP `BrowserDriver`
behind the existing trait, SSRF preflight and sandbox spec already in place
(`copperclaw-browser/src/lib.rs:21-26`, `spec.rs:142-343`). This is the
screenshot supply for P3. Acceptance/tests per M17-B2.

### V4. Screenshot-the-preview path — P1, S — lane V, after V1 + V3

**Change.** Teach the flow end-to-end and remove the seams: `browser_render`
against the preview URL (or `http://<container_ip>:<port>` host-side —
decide and document which; the browser child container must be able to reach
it under deny-default egress, allow-list the preview host:port at spawn).
Resulting PNG lands under `/data`, agent relays via `send_file`. If
`COPPERCLAW_BROWSER_ENABLED` is unset, the P3 ritual degrades to no
screenshot — never an error.

**Acceptance.** e2e with mock driver: expose → render → PNG exists → ritual
card carries it. Deny-default egress fixture proves the allow-list injection.

### G1. In-chat approvals — P0, M — lane G

**Problem.** Approval cards are informational; resolution is CLI-only
(`cclaw approvals approve`), so a phone-only operator cannot admit a new
sender, approve `install_packages`, or (post-V2) enable previews. The
plumbing exists on both ends: cards with buttons go out
(`modules/src/approvals.rs:379-423`), button taps come back in as Chat events
(`callback_query` / `block_actions`, whitelisted past the mention gate).

**Change.** Approval cards carry `approve:<approval_id>` / `deny:<approval_id>`
callback payloads. A router-side interceptor (coordinate the single insertion
point with lane C — it sits next to the mention gate) recognizes the payload,
verifies the tapping identity against the group's operator/approver set
(reuse the approver resolution the notifier already does), resolves via the
same path as the CLI handler, writes the audit row, and **edits the card** to
"Approved by <name>" (disabling the buttons). Non-approver taps get an
ephemeral/short "not authorized" reply and the card stays live. Every
`ApprovalKind` routes through this; no kind-specific forks.

**Acceptance.** Fixture per channel (telegram callback, slack block_action):
approver tap resolves + card edits; stranger tap refused + audited; CLI path
still works and races safely (first resolution wins, second is a no-op).

### P3. The "prototype ready" ritual — P0, S — lane P, after P2; graceful w.r.t. V-lane timing

**Problem.** Even with preview + files + cards all shipped, nothing makes the
agent *end* a build with a coherent demo — results arrive as whatever prose
the model felt like.

**Change.** Prompt (P1 block) + `skills/coding-task` make the final step of
every build todo list mandatory and concrete — one `send_card`:
title + one-liner, "What to try" bullets, screenshot attached (when V4
available), buttons: **Open preview** (URL), **Download** (triggers
`send_file` zip of the project, sans `node_modules`/`.git` — document the
`git archive` idiom), and the `artifact_path` host path in the footer for
desk users. Card degrades by capability: no preview → no button, never a
broken link. This card is copy + skill + one prompt line — no new tools.

**Acceptance.** X1 golden fixture asserts the ritual card shape. Skill lint
green.

---

## Wave 4 — depth and environment

### E1. Session-local installs that work *this* turn — P1, M — lane T

**Problem.** `install_packages` rebuilds the image for the **next** spawn
(`tools/self_mod.rs:26`); mid-build the agent needs a package *now*. Under
`AllowAll` egress `pip install --user` / `npm i` into `/data` already work,
but nothing teaches or smooths this, and `HOME=/data` cache placement is
accidental knowledge.

**Change.** Extend `install_packages` with `scope: "session"` (default stays
`"image"`): session scope runs the ecosystem-appropriate local install now
(`python3 -m venv /data/.venv && pip install`, `npm --prefix /data/.npm-global
-g`), reports what it did and the PATH/activation line, and *also* records
the package into the pending image config so the next spawn bakes it — the
"works now, permanent later" default the agent actually wants. Deny-default
egress failures return the allow-list hint. Skill update rides P2's file
(coordinate; or a follow-up skill PR).

**Acceptance.** Container-integration test (dockerized CI job): session-scope
install of a pip + an npm package → import/require succeeds in the same
session; image config diff contains the package.

### E2. Warm "prototyping" image variant — P1, M — lane E

**Problem.** The default bake is deliberately minimal (`setup/src/steps/
image.rs:184-215`); the first "build me a web app" burns its opening minutes
(or fails under deny-default) bootstrapping toolchains — and containers have
no apt egress at runtime (`image.rs:210-212`), so what isn't baked is hard to
get.

**Change.** A second image profile in setup (`image_profile =
minimal|prototyping`, per group): prototyping adds `sqlite3`,
`chromium` (headless, doubles as V4 fallback), `zip`, and pre-seeds global
`vite` + `create-vite` via the existing `packages_npm` mechanism. Setup step
stays idempotent; fingerprint mechanism handles rebuilds for free. Default
remains `minimal` (tenet 3); setup asks once.

**Acceptance.** Setup step unit (idempotent re-run), bake test renders the
expected Dockerfile, fingerprint changes exactly when the profile changes.

### R6 (= M17-A3, re-scoped). Progressive final answers — P2, M — lane R, after R5

M17-A3 shrinks once H1 exists: the HUD already covers "something is
happening." What remains is long *final* text landing all at once. Implement
A3's edit-based growth only for the final answer of turns that already ran
>30s, rich adapters only, default ON, same acceptance as M17-A3 otherwise.

### R7 (= M17-A6). Subagent fan-out + write-capable delegation — P1, L — lane R, after R6

As M17-A6 (unchanged): `explore` stays read-only; the gap between it and full
`create_agent` containers gets a middle tier for parallel build work using
the existing worktree mechanics (`spawn.rs:702-708`). Sequenced last in lane
R because everything earlier changes `drive_turn` under it.

### V5. Public tunnel module — P2, L — lane V, after V2

**Problem.** Preview is LAN-only; "send it to my cofounder" fails.

**Change.** A `copperclaw-modules` module wrapping an operator-provided
tunnel binary (cloudflared first; trait-shaped for tailscale-funnel later),
OFF by default, per-group opt-in, **every exposure G1-approval-gated** (kind:
`CredentialedExternalAction`), audit-rowed, auto-teardown with the preview it
fronts. Never bundled binaries; module errors with install instructions if
the binary is absent.

**Acceptance.** Mock-binary integration test: expose → approval → tunnel URL
in ritual card → teardown on preview close. Absent binary → clean actionable
error. Security review sign-off required before merge (touches the
outward-facing surface).

### C5 (= M17-C5 + C6). Adapter floor + shared markdown renderer — P2 — lane C, after C4a/C4b

Unchanged from M17: raise signal/whatsapp/mattermost to the rich-surface
floor (three parallel sub-cards, disjoint adapter dirs), then the shared
markdown→per-platform renderer in `channels/core`. Sequenced last in lane C
because C2/C3 change the surfaces it would render onto. M18 addition: the
renderer must own the fence-handling logic from C2 when it lands (C2's
splitter hooks migrate into it; note in both PRs).

### M1. Metrics rider — P2, S — lane M, absolute last

Sweep the metrics wishes recorded in each merged PR's description into
`copperclaw-metrics` in one card: HUD edit counts, verify-gate
refusals/passes, interjections consumed, preview WS upgrades, approval taps
(approved/denied/unauthorized), inbound files materialized per channel,
session-scope installs. One PR, no other card touches the crate.

---

## Wave summary

| Wave | Cards | Parallel lanes |
|---|---|---|
| 1 | R0→H1 (R); R1→ (C router); C1, C2 (C); T1 (T); P1 (P) | 5 |
| 2 | R2→R3→R4→R5 (R); C3→C4a+C4b (C); P2 (P); X1 (X) | 4 |
| 3 | V1‖V3→V4→ (V); G1 (G); V2 (V, after G1); P3 (P) | 3-4 |
| 4 | E1 (T); E2 (E); R6→R7 (R); V5 (V); C5 (C); M1 last | 5 |

Critical path: **H1 → R2 → R3 → X1** (steer → verify → prove). Everything
else parallelizes around it.

## Program-level acceptance

The X1 golden fixture passes, and a live smoke on the telegram dev group
(`CLAUDE.md` "Operating a live agent") demonstrates end-to-end: "build me a
tiny web todo app" → HUD visible within seconds → `/stop` + resteer honored →
verify gate blocks a fake completion → preview link opens from the phone →
ritual card with screenshot → `cclaw audit list` shows the approval and
preview rows.

## Deferred / rejected (don't re-litigate)

- **Token streaming through the SQLite transport** — rejected in M17-A3;
  still rejected. Edit-based growth only.
- **Deploy-to-cloud / hosting integrations** — out of scope (PLAN.md
  non-goals). V5's tunnel is the ceiling.
- **Interactive browser by default** — Phase 5b remains stretch; V3 ships
  read-only render.
- **Posting periodic "still working…" *new* messages** — explicitly banned;
  the HUD edits in place. New-message spam is the failure mode H1 exists to
  kill.
- **Auto-enabling preview for existing groups via migration** — rejected;
  V2's in-chat approval is the enablement path (tenet 3).
- **Inbound reactions as steering input** — nice, unscoped; revisit post-M18.
