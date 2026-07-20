# M22 — Codebase, autonomy, and skills program

## Context — why this milestone

M18–M21 turned Copperclaw into an autonomous **prototype builder**: a user
texts **"build me X"** on any channel and watches a fresh prototype get
built, proven (verify gate + self-review), seen (the agent screenshots its
own UI and iterates), shared, and made trustworthy underneath (M21
supervision/recovery). Reviewing the recent plans (`docs/plans/m17`–`m21`,
CHANGELOG `[Unreleased]`, git history) shows three ceilings the platform now
hits — and they map exactly to what the user asked to take "to the next
level":

- **Coding is build-from-scratch only.** Every coding flow assumes a blank
  prototype. There is no first-class way to point the agent at an *existing*
  repository and have it make a correct, verified change. Edits also land
  with no automatic feedback — the see→fix idea is prompt-level with "no
  runtime marker" (`crates/copperclaw-mcp/src/tools/computer_use.rs:215-221`),
  and universal-ctags is "baked-but-unconsumed."
- **Autonomy can propose but never act.** A scheduled/heartbeat turn is
  classified autonomous and `set_turn_provenance(autonomous, approved=false)`
  is hard-wired at `crates/copperclaw-runner/src/run/mod.rs:782` — the
  "live fresh-approval grant" the comment promises never shipped, so
  `blocker.rs`'s `BlockerCategory::Autonomous` blocks *every* credentialed
  action on a scheduled turn. There is also no first-class long-running
  goal, and the event/condition-trigger substrate
  (`crates/copperclaw-host-sweep/src/checks/condition_checkin.rs`) is dormant.
- **Skills are inert prose.** `crates/copperclaw-skills/src/materialize.rs`
  is complete and tested but **orphaned** (zero runtime call sites), so a
  skill's `scripts/`/`data/` never reach the container; selection is only
  `All`/`Explicit` (the documented "relevance scorer" does not exist), so all
  41 skills splice whole into the system prompt at spawn.

**Intended outcome:** after M22 the user can text **"change my X"** — point
the agent at an existing codebase and get a correct, verified edit; a
scheduled agent can take a *bounded, pre-authorized* action instead of only
drafting it; and skills become real capabilities (runnable helpers, selected
by relevance) instead of splice-in text.

Three themes, one per wave, each with a marquee felt change:

1. **Coding: from prototype to codebase** — the agent can open an existing
   repo, navigate it by symbol (LSP/ctags), edit with the toolchain talking
   back inline, and catch UI regressions by screenshot diff.
2. **Autonomy: propose → act, safely** — a scheduled agent performs a
   bounded, human-pre-authorized external action; goals and event triggers
   become first-class.
3. **Skills: real capabilities** — skills ship executable helpers, are
   selected by relevance, and carry versioning/lifecycle.

## Provenance

Written from an M22 subsystem map of `main` — a coding-surface audit
(`crates/copperclaw-mcp/src/tools/`, `copperclaw-runner`), an autonomy audit
(the read-then-propose brake at `run/mod.rs:774-782`, the dormant condition
check-ins, the twin recurrence mechanisms), and a skills audit (orphaned
`materialize.rs`, absent relevance scorer). Like M18–M21 this is written to be
executed by **parallel teams/agents, one task card per implementer**; each
card declares an exclusive scope and two cards in the same wave never share a
scope. Read the whole preamble before taking a card.

## Relationship to M21

M21 is **complete**: all cards merged (S1–S6, F1–F4, O1–O4, X-riders, M1
metrics rider). Record M21's final integrated gate test count at branch time
(gate = zero failures, not a fixed number). The last **released** migration
is `029_messages_out_retry_state.sql` (M21 S3), so **030 is the next free
number** — verify at branch time. M22 re-opens none of M21's
supervision/feedback/operator surfaces; it builds on the approval-card
machinery (`crates/copperclaw-host/src/handlers/approvals.rs`), the verify
gate (`.copperclaw/verify`), the `self_review` gate
(`crates/copperclaw-mcp/src/tools/self_review.rs`, `REVIEW_CYCLE_CAP`), the
central `tasks` scheduler (migration 028), and the runner test-clock seam
(M21 S6) for croner timing in fixtures.

