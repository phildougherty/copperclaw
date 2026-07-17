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
