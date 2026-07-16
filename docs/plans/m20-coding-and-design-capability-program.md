# M20 — Craft program: better code, better-looking prototypes, an agent that can see

Goal: after M18/M19, a user can text **"build me X"** on any channel and watch
a prototype get built, proven, and shared. M20 makes what gets built **good**:
better-engineered code (a real multi-stage quality gate, a baked toolchain, an
enforced self-review, quality-literate prompts) and better-looking UIs (a
genuine design skill, baked fonts, and — the marquee capability — the agent
**seeing its own UI** mid-build via an in-container screenshot loop and
iterating on it before delivery).

Three themes, one per wave:

1. **Bake the toolchain, open the eyes** — environment + tool foundations.
   Linters/typecheckers/fonts baked into the prototyping image; the verify
   gate grows named stages; a first-party in-container screenshot tool closes
   the stranded vision write path.
2. **Coding craft** — a code-quality prompt/skill floor, structured
   diagnostics, an enforced self-review gate (wiring the orphaned
   `code-review` skill into the loop), a shared contract + integration verify
   for `delegate_batch`, and compaction that preserves build knowledge.
3. **Design craft and the see→fix loop** — a real frontend-design skill,
   screenshot fidelity (viewport/format/console/geometry), and the ritual
   rewired so every UI prototype ships with a screenshot the agent took and
   iterated on itself.

Written 2026-07-16 from two fresh subsystem audits of `main` (coding
capability; visual-design capability). Like M18/M19 this document is written
to be executed by **parallel teams/agents, one task card per implementer**;
each card declares an exclusive scope and two cards in the same wave never
share a scope. Read the whole preamble before taking a card.

## Relationship to M19 (`docs/plans/m19-channel-ux-and-capability-program.md`)

M19 is **complete**: all 26 work items merged (`62e2c39`), final integrated
gate **7,473 passed / 0 failed**, migration 028 taken (A6). M20 does not
re-open any M18/M19 card; it builds on the verify gate (M18 R3), the
prototype-ready ritual (M18 P3), `delegate_batch` (M19 A1), the browser
crate's CDP client (M18 V3 / M19 A2), and the **generic vision read path**
that already exists: any tool result carrying `RawContent::Image` becomes a
provider image block (`crates/copperclaw-runner/src/run/tool_dispatch.rs:144-157`
→ `drive_turn.rs:578-583`; `crates/copperclaw-providers/src/anthropic.rs:472-478`),
and `view_image` (`crates/copperclaw-mcp/src/tools/view_image.rs`) reads a
local image into that path.

One M18 card is **partially superseded**: **V4's host-side screenshot
injection** (`COPPERCLAW_BROWSER_PREVIEW_ALLOW` et al.) has no producer — no
host code ever sets the browser env vars, and the in-container runner cannot
reach a Docker socket anyway (`browser_render.rs:390-402`), so in every
default deployment no prototype screenshot has ever existed. D1 replaces it
as the *prototype-build* screenshot mechanism. `browser_render` /
`browser_interact` keep their M19-A2 role as the operator-configured *web
browsing* surface; they are untouched except for a registration-error cleanup
folded into D1.

## Rules for every implementing team

Identical to M18/M19 — re-read `CLAUDE.md`, all of it applies:

1. `cargo fmt --all && cargo check --workspace && cargo clippy --workspace
   --all-targets -- -D warnings && cargo test --workspace --no-fail-fast`
   green before done. Baseline is **~7,473 tests** (M19's final integrated
   gate). Gate = zero failures, not a fixed count; do not regress it.
2. Workspace forbids `unsafe_code`; clippy warnings are errors. Pinned
   toolchain Rust 1.85 / edition 2024.
3. Every user-visible change gets a `CHANGELOG.md` line under
   `## [Unreleased]`. The changelog is a merge hotspot — write your line
   last, keep it to your card, resolve conflicts keep-both.
4. **No stubs in tree.** A smaller whole thing beats a half-wired big one. A
   registered tool works end-to-end.
5. **Secure-by-default (tenet unchanged).** New capability is opt-in unless a
   card explicitly changes a default — only D1 does, with a recorded
   no-new-privilege argument. Any card that changes a default or adds page-
   originated content to the transcript (D1, D5) requires a `security-review`
   pass recorded in the PR before merge. External content marks the turn
   untrusted (`mark_untrusted_context`).
6. **New DB state = a new numbered migration.** Next free is **029** — but
   **no M20 card is known to need one**: all new state is marker files under
   `<project>/.copperclaw/` or image content. Verify at branch time anyway.
7. **Fixtures before pipeline changes.** Any card touching
   inbound → router → runner → outbound → delivery adds/extends a replay
   fixture under `fixtures/` and registers it in
   `crates/copperclaw-host/tests/replay.rs` (registration is explicit — a
   fixture dir alone is dead data).
