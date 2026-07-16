# M19 — Channel-parity, legible-feedback, and advanced-capability program

Goal: after M18, a user can text **"build me X"** and watch a prototype get
built and proven. M19 makes that experience *the same on every channel*, makes
the agent *legible* — the user always knows what it's doing, whether it's
blocked, and what to do about it — and lifts the ceiling on what a single
request can accomplish (parallel builds, interactive browsing, public sharing,
self-improving skills, durable memory and autonomy).

Three themes, one per wave:

1. **Correct, consistent feedback everywhere** — kill the silent gaps and the
   capability drift so no channel lies about what it can show and no wall the
   agent hits is invisible to the user.
2. **Every channel a first-class surface** — raise the thin/bare adapters to
   the rich floor, retire the duplicated per-adapter formatting onto the shared
   renderer that already exists, and turn inbound reactions into real input.
3. **Advanced capabilities** — parallel fan-out with a join, interactive
   browsing, the public tunnel's missing agent verb, agent-authored skills,
   agent-written memory, and durable/event-driven autonomy.

Written 2026-07-16 from a three-subsystem audit of `main` at `4584e7f`
(channels/adapters, feedback/UX surfaces, capability surface). Like M18 this
document is written to be executed by **parallel teams/agents, one task card
per implementer**; each card declares an exclusive scope and two cards in the
same wave never share a scope. Read the whole preamble before taking a card.

## Relationship to M18 (`docs/plans/m18-prototype-builder-program.md`)

M18 is **complete**: every card merged, including V5 (the public-tunnel
module — merged as PR #55, commit `0904f27`; the M18 doc's "held for sign-off"
line predates the merge and is stale). The live Telegram demo-moment smoke
passed. Post-M18 hardening also landed: PR #57 (todo-store write race) and
PR #58 (HUD no-op edit suppression). M19 does **not** re-open any M18 card; it
builds on the surfaces M18 shipped (Task HUD, in-chat approvals, progressive
answers, the prototype-ready ritual, the verify gate, the browser render core,
`delegate`, the tunnel module) and closes the gaps the M18 audit deferred:

- The **inbound-reactions-as-steering** item M18 parked as "revisit post-M18"
  (M18 "Deferred / rejected").
- The **preview-exposure taint question** M18's live post-mortem flagged as "a
  candidate M19 item" (M18 bucket 2).
- The **`blocked` todo wire-status** extension M18's R3 note explicitly
  deferred as needing lane-C coordination (M18 "R3 history", design-decision
  bullet).
- The **first-class `scheduled_tasks` table** deferred in `PLAN.md`.
- The **interactive browser (Phase 5b)** M16 committed as demand-pull stretch
  (`PLAN.md` M16 Phase 5b; a non-goal only in its *always-on* form).

## Rules for every implementing team

Identical to M18 — re-read `CLAUDE.md`, all of it applies:

1. `cargo fmt --all && cargo check --workspace && cargo clippy --workspace
   --all-targets -- -D warnings && cargo test --workspace --no-fail-fast`
   green before done. Baseline is **~7,200 tests** (M18's final gate was
   7206). Gate = zero failures, not a fixed count; do not regress it.
2. Workspace forbids `unsafe_code`; clippy warnings are errors. Pinned
   toolchain Rust 1.85 / edition 2024.
3. Every user-visible change gets a `CHANGELOG.md` line under `## [Unreleased]`.
   The changelog is a merge hotspot — write your line last, keep it to your
   card, resolve conflicts keep-both.
4. **No stubs in tree.** A smaller whole thing beats a half-wired big one. A
   registered tool works end-to-end.
5. **Secure-by-default (tenet unchanged from M18).** New capability is opt-in
   unless the card explicitly changes a default. External content marks the
   turn untrusted (`mark_untrusted_context`); host-state mutations write audit
   rows; any outward-facing surface (A2 interactive browser, A3 public verb)
   requires a `security-review` pass recorded in the PR before merge.
6. **New DB state = a new numbered migration.** Next free is **028** — verify
   at branch time (A6 is the only card known to need one). Never edit a
   released migration.
7. **Fixtures before pipeline changes.** Any card touching
   inbound → router → runner → outbound → delivery adds/extends a replay
   fixture under `fixtures/` and registers it in
   `crates/copperclaw-host/tests/replay.rs` (registration is explicit — a
   fixture dir alone is dead data).
8. **File:line anchors are from the 2026-07-16 audit** — orientation, not
   gospel; verify before editing.
9. **PR per card, branch off `main`, one card per branch.** Operator merges
   agent PRs with merge commits (house style). Re-run the *integrated* gate
   after a multi-PR merge — a green per-branch gate does not prove the union is
   green or even fmt-clean (an M18 lesson that bit twice).

