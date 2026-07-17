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
