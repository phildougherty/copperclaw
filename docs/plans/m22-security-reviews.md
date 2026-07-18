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