## Rules for every implementing team

Identical to M18–M21 — re-read `CLAUDE.md`, all of it applies:

1. `cargo fmt --all && cargo check --workspace && cargo clippy --workspace
   --all-targets -- -D warnings && cargo test --workspace --no-fail-fast`
   green before done. Record the exact baseline test count at branch time;
   do not regress it.
2. The workspace forbids `unsafe_code`; clippy warnings are errors.
3. **CHANGELOG.md is the merge hotspot.** Every card adds lines under
   `## [Unreleased]`, grouped `### Added/Changed/Fixed`. Append-only; resolve
   conflicts by keeping both sides.
4. **No stubs in tree.** A card ships its behavior complete behind its
   flag/default or it does not merge.
5. **Secure-by-default.** Any outward-facing surface or default change needs
   a `security-review` pass recorded in `docs/plans/m22-security-reviews.md`
   before merge. In this program that is **A1/A2** (the autonomy grant — new
   capability to act), **C1** (post-edit toolchain execution — default
   behavior change), **C2** (opening an arbitrary existing repo into the
   sandbox), and **S1** (new files reaching the container).
6. **New DB state = a new numbered migration.** Next free is **030** — A1
   (grants) and A3 (goals) each need one; allocate **030** and **031** and
   confirm at branch time. Never edit a released migration.
7. **Fixtures before pipeline changes.** Any card touching the inbound →
   router → runner → outbound → delivery pipeline adds/extends a replay
   fixture under `fixtures/<channel>/<scenario>/` first, registered in
   `tests/replay.rs`. The per-wave X-riders are the backstop, not a
   substitute.
8. File:line anchors are **orientation, not gospel** — re-find the code; do
   not patch blind.
9. One PR per card. Name it `M22 <ID>: <title>`.
10. Re-run the *integrated* gate after a multi-PR merge — a green per-branch
    gate does not prove the union is green or fmt-clean.

## Scope / conflict map (lanes)

A lane is a set of files one team owns for the duration of its cards. Cards
within a lane are **sequential**; lanes run in **parallel**. `copperclaw-metrics`
is a hotspot no card touches except M1 — every other card records its metric
wishes in its PR description and M1 sweeps them at the end.

| Lane | Owns (file globs) | Cards |
|---|---|---|
| **T — coding tools** | `copperclaw-mcp/src/tools/{edit_file,multi_edit,apply_patch,copy_file,core,diagnostics,computer_use,todo,glob,grep}.rs`, `copperclaw-runner/src/policy.rs` | C1 → C6, C3 (find_symbol tool half) |
| **L — lsp/index** | `copperclaw-mcp/src/tools/find_symbol.rs (new)`, in-image LSP bridge (`copperclaw-runner/src/run/lsp.rs (new)`) | C3 |
| **P — project open** | `copperclaw-runner/src/run/project.rs (new)`, `container_manager/cold_start.rs` (repo-attach half), `skills/coding-task` | C2 |
| **R — delegation** | `copperclaw-runner/src/run/delegate_batch.rs`, `tools/agents.rs` | C5 |
| **DB — schema** | `copperclaw-db/migrations/030_*`, `031_*`, `tasks`/`goals`/`task_grants` tables | A1, A3, A5 |
| **H — host** | `copperclaw-host/src/handlers/approvals.rs`, `container_manager/cold_start.rs` (skills half), `operator grants surface` | A1, S1 |
| **W — sweep** | `copperclaw-host-sweep/src/{service.rs, checks/{condition_checkin,recurrence,scheduling,goals}.rs}` | A3, A4, A5 |
| **N — autonomy gate** | `copperclaw-runner/src/run/{mod.rs,blocker.rs,tool_dispatch.rs}` | A2 |
| **MCP — scheduling/skills tools** | `copperclaw-mcp/src/tools/{scheduling,save_skill,load_skill,list_skills(new)}.rs` | A1, A3, A4, S3 |
| **K — skills crate** | `copperclaw-skills/src/{materialize,registry,frontmatter,save}.rs` | S1, S2, S3, S4 |
| **X — verification** | `fixtures/**`, replay-test registration | X-riders, one per wave |
| **M — metrics rider** | `copperclaw-metrics` | M1 (absolute last) |

