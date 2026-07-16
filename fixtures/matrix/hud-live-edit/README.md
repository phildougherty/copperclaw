# matrix/hud-live-edit — M19 F1 live HUD edit on matrix

**Card:** M19 F1 (reconcile `EDIT_CAPABLE_CHANNELS` with real trait edit
support) — X-rider Wave 1.

**What it proves.** Matrix has always been listed in
`channels/core::capabilities::EDIT_CAPABLE_CHANNELS`, which promises the
M18 Task HUD it can edit a delivered message in place. Before F1 the
matrix adapter never overrode the trait `edit_message`, so the promise
was a lie — the HUD silently degraded. F1 fixed the adapter; this fixture
locks the *pipeline* consequence: on matrix the HUD posts **one**
breadcrumb chip at the first tool call and **edits that one chip in
place** on every later frame, never re-posting.

**Shape.** One matrix room message ("check the build") drives a two-turn
scripted provider: turn 1 is a `shell` tool call (`echo build ok`), turn 2
is the final text answer. Because matrix is edit-capable, the runner's
HUD resolves to `Behavior::Live`:

- `on_batch_start` posts the chip (a `MessageKind::Breadcrumb` row).
- `on_batch_end` and `finalize` emit `update_breadcrumb` System rows.
- The host delivery loop resolves each update's anchor via
  `lookup_prior_breadcrumb_external_id` and calls `deliver_breadcrumb`
  with `existing_message_id = Some(anchor)` so the adapter edits in place.

**Harness modelling.** The replay harness substitutes a `MockAdapter` for
the real matrix adapter, and the bare mock degrades every rich surface to
plain text (a re-post). The manifest's `model_rich_breadcrumbs: true`
makes the harness's `CappedAdapter` faithfully model matrix's real
`deliver_breadcrumb` contract instead: the first frame (no anchor) is a
`Breadcrumb`-kind post returning a stable id; every later frame
(`existing_message_id = Some(anchor)`) is routed through the inner mock's
`edit_message` so it lands in `MockAdapter::edits()` addressed to that one
anchor. This is the closest an in-process harness (no real Matrix
homeserver) can get to "the HUD edits a real matrix message in place".

**Assertions** (in `tests/replay.rs`, on top of the byte-stable JSONL
diff): exactly one `breadcrumb`-kind delivery (the single post), at least
one edit, all edits addressed to a single anchor id in the originating
room, and the model's final answer still delivered as its own chat
message.

**What it does NOT exercise.** The *real* matrix `edit_message` /
`deliver_breadcrumb` wire calls against a homeserver — those live in the
matrix adapter's own unit tests, and the "every listed channel really
overrides trait `edit_message`" invariant is the F1 drift guard
(`copperclaw-host-delivery/tests/edit_capable_edit_message_drift.rs`).
This fixture owns the host-side delivery-pipeline half: anchor resolution
and edit-not-repost threading.
