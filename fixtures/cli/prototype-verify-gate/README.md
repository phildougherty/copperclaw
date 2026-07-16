## cli / prototype-verify-gate (M18 X2 — the R3 verify-gate leg X1 shipped without)

This fixture closes the single biggest gap X1 (`prototype-golden`)
documented but could not cover: the **R3 verification gate's refuse →
fix → pass loop**. It is a sibling to `prototype-golden`, not a
replacement — the golden fixture still pins the happy path; this one
pins the gate.

One CLI inbound (`build a tiny todo app, verify it, and mark the task
done`) drives a scripted 10-round tool loop that walks the gate through
its full state machine:

1. `todo_add` — one item to complete.
2. `write_file` `proj/.copperclaw/verify` = `python3 -m py_compile app.py`
   — the agent records its own verify command (marks the project dirty).
3. `write_file` `proj/app.py` — a **syntactically broken** stub (marks
   dirty, resets the fix-cycle budget).
4. `todo_update` → `completed` — **REFUSED**: the project is dirty,
   `0 < FIX_CYCLE_CAP`, so the gate names the project + the recorded
   verify command and reports `2 fix cycle(s) remaining`.
5. `shell` `python3 -m py_compile app.py` (`cwd` = the project) — this
   IS the recorded verify command, so it's a verify run. It **fails**
   (Python `SyntaxError`); the gate records a fix cycle and stores the
   stderr tail.
6. `todo_update` → `completed` — **REFUSED again**, now `1 fix cycle(s)
   remaining` (the failed verify burned one).
7. `write_file` `proj/app.py` — the **fixed**, valid file (marks dirty,
   resets the budget).
8. `shell` `python3 -m py_compile app.py` — the verify run **passes**
   (exit 0), so the gate clears the dirty marker.
9. `todo_update` → `completed` — **ALLOWED**: no project is dirty, so the
   gate lets the completion through. The todo is genuinely completed.
10. Final assistant text.

Registered in `crates/copperclaw-host/tests/replay.rs` as
`cli_prototype_verify_gate_refuse_fix_pass`.

### How the gate is pointed at a writable tempdir (the T2 seam)

`verify_gate::data_root()` and `todo.rs`'s `todo_path()` resolve their
root from the process env var `COPPERCLAW_DATA_ROOT` (T2's unconditional
override, precedence `test-override > env > /data`). X1 could not use
this: the replay harness runs the runner **in-process on the host**, and
the workspace `forbid(unsafe_code)` — applied to the `copperclaw-host`
integration-test target via `[lints] workspace = true` — makes
`std::env::set_var` unavailable, so a test can't set that var in its own
process, and the gate reads it from process env at call time with no
per-instance seam.

So the registered test **re-execs itself**: the parent test creates a
fresh data-root dir, then spawns THIS test binary as a child
(`--exact cli_prototype_verify_gate_refuse_fix_pass`) with
`COPPERCLAW_DATA_ROOT` set via the **safe** `std::process::Command::env`
(which sets the child's environment, not the parent's — no `set_var`).
The child sees the env var, runs the real `ReplayHarness` against this
fixture, and the gate resolves against a real, writable per-run dir. No
production source change was needed — T2 already shipped the
`COPPERCLAW_DATA_ROOT` seam.

The fixture's tool calls therefore use a **fixed** project path under
`/tmp/copperclaw-x2-verify-gate` (the same `/tmp` convention
`prototype-golden` already uses), because the fixture's claude turns
hard-code the paths and the data root must match them.

### What this fixture asserts, for real

Beyond the byte-stable JSONL diff (`expected/*.jsonl`), the child test
asserts the refuse → fix → pass shape genuinely occurred, not that a
scripted sequence merely ran to completion:

- The two refusals reach the model as `tool_result` blocks (captured
  from the wiremock server's received request bodies): the refusal text
  (`unverified changes`), the recorded verify command
  (`python3 -m py_compile app.py`), and BOTH fix-cycle counts
  (`2 fix cycle(s) remaining` then `1 fix cycle(s) remaining`) — proof
  the loop advanced through a real failed verify rather than refusing
  statically.
- The failing verify run's stderr (`SyntaxError`) reached the model.
- The todo store (`agent_todos.json`, also under `COPPERCLAW_DATA_ROOT`)
  ends with the item `status: completed`, and the project's
  `.copperclaw/dirty` marker was cleared by the passing verify run.

Note the emitted `diff` card at message-out seq 17: the SECOND
`write_file` to `app.py` overwrites a file written earlier this session,
so `write_file` emits its overwrite-diff card — real behaviour, pinned
here.

### Not covered here (by design)

- **The happy path** (git-init a project, preview-expose, the P3 ritual
  card + screenshot). That's `prototype-golden`'s job — this fixture is
  deliberately the gate-only sibling.
- **The auto-`blocked` transition after `FIX_CYCLE_CAP` is exhausted.**
  This fixture always fixes the file before the second failed verify, so
  the cap is never reached. The auto-block transition has direct unit
  coverage in `copperclaw-mcp/src/tools/todo.rs`
  (`completion_auto_blocks_after_fix_cycle_cap_exhausted`); reproducing
  it end-to-end here would add turns without exercising any new pipeline
  path.
- **The HUD status-row leg.** See `prototype-golden/README.md` — the
  `cli` channel is not edit-capable, so its HUD is always the legacy
  `StatusRows` behaviour, gated behind a 60 s real-wall-clock first fire
  that a millisecond-scale replay never crosses. X2 did not add a clock
  seam, so this leg remains uncovered on `cli`; an edit-capable channel
  (telegram / slack / discord / matrix / webex) is the right vehicle.
