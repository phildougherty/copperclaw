# telegram/blocked-todo — M19 F4 blocked-todo rendering

**Card:** M19 F4 (make `blocked` todo state visible to users) — X-rider
Wave 1.

**What it proves.** M18's R3 added a real `TodoStatus::Blocked`
(+`blocked_reason`) to the runner's local store but flattened it to
`InProgress` on the portable wire enum, so a step that auto-blocked after
burning its verify fix-cycles showed as "in progress forever". F4 gave
`copperclaw_channels_core::TodoItemStatus` a real `Blocked` variant
(glyph `[!]` + one-line reason) and mapped `Blocked -> Blocked` in
`status_to_wire`. This fixture locks the user-visible half: on a rich
channel the delivered checklist renders the auto-blocked step **blocked**,
with its reason, not as in-progress.

**Shape (genuine auto-block).** The fixture drives the real gate, not a
hand-built list:

- Pre-seeded state under `COPPERCLAW_DATA_ROOT`: a project `proj/` that is
  dirty AND already at the fix-cycle cap (`.copperclaw/fix_cycles = 2 ==
  FIX_CYCLE_CAP`), with a recorded verify failure
  (`.copperclaw/last_failure`), plus one `in_progress` todo in
  `agent_todos.json`.
- The model attempts `todo_update(id=1, status="completed", evidence=…)`.
  The completion gate finds the dirty project already at the cap, so
  instead of refusing forever it **auto-transitions the todo to
  `blocked`**, attaches `last_failure` as the reason, and returns success.
- `emit_after_mutation` emits the post-mutation `TodoList`; the delivery
  loop degrades `deliver_todo_list` to its text fallback on the harness
  mock — the exact surface F4 taught the `[!]` glyph.

**Assertions** (in `tests/replay.rs`, on top of the byte-stable JSONL
diff): the delivered checklist contains `[!] Verify the build passes` and
the failure reason, does NOT contain the in-progress glyph `[~]` for that
step, and its footer counts `1 blocked`. It also reads the on-disk todo
store back and confirms the item genuinely auto-transitioned to `blocked`
with the reason attached — proving the render reflects real gate state.

**Re-exec.** Like the X2 `prototype-verify-gate` fixture, the todo store
and verify gate resolve against `COPPERCLAW_DATA_ROOT`, which
`forbid(unsafe_code)` blocks us from setting via `std::env::set_var`. So
the registered test re-execs the test binary as a child with the env var
set through the safe `Command::env`; the child runs the real
`ReplayHarness` against the pre-seeded, writable project state.

**Note on the HUD rows.** Telegram is edit-capable, so the run also emits
Task HUD breadcrumb frames. This fixture does NOT set
`model_rich_breadcrumbs`, so those degrade to plain text deliveries (the
bare-mock behaviour) — they're incidental here; the F1 fixture
(`matrix/hud-live-edit`) owns the edit-in-place HUD assertion.
