## cli / prototype-golden (M18 X1 — program acceptance fixture)

One CLI inbound (`build me a tiny HTTP todo app`) drives an 8-round
scripted tool loop:

1. `shell` — `mkdir` + `git init` a fresh project (no `cwd`; the
   directory doesn't exist yet).
2. `write_file` — `app.py`, a small stdlib-only (`http.server`)
   in-memory HTTP todo server.
3. `write_file` — `.gitignore`.
4. `shell` (`cwd` = the project dir) — `python3 -m py_compile app.py`,
   a real verification step: the agent proves its own code is at
   least syntactically valid before calling the build done.
5. `shell` (`cwd` = the project dir) — `git add -A && git commit`.
6. `expose_preview(port=8000)` — mock-brokered: the harness wires a
   `FixturePreviewBroker` (mirrors `copperclaw-host-delivery`'s own
   `MockPreviewBroker` test double) onto `DeliveryService` via the
   already-public `set_preview_broker` API, so the call gets back a
   real shareable-URL response without a real Docker container.
7. `send_card` — the P3 "prototype ready" ritual shape: title,
   one-liner body, a "what to try" field, a project-path field, and an
   "Open preview" URL button.
8. Final assistant text, closing the turn.

Registered in `crates/copperclaw-host/tests/replay.rs` as
`cli_prototype_golden_path`.

### Two pieces of the card's acceptance line this fixture does NOT cover

The X1 card describes the golden path as "creates `/data/todo-app` as
a git repo, ... hits the verify gate, passes it, ... HUD finalizes."
Two of those are **not exercised here**, for reasons specific to how
the replay harness runs (in-process, no real container) rather than
gaps in the underlying feature. Both are diagnosed precisely below so
a future card doesn't have to re-derive them.

**1. The R3 verification gate (`todo_update` refused while dirty, then
succeeds) is not exercised.**

`verify_gate.rs`'s `data_root()` and `todo.rs`'s `todo_path()` both
hardcode the literal path `/data` (the container's bind-mounted
session dir in production). The replay harness runs the runner
in-process on the **host machine**, with no real container — so a
tool call that touches `/data` touches the literal host path `/data`.
On this dev box (and, by construction, any unprivileged CI runner)
that path is a real, root-owned, unwritable directory:

```
$ ls -ld /data
drwxr-xr-x 3 root root 4096 May 18  2025 /data
$ touch /data/x
touch: cannot touch '/data/x': Permission denied
```

Both modules already have a test-only override (`data_root_test_override_set`
/ `todo_test_override_set`), but both are `#[cfg(test)]`-gated to
`copperclaw-mcp`'s **own** unit-test compilation — `cfg(test)` never
activates when another crate (here, `copperclaw-host`'s separate
`tests/replay.rs` integration-test binary) links `copperclaw-mcp` as
an ordinary dependency, so the override is invisible from the replay
harness no matter what.

We attempted the smallest fix that would close this gap: un-gate the
existing override functions (`#[doc(hidden)] pub fn`, zero production
call sites, default behaviour byte-identical) so the harness could
point `/data` at a hermetic per-run tempdir. The security review
flagged that change as a genuine capability weakening — "a bypass
mechanism for the verify gate's path resolution shipped into
production" — and declined it pending the user's own explicit
sign-off, which is outside this card's authorization. We reverted it
rather than route around the denial (see git history on this branch:
the revert is its own commit) and scoped the fixture down instead, per
the card's own instruction to prefer an honest smaller fixture over a
forced stub.

Practical effect: any `todo_add` / `todo_update` call in this harness
fails for real (`ToolError::Internal`, permission denied writing
`/data/agent_todos.json`), and any `write_file` / `shell(cwd=...)`
under `/data` never gets picked up by `verify_gate::project_root_of`
unless it's literally rooted there. So this fixture's project lives at
`/tmp/copperclaw-x1-golden/todo-app` instead (a path the harness
process can actually write) — real git init, real file writes, a real
verify command, all genuinely executed — but the todo/verify-gate
machinery, which is `/data`-only, never engages, and there is no
honest way to script a `todo_update` refusal against it here.

This is the same root cause R3's own status notes already flagged
(the "build-verify-loop" e2e fixture was left as an incomplete stretch
goal — "don't let it block merging"); this fixture's attempt just
diagnosed the *specific* reason precisely instead of leaving it as an
unexplained gap. The right fix is a small, explicitly-authorized
follow-up: either a `test-support` Cargo feature (compiled out of
every real binary, opted into only by `copperclaw-host`'s
`[dev-dependencies]`) or an unconditional env-var override analogous
to the shell tool's already-shipped, already-production
`COPPERCLAW_SHELL_STATE_FILE` (`computer_use.rs`'s
`SHELL_STATE_ENV_OVERRIDE`) — that one isn't `#[cfg(test)]`-gated at
all and is exactly the shape needed for `verify_gate`/`todo`, it just
doesn't exist for them yet. Either is a one-file, low-risk change; it
just needs sign-off as its own reviewed PR rather than being smuggled
in under X1's fixtures-only lane.

**2. The H1 Task HUD's live-editing / finalized "done in M:SS, N tool
calls" content never appears.**

`cli` is not in `copperclaw_channels_core::capabilities::EDIT_CAPABLE_CHANNELS`
(only `telegram` / `slack` / `discord` / `matrix` / `webex` are — the
real `CliAdapter` genuinely has no `edit_message` implementation, so
this isn't a stale list). Per `hud.rs`'s `TaskHud::new`, that resolves
the HUD's `Behavior` to `StatusRows` for every cli-channel inbound,
`hud_mode` notwithstanding — `Behavior::Live` / `Behavior::FinalOnly`
are structurally unreachable on this channel. `StatusRows` only
surfaces the pre-H1 periodic "still working" heartbeat, gated behind a
**60-second real wall-clock** `STATUS_INTERVAL`, and even that
heartbeat's `finalize()` arm is a no-op (`_ => {}`) — there is no
"done in M:SS" collapse for a bare channel at all, by design (see
`hud.rs`'s `finalize`). A scripted fixture that runs in milliseconds
never crosses that 60 s threshold, so the HUD path contributes exactly
zero rows to `messages-out.jsonl` / `delivered.jsonl` here — which is
itself the correct, byte-stable, "unchanged from pre-H1" assertion for
this channel, just not the "HUD finalizes" behaviour the card
describes (that needs an edit-capable channel — telegram / slack /
discord / matrix / webex — to even engage). The replay harness's own
`run_one_turn` comment already flags this exact gap in passing
("cli -> legacy status rows, none within a fast replay's 60s budget").
A future card wanting to pin live HUD behaviour needs a fixture on one
of the edit-capable channels, not `cli`.

### What this fixture DOES pin, for real

- The inbound → router → runner → outbound → delivery pipeline for a
  build-shaped request, end to end, with zero mocking of the pipeline
  itself.
- A real git-repo-per-project workflow (`git init`, `write_file`,
  commit) — genuinely executed against the filesystem, not stubbed.
- A real "verify before claiming done" step (`python3 -m py_compile`)
  — genuinely executed, genuinely exits 0.
- The M17 preview-expose relay end to end: `expose_preview` writes the
  reserved `__preview` request row, the delivery loop's `drain_mcp_calls`
  routes it to an injected broker, and the runner gets back a real
  `PreviewExposed` response — pinned via a deterministic
  `FixturePreviewBroker`, not a real Docker container (the real
  broker, `copperclaw-host`'s `PreviewManager`, has its own unit
  tests).
- The P3 ritual `send_card` shape (title, one-liner, a "what to try"
  field, a project-path field, an "Open preview" button) rendering
  through the cli channel's `deliver_card` → text-fallback path.
