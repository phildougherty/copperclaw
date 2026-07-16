## cli / prototype-verify-gate-multistage (M20 X-rider, Wave 1)

Sibling to `fixtures/cli/prototype-verify-gate` (M18 X2), which pins the
**pre-Q2** verify gate: one recorded command, refuse -> fix -> pass. This
fixture is the **Q2 extension**: a `.copperclaw/verify` with THREE named
stages, proving the todo-completion refusal narrows to name only the
stages that haven't yet recorded green — not a single project-wide
pass/fail bit — and that completion is allowed once every stage reads
green.

M18 X2's fixture is left untouched; this one does not replace it, and
does not re-exercise the fix-cycle (failed-verify-run) leg, which X2
already covers. This fixture's only new ground is the **multi-stage**
shape Q2 added.

### The scripted sequence

1. `todo_add` — one item to complete.
2. `write_file` `proj/.copperclaw/verify` — THREE stages:
   ```
   lint: python3 -m py_compile app.py
   typecheck: python3 -c "import app"
   test: python3 -c "import app; assert app.add_item([], 'milk') == ['milk']"
   ```
   (marks the project dirty and resets all stage state — Q2's
   `mark_dirty` behaviour).
3. `write_file` `proj/app.py` — a small, **correct** module
   (`add_item(items, item)`); unlike X2's fixture there is no broken
   file here — this fixture's job is the stage-narrowing shape, not the
   fix-cycle loop.
4. `shell` `python3 -m py_compile app.py` (`cwd` = the project) — matches
   the `lint` stage; **passes**, recorded green.
5. `todo_update` -> `completed` — **REFUSED**: `typecheck` and `test`
   haven't run since the dirty mark. The refusal names exactly those two
   stages and cites both their exact commands — `lint` (already green)
   is never re-named.
6. `shell` `python3 -c "import app"` — matches `typecheck`; **passes**,
   recorded green.
7. `todo_update` -> `completed` — **REFUSED again**, now naming ONLY
   `test` — proof the gate re-evaluates per-stage state on every
   attempt rather than caching the first refusal's stage list.
8. `shell` `python3 -c "import app; assert app.add_item([], 'milk') == \
   ['milk']"` — matches `test`; **passes**. All three stages now read
   green, so the project's dirty marker clears.
9. `todo_update` -> `completed` — **ALLOWED**: no project is dirty.
10. Final assistant text.

Registered in `crates/copperclaw-host/tests/replay.rs` as
`cli_prototype_verify_gate_multistage_refuse_narrow_pass`.

### The T2 re-exec seam (identical to X2)

Same problem X2 solved: `verify_gate::data_root()` / `todo.rs`'s path
resolution read `COPPERCLAW_DATA_ROOT` from process env, and the
workspace `forbid(unsafe_code)` blocks `std::env::set_var` in this
integration-test target. So the registered test re-execs itself as a
child (`--exact
cli_prototype_verify_gate_multistage_refuse_narrow_pass`) with
`COPPERCLAW_DATA_ROOT` set via the safe `std::process::Command::env`,
rooted at a **distinct** `/tmp` path
(`/tmp/copperclaw-m20x1-verify-stages`) from X2's
(`/tmp/copperclaw-x2-verify-gate`) so the two fixtures' child processes
never collide.

### What this fixture asserts, for real

Beyond the byte-stable JSONL diff (`expected/*.jsonl`), the child test
asserts the refuse -> narrow -> pass shape genuinely occurred:

- Both refusals reach the model as `tool_result` blocks (captured from
  the wiremock server's received request bodies) containing the
  `unverified changes` refusal text.
- The first refusal's `missing/failing stage(s): typecheck, test` names
  exactly the two unmet stages (order preserved from the verify file)
  and cites both their commands; it does NOT name `lint`.
- The second refusal narrows to `missing/failing stage(s): test (`
  only, and still never re-names `lint`.
- The todo store (`agent_todos.json`, also under `COPPERCLAW_DATA_ROOT`)
  ends with the item `status: completed`.
- The project's `.copperclaw/dirty` marker is cleared only once all
  three stages have passed.
- The project's `.copperclaw/stages` state file records all three of
  `lint` / `typecheck` / `test` as `passed: true` — the per-stage state
  file Q2 added, not just the absence of a dirty marker.

### Not covered here (by design — already covered elsewhere)

- **The fix-cycle (failed verify run) loop.** `fixtures/cli/prototype-verify-gate`
  (M18 X2) already pins refuse -> fix -> pass for a single recorded
  command, including a genuinely failing verify run and the two
  `fix cycle(s) remaining` counts. Reproducing a failing stage here
  would add turns without exercising any new Q2 pipeline path — Q2's
  stage-attributed failure text (`stage 'name' failed: <tail>`) has
  direct unit coverage in `copperclaw-mcp/src/tools/verify_gate.rs`
  (`record_stage_verify_failure_prefixes_tail_with_stage_name`).
- **The auto-`blocked` transition after `FIX_CYCLE_CAP` is exhausted.**
  Same reasoning as X2 — this fixture always advances a stage before any
  fix-cycle budget could be burned down; covered directly in
  `copperclaw-mcp/src/tools/todo.rs` unit tests.
- **The `ui_screenshot` vision-loop leg of the Wave-1 X-rider card.** See
  the top-level PR/commit notes: `ui_screenshot`'s `handle()` reaches a
  real chromium binary/process/WebSocket with no test-injectable seam
  (unlike `verify_gate`'s `COPPERCLAW_DATA_ROOT` override), so it cannot
  be driven through this in-process replay harness without either a live
  chromium or a product-code change — out of scope for the fixtures-only
  lane. The generic `RawContent::Image` -> provider image-block
  conversion this tool relies on is covered by a dedicated unit test,
  `ui_screenshot_tool_result_image_converts_to_provider_image_block` in
  `crates/copperclaw-providers/src/anthropic.rs`.