8. **Prompt additions must stay static per spawn.** `CODING_PREAMBLE`
   (`crates/copperclaw-host/src/container_manager/prompt.rs:172-204`) is a
   cache-pinned static const; Q4/D4 add static text only, never per-turn
   content, and Messaging/Minimal profiles gain zero bytes (pinned tests).
9. **File:line anchors are from the 2026-07-16 audits** — orientation, not
   gospel; verify before editing.
10. **PR per card, branch off `main`, one card per branch.** Operator merges
    with merge commits (house style). Re-run the *integrated* gate after a
    multi-PR merge — a green per-branch gate does not prove the union is
    green or even fmt-clean (an M18 lesson that bit twice).

## Scope / conflict map (lanes)

Same lane discipline as M18/M19 — a lane is a set of files one team owns for
the duration of its cards; cards within a lane are **sequential**, lanes run
in **parallel**. `copperclaw-metrics` is a hotspot: **no card except M1 edits
it**; cards record wanted metrics in their PR description and M1 sweeps them.

| Lane | Owns | Cards |
|---|---|---|
| **E — setup/image** | `crates/copperclaw-setup/src/steps/image.rs`, `crates/copperclaw-types/src/image.rs` (declared shared-crate touch) | Q1 |
| **T — Tool surface** | `crates/copperclaw-mcp/src/tools/**` | Q2 → Q3 → Q6 → Q7 + D1(tool half), D5(tool half) |
| **V — Preview + browser** | `crates/copperclaw-browser/**` | D1(driver half) → D2 → D5 |
| **P — Prompt + skills** | `crates/copperclaw-host/src/container_manager/prompt.rs`, `skills/**` | Q4 ‖ Q5 ‖ D3 (disjoint dirs) → D4 (prompt.rs, last) |
| **R — Runner core** | `crates/copperclaw-runner/src/compaction.rs` | Q8 |
| **X — Program verification** | `fixtures/**`, new e2e harness files only | X-riders per wave |
| **M — Metrics rider** | `crates/copperclaw-metrics` | M1 (single card, absolute last) |

Cross-lane touches are declared on each card. Two recur: **D1** spans V (the
in-container CDP driver) + T (the tool), one card like M19's A1; **Q4/D4**
both edit `CODING_PREAMBLE` — they are sequenced within lane P (Q4 first) so
the floor lands as two coherent, non-conflicting edits.

## Architecture decisions (made here — don't re-litigate)