## Scope / conflict map (lanes)

Same lane discipline as M18 — a lane is a set of files one team owns for the
duration of its cards; cards within a lane are **sequential**, lanes run in
**parallel**. `copperclaw-metrics` is a hotspot: **no card except M1 edits it**;
cards record wanted metrics in their PR description and M1 sweeps them.

| Lane | Owns | Cards |
|---|---|---|
| **R — Runner core** | `crates/copperclaw-runner/src/**` (esp. `run/hud.rs`, `run/mod.rs`, `run/drive_turn.rs`) | F2, F5, F6, A1(runner half) |
| **C — Channels + delivery** | `crates/copperclaw-channels/**`, `crates/copperclaw-host-delivery/**`, `crates/copperclaw-host-router/**`, `channels/core/**` | F1, F4, U1–U7 |
| **T — Tool surface** | `crates/copperclaw-mcp/src/tools/**` | A1(tool half), A5, A6(tool half) |
| **P — Prompt + skills** | `crates/copperclaw-host/src/container_manager/prompt.rs`, `crates/copperclaw-skills/**`, `skills/**` | A4 |
| **V — Preview + browser** | `crates/copperclaw-browser/**`, `crates/copperclaw-host/src/preview.rs`, `crates/copperclaw-modules/src/{preview,tunnel}.rs` | A2, A3, A7 |
| **G — Approvals** | `crates/copperclaw-modules/src/approvals.rs`, `crates/copperclaw-host/src/{approval_intercept.rs,handlers/approvals.rs}` | F3 |
| **H — Host sweep** | `crates/copperclaw-host-sweep/**` | A6(sweep half) |
| **X — Program verification** | `fixtures/**`, new e2e harness files only | X-riders per wave |
| **M — Metrics rider** | `crates/copperclaw-metrics` | M1 (single card, absolute last) |

Cross-lane touches are declared on each card. Two structural cross-lane
coordinations recur and are called out where they land: **F4** extends the
portable `TodoItemStatus` wire enum that ~8 adapter crates exhaustively match
on (a fan-out); **A1** needs a runner-side join seam (lane R) under a new tool
(lane T).

---

## Wave 1 — "correct, consistent feedback everywhere"

Kill the drift and the silent gaps. Mostly bug-fixes and small floors; low risk,
high felt-quality. Six cards, all parallelizable except where lane R serializes.

### F1. Reconcile `EDIT_CAPABLE_CHANNELS` with real trait edit support — P0, S — lane C

**Problem.** `matrix` and `webex` are listed in `EDIT_CAPABLE_CHANNELS`
(`channels/core/src/capabilities.rs:38-46`), so `supports_message_edit()`
(`:53`) promises the M18 HUD they can edit in place — but neither overrides the
trait `ChannelAdapter::edit_message` (`grep -c "async fn edit_message(" = 0`
for both `matrix/src/adapter.rs` and `webex/src/adapter.rs`). They edit only
through their internal deliver-action `"edit"` arm (`matrix/src/adapter.rs:848`,
`webex/src/adapter.rs:222` → `api.edit_message`). The host's HUD/approval path
calls the **trait** method (`host-delivery/src/dispatch.rs:131`, edit-action
path `service.rs:1337`); for matrix/webex those hit the trait default →
`AdapterError::Unsupported` + `inc_hud_edit(_,"unsupported_fallthrough")`
(`core/src/adapter.rs:429,439`). So the HUD silently never edits on two
channels that advertise that it will.