**Serialized hotspots, declared up front:**
`copperclaw-runner/src/run/tool_dispatch.rs` is touched by **C1/C6** (Wave 1),
**A2** (Wave 2), and **S4** (Wave 3). Because waves run in sequence this is
manageable, but it is the cross-wave serialization point — re-baseline
against it at each wave start. `container_manager/cold_start.rs` is touched by
**C2** (Wave 1, repo-attach) and **S1** (Wave 3, skills materialize) — different
waves, so sequential. **A1** spans DB + H + MCP as one card with one
implementer (schema + approval round-trip + tool arg). **CHANGELOG.md** and
**copperclaw-metrics** are the standard hotspots.

## Architecture decisions (made here — don't re-litigate)

**(a) Existing-repo delivery stays shell-driven; no mutating git MCP tools.**
The agent opens/edits/commits an existing repo through `shell` + the
read-only `git_*` inspection tools + diff cards — the standing shape. C2
adds a *project-attach flow* (infer verify stages, seed `DECISIONS.md`, index
symbols), not a `git_commit` tool. Mutating git tools remain rejected.

**(b) The autonomy brake stays closed by default; it opens only per-task,
bounded, and pre-authorized.** Blanket `approved` stays `false`. A scheduled
turn may act **only** within a capability grant a human approved at schedule
time — scoped to explicit capability classes, bounded by token budget +
`max_fires` + `expires_at`, revocable. Anything outside the grant still falls
to read-then-propose and emits an approval card. Enforcement lives in the
runner/blocker layer (`run/mod.rs`, `blocker.rs`, `tool_dispatch.rs`) because
self-generated wakes bypass the router — the gate cannot be routed around.

**(c) Grant authoring reuses the `save_skill` approval round-trip.** No new
approval machinery — the human who schedules the task approves the specific
capability scope + budget + expiry via the existing approval-card path
(`crates/copperclaw-host/src/handlers/approvals.rs`) before the grant
persists.

**(d) Goals extend the scheduler, not replace the todo/memory stores.** The
goal object (A3) is durable status/progress/budget the sweep drives against;
`agent_todos.json` remains the in-session plan and the memory store remains
the fact store. The goal *indexes over* them, it does not duplicate them.

**(e) Skills stay inline-default; relevance narrows what inlines.** Inline
mode remains the default (lowest risk); S2 adds `SkillsSelector::Relevant` so
only relevant skills splice in, cutting the all-skills prompt bloat noted in
`crates/copperclaw-runner/src/compaction.rs`. `load_skill`/tool-narrowing
(S4) stay secondary. ClawHub / cross-group sharing remain a hard non-goal.

**(f) The LSP bridge is container-local and read-only.** C3 runs
rust-analyzer/tsserver inside the sandbox for go-to-def/find-refs/hover,
surfaced through one read-only `find_symbol` tool; ctags is the fast fallback
when no language server fits. No host-side language server, no writes.

---

## Wave 1 — "Coding: from prototype to codebase"

Lanes T, L, P, R. Marquee: **C2** — the first turn where the agent opens a
repo it did not create and lands a verified change in it.

### C1. Post-edit verify hook — P0, M — lane T
- **Problem.** The see→fix idea is prompt-level with "no runtime marker"
  (`computer_use.rs:215-221`); edits land with no automatic feedback, so
  type/format breakage surfaces to the user, not the model.
- **Change.** After a successful mutation in `edit_file.rs`/`multi_edit.rs`/
  `apply_patch.rs`/`write_file` (`core.rs`), run the format+typecheck path
  already in `diagnostics.rs` scoped to the touched files and append the
  digest to the tool result. Hook in `computer_use.rs`. Opt-out config only.