**(a) The vision loop is in-container, first-party, loopback-only.** A new
`ui_screenshot` tool runs **inside the session container**, launching the
prototyping profile's already-baked chromium headless and driving it over the
existing hand-rolled CDP client (`crates/copperclaw-browser/src/cdp.rs`,
behind the mockable `CdpTransport` seam) against `http://127.0.0.1:<port>` —
no Docker daemon, no host wiring — and returns `RawContent::Image` directly,
so the generic read path converts it to a provider image block with zero new
plumbing. Rationale over host-side wiring: the host path is structurally
stranded (the runner IS in the container; `browser_render` needs a Docker
socket the container doesn't have, and V4's env injection has no producer),
and fixing it host-side would route a screenshot of a page that lives
*inside* the container across the container boundary and back. Security: this
adds **no new privilege** — the agent already has arbitrary `shell` in the
container and chromium on the prototyping image; the tool refuses any
non-loopback URL, so it is not a browsing capability, cannot reach the LAN or
host, and leaves the egress posture untouched. It is therefore registered
**by default** in Coding/Full profiles (the one default change in M20; the
argument and a `security-review` pass are recorded in D1's PR). Minimal
profile (no chromium): the tool probes for the binary at call time and
returns one clean actionable error naming the prototyping image profile.

**(b) Multi-stage verify keeps the observed-shell design.**
`.copperclaw/verify` stays one file; it may now contain **multiple lines,
each an independent stage**, with an optional `name:` prefix
(`lint: npx eslint .`); an unprefixed line gets a derived name. A one-line
unprefixed file is exactly today's behavior — full backward compatibility, no
format flag. `apply_verify_gate` (`computer_use.rs:389`) matches the trimmed
shell command against **any** stage and records that stage's pass/fail in a
per-project `.copperclaw/stages` state file; the todo-completion gate
(`todo.rs:697-735`) requires **every stage green since the last dirty mark**;
`last_failure` gains stage attribution. A dirty mark resets all stages.
Rejected alternatives: a `verify.d/` directory (more moving parts, worse for
the agent to author) and a first-party verify-runner tool (abandons the
observed-shell design that already works and is fixture-pinned).

**(c) Image profile: extend Prototyping, no third profile, one rebuild.**
`typescript`, `eslint`, `prettier`, `tailwindcss` join the npm bake; `ruff`
and fonts (`fonts-inter`, `fonts-jetbrains-mono`, `fonts-noto-color-emoji`)
join the apt bake. Honest cost: roughly 100–170 MB on the prototyping image;
the minimal profile grows **zero bytes** (pinned by the existing profile
tests). No third profile — a lint-capable-but-fontless matrix cell serves
nobody. The fingerprint fold follows the E2 conditional precedent so existing
groups are not force-rebuilt mid-session; every M20 card that *uses* the new
tools probes for the binary and degrades with a clear note when absent.

**(d) Self-review is an enforced gate, not a prompt ritual.** The M18 lesson
stands: enforcement via `todo_update` refusal is what made the verify gate
stick on small local models; a prompt-only ritual is skipped exactly when
quality is worst. A first-party `self_review` tool returns the project's diff
since the last review marker for the model to actually read, then accepts a
structured findings submission (or an explicit `no_findings`) and writes
`.copperclaw/reviewed`. Completing the **final/delivery todo** of a project
with dirty-since-review state refuses (same refusal mechanics as the verify
gate), capped at `REVIEW_CYCLE_CAP = 2`. Deliberately once-per-project-at-
delivery — per-todo review would double every build's turn count;
per-increment review stays a prompt-level habit (Q4). This is a discipline
gate, not adversarial security: a model *can* submit lazy findings; the
gate's job is to force the read-your-own-diff step to happen at all. The
orphaned `skills/code-review/SKILL.md` becomes the depth reference the tool's
description and refusal message point at — finally wired into the loop.

**(e) The design skill is authored in-repo; the see→fix loop is taught at the
floor.** `skills/frontend-design/SKILL.md` is written fresh in D3 (content
outline on the card). The loop lands in two layers: a short static addition
to `CODING_PREAMBLE` (the floor: "if it has a UI, screenshot it with
`ui_screenshot` before calling it done, look at it, fix what's ugly,
screenshot again") and the skill body (the depth: a concrete critique
checklist run against each screenshot). Enforcement is deliberately light —
"looked at it thoughtfully" can't be gated — but D4 makes the *delivery*
screenshot come from `ui_screenshot` + `send_file`, so a UI build with zero
screenshots becomes visible in fixtures and metrics.

---

## Wave 1 — "bake the toolchain, open the eyes"

Foundations. Three lanes fully parallel (E, T, V+T-half); everything later
depends on these.

### Q1. Bake the coding toolchain + design assets into the prototyping image — P0, M — lane E

**Problem.** Neither image profile bakes a linter, formatter, or typechecker
(`crates/copperclaw-setup/src/steps/image.rs:219-250`; the prototyping bundle
in `crates/copperclaw-types/src/image.rs` is apt `chromium`/`sqlite3`/`zip` +
npm `create-vite`/`vite`), and containers have no apt egress at runtime
(`image.rs:245-247`), so tools can never be installed later — a
lint/typecheck verify stage is structurally impossible today. `typescript`
isn't baked despite vite being baked. No font packages exist at all, so
deny-default-egress deployments render every UI in fallback fonts.

**Change.** Per decision (c): extend the `Prototyping` bundle constants in
`copperclaw-types/src/image.rs` — npm adds `typescript`, `eslint`,
`prettier`, `tailwindcss`; apt adds `ruff` (via the trixie package if
present; otherwise a pinned prebuilt binary fetched at image-*build* time,
which has egress), `fonts-inter`, `fonts-jetbrains-mono`,
`fonts-noto-color-emoji` (verify exact trixie package names at branch time).
Minimal stays empty (existing pinned test). Fold the new lists into the
per-group config fingerprint via the versioned bundle definition following
the E2 conditional precedent so existing groups are not force-rebuilt;
document the image-size growth honestly in the PR and CHANGELOG. Update the
prototyping bundle tests in `image.rs`.

**Acceptance.** A prototyping image build yields working `tsc --version`,
`eslint --version`, `prettier --version`, `ruff --version`, and
`fc-list | grep -i inter` non-empty, in-container. The minimal profile's
package list is byte-identical (test-pinned). An existing group with a
pre-Q1 image spawns without a forced rebuild; a fresh group gets the new
bundle.

### Q2. Multi-stage verify: named stages, per-stage state, stage-attributed failures — P0, M — lane T (`verify_gate.rs`, `computer_use.rs`, `todo.rs`)

**Problem.** The verify gate is one agent-chosen shell line matched exactly,
pass/fail by exit code (`verify_gate.rs:268-283`, shell match via
`apply_verify_gate` at `computer_use.rs:389`, gate at `todo.rs:697-735`,
`FIX_CYCLE_CAP = 2` at `verify_gate.rs:30`). There is no way to require lint
AND typecheck AND tests, and `last_failure` can't say *which* discipline
failed — so the fix cycle burns its two-strike budget rediscovering what
broke.

**Change.** Per decision (b): `.copperclaw/verify` may contain multiple
lines, each a stage, optional `name:` prefix; one unprefixed line = today's
behavior byte-for-byte. `apply_verify_gate` matches the shell command against
any stage and records per-stage pass/fail + timestamp in
`.copperclaw/stages` (JSON, same best-effort read/write style as the existing
markers — I/O errors log + swallow, never abort a tool call); `mark_dirty`
resets all stages. The todo gate requires all stages green post-dirty; its
refusal message names the missing/failing stages and the exact commands.
`record_verify_failure` prefixes the tail with the stage name so
`last_failure` is attributed (`stage 'typecheck' failed: <tail>`). Stage
commands whose binary is absent (pre-Q1 image) fail as a normal shell error —
the authoring guidance to probe lives in Q4/Q5, not here. Keep the
timed-out/backgrounded conservative dirty-marking per stage.

**Acceptance.** Unit: a 3-stage file where only `lint` ran green refuses todo
completion naming `typecheck, test`; a legacy one-line file behaves
identically to today (existing gate tests unmodified and green);
`last_failure` carries the stage name; a dirty mark resets all stages. The
M18 golden verify fixture (`fixtures/cli/prototype-verify-gate/`) passes
unchanged.

### D1. `ui_screenshot`: in-container screenshot of the agent's own app — P0, L — lane V (driver half) + lane T (tool half), security review recorded

**Problem.** The vision read path is fully wired and generic (any
`RawContent::Image` tool result becomes a provider image block,
`tool_dispatch.rs:144-157` → `drive_turn.rs:578-583`), but the write path is
stranded: `browser_render` requires a container runtime the session container
cannot reach (`browser_render.rs:390-402` — no Docker socket in-container, by
design), and M18 V4's env injection has no host-side producer. So in every
default deployment the agent has never once seen its own UI, and the ritual
screenshot silently degrades to nothing. Chromium is already baked in the
prototyping profile.

**Change.** Per decision (a): a new module in `copperclaw-browser` (e.g.
`incontainer.rs`) that launches the local chromium headless
(`--headless=new --no-sandbox` — the session container is the sandbox;
document this in the module) with CDP on a loopback port, reusing `cdp.rs`
via the `CdpTransport` seam; lazy singleton per session, idle-reaped. A new
`ui_screenshot` MCP tool (lane T half): args `url` (**loopback-only** — any
non-`127.0.0.1`/`localhost` URL is refused with a hint pointing at
`browser_render` for real browsing), optional `wait_ms` /
`wait_for_selector`; default capture is a 1280x800 **windowed** viewport
(not `captureBeyondViewport` full-page — that's what blows the 5 MB
`view_image`-class cap, `view_image.rs:25`). Returns `RawContent::Image`
(PNG) directly so the read path just works, plus a text line with the saved
path under `<project>/.copperclaw/screenshots/` so `send_file` can ship it at
delivery. Registered by default in Coding/Full profiles — record the
no-new-privilege argument (the agent already has `shell` + chromium) and a
`security-review` pass in the PR. Chromium absent (minimal profile) → one
clean actionable error naming the prototyping profile. Rider: when no
container runtime is reachable in-container, `browser_render` /
`browser_interact` error text points at `ui_screenshot` instead of a raw
runtime error (noting that V4's injection path is superseded).

**Acceptance.** Live in-container integration (prototyping image,
`#[ignore]`d Docker test per the E1 precedent): start a vite dev server,
`ui_screenshot("http://127.0.0.1:5173")` returns an image block the provider
path converts (extend the anthropic.rs image-block tests with a tool-result
fixture); a non-loopback URL is refused; the minimal profile returns the
actionable error; the PNG lands under `.copperclaw/screenshots/` and is
< 5 MB at the default viewport. Security review recorded in the PR.

### X-rider (Wave 1). Foundation fixtures — P1, S — lane X

Replay fixtures: a multi-stage verify pass/refusal sequence over the golden
coding fixture (extends M18 X2's `prototype-verify-gate`), and a
`ui_screenshot` turn whose image block round-trips the transcript (mock
CdpTransport). Register in `tests/replay.rs`.

---

## Wave 2 — "coding craft"

What the agent is taught, what it's forced to do, and what survives a long
build. Lane T serializes Q3 → Q6 → Q7 after Q2; lane P cards touch disjoint
dirs and run in parallel; lane R runs Q8 independently.

### Q3. `diagnostics`: structured lint/typecheck output — P1, M — lane T, after Q2; needs Q1's image for the live leg

**Problem.** All code feedback is raw `shell` output, head-truncated at
32 KiB/stream (`computer_use.rs:34`) — a long `tsc` error list truncates
precisely where the useful errors are, and the model burns turns re-running
with `tail_bytes`. There is no structured diagnostics surface at all.

**Change.** A `diagnostics` tool: given a project path, detect which of
eslint/tsc/ruff apply (config or file-extension sniff), run them with
machine-readable output (`eslint -f json`, `tsc --pretty false`,
`ruff --output-format json`), and return a **structured, capped digest**:
per-file error/warning counts, the first N full diagnostics (message,
file:line, rule), and totals — never a raw dump, so truncation stops eating
the signal. Tools absent in the image → a per-tool "not available in this
image" note, not an error. Read-only analysis with no gate interaction: the
Q2 verify stages remain the enforcement path; `diagnostics` is the fix-cycle
accelerator and its description says so. Register in Coding/Full profiles.

**Acceptance.** Unit against fixture projects: a TS project with 40 tsc
errors returns a digest listing counts + first N with file:line, well under
the shell truncation size; a Python project routes to ruff; a project with no
applicable tool says so cleanly; missing binaries degrade per-tool.

### Q4. Code-quality floor: prompt block + `coding-task` rewrite — P0, M — lane P (`prompt.rs`, `skills/coding-task/`)

**Problem.** The prompt and skills teach *process* (git, verify, deliver) but
zero code *quality*: no architecture/decomposition/dependency-choice guidance
anywhere, the prompt never mentions that `create-vite` is baked, and
`skills/coding-task/SKILL.md:52-59` ("no error handling for impossible
cases") has no counterweight — so prototypes ship with no input validation,
god-files, and hand-rolled versions of baked tooling.

**Change.** (a) Add a short static block to `CODING_PREAMBLE` (stays a static
const — rule 8): decompose before you type (name the modules first); prefer
baked tools (`create-vite` for web apps, typescript when scaffolded,
`sqlite3` for storage) over hand-rolling; handle the errors a *user* will
actually hit (bad input, empty state, network failure) even in a prototype;
and write the multi-stage `.copperclaw/verify` (lint/typecheck/test lines) at
scaffold time, not at the end. (b) Rewrite `skills/coding-task` as the depth
reference: a decomposition section (module boundaries, one responsibility per
file, when to split), a dependency-choice section (baked > fetched >
hand-rolled), a robustness section that explicitly bounds the old "no error
handling for impossible cases" line (impossible ≠ merely unlikely;
user-reachable paths always handled), and a verify-stages authoring section
teaching the Q2 format including probing for tool presence
(`command -v eslint`) before writing a stage that needs it.

**Acceptance.** Prompt snapshot tests updated; the Messaging/Minimal
byte-stability tests stay green; the skill validates; a scripted
mock-provider build shows the preamble text present exactly once in the
coding-profile prompt. (Behavioral quality is smoke-tested at program
acceptance, not unit-asserted.)

### Q5. `web-app-scaffold` skill — P1, S — lane P (new `skills/web-app-scaffold/` dir only; parallel with Q4)

**Problem.** `create-vite` and `vite` are baked but no prompt or skill
mentions them — the agent routinely hand-rolls `index.html` + script tags for
apps that deserve a real scaffold, and never seeds eslint/prettier/tsconfig
even though (post-Q1) the tools sit in the image.

**Change.** A new skill teaching the golden path for a web prototype:
`npm create vite@latest` variants (vanilla-ts default, react-ts when asked),
offline-safe because the packages are baked; seeding `tsconfig.json`, a
minimal eslint config, and prettier config in the same step; writing the
matching multi-stage `.copperclaw/verify` (`lint: …`,
`typecheck: tsc --noEmit`, plus a test/build line) at scaffold time; and,
once the dev server is up, the `ui_screenshot` habit (one line pointing at
`frontend-design` for the critique loop). Cross-referenced from `coding-task`
(Q4 lands first in lane P; Q5 adds the one-line pointer there).

**Acceptance.** Skill validates and loads; a live in-container smoke on the
prototyping image: following the skill verbatim yields a project whose verify
stages all pass with no network egress.

### Q6. Enforced self-review gate before delivery — P0, M — lane T (new `self_review` tool + `todo.rs`), after Q2

**Problem.** `skills/code-review/SKILL.md` carries real diff-review
discipline (including the adversarial-critic-via-`create_agent` pattern) but
is orphaned — nothing in the build loop invokes it, and nothing forces the
agent to read its own diff before declaring the prototype ready. Prompt-only
rituals are exactly what small local models skip (the failure mode the
`CODING_PREAMBLE` doc comment records).

**Change.** Per decision (d): a `self_review` tool that (1) computes the
project diff since the last review marker (or since first commit), (2)
returns it capped/chunked for reading, and (3) on a follow-up call accepts
structured `findings` (or an explicit `no_findings`) and writes
`.copperclaw/reviewed`. Extend the `todo.rs` completion gate: completing the
**final** todo of a project with dirty-since-review state refuses with a hint
teaching `self_review` and `load_skill("code-review")` — same refusal
mechanics as the verify gate, `REVIEW_CYCLE_CAP = 2` so it can't refuse
forever (cap burned → auto-`blocked`, mirroring `FIX_CYCLE_CAP`). Findings
the agent fixes re-dirty the project via the existing `mark_dirty_for_write`
(`verify_gate.rs:291`), so verify stages re-run after review fixes — the
ordering falls out of existing machinery. Once-per-project-at-delivery only;
per-increment review stays prompt-level (Q4). `verify_gate=off` groups skip
this gate too (one escape hatch, not two).

**Acceptance.** Unit: final-todo completion on a never-reviewed project
refuses with the teaching hint; after a `self_review` submission, completion
succeeds; a post-review edit re-dirties and re-refuses; the cap stops the
third cycle with `blocked`; non-final todos are never review-gated;
`verify_gate=off` is byte-stable with today. Fixture over the golden coding
flow.

### Q7. `delegate_batch` contract + post-merge integration verify — P1, M — lane T (`agents.rs`), after Q6 (lane order); needs Q2

**Problem.** `delegate_batch` workers (`agents.rs:201-354`, width ≤ 6, join ≤
600s) receive only an instruction string — no shared design or interface
contract — so parallel workers converge on incompatible shapes, and nothing
verifies that the assembled union even builds. The biggest quality gap in
fan-out builds.

**Change.** (a) An optional `contract` arg on `delegate_batch`: a
parent-authored shared brief (interfaces, file ownership map, naming
conventions) prepended verbatim to every worker's instructions and written to
each worker worktree as `.copperclaw/CONTRACT.md` so it survives worker
compaction. (b) After the join, if the parent project has a
`.copperclaw/verify`, mark the project dirty (Q2 machinery) so the parent
cannot complete the integration todo without re-running all stages against
the merged union — integration verify is enforcement by the existing gate,
not new machinery. (c) The tool description + `skills/create-agent` teach the
pattern: write the contract first, one component per worker, verify the
union. No reviewer-role worker (deferred — Q6's self-review covers the
parent-side read).

**Acceptance.** e2e (mock provider): a 3-worker batch each receives the
contract text; post-join the parent project is dirty and todo completion
refuses until stages pass on the merged tree; a batch without a contract
behaves exactly as today (back-compat).

### Q8. Compaction preserves build knowledge — P1, M — lane R (`compaction.rs`)

**Problem.** Compaction pins project path/branch/verify-command + the todo
list verbatim (`compaction.rs:384-430`, coding soft target 80K) but loses the
file inventory, public interfaces, and decisions/rejected-approaches over a
long build — post-compaction the agent re-explores its own project and
sometimes re-makes rejected choices: classic long-build quality decay.

**Change.** Extend the pinned project-facts header (within the existing 80K
soft target): (a) a generated file inventory of each project (paths from
`git ls-files`, capped); (b) the verify **stages** (Q2's multi-line file,
pinned verbatim exactly as the single command is today); (c) a `decisions`
section sourced from a new lightweight convention — the agent appends to
`<project>/.copperclaw/DECISIONS.md` (taught by Q4's skill rewrite: one line
per decision, "chose X over Y because Z") and compaction pins its tail. All
three capped and best-effort — a missing `DECISIONS.md` pins nothing. No new
tool: the file is written with the ordinary edit tools. Verify whether
`.copperclaw/` writes mark the project dirty (`mark_dirty_for_write` path)
and exempt them explicitly if so — a decision-log append must not invalidate
a green verify.

**Acceptance.** Unit: a compacted coding session's pinned block contains the
file inventory, all verify stages, and the DECISIONS tail; caps hold at the
soft target; a project with no DECISIONS.md compacts exactly as today plus
inventory; non-coding sessions unchanged (existing pinned tests);
`pair_safe_pivot` behavior unchanged.

### X-rider (Wave 2). Craft fixtures — P2, S — lane X

Fixtures: self-review refusal → review → completion sequence (Q6),
`delegate_batch` contract propagation + post-join dirty (Q7), compaction
digest with stages + decisions (Q8). Register in `tests/replay.rs`.

---

## Wave 3 — "design craft and the see→fix loop"

The visual wave. Lane V serializes D2 → D5 after D1; lane P runs D3 (new dir,
parallel-safe with Wave-2 P cards) then D4 (prompt.rs, after Q4).

### D2. Screenshot fidelity: viewport control, format/quality, size safety — P1, M — lane V, after D1

**Problem.** The CDP capture layer is hard-coded full-page
`captureBeyondViewport: true` (`cdp.rs:138`) with PNG only — no viewport
sizing, no mobile emulation, no JPEG/quality/clip — so a long page blows past
`view_image`'s 5 MB cap (`view_image.rs:25`) and there is no way to check
responsive layout at all.

**Change.** Extend the CDP layer (`Emulation.setDeviceMetricsOverride`;
`Page.captureScreenshot` format/quality/clip params) and surface through
`ui_screenshot` args: `viewport` presets (`desktop` 1280x800 default,
`mobile` 390x844 with mobile UA + touch metrics — one preset, not a device
matrix), `full_page: bool` (default false), `format: png|jpeg` with quality.
Size safety: if a capture exceeds the image cap, automatically retry as jpeg
q=70 and note the downgrade in the text part rather than erroring. Host-side
`browser_render` gains the same viewport/format args through the shared CDP
layer — declared touch, tests updated, default behavior byte-compatible for
existing fixtures when no args are passed.

**Acceptance.** Unit (mock `CdpTransport`): device-metrics + capture params
serialize correctly per preset; an oversize capture auto-downgrades with the
note; existing `browser_render` fixtures unchanged when args are absent. Live
smoke: a mobile-preset screenshot of a vite page differs in dimensions from
desktop.

### D3. `frontend-design` skill — P0, M — lane P (new `skills/frontend-design/` dir only)

**Problem.** There is not one design word in `prompt.rs` or any of the 40
skills — no typography, spacing, color, or layout guidance — so every UI the
agent builds is browser-default or generic-template-flavored, and the agent
has no vocabulary to critique its own screenshot even once it can see one
(D1).

**Change.** Author a genuinely good `skills/frontend-design/SKILL.md`
(content written in this card, operator-reviewed in the PR). Outline:
**Typography** — one personality pairing per app from the baked set (Inter
for UI, JetBrains Mono for data/code), a real scale (e.g. 1.25 ratio, 4–5
sizes, not eleven), line-height and measure rules. **Spacing** — one spacing
unit (4 or 8 px), everything a multiple; whitespace is a feature; group by
proximity. **Color** — one saturated accent + a neutral ramp; derive states
from the accent; a WCAG-AA contrast floor; desaturate accents on dark
backgrounds. **Layout** — hierarchy first (what is the ONE thing this screen
is for), alignment to a grid, max content widths, and designed (not
defaulted) empty/loading/error states. **Anti-generic rules** — no
default-blue gradient hero, no lorem ipsum (write real microcopy), no
three-equal-cards row by reflex, border-radius/shadow chosen once and reused,
tailwind (baked) used with a constrained palette. **Critique checklist** —
the 8-question list the see→fix loop (D4) runs against each screenshot
(hierarchy? alignment? contrast? crowding? real copy? states? one accent?
consistent radii?), under a stable heading D4's prompt line names.

**Acceptance.** Skill validates and loads; cross-referenced from
`web-app-scaffold` (one-line touch, coordinate within lane P); the critique
checklist lives under a stable, grep-able heading.

### D4. Wire the see→fix loop + the ritual screenshot — P0, M — lane P (`prompt.rs` + `skills/coding-task` touch), after Q4 + D1 + D3

**Problem.** Even with `ui_screenshot` (D1) and the design skill (D3),
nothing in the floor prompt or the prototype-ready ritual makes the agent
actually look at its UI — the ritual's screenshot line
(`prompt.rs:195-204`, "send the screenshot alongside with `send_file` when
one exists") assumes a screenshot that, in default deployments, has never
existed. The loop must be taught at the cache-pinned floor or small models
will never run it.

**Change.** (a) Add a static block to `CODING_PREAMBLE` (sequenced after
Q4's block in lane P so the floor lands coherently): for any app with a UI,
after the first visual milestone run `ui_screenshot`, LOOK at the image, run
the `frontend-design` critique checklist (`load_skill("frontend-design")`),
fix the worst two things, screenshot again — minimum one full cycle before
the delivery todo. (b) Rewrite the ritual's screenshot line: the delivery
screenshot is now *produced by the agent* — `ui_screenshot` the final state
and `send_file` it alongside the ready-card; the "when one exists" hedge
becomes "omit only when there is genuinely no UI". (c) `skills/coding-task`
gains the loop as a numbered step (touch coordinated within lane P).
Messaging/Minimal byte-stability preserved; prompt stays static per spawn.

**Acceptance.** Prompt snapshots updated; byte-stability tests green; e2e
(mock provider scripted to follow the floor): a UI build's transcript
contains screenshot → image block → critique → edit → re-screenshot before
the ready-card, and the ready-card turn includes a `send_file` of the final
screenshot; a CLI-only build takes zero screenshots.

### D5. `ui_inspect`: console errors + element geometry — P2, M — lane V (CDP) + lane T (tool half), after D2, security-review rider

**Problem.** The `Runtime`/`Log` CDP domains are never enabled
(`cdp.rs:240-244`), so a JS crash renders as a blank screenshot the agent
can't diagnose; and with no bounding-box or computed-style access the agent
can see a broken layout but not measure *why* (overflow, zero-height
container, off-screen element).

**Change.** Enable `Runtime`/`Log` in the in-container session and buffer
console entries; add CDP helpers for `DOM.getBoxModel` /
`CSS.getComputedStyleForNode` on a selector. Expose as `ui_inspect`
(loopback-only, same posture and no-new-privilege argument as D1): returns
recent console errors/warnings (capped, and **provenance-marked untrusted** —
the page could echo fetched content into its own console) and, given a
`selector`, its box + a curated subset of computed styles (display, position,
overflow, size, font-family — not the full 300-property dump). Also fold a
console-error count + first error into `ui_screenshot`'s text part so the
common case needs no second call.

**Acceptance.** Live smoke: a page that throws on load yields the error count
in `ui_screenshot`'s text and full detail via `ui_inspect`; a selector query
returns box + curated styles; console text marks the turn untrusted;
non-loopback refused. Security-review rider recorded in the PR.

### X-rider (Wave 3). Vision-loop fixtures — P2, S — lane X

Fixtures: the see→fix transcript shape (screenshot image block mid-build,
D4), viewport-preset screenshot metadata (D2), console-error surfacing (D5).
Register in `tests/replay.rs`.

---

## M1. Metrics rider — P2, M — lane M, absolute last

Sweep the metric wishes each M20 PR records in its description into
`copperclaw-metrics` in one card; no other card touches the crate. Expected
from this program: verify runs by stage + stage-attributed failures (Q2),
diagnostics runs by tool/outcome (Q3), self-review cycles + findings counts +
cap hits (Q6), delegate_batch contract presence + post-join verify outcomes
(Q7), compaction digest sections pinned (Q8), `ui_screenshot` calls by
profile/outcome/viewport + refused-URL count (D1/D2), see→fix cycles per
build + ritual screenshots delivered (D4), `ui_inspect` calls + console
errors surfaced (D5), image-profile bundle version at spawn (Q1).

---

## Wave summary

| Wave | Theme | Cards | Parallel lanes |
|---|---|---|---|
| 1 | Bake the toolchain, open the eyes | Q1 (E); Q2 (T); D1 (V+T) + X-rider | E, T, V fully parallel |
| 2 | Coding craft | Q3→Q6→Q7 (T); Q4 ‖ Q5 (P); Q8 (R) + X-rider | T is the long pole; P, R parallel |
| 3 | Design craft, see→fix | D2→D5 (V); D3 → D4 (P) + X-rider | V and P parallel |
| last | Metrics | M1 (M) | — |

Critical path to the felt outcome: **Q1 + Q2 + D1** unblock everything — run
them first, in parallel. **D4** is the marquee felt change (the agent visibly
iterating on its own UI); it needs Q4 + D1 + D3. **Q6** is the quality
backbone. Lane T (Q2 → Q3 → Q6 → Q7) is the schedule long pole; Q3 can slip
to Wave 3 if T lags — nothing depends on it.

## Program-level acceptance

1. **Coding craft.** A live smoke ("build me a habit-tracker web app") on the
   prototyping image: the agent scaffolds with create-vite + typescript,
   writes a 3-stage `.copperclaw/verify` at scaffold time, the todo gate
   refuses completion until all stages pass, `last_failure` names the failing
   stage when one fails, and delivery is refused until a `self_review` pass
   is recorded.
2. **Vision loop.** The same smoke's transcript shows at least one full
   see→fix cycle (screenshot image block → critique → edit → re-screenshot)
   and the ready-card ships with an agent-taken screenshot via `send_file` —
   in a **default deployment**: no Docker socket in-container, no operator
   browser configuration.
3. **Design floor.** The delivered UI uses a baked font (not browser
   default), passes the `frontend-design` checklist on operator eyeball, and
   renders identically in a deny-default-egress deployment (fonts local).
4. **Degradation.** On the minimal profile, `ui_screenshot` / `ui_inspect` /
   `diagnostics` return clean actionable errors naming the prototyping
   profile; Messaging/Minimal prompts gained zero bytes; existing image
   groups were not force-rebuilt.
5. The golden + craft + vision fixtures pass; the integrated gate is green
   (zero failures) after each multi-PR merge; security reviews recorded for
   D1 and D5.

## Deferred / rejected (don't re-litigate)

- **Visual regression / screenshot diffing** — pays off for *maintained*
  UIs, not single-shot prototypes; revisit when iterate-on-existing-project
  becomes a milestone.
- **LSP integration** — per-language servers baked + a protocol client is a
  milestone of its own; Q3's diagnostics digest buys most of the value at a
  fraction of the cost.
- **ctags-backed symbol tool** — universal-ctags stays baked-but-unconsumed;
  grep/explore cover prototype-scale codebases; demand-pull.
- **Design-asset CDN allow-list** — default egress is AllowAll so CDNs
  already work there; Q1's baked fonts + tailwind cover deny-default
  deployments; a curated allow-list adds policy surface for no remaining
  case.
- **A third image profile** — rejected; extend Prototyping (decision c)
  rather than grow a profile matrix.
- **A mobile device-emulation matrix** — D2 ships one mobile preset;
  per-device profiles are demand-pull.
- **Reviewer-role worker in `delegate_batch`** — Q6's parent-side self-review
  covers the read; a dedicated critic worker doubles fan-out cost for
  unproven gain.
- **`read_file` image support / images through external-MCP + subagent
  transcripts** — `view_image` + `ui_screenshot` cover every M20 flow; the
  multimodal-transcript plumbing has no card depending on it.
- **Host-side V4 screenshot wiring** — superseded (decision a), not
  deferred: the in-container tool is the prototype-screenshot path;
  `browser_render` / `browser_interact` remain the operator-configured
  browsing surface per M19 A2.
- **Token streaming, ClawHub/plugin registry, deploy-to-cloud, always-on
  browsing** — standing non-goals, unchanged.