**Change.** Override the trait `edit_message` on both adapters to route to their
existing internal edit path (reuse `api.edit_message`). Add a compile-or-test
drift guard: a test asserting **every** channel in `EDIT_CAPABLE_CHANNELS`
overrides the trait `edit_message` (same spirit as R0's name-drift test), so
this class of lie can't recur.

**Acceptance.** Unit: matrix/webex trait `edit_message` succeeds against the
mock API; the drift-guard test fails if a channel is added to the list without
a trait override. HUD live-edit fixture on matrix.

### F2. Surface actionable "I'm blocked" walls to the user — P0, M — lane R (+ `channels/core::error_card`, read-only)

**Problem.** Tool errors, policy/profile denials, provenance/taint denials,
verify-gate refusals, and egress denials are **model-only** — they render into
the model's history as `Tool{is_error:true}` and the user never sees them
(`mcp/src/tools/tool_dispatch.rs:47-50`; test
`policy_denied_tool_produces_refusal_in_history`, `run/mod.rs:2295-2330`;
verify-gate refusal `todo.rs:708-715`; egress hint `self_mod.rs:248-259`). The
hints are well-written but land where the user can't read them. When the model
then loops or silently gives up, the user is left staring at a HUD that just
stops — indistinguishable from a hang.

**Change.** When a turn ends **without** a user-facing reply and its tail is a
run of denials/errors on the same blocker (not a single recovered error), emit
one compact user-facing card via the existing `ErrorCard` surface
(`channels/core/src/error_card.rs`) summarizing *what* is blocked and the
*actionable* next step — reuse the already-good hint strings (egress-allow
command, "write a `.copperclaw/verify`", "this needs your approval"). Do **not**
echo raw internal error text verbatim (avoid leaking internals / injected
content); map to a curated per-blocker message keyed by the denial category.
Never fire on a turn that produced a normal answer.

**Acceptance.** e2e: a scripted turn that hits a hard egress denial and gives up
produces exactly one user-facing "couldn't reach the registry — ask an operator
to allow it" card; a turn that recovers produces none; a normal turn is
byte-stable. No raw `ToolError` string reaches the user.

### F3. Approval-card correctness + blocked-on-approval legibility — P0, M — lane G

**Problem.** Three approval-UX gaps from the audit:
- **Card stuck live after resolution.** `approval_intercept.rs:285-291`: if no
  `platform_message_id` was recorded at delivery, the "Approved by X" edit is
  silently skipped — the decision is applied but the card still shows live
  Approve/Deny buttons.
- **Silent conflict race.** A second (losing) tapper gets **no reply at all**
  (`approval_intercept.rs:241-256`) — their tap looks broken.
- **Silent expiry / invisible block.** An approval expires at
  `DEFAULT_APPROVAL_TTL` (`approvals.rs:243-246`) with no nudge, and an agent
  blocked waiting on approval is indistinguishable from a hung one (F2's
  sibling on the approval axis).

**Change.** (a) Root-cause and fix the `platform_message_id` skip so the card
always edits to its resolved state (persist the id reliably at delivery; if a
platform genuinely can't return one, fall back to a follow-up "Approved by X"
reply rather than leaving live buttons). (b) Give the conflict loser a short
"already resolved by <name>" reply. (c) On TTL expiry emit a terminal "this
request expired — ask the agent to try again" card edit. (d) Add a "waiting for
your approval" note to the HUD (`TaskHud::add_note`, `hud.rs:221-225`) so a
blocked agent is legible, not silent.

**Acceptance.** Fixtures (telegram callback, slack block_action): approver tap →
card edits to resolved even when id resolution is on the fallback path; loser
tap → "already resolved"; expiry → terminal card; HUD shows the waiting note.
CLI path still works and races safely (first wins).

### F4. Make `blocked` todo state visible to users — P1, M — lane C (+ `mcp/src/tools/todo.rs`, one function, coordinate)

**Problem.** M18's R3 added a real `TodoStatus::Blocked` (with `blocked_reason`)
to the runner's local store but deliberately **did not** extend the portable
wire enum — `status_to_wire` maps `Blocked → InProgress` in the rendered chip
(`todo.rs:252-264`), so a user watching the pinned checklist sees a step stuck
"in progress" forever when it actually auto-blocked after burning its verify
fix-cycles. The real status + reason live only in model-facing JSON. R3's note
explicitly flagged this as owed lane-C coordination once the shared renderer
landed (it has — C5b).

**Change.** Extend `copperclaw_channels_core::TodoItemStatus` with a `Blocked`
variant; give it a distinct glyph in the text fallback (`todo_list.rs:91-97`,
e.g. `[!]`) and a short one-line reason where the rich `deliver_todo_list`
surfaces allow it. Update the exhaustive matches across the ~8 adapter crates
(this is the fan-out R3 avoided) and map `Blocked → Blocked` in `status_to_wire`.

**Acceptance.** A todo that auto-blocks renders as blocked (not in-progress) on
every adapter's chip and text fallback; the exhaustive-match update compiles
workspace-wide; a fixture shows the blocked glyph + reason on telegram.

### F5. HUD covers the pre-first-tool and pure-reasoning wait — P1, M — lane R (`run/hud.rs`), after F2

**Problem.** The HUD posts only at the **first tool call** (`on_batch_start` →
first `emit_live_update`), and `finalize` skips entirely when `tool_runs == 0`
(`hud.rs:331-335`). A multi-minute pure-reasoning answer on an edit-capable
channel shows **nothing** until the answer lands — the ticker only starts after
the first batch (`:275`). Progressive reveal (R6) covers the *answer* landing,
not the *thinking* wait before it.

**Change.** On edit-capable channels, post an initial "thinking…" HUD frame
after a short threshold from turn start (suggest 5–8s, so fast turns stay
byte-stable and never post), with the elapsed-clock ticker running from then.
On a zero-tool turn, finalize that frame (or fold it into the answer) instead of
leaving it dangling. Keep `hud_mode` semantics and the no-op-edit suppression
(`hud.rs:526-558`) intact.

**Acceptance.** Unit: a scripted 20s pure-reasoning turn on a mock edit-capable
adapter posts a thinking frame within the threshold and finalizes it; a <5s turn
posts nothing (byte-stable); `hud_mode=off` unchanged.

### F6. Richer bare-channel status + an intermediate stuck signal — P1, S — lane R (`run/hud.rs`), after F5

**Problem.** On bare / edit-incapable channels the only progress signal is the
60s status row, a **fixed string** with no progress detail
(`hud.rs:444-465`: "Still working … N tool calls … latest: X"). And there's
nothing between the HUD ticker and the 5-minute apology (`apology.rs`,
`APOLOGY_AFTER_SECS=300`) — a slow build looks fine for 5 minutes, then
suddenly apologizes.

**Change.** (a) Fold the Live-HUD detail line (current todo step + `done/total`)
into the bare-channel status row so bare channels get real progress, not a fixed
string. (b) Add an intermediate soft "this is taking longer than usual, still
going" refinement to the status row past a threshold (well short of the 5-min
apology), so the bare-channel experience degrades gracefully instead of cliff-
edging into an apology.

