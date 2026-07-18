# M22 — Security review verdicts

Per the M22 program rules (rule 5), every outward-facing surface or
default-behavior change gets a `security-review` pass recorded here before the
card merges. In this program that is:

- **C1** — post-edit toolchain execution (default behavior change: runs
  format/typecheck commands inside the existing sandbox after every edit).
- **C2** — opening an arbitrary existing repo into the sandbox.
- **A1 / A2** — the autonomy capability grant (new capability to *act* on a
  scheduled turn) and its enforcement at the runner gate.
- **S1** — new files (skill `scripts/`/`data/`) reaching the container.

Each entry: scope, threat model, mitigations, residual risk, verdict.

---

<!-- Cards append their verdicts below, newest last. -->

## C1 — Post-edit verify hook (Wave 1, lane T)

**Scope.** After a successful `edit_file` / `multi_edit` / `apply_patch` /
`write_file` mutation, the tool auto-invokes the applicable checker
(`eslint` / `tsc` / `ruff`, chosen by the touched file's extension) scoped to
*that single file*, parses the machine-readable output with the existing
`diagnostics.rs` parsers, and appends a concise digest under
`post_edit_diagnostics` in the tool result. Default ON; opt out per session
with `COPPERCLAW_POST_EDIT_VERIFY=0` (`0`/`false`/`off`/`no`). New/changed
code lives entirely in `crates/copperclaw-mcp/src/tools/{diagnostics,
edit_file,multi_edit,apply_patch,computer_use}.rs`.

**Threat model.** This is a default-behavior change that *executes toolchain
commands* after every edit, so the questions are (a) does it widen the trust
boundary, (b) can it be steered to run something unintended, and (c) can it
turn a benign edit into a failure or a resource problem.

**Mitigations / argument.**

- **No new trust boundary.** The commands run inside the SAME per-session,
  ephemeral, network-egress-brokered container the agent already has, with the
  SAME privileges it already has to run arbitrary shell (`shell` tool). The
  agent could already type `ruff check app.py` by hand; C1 only *auto-invokes*
  an already-available check on the file just written. It grants no new
  capability, reaches no new resource, and crosses no process boundary the
  agent couldn't already cross.
- **Fixed command set, no interpolation into a shell.** The three commands are
  hard-coded argv vectors (`eslint <file> -f json`, `tsc --noEmit --pretty
  false <file>`, `ruff check --output-format json <file>`) spawned via
  `Command::new(...).args(...)` — never a `bash -c` string — so the only
  attacker-influenced value is the file's own basename, passed as a single
  argv element with no shell interpretation. The checker binary is chosen from
  a closed match on the file extension, not from any model-supplied string.
- **Bounded.** Each run is wrapped in the existing 120 s `TOOL_TIMEOUT`,
  scoped to one file (not a whole-repo walk), and its binary is probed with
  `command -v` first — an absent toolchain (the pre-`prototyping`-profile /
  minimal image) degrades to "no digest", never a crash.
- **Fail-safe.** The hook is strictly best-effort: a spawn failure, parse
  failure, timeout, unsupported file type, or clean result all return `None`
  and leave the successful edit's result shape untouched. It can never convert
  a successful mutation into a tool error, and it never touches the
  `.copperclaw/verify` gate / dirty markers (that stays the shell verify
  path's job).
- **No new sink.** Output is a capped, structured digest fed back to the model
  in-loop; nothing is delivered externally, persisted, or granted elevated
  provenance.

**Residual risk.** (1) `tsc` invoked on a single file outside its `tsconfig`
project can emit false-positive "cannot find module" style diagnostics; this
is informational feedback the model judges, not an enforcement gate, and the
opt-out exists for noisy projects. (2) A pathological source file could make a
checker slow, bounded by the 120 s timeout. (3) Latency: one extra bounded
subprocess per edit to a `.ts/.js/.py` file — acceptable for the in-loop
feedback it buys, and off for non-source edits. All low.

**Verdict: APPROVED.** Default-ON is safe: no trust-boundary expansion, fixed
non-interpolated command set, fully fail-safe, bounded, and reversible via a
per-session env opt-out.
## C2 — Open/attach an existing repository — VERDICT: PASS

**Scope.** New runner-side attach flow (`copperclaw-runner/src/run/project.rs`)
that establishes an *existing* (cloned/handed) repository as the session's
working project: infers `.copperclaw/verify` stages from the repo's toolchain
manifests, seeds `.copperclaw/DECISIONS.md`, triggers the C3 symbol-index seam,
and drops a `.copperclaw/attached` marker so the verify + self-review gates
apply to the existing code. Wired into the runner poll loop (auto-attach of
`git clone`d repos) and observed host-side at cold start
(`container_manager/cold_start.rs`). The `coding-task` skill documents it.

**Threat model.** The repository is *arbitrary external content* pulled into
the sandbox. Concerns: (1) the clone is a network fetch of an attacker-chosen
URL (SSRF / egress); (2) a hostile repo could try to make the attach flow
execute code, exfiltrate, escape the project dir, or blow memory; (3) a hostile
manifest could inject shell commands into the recorded verify stages that later
run when the agent executes verify.

**Mitigations.**
- **No new network capability.** Cloning is the agent's own `shell` git
  (architecture decision (a) — no `git_clone`/`git_commit` MCP tool), which
  already flows through the existing `copperclaw-modules` egress/SSRF/allowlist
  guard. The attach flow itself operates only on a **local path** — it fetches
  nothing and adds no reachable network surface.
- **No code execution during attach.** The flow only *reads* manifests/README
  and *writes* under `<repo>/.copperclaw/`. It never runs repo scripts, build
  steps, or git subprocesses. Verify stages are *recorded*, not run; they only
  execute later when the agent runs them via `shell` inside the sandbox — the
  standing, gated behaviour.
- **No command injection via manifests.** Verify commands are **fixed
  templates** (`cargo test`, `make test`, `pytest`, …). For `package.json`,
  only an **allowlist** of script *names* (lint/typecheck/test/build) maps to a
  stage, the emitted command is always `npm run <fixed-key>` / `npm test`
  (never the raw script body), and stage *names* are fixed in code — so a
  hostile key such as `"test\nrm -rf /"` maps to nothing and cannot inject an
  extra `.copperclaw/verify` line (regression-tested:
  `hostile_manifest_key_cannot_inject_a_verify_line`).
- **Bounded reads, no symlink traversal.** Manifests/READMEs are read under a
  256 KiB cap; the structure summary is a single non-recursive `read_dir` that
  lists symlink names but never follows them. `DECISIONS.md` is seeded only
  from the repo's own verbatim text (no paraphrase/hallucination) and byte-
  capped. Writes are confined to `<repo>/.copperclaw/` and are best-effort
  (a marker write failure never fails the turn).
- **No clobbering / no blast radius on prototypes.** Attach never overwrites an
  existing `.copperclaw/verify`; it is idempotent via the `attached` marker;
  and auto-attach is gated on an `origin` remote so blank `git init` prototypes
  are never touched.

**Residual risk.** Low. A malicious repo's *test/build command is by design run
by the agent later* inside the sandbox — that is the existing, egress-guarded
threat surface for any code the agent works on, unchanged by C2 (C2 records
which command to run, from a fixed template, not from repo content). The
project directory bind-mount, egress guard, and per-task budgets remain the
enforcing boundaries. No new privilege, no new network reach.

**Verdict: PASS** — reuses the existing egress/SSRF posture, adds no outward
capability, and the attach flow is read-only-plus-`.copperclaw/`-writes with
allowlisted, injection-safe stage inference.

---

## A1 — Task capability grants: schema + approval-gated authoring (Wave 2, lanes DB+H+MCP)

**Scope.** A1 introduces the *data + authoring* half of the autonomy brake in
reverse: a `task_grants` table (migration 030) that records a durable,
human-approved authorization for an AUTONOMOUS fire of a scheduled task to take
a real external action; a `schedule_task` `grant` arg that proposes one; the
approval round-trip that persists it; and the `effective_grant` read API A2
consumes. A1 does NOT itself open any gate — it stores authorization and
provides inert-by-default reads. Enforcement (turning a live grant into an
`approved` turn) is A2. New/changed code:
`crates/copperclaw-db/migrations/030_task_grants.sql`,
`crates/copperclaw-db/src/tables/task_grants.rs` (+ `tasks.rs` lookup helper +
`migrate.rs` registration), `crates/copperclaw-mcp/src/tools/scheduling.rs`
(+ `context.rs` spec/effect), `crates/copperclaw-runner/src/tools.rs`
(system-row serialization), `crates/copperclaw-host-delivery/src/service.rs`
(`raise_task_grant_approval`), `crates/copperclaw-host/src/handlers/approvals.rs`
(`apply_task_grant`).

**Threat model.** This is a NEW capability to *record authorization to act*, so
the questions are: (a) can a grant be created without a human approving it;
(b) can a grant authorize more than the human agreed to (scope creep, unbounded
spend, non-expiring / standing grants); (c) can a stale, revoked, or exhausted
grant still read as live; (d) can the agent forge or self-elevate a grant by
steering the payload the host trusts; (e) blast radius if the matcher is wrong.

**Mitigations / argument.**

- **No grant without human approval.** There is deliberately no "insert pending"
  path in the DB layer — the ONLY writer of a `task_grants` row is
  `apply_task_grant`, which runs on the operator-approve edge. The pending state
  lives entirely in `pending_approvals`. So "no row" and "not approved" are the
  same observable state: `effective_grant` returns `None` until a human
  approves. Unit-tested end-to-end (`approve_task_grant_persists_and_reads_live`
  vs `deny_task_grant_leaves_no_grant`: deny persists nothing).
- **Bounded, never standing (decision (b)).** The authoring tool REQUIRES an
  absolute `expires_at`, caps it at `MAX_GRANT_HORIZON_DAYS` (365) so a
  "bounded" grant can't be effectively perpetual, requires at least one spend
  bound (`token_budget` or `max_fires`, both validated positive), and rejects a
  past-dated expiry. Every bound is re-derived at read time by `effective_grant`
  (expiry vs supplied `now`, `tokens_consumed >= token_budget`,
  `fires_consumed >= max_fires`) — an over-budget / expired grant reads inert
  regardless of `status`. Unit-tested (`over_fire_budget_reads_inert`,
  `over_token_budget_reads_inert`, `expired_grant_reads_inert`,
  `expiry_is_evaluated_against_supplied_now`).
- **Revocable + inert-by-default reads.** `revoke` flips `status` + stamps
  `revoked_at`; `effective_grant` requires approved-AND-not-revoked, so a
  revoked grant reads `None` even with budget/time left
  (`revoked_grant_reads_inert`). The newest grant is authoritative and does NOT
  fall back to an older live one, so re-authoring supersedes cleanly
  (`newest_grant_is_authoritative_and_supersedes`).
- **The host, not the agent, controls the trusted fields.** The agent proposes
  scope + bounds, but the *task binding* is host-resolved: the delivery raise
  looks up the concrete `task_id` from the session's own tasks
  (`latest_for_session_by_name`, scoped to the firing session) and stamps it
  into the pending payload — the agent cannot point a grant at another session's
  or group's task. `granted_by` is the approving operator identity
  (`decided_by`), never agent-supplied. Scope tokens are validated against a
  fixed grammar (non-empty class per token) before an approval is ever raised.
- **Tight, auditable matcher.** `scope_permits` is case-sensitive, does class /
  resource matching only, and has NO implicit cross-class or wildcard-widening
  behavior beyond an explicit `class:*`/bare-class class-level grant. An empty
  scope permits nothing. Exhaustively unit-tested (exact, resource-no-widen,
  class-level-covers-all, no-cross-class, multi-token-any, empty-permits-nothing,
  colon-in-resource). A2 depends on these exact semantics — documented in the
  module doc-comment.
- **Fail-safe raise.** If the task can't be resolved (e.g. its create failed) or
  the payload is malformed, `raise_task_grant_approval` records a self-mod
  failure the agent sees rather than silently dropping — and NO approval is
  raised, so nothing can be approved into existence for a non-existent task.

**Residual risk.** Low-moderate, and bounded to the authoring surface. The grant
merely *records* permission; the actual gate that turns permission into action
is A2 (reviewed separately, MOST security-sensitive). The one soft edge is task
binding by (session, name) rather than a client-chosen id — mitigated by
resolving newest-in-session at raise time (the just-created task) and by the
FK to `tasks(id)`; a reused name within a session picks the freshest row, which
is the intended target. Grant *consumption* accounting (`consume_fire` /
`consume_tokens`) is exposed but only wired by A2; until then a grant's fires/
tokens do not auto-decrement, which is safe (it can only read MORE inert, never
less, than reality once A2 records spend).

**Verdict: PASS** — approval-gated (no persist without human approval), bounded
(mandatory capped expiry + required spend bound), revocable, inert-by-default at
every read, host-controlled task binding + granter identity, and a tight
exhaustively-tested scope matcher. A1 adds authorization *data*, not an open
gate; the gate itself is A2's review.

## A2 — Enforce grants at the autonomy gate (Wave 2, lane N) — MARQUEE, MOST SECURITY-SENSITIVE

**Scope.** A2 is the card that turns A1's stored authorization into the ability
to *act*. It is the one place in the program that opens the autonomy brake, so
this verdict is exhaustive. Files: `crates/copperclaw-runner/src/run/{mod.rs,
blocker.rs,tool_dispatch.rs}` (+ a required `RunnerDeps.active_grant` field and
its one construction line in `main.rs`). No changes to `policy.rs`, the DB, the
host approval path, or the sweep — A2 consumes A1's `scope_permits` /
`effective_grant` contract.

### The exact condition under which `approved` becomes true

There is no turn-wide "approved" flip. `set_turn_provenance(autonomous, false)`
keeps the blanket taint-clearing bool wired `false` for every autonomous turn
(as before). The *only* thing that admits a credentialed external action on an
autonomous turn is, per-call in `tool_dispatch::invoke_tool`, a
`AutonomyVerdict::Granted` from `autonomy_verdict(...)`, which is returned iff
**all** of:

1. the turn is autonomous (`is_autonomous_turn()`), AND
2. the tool is in the credentialed-external set (`policy::is_credentialed_external`),
   AND
3. a grant snapshot is present on `RunnerDeps.active_grant`, AND
4. that grant `is_live(now)` — not expired, fires_remaining not `Some(<=0)`,
   tokens_remaining not `Some(<=0)` (a defence-in-depth re-check on top of the
   host's `effective_grant`, evaluated against the runner's own clock), AND
5. `grant.permits(required_capability(tool, input))` is true — A1's
   case-sensitive, non-widening `scope_permits`.

When granted, A2 drops **only** `trust.autonomous` for **that single call** (a
locally-built `TurnTrust`), then runs the unchanged `policy.evaluate`. It does
**not** set `trust.approved` — so the taint gate remains fully in force: a
granted action on a web-tainted turn is still blocked as `untrusted-provenance`
until a fresh human approval. A grant opens the *autonomy* gate for its scope,
never the taint gate. (Test: `grant_opens_autonomy_but_not_the_taint_gate`.)

### Proof that blanket approval never happens

- `set_turn_provenance`'s second argument is a literal `false` at the only call
  site (`run/mod.rs`); grep confirms no other caller passes `true` for an
  autonomous turn.
- The autonomy verdict is computed **per tool call** from `(tool_name,
  structural args, live grant, now)`; there is no state that says "this whole
  turn is approved."
- The scope check is A1's `scope_permits`, which returns `false` for an empty
  scope, requires an exact class/resource match, and never widens across
  classes. An unrelated grant (`web_fetch`) does not authorize `install_packages`
  (test `out_of_scope_grant_blocks_with_clear_reason_and_no_charge`).
- A human (non-autonomous) turn is never gated and never consults the grant, so
  a stray snapshot can't leak into a human turn
  (`grant_does_not_affect_human_turns`).

### The tool → required-capability mapping and why it can't be widened

`required_capability(name, input)` is deliberately narrow and derives the token
**only from the tool name and its structural arguments — never from model- or
content-supplied free text**:

- `mcp__<server>__<tool>` → `mcp:<server>` (server taken from the runtime
  namespace, not the arguments).
- `web_fetch` → `web_fetch:<host>` (host parsed from the `url` arg, userinfo +
  port stripped, lower-cased) or bare `web_fetch` when unparseable.
- every other credentialed-external tool → its own name as the bare class.

A prompt-injected turn therefore cannot manufacture authorization: to run
`install_packages` the grant must literally carry `install_packages` (or a bare
class token that A1's matcher treats as class-level). The `required` token is
never lifted from a fetched page, a memory hit, or a model-chosen string; the
worst a poisoned turn controls is the URL host (which only *narrows* the token —
a host-scoped grant still has to name that exact host). The mapping fails
closed: an unmapped tool maps to its bare name, which a resource-scoped grant
will not match. (Tests: `required_capability_maps_each_action`,
`autonomy_verdict_gates_only_autonomous_credentialed_external`.)

### Budget / expiry enforcement (block-not-charge)

- **Fires** are the in-advance budget: one fire = one turn. `charge_grant_fire_once`
  emits a `grant_consume { grant_id, task_id, fires: 1 }` System row to
  `outbound.db` **once per turn** (latched on `GrantGateState.fire_consumed`,
  reset each turn), and only **after** the action clears every policy layer and
  is about to dispatch — so a taint-blocked or policy-denied action is never
  charged (`grant_opens_autonomy_but_not_the_taint_gate`,
  `out_of_scope_grant_..._no_charge`). Cross-fire enforcement: the host writes a
  live snapshot only while `effective_grant` reports fires/tokens/expiry
  remaining, and the delivery handler applies each `grant_consume` back to
  central via `consume_fire` (companion plumbing, below) so `max_fires` bounds
  across fires.
- **Expiry / exhaustion**: an inert snapshot (expired, or `fires_remaining` /
  `tokens_remaining` `Some(<=0)`) fails `is_live`, so `autonomy_verdict` returns
  `NotGated` and the action falls to the policy layer's blanket autonomous deny —
  it is **blocked, not allowed-then-charged** (tests
  `autonomy_verdict_inert_grant_authorizes_nothing`,
  `expired_grant_leaves_autonomous_action_blocked`,
  `load_turn_grant_absent_or_inert_is_none`).

### Out-of-scope actions still block + propose

An autonomous credentialed-external action not covered by a live grant returns a
model-facing deny naming the required capability and the grant scope, carrying
the stable `autonomous (heartbeat/scheduled) turn` hint. `blocker.rs::classify`
routes it (and the blanket policy deny) to `BlockerCategory::Autonomous`, whose
wall card tells the user to send the request themselves or pre-authorize the
task with a bounded, expiring grant. Read-then-propose is intact: memory search,
`send_message`, and local tools always pass on an autonomous turn
(`ungranted_autonomous_action_is_blocked_and_can_still_propose`).

### No autonomous path bypasses the check

The gate lives in `tool_dispatch::invoke_tool`, the single funnel every
model-requested tool call passes through — first-party tools, external MCP
(`mcp__*`), and the host-brokered preview verbs all route through it, and the
credentialed-external classifier already covers each. Self-generated wakes never
touch the router, so there is no router-side path to route around; the runner is
the correct and only place for the gate (decision (b)). `web_search` (autonomy-
gated, taint-exempt) and the LAN preview verbs are still autonomy-gated here, so
the grant is the only opener for them too.

### Residual risk / companion plumbing (out of A2's runner scope)

A2 delivers the complete, tested **runner enforcement**. Two host-side seams are
required for the *live* path and are documented as companion plumbing (they land
outside A2's exclusive file scope — the sweep and host delivery are owned by
other lanes): (1) a writer that snapshots `task_grants::effective_grant` to
`<session>/grant.json` at fire/spawn time, and (2) a delivery handler that
applies the runner's `grant_consume` rows to central via
`consume_fire`/`consume_tokens`. **Until both land the gate stays closed** —
`grant.json` is absent, so `load_turn_grant` returns `None` and every autonomous
credentialed-external action is blocked (byte-identical to today's safe
default). This is a fail-closed staging, not an open hole: the enforcement can
only *deny*; it cannot *grant* until a host-written, human-approved snapshot
exists. Precise cross-turn token decrement is likewise deferred to the delivery
handler; within a turn, fires + expiry + the host's `effective_grant` are the
hard bounds.

**Verdict: PASS.** The gate opens only per-task, per-call, within an
approved+live+in-scope grant, never blanket; the required-capability mapping is
runtime-derived and un-widenable; budget/expiry are enforced block-not-charge;
out-of-scope and ungranted actions stay blocked and fall to read-then-propose;
and there is no autonomous path to an external sink that skips the check. The
one caveat is the fail-closed staging of the two host companion seams, which
cannot weaken the default (absent snapshot ⇒ closed brake).

### A2H — the two host companion seams land (gate now LIVE)

**Scope.** A2H implements the exact two seams the A2 review flagged as
"companion plumbing, out of A2's runner scope," turning A2's fail-closed staging
into the live path. No change to A2's runner enforcement — A2H only *feeds* it.
Files: `crates/copperclaw-host/src/container_manager/tasks_snapshot.rs` (the
grant.json writer), `crates/copperclaw-host-delivery/src/service.rs` (the
`grant_consume` handler), plus the coverage meta-test. It consumes A1's
`effective_grant` / `consume_fire` / `consume_tokens` unchanged.

**Seam 1 — grant.json writer (secure-by-default).** `write_grant_snapshot`
resolves the firing task from the session's pending `kind:task` inbound (the same
`content.task_id` → `series_id` resolution the runner's `firing_task_id` uses),
reads `task_grants::effective_grant`, and writes `<session>/grant.json` in the
`TurnGrant` shape ONLY when a live grant exists. Every other outcome — no firing
task, an inert grant (none / revoked / expired / exhausted), or a DB read error —
REMOVES any stale snapshot and writes nothing. So the writer can only ever hand
the runner a grant a human approved and that is still live; it can never
manufacture authorization, and a transient failure fails closed (absent
grant.json ⇒ `load_turn_grant` = `None` ⇒ closed brake). It is folded into
`write_tasks_snapshot`, which runs at container spawn and on the manager-tick
refresh, so the firing turn sees its grant and a mid-session refresh re-reads the
(possibly now-depleted) grant. The runner independently re-verifies the
snapshot's `task_id` and re-checks `is_live` (defence-in-depth), so a stale or
cross-task snapshot still cannot authorize a fire.

**Seam 2 — `grant_consume` budget writeback.** The delivery handler
(`apply_grant_consume`) applies the runner's `grant_consume` System row to the
central grant via `consume_fire` (per `fires`, default 1, clamped non-negative)
and `consume_tokens` (only for a positive `tokens` count). This is a pure DEBIT
of an already-approved grant — it can only *reduce* headroom, never widen scope
or credit budget — so it is applied immediately, not approval-gated. It closes
the cross-fire enforcement gap A2 noted: `max_fires` and token budgets now
genuinely deplete centrally, so once spent `effective_grant` reads inert and the
NEXT spawn's grant.json is absent — the gate re-closes on exhaustion. A malformed
payload (missing `grant_id`, or an unknown grant id) errors rather than silently
crediting, surfacing as a self-mod failure.

**Residual risk.** Budget depletion is eventually-consistent within a single
long turn: the runner charges one fire per turn locally and the host applies the
debit when the `grant_consume` row is delivered, so the hard in-turn bounds are
the snapshot's `fires_remaining` / `expires_at` (re-checked by `is_live`) plus
the host's `effective_grant` at each spawn/refresh; a burst of fires inside one
uninterrupted turn is bounded by the runner's per-turn single-charge latch, not
by a live central read. This matches A2's documented "within a turn, fires +
expiry + the host's `effective_grant` are the hard bounds." No new external
surface, no new approval path, no widening.

**Verdict: PASS.** The two seams flip the gate from closed to live without
weakening any default: the writer emits authorization only for a human-approved,
live, task-matched grant and fails closed on every other path; the consume
handler can only debit. The autonomy brake now opens exactly per-task, bounded,
pre-authorized, and revocable — and re-closes automatically on revoke, expiry,
or budget exhaustion.

## S1 — Wire `materialize` into container spawn (Wave 3, lanes K+H) — MARQUEE

**Scope.** S1 gives `copperclaw_skills::materialize` its first runtime call
site. At cold start (`container_manager/cold_start.rs`,
`ContainerManager::materialize_session_skills`, invoked from
`begin_spawn_attempt` alongside C2's repo-attach detection) the host resolves the
group's `SkillsSelector` and symlinks each *selected* skill's source directory
into `<session_root>/skills/<skill_id>`. `<session_root>` is the container's
`/data` bind mount, so the farm appears in-container at `/data/skills/`. The
new default-behavior change under review: **a selected skill's `scripts/` /
`data/` files now reach the sandbox** — previously only its `SKILL.md` body did
(spliced into the system prompt, or written to `skills.json`).

**Threat model.** (1) A skill dir pointing outside the configured skill roots
(a malicious/compromised per-group override symlinked at an arbitrary host path)
being linked into `/data`, exposing host files to the agent. (2) The materialize
step failing a spawn or letting an attacker widen what reaches the container.
(3) New, untrusted content executing in the sandbox.

**Mitigations.**
- **Escape guard (defense-in-depth), activated here.** `materialize_session_skills`
  passes a *non-empty* `allowed_roots` = `[global_skills_dir, <groups_dir>/<ag>/skills]`.
  `materialize.rs` canonicalizes each skill's `dir` and rejects
  (`SkillError::EscapedRoot`, per-skill, spawn continues) any that does not
  fall under a root. An empty `allowed_roots` (which would *disable* the check)
  is never passed. This is the exact guard the crate was built for; S1 does not
  widen it.
- **No new trust boundary.** The only files that can be linked are skills the
  registry already discovered under the operator-configured `skills_dir` and the
  approval-gated per-group override dir (`save_skill`, M19 A4). This is the *same
  content set* already trusted to shape the agent's behaviour through the system
  prompt / `skills.json` — S1 introduces **no new external input**. A skill's
  markdown already directs the agent; its helper script is the same author's
  code, now executable instead of merely quoted.
- **Selection parity.** The materialized set is resolved through the same
  `SkillsSelector` + coding-skills cap (`CODING_SKILL_NAMES`, `coding_enabled`)
  as the prompt, so materialize never stages a skill the group has not selected
  (verified by the `honours_explicit_selector` and
  `excludes_coding_skills_when_coding_disabled` tests). A skill an operator
  excluded from the prompt does not get its scripts staged either.
- **Fail-safe, never fail-spawn.** Missing skills dir, scan failure, or a
  per-skill link error is logged and swallowed; a skill that fails to
  materialize is simply not runnable that spawn. Idempotent across spawns
  (re-points stale links, leaves matching ones).
- **Executable bit is the author's, not new privilege.** The farm is symlinks;
  a script is executable in-container only if its source file already carried
  the bit on the host — S1 grants no capability the agent's `shell` (which can
  already `chmod`/write under `/data`) lacks.

**Residual risk / honest limitation.** The farm is a symlink tree whose targets
are canonical *host* paths (e.g. the repo/install `skills/` dir). For those
symlinks to *resolve inside the container*, the skills source must also be
reachable at that host path within the sandbox (a read-only bind mount of the
skills root). Adding that mount lives in the container-spec assembly
(`container_manager/spawn.rs`), which is outside S1's exclusive file scope, so
it is called out as the one remaining wire for full in-container execution;
until it lands the farm resolves host-side (proven by the tests) but the
in-container symlinks dangle. This does **not** weaken any security default —
a dangling symlink exposes nothing; when the read-only mount is added it exposes
only the already-trusted, escape-guarded skills tree, read-only.

**Verdict: PASS.** The default change (selected skills' support files reach the
container) stays inside the existing trust boundary: the content is
operator/agent-authored and already trusted, the escape guard bounds what can be
linked and is passed a non-empty root set, selection parity prevents staging
unselected skills, and every failure path fails safe without failing the spawn.
No new external input, no privilege the agent's shell lacked, and the only
outstanding item (the read-only skills-source mount) is additive and itself
escape-guarded.

### S1M — the read-only skills-source bind mount lands (marquee now truly live)

**Scope.** S1M implements the one outstanding wire the S1 verdict flagged above:
the read-only bind mount of the skills source path(s) so the farm's canonical
host-path symlinks resolve *inside* the container, making a materialized skill's
helper script executable in the sandbox (the S1 marquee). It lives entirely in
`container_manager/spawn.rs` (`ContainerManager::apply_skills_source_mounts`,
called from `build_spec`), outside S1's file scope — no change to
`cold_start.rs`/`materialize.rs`. The farm's links target the *canonical* skill
dir (`skill.dir.canonicalize()`), so the mount binds the canonical `skills_dir`
(and, when present, the per-group override `<groups_dir>/<ag>/skills`) at its
identical host-absolute path (source == target).

**Confirming the anticipated properties.** This is exactly the additive,
escape-guarded mount the S1 verdict said "does not weaken any security default":
- **Read-only.** Both mounts are `read_only: true` — the container can never
  write back into the host skills source (no path to poison the shared skill
  tree from inside the sandbox).
- **Already-trusted content.** The bytes exposed are the same operator/agent-
  authored skill files S1 already stages and whose `SKILL.md` bodies already
  shape the agent via the prompt; the mount introduces **no new external
  input** and reaches no host content the farm didn't already reference.
- **Escape-guarded.** Each raw source is validated with
  `mount_guard::validate_source` before mounting — the per-group override
  against `groups_dir` (identical to how the per-group memory mount is
  validated), the global source against itself (absolute / no-`..` /
  canonicalizable). A swapped-symlink component that escapes its root drops
  *that* mount. This is defense-in-depth *on top of* `materialize.rs`'s own
  per-skill escape guard, so what can reach `/data/skills` is bounded twice.
- **Fails safe, never fails the spawn.** A missing / non-directory / non-
  canonicalizable / validation-failing source skips its mount and logs at
  `warn!`; no global `skills_dir` is a clean no-op. The spawn always proceeds.
- **No widening.** No egress change, no write access, no new capability — a
  read-only bind of a bounded, already-trusted tree. Deduped against paths
  already mounted (and the two skills roots against each other), so it is
  idempotent across spawns.

**Residual risk.** Negligible and strictly smaller than S1's own: S1 already
placed this content on-disk under `/data/skills`; S1M only makes the symlinks it
created resolve, read-only, at their canonical host path. No new sink, no new
input, no privilege the agent's `shell` lacked.

**Verdict: PASS.** The mount completes the S1 marquee without weakening any
default — read-only, of already-trusted escape-guarded skill content, deduped,
and fail-safe.
