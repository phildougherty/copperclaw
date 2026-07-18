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