**Acceptance.** Unit: the bare-channel status row carries the todo step when one
exists; the intermediate message fires past its threshold and not before; child-
agent sessions still get no status spam (`context.rs:718-735` unchanged).

### X-rider (Wave 1). Feedback fixtures — P1, S — lane X

Extend the golden and per-channel fixtures to lock the new feedback: F1
matrix HUD edit, F4 blocked-todo rendering, F5 thinking-frame emission. Register
each in `tests/replay.rs`.

---

## Wave 2 — "every channel a first-class surface"

Ten of 21 adapters are "rich"; eleven are bare (text fallback for every rich
surface), and the shared markdown renderer that exists is used by **no** adapter.
This wave closes the parity gap. Adapter-floor cards are disjoint directories —
run them in parallel like M18's C5 sub-cards.

### U1. Signal rich-surface floor — P1, S — lane C (ADAPTER-signal)

**Problem.** `signal` is the thinnest "rich" adapter: it overrides only
`deliver_card` (`adapter.rs:371`) and `edit_message` (`:337`). `deliver_diff`,
`deliver_collapsible`, `deliver_todo_list`, `deliver_thinking`, `deliver_error`,
and `deliver_breadcrumb` all fall through to plain text — so the HUD, diffs, and
todo chips render as bare prose.

**Change.** Add a `render.rs` (mirroring the C5 pattern) implementing the six
missing `deliver_*` surfaces natively where Signal supports them; keep text
fallback where it genuinely can't.

**Acceptance.** Per-surface unit against the mock Signal server; a HUD fixture
shows an in-place breadcrumb rather than stacked prose.

### U2. Teams in-place edit + reactions — P1, S — lane C (ADAPTER-teams)

**Problem.** `teams` overrides cards/diff/collapsible/todo/thinking/error but
**not** the trait `edit_message` — so it's not in `EDIT_CAPABLE_CHANNELS`, gets
no live HUD, and every "edit" becomes new-message spam. It also lacks
`add_reaction` and `plain_text_fallback`.

**Change.** Override trait `edit_message` (Teams supports message updates), add
it to `EDIT_CAPABLE_CHANNELS` **in the same PR** (the module's keep-in-sync
rule, `capabilities.rs:26-32`), add the `add_reaction` trait override and
`plain_text_fallback`.

**Acceptance.** Teams live-HUD fixture edits one message ≥N times instead of
posting N; drift guard (from F1) passes with Teams added.

### U3. Mattermost breadcrumb + reaction + typing — P1, S — lane C (ADAPTER-mattermost)

**Problem.** `mattermost` is edit-capable and renders most surfaces but does
**not** override `deliver_breadcrumb` (tool-progress chips degrade to plain
rows), has `add_reaction` only via its deliver-action (not the trait the host
calls, `service.rs:1345`), and overrides no `set_typing` (no "working" signal
at all).

**Change.** Add `deliver_breadcrumb`, the trait `add_reaction`, and a real
`set_typing`.

**Acceptance.** Breadcrumb renders in place; host-driven `add_reaction` reaches
Mattermost; typing shows during a run.

### U4. Native cards on gchat + matrix — P1, S — lane C (ADAPTER-gchat, ADAPTER-matrix)