- **Migration.** No. **Security review.** Yes (default behavior change —
  executes toolchain commands post-edit inside the existing sandbox; record
  the argument).
- **Acceptance.** Unit: a bad edit yields a non-empty digest in the result.
  Integration: two-turn transcript — turn 1 breaks a type, turn 2 fixes it
  off the fed-back digest. Fixture: recorded diagnostics digest.

### C2. Open/attach an existing repository — P0, L — lane P — MARQUEE
- **Problem.** Every coding flow assumes a blank prototype; there is no
  first-class way to establish an *existing* repo as the working project.
- **Change.** New `project.rs` attach flow: given a cloned/handed repo path,
  **infer verify stages** from the repo (`package.json` scripts, `Makefile`,
  `Cargo.toml`, `pyproject`) into `.copperclaw/verify`, seed `DECISIONS.md`
  from the repo README/structure, trigger the C3 symbol index, and mark the
  project so the verify + self-review gates apply to existing code. Cloning
  itself is `shell` git (decision **a**). Wire into `cold_start.rs` (repo
  path) and the `coding-task` skill.
- **Migration.** No. **Security review.** Yes (arbitrary external repo into
  the sandbox — reuse the egress/SSRF posture; record it).
- **Acceptance.** Unit: verify-stage inference for a fixture repo. Integration:
  agent opens a fixture repo, runs its inferred verify, makes an edit, and the
  gate passes. Fixture: a small multi-file repo under `fixtures/`.

### C3. `find_symbol` + LSP-backed navigation — P1, L — lane L, after C2
- **Problem.** universal-ctags is baked-but-unconsumed and the LSP bridge is
  design-only deferred; the agent greps blindly for definitions/references.
- **Change.** New read-only `find_symbol` MCP tool (mirror `grep.rs`/`glob.rs`
  shape) backed by a container-local LSP bridge (`run/lsp.rs`, decision **f**)
  for go-to-def/find-refs/hover, with ctags as fallback. Register in
  `tools/mod.rs` + `policy.rs`.
- **Migration.** No. **Security review.** No (read-only, no outbound).
- **Acceptance.** Unit: symbol lookup returns file:line + refs for a fixture
  repo. Integration: agent resolves a definition without a full-repo grep.

### C4. Screenshot-diff visual regression — P1, M — lane T, after C1
- **Problem.** Editing an existing UI can silently regress it; there is no
  before/after visual check (deferred until the iterate-on-existing
  milestone — this is it).
- **Change.** Baseline + diff over `ui_screenshot` output: capture a baseline
  before a UI edit, re-capture after, and surface a perceptual diff digest
  (reuse `ui_screenshot.rs` + `view_image.rs`). Feed the diff back like C1.
- **Migration.** No. **Security review.** No (loopback-only, existing path).
- **Acceptance.** Unit: diff detects a changed region on fixture images.
  Integration: a UI edit that regresses layout is flagged in-turn.

### C5. Reviewer role in `delegate_batch` — P1, M — lane R, after C1
- **Problem.** The reviewer-role worker is deferred; `delegate_batch` has no
  first-class "review this diff" role, so review is ad hoc.
- **Change.** Add a review role in `run/delegate_batch.rs` + `tools/agents.rs`
  that receives the diff and drives the existing `code-review` skill (already
  wired by `self_review.rs`); its findings gate the merge.
- **Migration.** No. **Security review.** No.
- **Acceptance.** Unit: reviewer role dispatched with diff payload.
  Integration: a batch where reviewer findings block merge.

### C6. Promote the see→fix loop to a runtime gate — P1, M — lane T, after C1
- **Problem.** The screenshot→critique→fix loop has no runtime state
  (`computer_use.rs:215-221`), so completion gates and the HUD can't require
  or observe it.
- **Change.** Model the loop as a gate state alongside the verify + self_review
  gates (`self_review.rs` `REVIEW_CYCLE_CAP` + `todo.rs` completion gate), so
  a UI task can't complete without a post-fix screenshot.
