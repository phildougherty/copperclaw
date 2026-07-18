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
