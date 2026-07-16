# signal/hud-breadcrumb — M19 U1 live HUD breadcrumb on signal

**Card:** M19 U1 (signal rich-surface floor) — X-rider Wave 2 (parity
fixtures).

**What it proves.** Before U1, `signal` was the thinnest "rich" adapter:
it overrode only `deliver_card` and `edit_message`, so `deliver_breadcrumb`
(and the rest of the rich surfaces) fell through to plain text — the Task
HUD rendered as stacked prose, one fresh line per tool boundary. U1 added
a `render.rs` giving signal a native `deliver_breadcrumb`. Because signal
is in `EDIT_CAPABLE_CHANNELS`, the runner's HUD now resolves to
`Behavior::Live`: it posts **one** breadcrumb chip at the first tool call
and **edits that one chip in place** on every later frame — an in-place
breadcrumb, not stacked prose (the U1 acceptance line).

**Shape.** One signal message ("check the build") drives a two-turn
scripted provider: turn 1 is a `shell` tool call (`echo build ok`), turn 2
is the final text answer. The HUD:

- `on_batch_start` posts the chip (a `MessageKind::Breadcrumb` row).
- `on_batch_end` and `finalize` emit `update_breadcrumb` System rows.
- The host delivery loop resolves each update's anchor via
  `lookup_prior_breadcrumb_external_id` and calls `deliver_breadcrumb`
  with `existing_message_id = Some(anchor)` so the adapter edits in place.

**Harness modelling.** The replay harness substitutes a `MockAdapter` for
the real signal adapter; the bare mock degrades every rich surface to a
re-post. The manifest's `model_rich_breadcrumbs: true` makes the harness's
`CappedAdapter` model signal's real `deliver_breadcrumb` contract instead:
the first frame (no anchor) is a `Breadcrumb`-kind post returning a stable
id; every later frame (`existing_message_id = Some(anchor)`) routes through
the inner mock's `edit_message` so it lands in `MockAdapter::edits()`
against that one anchor. This is the closest an in-process harness (no real
signal-cli daemon) can get to "the HUD edits a real signal message in
place". This is the exact same mechanism the F1 `matrix/hud-live-edit`
fixture uses — U1 raises signal to that same live-HUD floor.

**Assertions** (in `tests/replay.rs`, on top of the byte-stable JSONL
diff): exactly one `breadcrumb`-kind delivery (the single post), at least
one edit, all edits addressed to a single anchor id in the originating
signal conversation, and the model's final answer still delivered as its
own chat message.

**What it does NOT exercise.** The *real* signal `deliver_breadcrumb` /
`edit_message` wire calls against a signal-cli daemon — those live in the
signal adapter's own unit tests (`copperclaw-channels-signal`), and the
"every listed channel really overrides trait `edit_message`" invariant is
the F1 drift guard
(`copperclaw-host-delivery/tests/edit_capable_edit_message_drift.rs`).
This fixture owns the host-side delivery-pipeline half: anchor resolution
and edit-not-repost threading on signal.

Hand-authored from the matrix/hud-live-edit template (same live-HUD floor,
different edit-capable channel). Regenerate `expected/*.jsonl` from a real
run with `COPPERCLAW_XR2_GENERATE=1 cargo test -p copperclaw-host --test
replay signal_hud_breadcrumb`.