- **Migration.** No (per-session config only if made configurable).
  **Security review.** No.
- **Acceptance.** Unit: completion blocked until the loop marker is satisfied
  for UI tasks. Integration: fixture UI task showing marker set→cleared.

### CX. Wave-1 coding fixtures — P2, S — lane X
Fixture repo(s), post-edit-digest and screenshot-diff replay coverage,
registered in `tests/replay.rs`.

---

## Wave 2 — "Autonomy: propose → act, safely"

Lanes DB, H, W, N, MCP. Marquee: **A2** — the first turn where a scheduled
agent *sends the message it was told it could send* instead of only drafting
it.

### A1. Task capability grants — schema + approval-gated authoring — P0, M — lane DB + H + MCP
- **Problem.** `schedule_task` (`tools/scheduling.rs`) stores only a prompt
  string; nothing records "this task may do Y," so the runner has nothing to
  consult on an autonomous turn.
- **Change.** **Migration 030**: a `task_grants` child of `tasks` (migration
  028 lineage) — `capability_scope`, `token_budget`, `max_fires`,
  `expires_at`, `granted_by`, `revoked_at`. `schedule_task` gains an optional
  `grant` arg; authoring is approval-gated exactly like `save_skill`
  (decision **c**) — the scheduling human approves the specific scope +
  budget + expiry via the approval card before it persists. Grants are
  revocable + expiring.
- **Migration.** Yes — **030**. **Security review.** Yes.
- **Acceptance.** Unit: grant persists only after approval; expired/
  over-budget/revoked grant reads inert. Integration: schedule-with-grant
  round-trip through the approval card. Fixture: grant row.

### A2. Enforce grants at the autonomy gate — P0, L — lane N, after A1 — MARQUEE, MOST SECURITY-SENSITIVE
- **Problem.** `approved` is hard-wired `false` at `run/mod.rs:782`, so
  `blocker.rs`'s `BlockerCategory::Autonomous` blocks every credentialed
  action on a scheduled turn. Self-wakes bypass the router, so the gate must
  live in the runner (decision **b**).
- **Change.** At turn start (`run/mod.rs:774-782`), look up the firing task's
  grant and call `set_turn_provenance(autonomous=true, approved=<scoped>)`
  where `approved` is **capability-scoped, never blanket**. Make `blocker.rs`
  and `tool_dispatch.rs` `external_action_approved()` consult the grant's
  scope + remaining budget + expiry instead of a bare bool. Any action
  *outside* the grant stays blocked → emits an approval card.