**Problem.** Neither `gchat` nor `matrix` overrides the trait `deliver_card`
(`grep -c = 0`): gchat renders cards only through its internal `dispatch_action`
path (`adapter.rs:864`) and matrix degrades cards to text entirely. So the M18
approval/ritual cards render as plain text on both.

**Change.** Override the trait `deliver_card` on both — gchat via its
Cards-v2/AdaptiveCard builder, matrix via `formatted_body` HTML with a
buttons-as-links fallback (Matrix has no native buttons — degrade to labeled
links, same shape as `Card::to_text_fallback` but HTML).

**Acceptance.** Approval card renders natively (buttons where supported, links
where not) on both; text fallback unchanged where a field is unsupported.

### U5. Bare-adapter card + todo floor (high-value subset) — P2, M — lane C (ADAPTER-deltachat, ADAPTER-line)

**Problem.** Eleven adapters have zero rich-surface support. Most are
intentionally minimal (email/webhook/comment-only), but a couple are genuine
interactive chat surfaces a user would expect parity on: `deltachat` (full chat,
inbound files already work) and `line` (postback inbound is even stubbed at
`router.rs:146`). On these, HUD/diff/todo all render as plain prose.

**Change.** Give deltachat and line a `render.rs` covering at least
`deliver_card`, `deliver_todo_list`, and `deliver_diff` natively; wire LINE
postback inbound (`router.rs:146`) so its buttons actually route (needed for
in-chat approvals to work there at all). Leave the truly outbound-only adapters
(resend, github, linear, x, webhooks, wechat, emacs, imessage) as bare — note
that decision in the PR so it isn't rediscovered.

**Acceptance.** deltachat/line render an approval card and todo chip natively;
LINE postback tap produces a routed inbound event.

### U6. Adopt the shared `core/markdown` renderer in adapters — P2, L — lane C, after U1–U5

**Problem.** `channels/core/src/markdown/` (`render(md, Flavor)` with
`Html|Discord|Slack|Mattermost|WhatsApp|Plain`, `render.rs:21,56`) exists and is
tested, but **no adapter consumes it** — its only callers are the host outbound
splitter. Every adapter hand-rolls its own escaper: telegram `markdown_to_html`
(`adapter.rs:1147`) + `escape_markdown_v2` (`api.rs:69`), slack mrkdwn
(`adapter.rs:718,932,1049`), discord `escape_discord_markdown` (`:911`), matrix
`escape_html_matrix` (`:825`), gchat `escape_html_gchat` (`:847`), mattermost
and whatsapp `render.rs`. The `Flavor::{Discord,Slack,Mattermost,WhatsApp}`
variants describe adapters that never call the renderer — dead capability and a
standing drift risk (a formatting bug must be fixed in N places).

**Change.** Migrate each adapter's outbound text formatting to call
`core::markdown::render(md, flavor)`, deleting the bespoke escapers. Where an
adapter's current output is pinned by tests, either prove byte-compatibility or
update the fixtures with the diff called out. This is a consolidation, not a
behavior change — do it last in lane C so the floors (U1–U5) don't churn under
it.

**Acceptance.** Adapters format via the shared renderer; the per-adapter escaper
functions are removed (or reduced to thin flavor selectors); splitter/renderer
tests still green; no visible output regression on the pinned fixtures.

### U7. Inbound reactions as agent-visible input — P1, M — lane C (router + ADAPTER-telegram/slack/discord/whatsapp-cloud)

**Problem.** **No adapter parses inbound reaction events** — a user reacting
👍/✅/👀 on any channel produces zero agent-visible signal (audit: grep for
inbound reaction parsing across all `parse.rs`/`events`/`ingress` returned zero
hits; every reaction reference is outbound). M18 parked this as "revisit
post-M18." It's the most natural lightweight steering input — "yes, ship it"
without typing.

