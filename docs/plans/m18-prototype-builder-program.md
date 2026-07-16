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
     in `crates/copperclaw-db/migrations/` (next free: **028** — verify at
     branch time; E2 took 027, and was the only remaining card known to need one).
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

## Execution status (updated 2026-07-16, seventh session — Wave 3 batch merged)

**Wave 3 launched via six parallel worktree agents (seventh session) and all
six are now MERGED to `main`:** **T2** (#40), **V1** (#41), **V3** (#42),
**E2** (#43), **R5** (#44), **G1** (#45) — merged in that order (merge commits,
house style). Local `main` = `origin/main` at `0f26247`. Migration **027** is
TAKEN by E2 (`027_container_config_image_profile.sql`) — next free is **028**.

**Integration was verified, not assumed.** Each PR passed its own gate on its
branch, but several auto-merged into overlapping files that never compiled
together until now: `runner_config.rs` (E2+R5), `preview.rs` (E2+V1),
`service.rs` / `handlers/approvals.rs` (E2+G1), `tests/replay/harness.rs`
(R5+G1), plus the usual CHANGELOG keep-both across all six and a clean
Cargo.lock auto-merge (V1+V3 both added `tokio-tungstenite`). After all six
merged, the full gate was run on the integrated tree: **fmt / check / clippy
-D warnings clean, 7096 passed / 0 failed** (the `copperclaw-skills` coverage
suite that flaked for G1 under isolation-load passed clean here). One
post-merge fixup was required and landed directly on `main` as `0f26247`:
**G1 (#45) shipped several >100-char lines rustfmt wanted wrapped** (incl. its
new `approval_intercept.rs`), so the merged `main` initially failed
`cargo fmt --all -- --check` despite G1's PR claiming fmt-clean — a fmt-only,
no-logic commit fixed it. Lesson for future sessions: re-run the *integrated*
gate after a multi-PR merge; a green per-branch gate does not prove the union
is green (or even fmt-clean).

Housekeeping: three of the six agents each accidentally ran a `git checkout -b`
in the shared checkout before working in their worktree and self-corrected; the
shared checkout was re-verified clean after each. The finished agent worktrees
under `.claude/worktrees/agent-*` were pruned during the merge.

**Waves 1 and 2 are complete and merged** (R4 #39, C4a #37, C4b #38 landed
since the previous update; local `main` = `origin/main` at `544f12e`).
Next free migration is **028** (E2's PR #43 takes 027). Test baseline is
~7,000 (gate = zero
failures, not a fixed count). Every remaining card was re-audited against
`main` on 2026-07-16 (five parallel subsystem audits: preview/browser,
approvals, runner, environment, channels); stale anchors were corrected
and each remaining card now carries a "**Verified state (2026-07-16)**"
block — where that block disagrees with the original card prose, the
block wins. Two owed follow-ups are now real cards: **T2** (unconditional
data-root env override + a test-race fix) and **X2** (extend the golden
fixture over the verify gate), both at the end of the Wave 2 section.

Next unblocked cards by lane (prerequisites now MERGED, so all of these are
ready to start): **E1** (lane T, T2 merged), **X2** (lane X, T2 merged),
**V4** (lane V, V1+V3 merged), **V2** (lane V, G1 merged), **P3** (lane P,
prefers V4 — now available), **C5** sub-cards (lane C). M1 stays last. The
seventh-session batch (T2 #40, V1 #41, V3 #42, E2 #43, R5 #44, G1 #45 — all
merged) covered every card that was unblocked at session start. The operator has directed merges
of agent-authored PRs to
`main` each time so far (2026-07-15/16); merges use merge commits (house
style). CHANGELOG keep-both conflicts between card branches are the norm
— resolve by keeping both entries, then merge.

### Eighth session — wave 3 (in flight) + teed-up wave 4

**Wave A — MERGED (eighth session):** **P3** (#46), **V4** (#47), **X2** (#48),
**E1** (#49) — four parallel worktree agents on disjoint lanes (V/P/X/T), all
merged in that order. Integrated gate on `main` after merge: **fmt / check /
clippy -D warnings clean, 7121 passed / 0 failed** (no flakes this run; all
four agents self-verified `cargo fmt --check`, so no G1-style post-merge fmt
fixup was needed). Only CHANGELOG conflicted within the wave (V4/E1 touch
different `copperclaw-mcp` files; P3 is prompt+skills; X2 is fixtures) —
resolved keep-both. Notable: V4 renders **host-side container-direct**
(`http://<container_ip>:<port>`, not the cookie-gated proxy URL) so it left
`preview.rs` untouched — meaning **V2 no longer has a preview.rs conflict with
V4** and could in principle have run alongside it; kept the sequencing anyway.
E1's "works now" install runs **in-container** (the tool surface's process),
not host-side, because `ContainerRuntime` exposes no host→container exec
primitive — a documented correction to the card's framing. Local `main` =
`origin/main` at `599a05e`.

**Wave B — MERGED (eighth session):** **R6** (#50), **V2** (#51), **C5** (#52) —
three parallel agents on disjoint lanes (R/V/C), all merged. Integrated gate on
`main` after merge: **fmt / check / clippy -D warnings clean, 7166 passed / 0
failed**. Only CHANGELOG conflicted (the three cards touch disjoint code).
**C5 was split:** part 1 (the three adapter rich-surface floors +
`EDIT_CAPABLE_CHANNELS` sync + the two PR-#30 doc-comment fixes) landed in #52;
the shared markdown renderer + `fence.rs` migration were deferred as **C5b**
(now a Wave C card, in flight). Notable: V4's host-side-container-direct render
meant V2 had no `preview.rs` conflict after all. Local `main` = `origin/main`
at `10e473b`.

**Wave C — IN FLIGHT (eighth session), three parallel agents:** **R7** (lane R,
`m18/r7-subagent-fanout` — write-capable parallel-delegation middle tier reusing
the worktree mechanics), **C5b** (lane C, `m18/c5b-shared-renderer` — the
deferred shared markdown renderer + `fence.rs` migration from C2), **V5** (lane
V, `m18/v5-public-tunnel` — public tunnel module). **V5 is HELD for explicit
human security sign-off** — it touches the outward-facing surface, the plan
mandates a security review before merge, and the launching session will NOT
auto-merge it; the V5 agent runs `/security-review` and writes the threat model
into its PR body for the human to sign off. After Wave C merges (V5 pending
sign-off), **M1** (lane M, ABSOLUTE LAST) sweeps every merged PR's "Metrics
wishes" (incl. #40-#52 and all Wave C PRs). Then the only program-acceptance
item left is the live telegram smoke test (needs a message from the operator's
phone; the automatable cli-channel proxy can be run in the meantime).

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
| R4 | **Merged.** Compaction that survives long builds: calibrated ~3.5-chars/token whitespace-collapsed estimator (no `tiktoken-rs` dep — rationale + error bounds in `compaction.rs`), profile-conditional soft target (Coding/Full 80K, chat 40K, still config-clamped), and a verbatim-pinned project-facts header sourced from R3 verify-gate state + the todo list (survives 3+ compactions losslessly). `pair_safe_pivot` unchanged. Full workspace gate green. | #39 |
| C3 | **Merged.** Recovered an earlier session's uncommitted WIP (checkpointed as `c97a620`), verified it carefully rather than trusting it, fixed two clippy issues (`route_impl`'s `#[allow(clippy::unused_async)]`, an underscore-prefixed test field that was actually in use), added the missing e2e replay fixture (`fixtures/telegram/inbound-document-attachment/`) plus a file-readability test, and confirmed the read-only touch on `container_manager/spawn.rs` needed no changes. Full workspace `cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace --no-fail-fast` all green (6,977 passed, 0 failed). Unblocks C4a/C4b. | #33 |
| C4b | **Merged.** Discord inbound files: `DiscordRest::download_cdn_file` (auth-header-free GET — Discord CDN URLs are public, unlike Slack's `url_private`), `events::message_create_to_inbound_downloaded` fetches the first attachment, enforces the new `max_attachment_bytes` config (default 25 MiB), stages via the C3 `stage_inbound_file` (`staged_path` only, never `path`), and inlines small images as `data_base64` for vision parity. `too_large` / `download_failed` system-row taxonomy mirrors Telegram; never a silent drop. New fixture `fixtures/discord/inbound-file-attachment/` (+ 2 replay tests) mirrors the C3 telegram fixture. `channels/core` and the router consumed unchanged. Full workspace gate green (6,999 passed, 0 failed). | #38 |
| X1 | **Merged.** `fixtures/cli/prototype-golden/` — scripted mock-provider e2e for the golden path, real end-to-end pipeline exercise. Two pieces explicitly NOT covered, each root-caused precisely in the PR/fixture README: the R3 verify-gate/todo mechanic (`/data` is hardcoded in `verify_gate.rs`/`todo.rs` with only a `#[cfg(test)]`-gated override invisible to the `copperclaw-host` integration-test binary, and `/data` is a real unwritable root-owned path on any host running the suite — the minimal un-gating fix was attempted and reverted after security review correctly flagged it as a capability weakening needing explicit sign-off) and the H1 live Task HUD (`cli` isn't edit-capable, so `Behavior` is always `StatusRows`, which needs 60s real wall-clock to fire once and has no finalize arm at all). Follow-up worth a dedicated card: an unconditional (non-`#[cfg(test)]`) env-var override for the `/data` root, mirroring the shell tool's `COPPERCLAW_SHELL_STATE_FILE` precedent, would let a future fixture close both gaps. Full workspace check suite green (6,964 passed, 0 failed). | #34 |
| P2 | **Merged.** Skills refresh — five coding skills (`coding-task`, `testing`, `debug`, `preview`, `send-file`) now teach the R3 verify contract (`.copperclaw/verify` + completion gate, quoting R3's actual refusal wording), T1's `shell tail_bytes` + paged `read_file`, and the artifact-delivery close. Copy only, no `crates/**` changes. P3's fuller `send_card` ritual is noted as forthcoming (P3 unimplemented), not taught. Skills coverage validation 9/9 green in isolation; full gate green. | #36 |
| C4a | **Merged.** Slack inbound files: `SlackApi::download_file` fetches `url_private` with the bot token (reused `SlackApi`'s existing `.bearer_auth`); events router downloads a message's first file, enforces a new `SlackConfig.max_attachment_bytes` (default 20 MB), stages per the frozen C3 contract (`staged_path`, never `path`), inlines `data_base64` for small images, and downgrades size/transport failures to `too_large` / `download_failed` system rows (never a silent drop). Adapter unit tests (mock Slack server) + new e2e fixture `fixtures/slack/inbound-file-attachment/`. Full workspace gate green (6,991 passed, 0 failed). Lane-C/Slack-only; channels/core, router, telegram, discord untouched. | #37 |
| T2 | **Merged.** `COPPERCLAW_DATA_ROOT` unconditional override (precedence: test-override > env > `/data`); todo store resolves under the shared root; `artifact_path.rs` test race fixed with a `Mutex` guard. Env-branch tested via a pure `resolve_data_root` helper (workspace `forbid(unsafe_code)` blocks `set_var`). Security threat-model sign-off in PR body. Gate green (7025 passed, 0 failed). Unblocks E1, X2. | #40 |
| V1 | **Merged.** WebSocket pass-through in the preview proxy: axum `WebSocketUpgrade` + `tokio-tungstenite` bridge to `container_ip:port`; cookie gate runs ahead of upgrade (403 without cookie); idle-reaper bumped on frames + a 60s keepalive tick so live tabs aren't reaped. `skills/preview/SKILL.md` polling caveat removed. HTTP paths byte-identical. Gate green (0 failures). | #41 |
| V3 | **Merged.** Live browser driver: hand-rolled minimal CDP client over `tokio-tungstenite` (chromiumoxide/headless_chrome rejected — they launch a local process, we connect to a child *container*), behind a mockable `CdpTransport` seam; the deferred live spawn path is wired; `browser_render.handle()` drives it. Safety preserved (SSRF preflight, deny-default egress, `COPPERCLAW_BROWSER_ENABLED` opt-in, output stays `Untrusted`). Live path compiles+wired but not CI-exercised (needs a real Docker + Chromium image). Screenshot temp-dir placement (`COPPERCLAW_BROWSER_OUTPUT_DIR`) is refined by V4. Gate green (7037 passed, 0 failed). | #42 |
| E2 | **Merged.** Per-group `image_profile = minimal\|prototyping` (default minimal). Prototyping bakes `sqlite3`/`chromium`/`zip` (apt) + global `vite`/`create-vite` (npm). `ImageProfile` lives in `copperclaw-types` (shared by db + container-rt). Migration **027** (`027_container_config_image_profile.sql`). Fingerprint fold is *conditional* (only non-minimal contributes hash bytes) so existing minimal groups aren't force-rebuilt on upgrade. Gate green (7047 passed, 0 failed). | #43 |
| R5 | **Merged.** Hot in-session provider failover: host resolves the ordered healthy chain at spawn → `runner.json` `failover_chain` → runner walks `[primary, ...chain]` in `provider_call.rs`'s exhaustion branch, retries the *current* call, HUD notes "switched to <provider>", per-attempt `usage_report` keeps host-side health authoritative. Security boundary: chain limited to entries the container could already reach (no new credentials shipped in); a distinct-key second Anthropic account is excluded + test-pinned. Empty chain = byte-identical pre-R5. Gate green (7034 passed, 0 failed). | #44 |
| G1 | **Merged.** In-chat approvals: approval card emits `approve:<id>`/`deny:<id>` buttons; router interceptor in `route_one` (between sender-scope and mention gate, via the `hooks.rs` pattern — no circular dep) resolves the tapping identity against the Owner/Admin **roles** infra (global or agent-group-scoped), routes through the CLI DB decision path (`decided_by` threaded), persists `pending_approvals.platform_message_id` at delivery and edits the card to "Approved/Denied by <name>". Non-approver taps refused + audited. Race-safe (first resolution wins). Explicit refusal arms added for `one_cli`/`credentialed_external_action`. Slack needed no adapter edit (`build_card_blocks` already emits `actions`). Two fixtures (telegram callback, slack block_action). Own-branch gate: 7013 passed, 9 failed = the known `copperclaw-skills` coverage flake (9/9 pass in isolation). Post-merge fixup `0f26247` rustfmt-wrapped several >100-char lines G1 shipped unformatted (incl. new `approval_intercept.rs`) — the PR's fmt-clean claim was inaccurate; caught by the integrated-gate re-run. | #45 |
| X2 | **Merged.** Golden-fixture gaps closed: new `fixtures/cli/prototype-verify-gate/` genuinely exercises the R3 refuse→fix→pass loop (real gate refusing on cycle counts, then allowing) via T2's `COPPERCLAW_DATA_ROOT` — set through a self-re-exec child (`forbid(unsafe_code)` blocks `set_var`), no product-code change; and `prototype-golden` extended to assert P3's ritual `send_card` + `send_file` screenshot shape. HUD status-row leg stays uncovered (cli not edit-capable; 60s wall-clock) and documented. Gate 7098/0, fmt clean. | #48 |
| V4 | **Merged.** Screenshot-the-preview: renders **host-side container-direct** (`http://<container_ip>:<port>`, NOT the cookie-gated proxy URL — a cookieless render would 403), leaving `preview.rs` untouched (reads `PreviewEntry.container_ip/port`). PNG lands under `<data_root>/screenshots` (`COPPERCLAW_DATA_ROOT`-aware) for in-container `send_file`; new `COPPERCLAW_BROWSER_PREVIEW_ALLOW` injects the bridge IP:port into the child's deny-default egress allow-list; unset `COPPERCLAW_BROWSER_ENABLED` → ritual omits screenshot, never errors. No replay fixture (live path needs Docker/Chromium, same as V3). Gate 7105/0, fmt clean. | #47 |
| P3 | **Merged.** The "prototype ready" ritual: one closing bullet in the static `CODING_PREAMBLE` (Coding/Full only, cache-prefix unchanged, Messaging/Minimal +0 bytes) mandating one `send_card` (title + one-liner + "What to try" + Open-preview URL button + Download `value` button answered next turn with a `git archive` zip via `send_file` + `artifact_path` footer + screenshot alongside via `send_file`). `skills/coding-task` hedge dropped; `skills/send-card` ritual example added. Copy + skills + one prompt line, no new tools. Skills lint 86/0 isolated; gate 7096/0, fmt clean. | #46 |
| E1 | **Merged.** `install_packages` gains `scope: session` (default `image`): runs the ecosystem-local install now (`pip` into `/data/.venv`, `npm --prefix /data/.npm-global -g`) AND records the package into pending image config to bake next spawn; ack text states pending-approval vs done; NEW deny-default-egress-failure detection appends the `cclaw set-egress-allow` hint. Design correction: the "works now" install runs **in-container** (the tool surface's process), not host-side — `ContainerRuntime` has no host→container exec primitive. Real pip/npm e2e is an `#[ignore]`d Docker test; skill teaching deferred (lane P owned skills this wave). Gate 7110/0, fmt clean. | #49 |
| R6 | **Merged.** Progressive final answers: edit-based paced reveal of the FINAL answer only, gated on rich adapter AND elapsed ≥30s AND answer in `280..expander-scale` chars (excludes >30-line/>64KB answers so it never overlaps the long-output collapsible chip). First chunk is a `send_message` anchor, later chunks `edit_message` on that seq — exactly one Chat row, no double-post, reuses H1's `emit_task_hud` pattern. Byte-stable fallback for <30s/bare-adapter. No token streaming (rejected). Gate 7130/0, fmt clean. | #50 |
| V2 | **Merged.** One-tap preview enablement: `PreviewError::Disabled` raises a G1 in-chat "Enable previews" approval card (new `ApprovalKind::EnablePreview` + `apply_enable_preview`, routed through G1's interceptor + shared DB path; flips `preview_enabled`, secure-by-default preserved, idempotent). Amended part 2: idle-reap keeps the listener as a `Live`/`Tombstone` phase machine serving an expired page + a one-shot per-token re-expose while the container is up (the original "re-mint expired token" was impossible — reaping cancels the listener). Gate 7130/0, fmt clean. | #51 |
| C5 | **Merged (part 1; renderer split to C5b).** Adapter rich-surface floors for signal / whatsapp-cloud / mattermost (each a new `render.rs`): mattermost + signal gained `edit_message` → added to `EDIT_CAPABLE_CHANNELS`; whatsapp-cloud correctly stays non-edit-capable (Cloud API can't edit sent messages). Plus the two PR-#30 stale-doc-comment fixes (`emit_breadcrumb`→`emit_task_hud`). Shared markdown renderer + `fence.rs` migration deferred to **C5b** (Wave C). Gate 7148/0, fmt clean. | #52 |
| C5b | In flight (Wave C) — shared markdown renderer in `channels/core` + `fence.rs` migration from C2 (`m18/c5b-shared-renderer`). | — |
| R7 | In flight (Wave C) — write-capable parallel-delegation middle tier (`m18/r7-subagent-fanout`). | — |
| V5 | In flight (Wave C), **HELD for human security sign-off** — public tunnel module, `/security-review` + threat model in PR body (`m18/v5-public-tunnel`). | — |
| M1 | Not started — ABSOLUTE LAST; metrics sweep of all merged PRs (#24-#52 + Wave C). | — |

### R3 history (merged as #32 — skip unless you're touching the gate)

R3 is fully merged. This section is preserved as the implementation map
for anyone touching the verify gate later. The gate as merged:

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

The original two-part design note is preserved below for anyone
re-deriving or auditing the approach; the bullets above are authoritative
for what's in the tree. The live hand-verify smoke test the PR shipped
without is still owed (see "Program-level acceptance").

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
tests, same shape as `todo_env_lock()`. Confirmed still unfixed on
2026-07-16 — T2 now owns this rider.

### Facts later cards need (learned during Waves 1-2 — trust these over the audit anchors)

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
  above). Migration 027 is now TAKEN by E2
  (`container_config_image_profile`); next free migration is **028**.
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
  the runner's collapsible expander (`EXPANDER_LINE_THRESHOLD` = 30,
  `copperclaw-runner/src/tools.rs:1198` — anchor drifted from :1482 as the
  file grew) — keep splitter fixtures under it.
- **T1 correction:** `read_file` already had `offset`/`limit`/`mode` on main;
  T1 added `total_lines` (lines mode only), `shell tail_bytes` (clamped to
  the 32 KiB cap), and truncation hints. Card prose elsewhere assuming "no
  paging existed" is stale.
- **R0 follow-ups:** comment-only references to the deleted floor remain in
  `copperclaw-host/src/container_manager/runner_config.rs:78` and
  `copperclaw-db/src/tables/container_configs.rs:155` — clean up
  opportunistically from the owning lanes.
- **Test baseline** is now ~7,000 (C4b's gate run: 6,999 passed; CLAUDE.md's
  ~6,660 is stale). Gate = zero failures, not a fixed count.
- **Process:** each merged PR's description records "Metrics wishes (for
  M1)". PRs #24-#39 have been swept into the M1 card below (2026-07-16);
  M1 must additionally sweep any PR merged after that date.

## Scope / conflict map (lanes)

A lane is a set of files one team owns for the duration of its cards. Cards
within a lane are **sequential**; lanes run in **parallel**.

| Lane | Owns | Cards |
|---|---|---|
| **R — Runner core** | `crates/copperclaw-runner/src/**` (except `policy.rs` where noted) | R0, H1, R2, R3, R4, R5, R6, R7 |
| **T — Tool surface** | `crates/copperclaw-mcp/src/tools/**` | T1, T2, E1 |
| **P — Prompt + skills** | `crates/copperclaw-host/src/container_manager/prompt.rs`, `skills/**` | P1, P2, P3 |
| **C — Channels + delivery** | `crates/copperclaw-channels/**`, `crates/copperclaw-host-delivery/**`, `crates/copperclaw-host-router/**` | C1, C2, C3, C4a, C4b, R1(router), C5 |
| **V — Preview + browser** | `crates/copperclaw-host/src/preview.rs`, `crates/copperclaw-modules/src/preview.rs`, `crates/copperclaw-browser/**` | V1, V2, V3, V4, V5 |
| **G — Approvals** | `crates/copperclaw-modules/src/approvals.rs`, `crates/copperclaw-host/src/handlers/**` (approvals), approval routing glue | G1 |
| **E — Environment** | `crates/copperclaw-setup/src/steps/image.rs`, `crates/copperclaw-container-rt/**` | E2 |
| **X — Program verification** | `fixtures/**`, new e2e harness files only | X1, X2 |
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

**Verified state (2026-07-16) — the card needs a plumbing design the
original text glossed over.** `FallbackChain` + health live HOST-side and
are hydrated from the central DB per host operation
(`copperclaw-providers/src/failover.rs:94` chain, `:112` `HealthStatus`,
`:374` `HealthMap`; `select` `:477`, `record_failure` `:527`,
`record_success` `:550`; host hydrate/persist in
`container_manager/provider_failover.rs:86`/`:111`). The in-container
runner has NO handle to the chain: mid-turn it only retries the SAME
provider (two retry layers, `provider_call.rs:105` and `:176`,
`MAX_PROVIDER_ATTEMPTS`), then `TurnOutcome::Failed`
(`provider_call.rs:153`, stream path `:302-318`) → apology
(`run/mod.rs:1319`). Failover today happens only *between* turns via the
host's `fold_recent_turns` error classification. So R5's first decision
is the plumbing: recommended shape — the host resolves the ordered
healthy chain at spawn and writes it into `runner.json`; the runner walks
it in `provider_call.rs`'s exhaustion branch (`:130-156`) and reports
which entry served the turn back through `agent_turns` so host-side
health stays authoritative. **Security flag for the PR:** shipping
multiple providers' credentials into the container enlarges the
in-container secret surface — prefer limiting the in-container chain to
entries the group could already reach (same credential or
gateway-brokered), and say so in the PR. `TaskHud::add_note()` is already
shipped and tested awaiting exactly this caller (`hud.rs:188`, doc
`:181-187`). Effort re-rated M → L if the runner.json plumbing is chosen.

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

### T2 (new, follow-up from X1). Unconditional data-root override + mcp test-race fix — P1, S — lane T, before E1

**Problem.** The verify-gate/todo data root is hardcoded `/data` with a
`#[cfg(test)]`-only override (`verify_gate.rs:32` + `:75-78`; `todo.rs:43`
+ `:75-78` — verified 2026-07-16), so the R3 gate is invisible to the
`copperclaw-host` integration-test binary and the X1 golden fixture ships
with the verify-gate leg explicitly uncovered. A previous minimal
un-gating attempt was reverted after security review correctly flagged it
as a capability weakening needing explicit sign-off — that sign-off is
this card's gate, not an afterthought.

**Change.** Mirror the `COPPERCLAW_SHELL_STATE_FILE` precedent
(`computer_use.rs:69-77`, resolver `shell_state_path`): one unconditional
env var (suggest `COPPERCLAW_DATA_ROOT`) consulted by
`verify_gate::data_root()` and `todo.rs`'s path resolution. The PR must
document the threat model (the runner process env is host-controlled at
spawn; an in-container agent's `shell` calls cannot alter it) and record
explicit security sign-off. Rider: serialize `artifact_path.rs`'s two
tests with a `Mutex` guard (same shape as `todo_env_lock()`) — a
confirmed latent race, still unfixed as of 2026-07-16.

**Acceptance.** A host-side integration test can point the gate at a
tempdir and exercise refusal/pass shapes; production behavior with the
var unset is byte-identical; security sign-off recorded in the PR body.

### X2 (new). Close the golden-fixture gaps — P1, S — lane X, after T2

Extend `fixtures/cli/prototype-golden/` (registered at
`crates/copperclaw-host/tests/replay.rs:569` — registration is explicit,
a fixture dir alone is dead data) to cover the two legs X1 shipped
without: the verify-gate refuse → fix → pass loop (testable once T2's
override exists) and, if the harness gains a clock seam, one HUD
status-row emission (the `cli` channel is not edit-capable, so
`Behavior` is always `StatusRows` with a 60s real-wall-clock first fire —
that seam is the hard part; do not block the verify-gate leg on it).
Update the fixture README's "not covered" section to match reality.

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

**Verified state (2026-07-16).** All anchors confirmed in
`crates/copperclaw-host/src/preview.rs`: upgrade detection `is_upgrade()`
`:582-588`; the 501 refusal in `proxy_handler` `:634-640` (fires after
the cookie gate, so only authenticated requests reach it); axum **0.7**
`Router::new().fallback(proxy_handler)` served at `:449-459`, upstream
via `reqwest` with redirects disabled `:444-447`. Cookie gate: cookie
`cclaw_preview` (`:75`), mint path `/__preview/<token>` sets
HttpOnly/SameSite=Lax and 302s (`:611-625`), constant-time compare
(`:659-669`). Gotcha the card text missed: `last_activity` is bumped ONLY
on successfully proxied requests (`:642-643`) — the WS bridge must bump
it on frame traffic (both directions) or an active socket gets reaped at
the 30-min idle timeout (`PREVIEW_IDLE_TIMEOUT` `:71`, reaper `:156-191`).
`skills/preview/SKILL.md:66-69` still teaches "prefer polling" (P2 left
it intact) — remove it in this card's skill touch.

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
approve → retry succeeds. Expired-link unit: one recovery per token;
second reuse gets the terminal "ask the agent to re-expose" page.

**Verified state (2026-07-16) — part 2 of the card as originally written
cannot work; amended.** There is no 30-minute token TTL: the "30 min" is
the *idle reaper* (`preview.rs:71`, `:156-191`), and reaping cancels the
whole per-preview axum listener (`entry.cancel.cancel()`, `preview.rs:240`)
— the host port closes, a stale link gets connection-refused (never a
403), and a "tokened GET re-mints" handler has no listener to run on.
Other facts: `PreviewError::Disabled` arises in `PreviewManager::expose`
(`preview.rs:379-383`) when the group's `container_configs` row is absent
or `preview_enabled=false`; its Display (`modules/preview.rs:77-82`)
already carries the copy-pasteable cclaw fix commands — reuse that text
on the approval card. `preview_enabled` flips via `set_preview_enabled`
(`container_configs.rs:880-900`); the cclaw path is `EDITABLE_SCALAR_FIELDS`
(`cclaw/src/lib.rs:1958`). Live previews already re-expose idempotently
(`preview.rs:390-400`).

**Amended change for part 2 (expired links).** On idle-reap, don't drop
the listener: tear down only the upstream proxying (freeing the
container-side resources the reaper exists to reclaim) and leave the
bound port serving a static "preview expired" tombstone page. A GET of
the original `/__preview/<token>` against the tombstone re-exposes the
same session:port **once per token** iff the session container is still
up (same audit row as a fresh expose); otherwise the page says to ask
the agent to re-expose. Full teardown (port released) still happens on
session stop/close/shutdown, exactly as today. Cookie gate and
constant-time comparison unchanged.

### V3 (= M17-B2). Live browser driver — P0, L — lane V (browser crate), parallel with V1

As M17-B2, unchanged in intent: implement the concrete Chromium/CDP
`BrowserDriver` behind the existing trait. This is the screenshot supply
for P3. Acceptance/tests per M17-B2.

**Verified state (2026-07-16) — the M17 anchors were wrong; corrected.**
The trait is `BrowserDriver` at `copperclaw-browser/src/driver.rs:57-66`;
only a `#[cfg(test)]` `MockDriver` exists (`driver.rs:123-146`) — no CDP
code anywhere in the crate. The SSRF preflight orchestration is
`render()` at `driver.rs:74-113` (target preflight `:82-85`,
per-redirect re-guard `:97-99`) — NOT `lib.rs:21-26`, which is the doc
comment describing the deferred runtime path. There is no `spec.rs` in
this crate (the plan conflated `copperclaw-container-rt`'s): the child
sandbox spec is `build_browser_container_spec()` at
`container.rs:126-156` — deny-default egress `:143-144`, forbidden-env
allow-list `:35-47`, unprivileged user 65534, `copperclaw.role=browser`
orphan-sweep labels `:136-137`. The MCP tool is named **`browser_render`**
(`copperclaw-mcp/src/tools/browser_render.rs`), gated by
`COPPERCLAW_BROWSER_ENABLED` (`:54`, `:125-135`, `:200-203`); today it
runs every safety step then errors "driver not provisioned" (`:290-295`).
V3 therefore has three concrete legs: (a) the CDP driver impl behind the
trait, (b) the privileged spawn path that actually runs the built spec
(explicitly deferred at `container.rs:23-25` — no `runtime.spawn` call
exists), (c) replace `handle()`'s terminal error with the live render.

### V4. Screenshot-the-preview path — P1, S — lane V, after V1 + V3

**Change.** Teach the flow end-to-end and remove the seams: `browser_render`
against the preview URL (or `http://<container_ip>:<port>` host-side —
decide and document which; the browser child container must be able to reach
it under deny-default egress, allow-list the preview host:port at spawn).
Resulting PNG lands under `/data`, agent relays via `send_file`. If
`COPPERCLAW_BROWSER_ENABLED` is unset, the P3 ritual degrades to no
screenshot — never an error.

**Acceptance.** e2e with mock driver: expose → render → PNG exists →
ritual close carries it (via `send_file`, see verified note). Deny-default
egress fixture proves the allow-list injection.

**Verified state (2026-07-16).** `egress_allow_for()`
(`browser_render.rs:180-188`) currently allow-lists only the navigation
target's host:port (one entry, port defaulting to 443); the injection
point for the preview host:port is the `egress_allow` vec built at
`browser_render.rs:231-237` and consumed at `container.rs:144`. The
preview's host port lives on `PreviewEntry.host_port` (`preview.rs:113`).
One correction to the card prose: `send_card` cannot attach a local
file — its image field is `image_url` and must be http(s)
(`interactive.rs:210`) — so the screenshot PNG reaches the user via
`send_file`, sent alongside the P3 ritual card (P3's text is amended to
match). "Screenshot in the card" would require serving the PNG over the
preview URL; don't build that for v1.

### G1. In-chat approvals — P0, L (re-rated from M after audit) — lane G

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

**Verified state (2026-07-16) — the "plumbing exists on both ends" claim
is half true; the card grew, hence the re-rate.** What exists: button-tap
*ingestion* is fully plumbed — telegram `callback_query_to_event`
(`ingress/mod.rs:111-175`) and slack `parse_block_actions`
(`events/router.rs:263-380`) both synthesize `Chat` rows carrying
`content.callback`, and the mention gate whitelists them
(`mention.rs:157-163`, `is_interaction_payload`). What does NOT exist,
each a required leg of this card:

- **The approval card is buttonless today.** `ApprovalCardHandler`
  (`approvals.rs:373-424`, registered as delivery action `"approval_card"`
  at `:488`) emits `{card:{type:"approval",approval_id,title}}` — no
  buttons, no callback payloads. G1 adds `approve:<id>` / `deny:<id>`
  buttons here. On Slack the card path additionally doesn't emit
  `actions` blocks at all yet (`slack/adapter.rs:745-747`) — G1 adds
  that (ADAPTER-slack file: coordinate the single-file touch with lane C).
- **No approver identity.** CLI resolutions hardcode `decided_by="host"`
  (`handlers/approvals.rs:146-147`), and the "approver resolution the
  notifier does" is really just "post to the agent group's primary
  messaging group" (`boot.rs:63-156`) — no operator/approver set exists.
  G1 must define the check (recommended: reuse the Owner/Admin roles
  infra in `handlers/roles.rs`; fall back to registered-sender-in-
  primary-group only if roles prove too coarse — decide and document)
  and thread the tapping identity into `record_decision`.
- **No card-edit round-trip.** `edit_message` edits TEXT only and is
  keyed by outbound seq (`host-delivery/src/service.rs:1278-1310`); the
  approvals module never learns its card's seq or platform id. The
  `pending_approvals.platform_message_id` column already exists
  (surfaced at `handlers/approvals.rs:582`) but nothing writes it — G1
  persists the card's message id there at delivery time, and the
  "Approved by <name>" update is a text edit (native buttons vanish on
  edit on most platforms — acceptable: the disabled state IS the text).
- **Two decision logs exist** — the in-memory `ApprovalsModule`
  (`approvals.rs:291-349`) and the DB `pending_approvals` the CLI writes.
  Route the interceptor through the same DB path as the CLI handler
  (`handlers/approvals.rs:92-154`) so they can't diverge. While in that
  dispatcher: it switches on action *strings* (`:131-143`) and has no
  arms for `one_cli` / `credentialed_external_action` — add explicit
  refusals at minimum (V5 needs the latter arm for real).
- **Interceptor insertion point confirmed:** `host-router/src/route.rs`,
  between the sender-scope gate (ends `:422`) and the mention gate
  (`:424`), registered via the `hooks.rs` gate pattern so the modules
  crate supplies it without a circular dep.

### P3. The "prototype ready" ritual — P0, S — lane P, after P2; graceful w.r.t. V-lane timing

**Problem.** Even with preview + files + cards all shipped, nothing makes the
agent *end* a build with a coherent demo — results arrive as whatever prose
the model felt like.

**Change.** Prompt (P1 block) + `skills/coding-task` make the final step of
every build todo list mandatory and concrete — one `send_card`:
title + one-liner, "What to try" bullets, screenshot sent alongside via
`send_file` (when V4 available — see verified note: cards cannot attach
local files), buttons: **Open preview** (URL), **Download** (triggers
`send_file` zip of the project, sans `node_modules`/`.git` — document the
`git archive` idiom), and the `artifact_path` host path in the footer for
desk users. Card degrades by capability: no preview → no button, never a
broken link. This card is copy + skill + one prompt line — no new tools.

**Acceptance.** X1 golden fixture asserts the ritual card shape (new
`#[tokio::test]` registration in `tests/replay.rs` if a new fixture is
added — registration is explicit). Skill lint green.

**Verified state (2026-07-16).** Everything needed exists: `send_card`
(`copperclaw-mcp/src/tools/interactive.rs:107`, schema `:138-217` —
title/body/fields/buttons, each button exactly one of `value` (≤64 bytes)
or `url`; validated via `Card::validate()`). Its image field is
`image_url` and must be http(s) (`interactive.rs:210`) — a local PNG
cannot ride the card, hence the `send_file`-alongside wording above.
`artifact_path` returns JSON `{container_path, host_path, note}`
(`artifact_path.rs:99-103`). `skills/coding-task/SKILL.md:129-134`
already teaches the mandatory tool-based close and names this card:
"(A richer close card is forthcoming — P3 ...)" — drop that hedge and
teach the `send_card` ritual in its place. `skills/send-card/SKILL.md`
(pre-M18) documents the schema, per-channel rendering, and degradation
(text fallback via `Card::to_text_fallback`, `card.rs:349-410` — a URL
button degrades to a `- [Label] -> https://...` text line, never a broken
native button); extend it with the ritual example rather than
duplicating schema docs into coding-task.

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

**Verified state (2026-07-16) — two corrections.** Current flow:
`install_packages` (`self_mod.rs:6`, handler `:47-76`; schema is
apt/npm/reason only — no `scope` field exists) emits an approval request
(`OutboundToolEffect::InstallPackages`); on approval,
`apply_install_packages` (`handlers/approvals.rs:392-432`) merges into
`container_configs.packages_apt/_npm` and deliberately does NOT rebuild —
the rebuild is lazy at next spawn via the `config_fingerprint` compare
(`spawn.rs:274-343`; fingerprint over apt+npm+skills+mcp_servers,
`container_configs.rs:640-664`). `HOME=/data` confirmed at
`spawn.rs:623-624`. Corrections: (1) session-scope installs still ride
the same approval kind — the "works now" install executes only after the
approval resolves (or instantly once G1's in-chat approve exists); the
tool's ack text must say which state it's in. (2) "Deny-default egress
failures return the allow-list hint" — **no such hint exists anywhere
today**: denial is a raw network failure at the DNS/nftables layer
(`egress.rs:78-84`, spec `EgressMode` at `container-rt/src/spec.rs:121`;
the only agent-facing network message is the SSRF guard in
`net_guard.rs`, which is unrelated). The hint is NEW work in this card:
detect the failure in the session-scope install path and append the
`cclaw` egress-allow command to the tool error.

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

**Verified state (2026-07-16).** `DEFAULT_BASE_APT_PACKAGES` is exactly
at `image.rs:184-215` (anchor still good; base `debian:trixie-slim`,
`:33`); the no-apt-egress-at-runtime rationale is the comment at
`image.rs:209-213`. There is NO `image_profile` anywhere — net-new. Two
design constraints the card text implied but didn't state: (a) it lives
per-group in `container_configs` alongside `packages_apt`/`packages_npm`,
so **this card takes migration 027** (verify at branch time); (b) it MUST
be folded into `compute_fingerprint` (`container_configs.rs:640-664`) or
profile changes won't trigger rebuilds — note `tool_profile` is
deliberately fingerprint-EXCLUDED (runner-config-only); do not copy that
precedent. `packages_npm` merges via `add_package_npm`
(`container_configs.rs:611-631`). Two distinct fingerprints exist — the
setup/base-image Docker label (`image.rs:51`, pull-verification only) and
the per-group config fingerprint; this card touches the latter.

### R6 (= M17-A3, re-scoped). Progressive final answers — P2, M — lane R, after R5

M17-A3 shrinks once H1 exists: the HUD already covers "something is
happening." What remains is long *final* text landing all at once. Implement
A3's edit-based growth only for the final answer of turns that already ran
>30s, rich adapters only, default ON, same acceptance as M17-A3 otherwise.

**Verified state (2026-07-16).** The final answer is one terminal
`SendMessageSpec` emit in `drive_turn.rs:392-430` (when
`output.tool_calls.is_empty()`); no incremental growth exists. The
machinery to reuse is H1's post-once-then-edit anchor:
`ToolContext::emit_task_hud(breadcrumb, first)` (`context.rs:634`, doc
`:625-633` — `first:false` becomes an `edit_message` on edit-capable
adapters, anchored by the stable `tool_name`), driven from
`hud.rs:307` (`emit_live_update`) / `:259` (`finalize`). Rich-adapter
detection is `copperclaw_channels_core::capabilities::supports_message_edit`
(`capabilities.rs:45`). Elapsed time already exists as
`TaskHud.started_at` (`hud.rs:110`, read at `:278`) — thread it, don't
re-track.

### R7 (= M17-A6). Subagent fan-out + write-capable delegation — P1, L — lane R, after R6

As M17-A6 (unchanged): `explore` stays read-only; the gap between it and full
`create_agent` containers gets a middle tier for parallel build work using
the existing worktree mechanics. Sequenced last in lane R because everything
earlier changes `drive_turn` under it.

**Verified state (2026-07-16) — anchors corrected.** The two ends of the
gap: `explore` (`copperclaw-mcp/src/tools/explore.rs`) is an in-process,
ephemeral, bounded LLM loop — read-only allowlist
`["grep","glob","read_file","web_fetch"]`, max 5 turns / 50K tokens / 60s
defaults, nesting refused, returns one string, touches no DB/container.
`create_agent` (`copperclaw-modules/src/agent_to_agent/create_agent.rs`)
is a persistent sibling: own `agent_groups`/`sessions` rows, own
container via the reconcile loop, permission-gated, depth-capped
(`depth.rs`), writable git worktree of the parent repo on branch
`sib/<session-id>`. The worktree mechanics drifted from the M17 anchor:
call site `apply_parent_workspace(...)` at `spawn.rs:708`, the functions
at `spawn.rs:1280` (`apply_parent_workspace`) and `spawn.rs:1546`
(`provision_parent_worktree`, worktree dir
`<repo>/.copperclaw/wt/<sibling_sid>`). There is no middle tier and no
parallel-explore orchestration primitive today — that's the card.

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

**Verified state (2026-07-16).** Zero tunnel-related code exists anywhere
(repo-wide grep: no tunnel/cloudflare/ngrok/frp hits) — fully greenfield.
`ApprovalKind::CredentialedExternalAction` exists
(`copperclaw-types/src/approval.rs:19`), but note the CLI approve
dispatcher switches on action *strings* and has no arm for it
(`handlers/approvals.rs:131-143`) — G1 adds at least a refusal arm; V5
adds the real one. Module shape to follow: implement the `Module` trait
(`modules/src/context.rs:21`, `install` at `:26`), `pub mod` in
`modules/src/lib.rs:23-33`, registered in the host boot sequence in
priority order.

### C5 (= M17-C5 + C6). Adapter floor + shared markdown renderer — P2 — lane C, after C4a/C4b

Unchanged from M17: raise signal/whatsapp/mattermost to the rich-surface
floor (three parallel sub-cards, disjoint adapter dirs), then the shared
markdown→per-platform renderer in `channels/core`. Sequenced last in lane C
because C2/C3 change the surfaces it would render onto. M18 addition: the
renderer must own the fence-handling logic from C2 when it lands (C2's
splitter hooks migrate into it; note in both PRs).

**Verified state (2026-07-16).** Per-adapter gaps (all three fall through
to the trait-default text fallbacks for `deliver_card` / `deliver_diff` /
`deliver_collapsible` / `deliver_todo_list` / `deliver_thinking` /
`deliver_error`, and none overrides the `edit_message` trait method — so
none gets the H1 HUD): **signal** overrides only `set_typing`
(`adapter.rs:283`; edit rides `deliver_action`, `:332`);
**whatsapp-cloud** overrides only a `set_typing` read-receipt shim
(`:165`; edit explicitly Unsupported); **mattermost** overrides nothing
rich (deliver-only; edit via a `deliver` action PATCH). The floor is the
trait-default set in `channels/core/src/adapter.rs:124-429`. When an
adapter gains `edit_message`, add its channel to `EDIT_CAPABLE_CHANNELS`
(`capabilities.rs:38`, currently 5 entries) **in the same PR** — the
module carries an explicit keep-in-sync rule (`:26-32`). The shared
renderer is fully greenfield: no markdown module exists in
`channels/core` (each adapter formats its own — telegram
`markdown_to_html`, discord `escape_discord_markdown`, slack mrkdwn); the
only shared piece is `Card::to_text_fallback` (`card.rs:349`). `fence.rs`
is ready to absorb as planned — self-contained, dependency-free, with an
explicit C5 migration note at `fence.rs:22-24`. Also owed here (PR #30
flag, confirmed still present): fix the two stale doc comments naming the
removed `emit_breadcrumb`/`emit_breadcrumb_finish` at
`host-delivery/src/service.rs:1707` and `:2378`.

### M1. Metrics rider — P2, M — lane M, absolute last

Sweep the metrics wishes recorded in each merged PR's description into
`copperclaw-metrics` in one card. One PR, no other card touches the crate.
The wishes from PRs #24-#39 were swept on 2026-07-16 and are inlined
below (M1 must re-sweep any PR merged after that date, including the
Wave 3/4 PRs, which will add: preview WS upgrades, approval taps
approved/denied/unauthorized, session-scope installs, tunnel exposures):

- **#24 (C1):** `slack_typing_skipped_total{reason="non_assistant_surface"}`;
  `slack_typing_set_status_total{result}`; HUD-decision counts labeled by
  `typing_indicator_visible`.
- **#25 (T1):** shell truncation events by mode (head/tail); `read_file`
  lines-mode calls + pages-per-file distribution; histogram of pre-cap
  stream size on truncated shell calls.
- **#26 (P1):** sessions spawned per tool profile;
  `load_skill("coding-task")` invocations with the inline block present
  vs absent; system-prompt byte size per spawn by profile.
- **#27 (R0):** policy denials labeled by layer (role/skill/profile/
  provenance) and tool name; "Unknown tool" dispatch errors by tool name.
- **#28 (C2):** `copperclaw_delivery_fence_split_total{channel_type,kind}`;
  `copperclaw_delivery_fence_unbalanced_input_total{channel_type}`.
- **#29 (R1):** `copperclaw_slash_commands_total{command,channel_type}`;
  `copperclaw_control_rows_pending` gauge;
  `copperclaw_status_answer_seconds` histogram.
- **#30 (H1):** `copperclaw_hud_posts_total{agent_group}`;
  `copperclaw_hud_edits_total{agent_group,trigger}`;
  `copperclaw_hud_degraded_total{channel_type,reason}`;
  `copperclaw_hud_finalize_seconds` histogram.
- **#31 (R2):** counter for mid-turn stops vs interjections consumed.
- **#32 (R3):** verify-gate counter labeled
  `outcome=refused|blocked|passed` (+ verify-run pass/fail).
- **#33/#37/#38 (C3/C4a/C4b):** per-channel inbound files materialized +
  failures (`channel`, `outcome=ok|too_large|download_failed`); histogram
  of downloaded attachment bytes per channel (tunes
  `max_attachment_bytes` defaults).
- **#34 (X1):** preview-expose calls served vs timed out
  (`outcome=served|timeout`).
- **#39 (R4):** `compaction_triggered_total{profile}`;
  `compaction_estimated_tokens` histogram at trigger;
  `compaction_facts_header_bytes` histogram.
- (#35, #36 recorded no wishes.)

---

## Wave summary

| Wave | Cards | Status / parallel lanes |
|---|---|---|
| 1 | R0→H1 (R); R1 (C router); C1, C2 (C); T1 (T); P1 (P) | **Done** — all merged |
| 2 | R2→R3→R4 (R); C3→C4a+C4b (C); P2 (P); X1 (X) | **Done** — all merged; T2 #40 merged, X2 now unblocked |
| 3 | V1‖V3→V4 (V); G1 (G); V2 (V, after G1); P3 (P); R5 (R); T2→X2 | **V1 #41, V3 #42, R5 #44, T2 #40, G1 #45 all merged.** V4/V2/P3/X2 now unblocked; V4 (needs V1+V3) is the next critical-path card |
| 4 | E1 (T, after T2); E2 (E); R6→R7 (R); V5 (V); C5 (C); M1 last | E2 #43 merged (migration 027); E1 (needs T2 — now merged) + rest ready |

Remaining critical path to the demo moment: **V3 → V4 → P3** (V3 is the
long pole — the only L-sized greenfield build left on the path), with
**G1 → V2** required for the phone-only enablement leg and **T2 → X2**
required for program acceptance. R5-R7, E1/E2, C5, V5 parallelize around
it. (R5 was originally sequenced in Wave 2's lane R; it remains unstarted
and now runs alongside Wave 3 — nothing depends on it.)

## Program-level acceptance

The X1 golden fixture passes (extended by X2 to actually cover the
verify-gate leg), and a live smoke on the telegram dev group (`CLAUDE.md`
"Operating a live agent") demonstrates end-to-end: "build me a tiny web
todo app" → HUD visible within seconds → `/stop` + resteer honored →
verify gate blocks a fake completion → preview link opens from the phone →
ritual card + screenshot file → `cclaw audit list` shows the approval and
preview rows.

Still owed as of 2026-07-16: the live smoke test has never been run (R3
merged without it; nothing since has run it either), and X2 is what makes
the fixture cover the gate. Neither blocks card work; both block calling
the program done.

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