- **Migration.** No (consumes A1's). **Security review.** Yes — *the*
  gate-opening card; full verdict recorded in
  `docs/plans/m22-security-reviews.md`.
- **Acceptance.** Unit: a granted capability sets `approved` true only within
  scope/budget/expiry; ungranted stays blocked + emits a card. Integration:
  end-to-end scheduled fire that sends a granted message and is blocked on an
  ungranted one. Fixture: two wake transcripts (granted-act, ungranted-propose).

### A3. First-class long-running goal object — P1, L — lane DB + W + MCP, after A1
- **Problem.** Autonomy is only a scheduled prompt string + `agent_todos.json`
  + the memory store; a multi-day objective has no durable status/progress/
  budget the sweep can drive against.
- **Change.** **Migration 031**: a `goals` table (objective, status, progress
  log, cumulative budget). A new `checks/goals.rs` sweep module drives goal
  check-ins via the existing `kind:task` fan-out; goal budget draws on the
  grant (decision **d**). MCP `create_goal`/`list_goals`/`update_goal`.
- **Migration.** Yes — **031** (coordinate with A1's 030). **Security review.**
  No (internal state).
- **Acceptance.** Unit: goal state transitions + budget accrual. Integration:
  multi-fire goal reporting progress across wakes.

### A4. Revive condition/event check-ins — P2, M — lane W + MCP
- **Problem.** `checks/condition_checkin.rs` is dormant — `ConditionStore` is
  in-memory/default-empty with no registration surface, and the sampler in
  `service.rs` only fills `pending_inbound`, so `IdleForAtLeastSecs`/`FlagSet`
  never fire.
- **Change.** Add a registration surface (MCP tool, grant-scoped) and populate
  the missing sampler fields in `service.rs` so idle/flag conditions fire
  event-driven wakes. Condition-driven wakes stay subject to A2's grant gate.
- **Migration.** Only if conditions persist across restart. **Security review.**
  No.
- **Acceptance.** Unit: sampler populates all three condition kinds; a
  registered idle condition fires. Integration: idle-triggered check-in wake.

### A5. Consolidate the two recurrence mechanisms — P2, M — lane W + DB
- **Problem.** Per-session `messages_in.recurrence` self-replication
  (`checks/recurrence.rs`) duplicates the central `tasks` scheduler and has
  no lifecycle/list/pause.
- **Change.** Redirect self-replication to the central `tasks` path so all
  recurrence gains `list/pause/resume/cancel`; deprecate the per-session path
  behind a shim.
- **Migration.** Possibly (data path). **Security review.** No.
- **Acceptance.** Unit: recurrence rows created as tasks. Integration: a
  recurring session managed via `list_tasks`/`pause_task`.

### AX. Wave-2 autonomy fixtures — P2, S — lane X
Grant + wake fixtures (granted-act, ungranted-propose, goal-progress),
reusing the M21 runner test-clock seam for croner timing.

---

## Wave 3 — "Skills: real capabilities"

Lanes K, H, T, MCP. Marquee: **S1** — wiring the orphaned `materialize.rs` so
a skill's helper scripts reach the container and a skill can *run*, not just
instruct.

### S1. Wire `materialize` into container spawn — P0, M — lane K + H — MARQUEE
- **Problem.** `materialize.rs` is complete + tested but has zero call sites;
  a skill's `scripts/`/`data/` never reach the container, so skills are inert
  text.
- **Change.** Call `copperclaw_skills::materialize` at cold start
  (`container_manager/cold_start.rs`) for the resolved `SkillsSelector`,
  symlinking each selected skill dir into the container (escape guard already
  in `materialize.rs`).
- **Migration.** No. **Security review.** Yes (new files reaching the sandbox;
  record the default-change argument).
- **Acceptance.** Unit: materialize invoked with the resolved skill set.
  Integration: a skill helper script is executable inside a spawned container.
  Fixture: a skill dir with a script.

### S2. Relevance scorer for skill selection — P1, M — lane K
- **Problem.** `description` is documented as feeding a "relevance scorer"
  that does not exist; selection is only `All`/`Explicit` (`registry.rs`), so
  all 41 skills splice into the system prompt (the bloat noted in
  `compaction.rs`).
- **Change.** Add `SkillsSelector::Relevant` — a scoring pass over
  `frontmatter.description` (reuse the FTS path the memory store already
  uses) so only relevant skills inline; keep `All`/`Explicit` (decision **e**).
- **Migration.** No. **Security review.** No.
- **Acceptance.** Unit: scorer ranks a fixture skill set. Integration: prompt
  size drops for an off-topic task.

### S3. Skill versioning + `list_skills` — P2, M — lane MCP + K
- **Problem.** `save.rs` has no versioning or list surface, so agent-authored
  skills can't be enumerated or safely re-saved.
- **Change.** Add a version field (frontmatter) + a `list_skills` read tool;
  make `save_skill` version-aware. Files: `save.rs`, `tools/save_skill.rs`,
  new `tools/list_skills.rs`.
- **Migration.** No (skills are files). **Security review.** No.
- **Acceptance.** Unit: re-save bumps version; list returns saved skills.
  Integration: author→list→reload round-trip.

### S4. Activate `tools:` frontmatter + inline-mode active-skill narrowing — P2, M — lane K + T
- **Problem.** The `tools:` frontmatter key is reserved-but-unused and
  active-skill allowed-tools narrowing is OFF in inline (default) mode, so
  `load_skill` is inert and a skill can't scope the tool surface.
- **Change.** Parse `tools:` in `frontmatter.rs`; make `load_skill` narrow the
  active-skill allowed-tools in inline mode too (the path
  `tool_dispatch.rs` already models for callable mode).
- **Migration.** No. **Security review.** No.
- **Acceptance.** Unit: `tools:` parsed + enforced. Integration: loading a
  skill narrows dispatch policy under inline mode.

### SX. Wave-3 skills fixtures — P2, S — lane X
Materialized-script + relevance-selection + versioning replay coverage.

---

## M1. Metrics rider — P2, M — lane M, absolute last

The only card touching `crates/copperclaw-metrics` (serialized hotspot).
Counters swept from the per-card wishes: grants issued/approved/expired,
autonomous actions taken vs blocked-and-proposed (A2), goal
progress/completions (A3), condition-check-in fires (A4), post-edit
diagnostics fired (C1), symbol lookups (C3), visual-regression flags (C4),
skills materialized + relevance-filtered (S1/S2). Update `docs/observability.md`.

## Wave summary

| Wave | Theme | Cards | Marquee |
|---|---|---|---|
| 1 | Coding: prototype → codebase | C1, C2, C3, C4, C5, C6, CX | C2 open existing repo |
| 2 | Autonomy: propose → act safely | A1, A2, A3, A4, A5, AX | A2 grant-gated action |
| 3 | Skills: real capabilities | S1, S2, S3, S4, SX | S1 materialize helpers |
| — | Metrics rider | M1 | — |

## Program-level acceptance (end-to-end smoke scenarios)

1. **Change my repo.** From a channel: "clone <repo> and fix the failing
   test." The agent clones (shell), C2 infers verify stages, C3 navigates by
   symbol, C1 feeds back a broken edit, the agent self-fixes, the verify gate
   passes, and a diff card is delivered.
2. **Bounded autonomous action.** Schedule "each morning, message the standup
   channel with yesterday's commits," granting `send_message:<channel>` with a
   budget + 30-day expiry (approval card). Next fire: the agent *sends* it.
   Change the prompt to also email someone (ungranted) → that action is
   blocked and surfaces as an approval card, everything else still runs.
3. **Runnable skill.** A skill that ships a helper script is materialized into
   the container (S1) and the agent executes it. An off-topic inbound inlines
   only the relevant skills (S2), and the prompt is measurably smaller.

## Verification (how to exercise M22 end-to-end)

- **Gate:** `cargo fmt --all && cargo check --workspace && cargo clippy
  --workspace --all-targets -- -D warnings && cargo test --workspace
  --no-fail-fast` — zero failures against the recorded baseline.
- **Replay fixtures** (the deterministic pipeline check): the three X-riders
  under `fixtures/` + `tests/replay.rs` cover post-edit digest, grant
  wake (granted-act vs ungranted-propose), and materialized-script paths.
- **Live smoke** (per `CLAUDE.md` operating notes): `./rebuild.sh`, then via
  `cclaw chat` run scenarios 1–3 above; confirm with `cclaw audit list`,
  `cclaw sessions list`, and the per-session `inbound.db`/`outbound.db` that
  the grant gate, goal fan-out, and skill materialization behaved. Security
  reviews for A1/A2/C1/C2/S1 recorded in `docs/plans/m22-security-reviews.md`
  before those cards merge.

## Deferred / rejected (don't re-litigate)

- Mutating git MCP tools (`git_commit`/`push`) — remain rejected; shell + diff
  cards deliver repo changes (decision **a**).
- ClawHub / skill registry / cross-group skill sharing — HARD non-goal.
- Blanket / standing "always approved" autonomy — grants stay per-task,
  bounded, expiring, revocable (decision **b**).
- Host-side language servers or LSP writes — the bridge is container-local and
  read-only (decision **f**).
- Making `callable` the default skills mode — inline stays default; relevance
  narrows it (decision **e**).
- Token streaming, periodic "still working" messages, deploy-to-cloud — remain
  rejected from prior programs.
