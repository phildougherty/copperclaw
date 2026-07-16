# deltachat/card-and-todo — M19 U5 native card + todo chip on deltachat

**Card:** M19 U5 (bare-adapter card + todo floor) — X-rider Wave 2 (parity
fixtures).

**What it proves.** `deltachat` is a genuine interactive chat surface
(full chat, inbound files) that had **zero** rich-surface support — the
HUD, diffs, todo chips, and cards all rendered as plain prose. U5 gave it
a `render.rs` covering `deliver_card`, `deliver_todo_list`, and
`deliver_diff` natively. This fixture locks the *pipeline* consequence: an
approval card and a todo chip emitted by the runner reach the deltachat
adapter's rich-surface hooks, not a flattened prose `deliver`.

**Shape.** One deltachat message ("plan and stage the demo") drives a
four-turn scripted provider:

1. `todo_add "Scaffold the demo"` → emits a `TodoList` chip.
2. `todo_add "Wire the approval card"` → emits the updated `TodoList` chip
   (now two pending items).
3. `send_card` → an approval card (title, body, a `Status` field, an
   `Approve` **callback** button + an `Open preview` **url** button).
4. The closing text answer.

**Why `COPPERCLAW_DATA_ROOT` / a subprocess re-exec.** The `todo_*` tools
persist to a JSON store under the container data root
(`verify_gate::data_root()` → `/data` in production). The suite's
`forbid(unsafe_code)` makes `std::env::set_var` unavailable, so — exactly
like the F4 `telegram/blocked-todo` and X2 `cli/prototype-verify-gate`
fixtures — `tests/replay.rs` re-execs *this* test binary as a child with
`COPPERCLAW_DATA_ROOT` set through the safe `Command::env`. The child runs
the real `ReplayHarness` against a writable, per-run todo store.

**Harness modelling.** The replay harness substitutes a `MockAdapter` for
the real deltachat adapter. `model_rich_cards: true` makes the harness's
`CappedAdapter` model deltachat's card-capable contract: `deliver_card`
records a `MessageKind::Card`-kind delivery whose `content.card` keeps the
full structured card (both buttons intact) — a native card on the wire,
not prose. The **todo chip** takes the trait-default `deliver_todo_list`
text fallback on the bare mock — the exact surface U5's native renderer
replaces — so the delivered checklist carries the status glyphs
(`[ ]` pending) and the `0/2` footer; that its native structural rendering
is faithful on the wire is the deltachat adapter unit test's job, while
this fixture proves the chip is routed to `deliver_todo_list` with the
right content. (We model the card natively but not the todo to keep the
harness surface minimal; the F4 `telegram/blocked-todo` fixture already
locks the todo-chip text-fallback delivery contract.)

**Assertions** (in `tests/replay.rs`, on top of the byte-stable JSONL
diff): a `card`-kind delivery to the deltachat chat whose `content.card`
carries the title, body, field, and **both** structured buttons; a
`chat`-kind todo-chip delivery carrying both todo texts with their glyphs
and the `0/2` footer; and the closing text answer delivered as its own
chat message. The on-disk todo store (written under the data root)
corroborates the two items really exist.

**What it does NOT exercise.** The *real* deltachat JSON-RPC wire calls
its `render.rs` makes — those live in the deltachat adapter's own unit
tests (`copperclaw-channels-deltachat`). This fixture owns the host-side
pipeline leg: a card is delivered to a card-capable channel's
`deliver_card` with structure intact, and a todo chip is routed to
`deliver_todo_list`, on a formerly-bare interactive chat surface.

Hand-authored (scripted turns). Regenerate `expected/*.jsonl` from a real
run with `COPPERCLAW_XR2_GENERATE=1 cargo test -p copperclaw-host --test
replay deltachat_card_and_todo`.
