# teams/hud-live-edit — M19 U2 live HUD edit on teams

**Card:** M19 U2 (teams in-place edit + reactions) — X-rider Wave 2
(parity fixtures).

**What it proves.** Before U2, `teams` overrode the card / diff /
collapsible / todo / thinking / error surfaces but **not** the trait
`edit_message` — so it was absent from `EDIT_CAPABLE_CHANNELS`, the HUD
never resolved to live, and every "edit" the HUD attempted became a fresh
message (new-message spam). U2 added a trait `edit_message` override
(Teams supports message updates via `PATCH …/messages/{id}`) and added
teams to `EDIT_CAPABLE_CHANNELS` in the same PR. Now the runner's HUD
resolves to `Behavior::Live`: it posts **one** breadcrumb chip at the
first tool call and **edits that one message in place** on every later
frame instead of posting N.

**Shape.** One teams message ("check the build") drives a two-turn
scripted provider: turn 1 is a `shell` tool call (`echo build ok`), turn 2
is the final text answer. The HUD posts the chip on `on_batch_start` and
emits `update_breadcrumb` System rows on `on_batch_end` / `finalize`; the
host delivery loop threads the prior chip's anchor id through as
`existing_message_id` so the adapter edits in place.

**Harness modelling.** The replay harness substitutes a `MockAdapter` for
the real teams adapter; the bare mock re-posts every frame. The manifest's
`model_rich_breadcrumbs: true` makes the harness's `CappedAdapter` model
teams' real edit-in-place `deliver_breadcrumb` contract: the first frame
(no anchor) is a `Breadcrumb`-kind post returning a stable id; every later
frame routes through the inner mock's `edit_message`, landing in
`MockAdapter::edits()` against that one anchor. Same mechanism the F1
`matrix/hud-live-edit` fixture uses — U2 brings teams onto that live-HUD
floor.

**Assertions** (in `tests/replay.rs`, on top of the byte-stable JSONL
diff): exactly one `breadcrumb`-kind delivery (the single post — proof the
old "post N messages" spam is gone), at least one edit, all edits
addressed to a single anchor id in the originating teams conversation, and
the model's final answer still delivered as its own chat message.

**What it does NOT exercise.** The *real* teams `edit_message` /
`deliver_breadcrumb` wire calls against the Bot Framework — those live in
the teams adapter's own unit tests (`copperclaw-channels-teams`), and the
"every listed channel really overrides trait `edit_message`" invariant is
the F1 drift guard
(`copperclaw-host-delivery/tests/edit_capable_edit_message_drift.rs`),
which now includes teams. This fixture owns the host-side
delivery-pipeline half: anchor resolution and edit-not-repost threading on
teams.

Hand-authored from the matrix/hud-live-edit template. Regenerate
`expected/*.jsonl` from a real run with `COPPERCLAW_XR2_GENERATE=1 cargo
test -p copperclaw-host --test replay teams_hud_live_edit`.