**Change.** Define an inbound-reaction contract in `channels/core`
(`content.reaction { emoji, target_seq, actor }`). Parse it in the adapters that
deliver reaction events natively — telegram `message_reaction`, slack
`reaction_added`, discord `MESSAGE_REACTION_ADD`, whatsapp-cloud reaction
messages — and whitelist it past the mention gate like the existing callback
payloads (`mention.rs`, `is_interaction_payload`). The runner treats a reaction
on the agent's own last message as a lightweight signal: a curated set (✅/👍 =
affirmative, 👀 = "I'm looking", ❌/👎 = negative) is injected as a one-line
interjection (reuse R2's mid-turn steering seam) rather than a full turn. Secure:
a reaction is external content — it marks provenance like any inbound.

**Acceptance.** Per-adapter unit: a native reaction event becomes a normalized
inbound reaction row; e2e fixture: 👍 on the agent's "shall I deploy?" message
lands as an affirmative interjection within one tool-batch boundary; a reaction
on an unrelated message is ignored, never a full spurious turn.

### X-rider (Wave 2). Parity fixtures — P2, S — lane X

One fixture per newly-rich surface (signal breadcrumb, teams edit, gchat/matrix
card, deltachat/line card, inbound reaction). Register in `tests/replay.rs`.

---

## Wave 3 — "advanced capabilities"

Lift the ceiling. Each card is larger and more independent; the security-
sensitive ones (A2, A3, A7) gate on a recorded `security-review` pass.

### A1. Single-call parallel fan-out with a join — P0, L — lane T (tool) + lane R (join seam), after M18's R7 `delegate`

**Problem.** Fan-out today is N separate `delegate` / `create_agent` calls whose
results report **asynchronously** into `messages_in`; there is no single-call
"spawn N workers and await their joined results" primitive and no
gather/await-all (audit: `agents.rs:113-114,141-143` literally tell the model to
"call `delegate` several times"). A parent that wants to build three components
in parallel and assemble them has no way to block on all three — the single
biggest structural capability gap.

**Change.** A `delegate_batch` tool (or a `join`/`await` companion to
`delegate`) that spawns N delegates in isolated `sib/<id>` worktrees (reuse R7's
`SpawnProfile` + depth/permission caps, `agent_to_agent/depth.rs`,
`spawn.rs:1280,1546`), blocks the parent turn until all report (or a timeout),
and returns the aggregated per-worker results as one tool response. This needs a
**runner-side join seam** (lane R) — the parent turn must yield while children
run and resume on the joined result, analogous to how external-MCP calls
block-poll (`external_mcp.rs:11-15`). Cap fan-out width; each worker stays
contained (NULL messaging group → reports only to parent, never to user chat,
exactly as `delegate` does today).

**Acceptance.** e2e (mock provider): a parent `delegate_batch` of 3 workers each
editing a distinct file returns one aggregated result after all 3 finish;
worktrees are isolated (no cross-write); a worker failure surfaces as a per-
worker error in the aggregate, not a lost turn; depth cap refuses a
`delegate_batch` from a max-depth child.

### A2. Interactive browser (Phase 5b, demand-pull) — P1, L — lane V, security-review gated

**Problem.** The browser is read-only render (`dom_text`/`screenshot`/
`aria_snapshot`); click/type/scroll are explicitly out of scope
(`copperclaw-browser/src/lib.rs:5-6`, `browser_render.rs:4-5`). But the CDP
driver (`cdp.rs`, `live.rs`), the sandboxed child container spec (`container.rs`,
deny-default egress + unprivileged user + orphan labels), and the per-redirect
`NavigationGuard` SSRF plumbing already exist — interactive actions are an
incremental extension of a live path, not greenfield. M16 committed 5b as a
demand-pull stretch (gated by phases 0+1+3, all shipped).

**Change.** Add interactive CDP actions (click / type / scroll / wait-for-
selector) behind a stricter opt-in flag (separate from
`COPPERCLAW_BROWSER_ENABLED`), demand-pull only (no autonomous browsing loop),
output stays `Provenance::Untrusted`, SSRF re-guarded per navigation. Keep the
non-goal boundary explicit: no always-on interactive browsing, no browser-
writes-memory in this card.

**Acceptance.** Live integration (Docker + Chromium): a scripted click-then-read
against a local page returns the post-click DOM; the SSRF guard blocks a
navigation to a private address mid-interaction; the flag off → today's read-
only behavior, byte-stable. `security-review` pass recorded in the PR.

### A3. Public-tunnel model verb (activate V5) — P1, M — lane V + lane G, security-review gated

**Problem.** The V5 tunnel module (`copperclaw-modules/src/tunnel.rs`) is
**merged** (PR #55, commit `0904f27`) and fully approval-gated, but has **no
agent-facing verb** — it's reachable only through the approval plumbing
(`handlers/approvals.rs:659,1165-1201`); grep for `make_public`/`expose_public`
in the runner finds nothing, and `run/preview.rs` defines only
`expose_preview`/`close_preview`. So "send it to my cofounder" still can't be
initiated by the agent.

**Change.** Wire a `make_preview_public` tool (relayed like `expose_preview`
through the reserved `__preview` MCP path) that calls the existing
`TunnelBroker::expose`, raising the `CredentialedExternalAction` approval V5
already implements; on approval, the P3 ritual card gains a public-URL button.
Auto-teardown with the preview (V5's `close_for_preview`) is already wired —
just surface it. No new tunnel logic; this is the missing verb + prompt/skill
teaching + the ritual-card button.

**Acceptance.** e2e (mock tunnel binary): agent `make_preview_public` → approval
card → approve → public URL returned and rendered as a ritual-card button →
preview close tears down the tunnel. Absent binary → clean actionable error.
`security-review` pass recorded (outward-facing surface).

### A4. Agent-authored / persistent skills — P2, M — lane P

**Problem.** Skills are host-discovered and symlink-materialized **at spawn**
(`copperclaw-skills/src/lib.rs:6-30`); there is no in-session mechanism to save
a reusable skill. An agent can `write_file` a `SKILL.md` but nothing re-discovers
or exposes it — the write_file→discovery loop is open. A prototype builder that
works out a good repeatable procedure can't durably teach itself.

**Change.** A guarded `save_skill` capability (approval-gated, secure-by-default)
that validates a proposed `SKILL.md` against the frontmatter rules
(`frontmatter.rs`: `name` == dir, kebab-case, optional `allowed-tools`) and
writes it into the group's per-group skills override dir, registering it for the
next spawn (hot re-discovery within the session is a stretch — the next spawn
picking it up is the whole thing). Reuse the allowed-roots containment check
(`lib.rs:26-29`). This is a capability, not a registry — no cross-group sharing,
no ClawHub (a standing non-goal).

**Acceptance.** e2e: agent `save_skill` → approval → the SKILL.md lands in the
group override dir and validates; a subsequent spawn discovers and exposes it;
an invalid frontmatter is refused with the precise validation error.

### A5. Agent-facing memory write — P2, M — lane T

**Problem.** The M16 Phase-3 memory store (per-group SQLite, FTS5 + cosine,
provenance-tagged, migration 021) is real, but the **in-container surface is
read-only** (`memory_search` / `memory_get`, `tools/memory.rs:5-9`) — the
writing + embedding side lives host/runner-side. The agent can recall but can't
deliberately remember, so durable facts depend on the runner's implicit capture.

**Change.** A guarded `memory_save` tool that writes a `trusted`-provenance entry
to the group memory store (embedding generated via the broker, same path the
runner uses), letting the agent persist a fact across sessions on purpose. Rate/
size-cap it; mark entries with source. Provenance stays honest: the agent can
only write `trusted` entries; nothing lets an untrusted turn launder content into
trusted memory (guard against a tainted turn calling `memory_save` — refuse or
force `untrusted`).

**Acceptance.** Unit: `memory_save` writes a retrievable entry that a later
`memory_search` returns with `trusted` provenance; a tainted-turn `memory_save`
is refused or downgraded; the cap rejects oversized bodies.

### A6. Durable autonomy: first-class `scheduled_tasks` table — P2, M — lane T (tool) + lane H (sweep), migration 028

**Problem.** Scheduling today is cron/time re-wake only: `schedule_task` writes
into `messages_in` with a `recurrence` + `process_after`, and the sweep fans it
out (`host-sweep/src/checks/recurrence.rs`, `wake.rs`). `PLAN.md`'s deferred
list calls for a first-class `scheduled_tasks` table so tasks can be
listed/cancelled without scanning the message log, and there is no event-driven
trigger or durable background-job abstraction distinct from message re-delivery.

**Change.** Add the `scheduled_tasks` table (migration 028: schedule spec, next-
fire, owning group/session, payload, enabled) and back the existing
`schedule_task`/`list_tasks`/`cancel_task`/`pause_task`/`resume_task`/
`update_task` tools (`tools/scheduling.rs`) with it instead of the message-log
scan; the sweep's recurrence check reads the table. Keep behavior compatible
(existing recurrences continue to fire). Event-driven triggers (webhook/
filesystem) are explicitly a follow-up, not this card — note the seam.

**Acceptance.** `list_tasks` returns from the table without scanning
`messages_in`; `cancel_task` removes a row and the sweep stops firing it; a
migration test proves existing recurring rows migrate/fire unchanged.

### A7. Preview-exposure provenance refinement — P2, S — lane V (+ `runner/src/policy.rs`, coordinate)

**Problem.** M18's live post-mortem (bucket 2) found that a "research … then
build" request web-tainted the turn, so the M16 provenance gate denied
`expose_preview` as a credentialed external action — the user's preview didn't
appear until a fresh untainted turn. Exposing a **LAN-only** preview is not the
same risk class as `web_search`/`web_fetch`/a public tunnel; gating it behind
the taint gate is a felt UX papercut.

**Change.** Reclassify `expose_preview` (LAN-only) so it is **not** blocked by
context taint — it stays approval/permission-gated per group but is not a
`CredentialedExternalAction` for the taint check (contrast `make_preview_public`
from A3, which absolutely remains taint-gated because it's outward-facing).
Document the boundary: LAN preview = local surface, public tunnel = external.

**Acceptance.** e2e: a web-tainted "research then build" turn can `expose_preview`
to LAN without a fresh approval; the same turn still cannot `make_preview_public`
without approval; audit rows unchanged.

### X-rider (Wave 3). Capability fixtures — P2, S — lane X

Fixtures for the fan-out aggregate (A1), the public-verb ritual card (A3), and
the preview-taint reclassification (A7).

---

## M1. Metrics rider — P2, M — lane M, absolute last

Sweep the metric wishes each M19 PR records in its description into
`copperclaw-metrics` in one card; no other card touches the crate. Expected
additions from this program: edit-drift-fallthrough counts (F1), user-facing
wall cards emitted by blocker category (F2), approval-card resolution/expiry/
conflict outcomes (F3), blocked-todo renders (F4), thinking-frame emissions
(F5), per-adapter rich-surface upgrades exercised (U1–U5), shared-renderer
adoption coverage (U6), inbound reactions by emoji/outcome (U7), fan-out width +
per-worker outcomes (A1), interactive-browser actions (A2), public-tunnel
exposures (A3), skills saved (A4), memory writes by provenance (A5), scheduled-
task lifecycle (A6).

---

## Wave summary

| Wave | Theme | Cards | Parallel lanes |
|---|---|---|---|
| 1 | Correct, consistent feedback | F1 (C); F2→F5→F6 (R); F3 (G); F4 (C) + X-rider | C, R, G disjoint; F5/F6 serialize in R |
| 2 | Every channel first-class | U1–U5 (C, disjoint adapter dirs) ‖ U7 (C router+adapters) → U6 (C, last) + X-rider | adapter floors fully parallel; U6 after floors |
| 3 | Advanced capabilities | A1 (T+R); A2, A3, A7 (V); A4 (P); A5 (T); A6 (T+H) + X-rider | mostly independent; A3 after V5 sign-off; A7 coordinates policy.rs |
| last | Metrics | M1 (M) | — |

Critical path to the felt "same great experience everywhere" outcome:
**F1 + F4 + U1–U5** (parity) and **F2 + F3 + F5** (legibility) are the
highest-value, lowest-risk cards — do them first. **A1** is the marquee
capability and the long pole in Wave 3. **A2/A3** gate on security review.

## Program-level acceptance

1. **Parity.** On every rich adapter, the HUD, todo chip (including `blocked`),
   diff, and approval card render natively; no channel in `EDIT_CAPABLE_CHANNELS`
   silently fails to edit; a bare/interactive chat channel (deltachat/line) shows
   a real card and todo chip, not prose.
2. **Legibility.** A live smoke on the telegram dev group: a build that hits an
   egress or approval wall shows the user a clear, actionable card (not silence);
   a pure-reasoning wait shows a thinking HUD; a blocked-on-approval agent is
   visibly waiting, not hung; a 👍 reaction steers the run.
3. **Capability.** A parent request that fans out three parallel build workers
   and assembles their joined output completes in one turn (A1); the prototype
   ritual can produce a public URL after approval (A3); the agent can save a
   skill and use it next session (A4) and remember a fact across sessions (A5).
4. The golden + parity + capability fixtures pass, and the integrated gate is
   green after each multi-PR merge.

## Deferred / rejected (don't re-litigate)

- **Runtime plugin / ClawHub skill registry** — standing non-goal (`PLAN.md`
  M16); A4's `save_skill` is per-group only, no cross-group sharing.
- **Always-on / autonomous interactive browsing and browser-writes-memory** —
  A2 is demand-pull click/type only; the autonomous loop and memory-write remain
  out of scope (`PLAN.md` M16 non-goals).
- **Deploy-to-cloud / persistent hosting** — out of scope; the public tunnel
  (A3) is the ceiling, exactly as M18 held.
- **Token streaming through the SQLite transport** — rejected in M17/M18; still
  rejected. Progressive reveal (R6) + the HUD are the mechanisms.
- **Event-driven triggers (webhook/filesystem) and a general background-job
  runtime** — A6 delivers the durable table; event triggers are a noted seam,
  a follow-up milestone.
- **Raising the truly outbound-only adapters** (resend, github, linear, x,
  webhooks, wechat, emacs, imessage) to the rich floor — deliberately left bare
  in U5; their surface doesn't warrant it.
- **External-MCP connection caching** (`mcp/src/external.rs:11-15`, connect-per-
  call today) — a known optimization, out of scope unless M19 usage makes it a
  hotspot.
