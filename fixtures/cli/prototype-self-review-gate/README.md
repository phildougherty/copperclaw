## cli / prototype-self-review-gate (M20 Q6)

Pins the enforced self-review gate before final delivery: completing the
final/delivery todo of a project that has never been reviewed refuses,
naming `self_review` and `load_skill("code-review")`; calling `self_review`
(read, then submit `no_findings: true`) records the review; completion then
succeeds.

Sibling to `fixtures/cli/prototype-verify-gate` (M18 X2) and
`fixtures/cli/prototype-verify-gate-multistage` (M20 X-rider Wave 1), which
pin the SEPARATE M20 Q2 verify gate. This fixture deliberately records and
runs a trivial one-stage `.copperclaw/verify` (`python3 -m py_compile
app.py`) early so the pre-existing verify gate reads clean by the time the
todo-completion attempts happen — the refusal under test here is
exclusively the NEW Q6 review gate's, not a re-exercise of Q2's.

### The scripted sequence

1. `todo_add` — one item ("Build and ship the greeting script"). A
   single-item list makes completing it trivially the FINAL/delivery
   todo — see `crate::tools::todo`'s Q6 completion-gate extension for the
   exact "final" definition (no other item still `pending`/`in_progress`).
2. `write_file` `proj/app.py` — a one-line scaffold (`print('hello')`).
3. `shell` `git init && git add -A && git commit -m 'init: scaffold'`
   (`cwd` = the project) — every coding project is supposed to be its own
   git repo from the first edit (`skills/coding-task/SKILL.md`); `self_review`
   only ever engages for a project that is one (see the module docs in
   `crates/copperclaw-mcp/src/tools/self_review.rs`).
4. `write_file` `proj/app.py` — the real implementation (a `greet()`
   function), left UNCOMMITTED. This is the diff `self_review` will show.
5. `write_file` `proj/.copperclaw/verify` — records the one-stage verify
   command. Writes under `.copperclaw/` are exempt from the M20 Q8
   dirty-marking rule, so this alone doesn't dirty the project.
6. `shell` `python3 -m py_compile app.py` (`cwd` = the project) — matches
   the recorded verify stage exactly; **passes**, clearing the M20 Q2
   verify gate's dirty marker. From here on, only the Q6 review gate can
   refuse completion.
7. `todo_update` -> `completed` — **REFUSED**: the project has never been
   self-reviewed. The refusal names it as the final/delivery todo, teaches
   `self_review` and `load_skill("code-review")`, and reports "1 review
   cycle(s) remaining" (the first of `REVIEW_CYCLE_CAP = 2`).
8. `self_review` (`project` only, no `findings`/`no_findings`) — the READ
   phase: returns the diff since the project's first commit (there is no
   prior review marker yet), which is exactly the `greet()` implementation
   from step 4.
9. `self_review` (`no_findings: true`) — the SUBMIT phase: writes
   `.copperclaw/reviewed`, recording the current state as reviewed.
10. `todo_update` -> `completed` — **ALLOWED**: the project is no longer
    dirty-since-review (nothing changed since the marker was written), and
    the (already-clean) verify gate has nothing to add.
11. Final assistant text.

Registered in `crates/copperclaw-host/tests/replay.rs` as
`cli_prototype_self_review_gate_refuse_review_pass`.

### The re-exec seam (identical to X2 / M20X1)

Same problem those fixtures solved: `verify_gate::data_root()` (which
`self_review`'s session-wide scan also consults) and `todo.rs`'s path
resolution read `COPPERCLAW_DATA_ROOT` from process env, and the workspace
`forbid(unsafe_code)` blocks `std::env::set_var` in this integration-test
target. So the registered test re-execs itself as a child (`--exact
cli_prototype_self_review_gate_refuse_review_pass`) with
`COPPERCLAW_DATA_ROOT` set via the safe `std::process::Command::env`,
rooted at a **distinct** `/tmp` path
(`/tmp/copperclaw-q6-self-review-gate`) from X2's
(`/tmp/copperclaw-x2-verify-gate`) and M20X1's
(`/tmp/copperclaw-m20x1-verify-stages`) so the three fixtures' child
processes never collide.

### What this fixture asserts, for real

Beyond the byte-stable JSONL diff (`expected/*.jsonl`), the child test
asserts the refuse -> review -> pass shape genuinely occurred (via the
provider request bodies the wiremock server captured, since the refusal
and the `self_review` tool responses are handed back to the model as
`tool_result` blocks, not written to `messages_out`/`delivered`):

- The refusal names the final/delivery todo, teaches `self_review` and
  `load_skill("code-review")`, and reports the review-cycle budget.
- `self_review`'s READ-phase diff (containing the real `greet()`
  implementation) reached the model — proof the tool ran a genuine `git
  diff`, not a stub.
- `self_review`'s SUBMIT-phase acknowledgement (`"mode": "submit"`) reached
  the model.
- The todo store (`agent_todos.json`, also under `COPPERCLAW_DATA_ROOT`)
  ends with the item `status: completed`.
- `<project>/.copperclaw/reviewed` exists on disk — the marker
  `self_review`'s submit phase owns.

### Not covered here (by design — already covered elsewhere)

- **The post-review re-dirty / re-refuse leg and the `REVIEW_CYCLE_CAP`
  auto-`blocked` transition.** Both have direct, thorough unit coverage in
  `crates/copperclaw-mcp/src/tools/todo.rs`
  (`post_review_edit_redirties_and_rerefuses`,
  `review_cap_stops_the_third_cycle_with_blocked`) and
  `crates/copperclaw-mcp/src/tools/self_review.rs`
  (`review_state_dirty_after_edit_following_a_clean_review`). Reproducing
  either here would add turns (and a second `self_review` diff to author)
  without exercising any new pipeline path — the same reasoning
  `prototype-verify-gate-multistage`'s README gives for not re-covering the
  fix-cycle loop.
- **Non-final todos are never review-gated**, and **a non-git project is
  never gated at all.** Both are pure gate-logic properties independent of
  the channel pipeline — unit-tested directly in `todo.rs`
  (`non_final_todo_completion_is_never_review_gated`,
  `non_git_project_is_never_review_gated`) and `self_review.rs`
  (`review_state_not_applicable_for_non_git_dir`).
- **`verify_gate=off` byte-stability.** Also a pure gate-logic property
  (`crate::context::ToolContext::verify_gate_enabled`), unit-tested in
  `todo.rs` (`review_gate_off_when_verify_gate_disabled`) rather than
  threaded through a group-config fixture variant.
