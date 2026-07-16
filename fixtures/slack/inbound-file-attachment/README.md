## slack / inbound-file-attachment

M18 C4a acceptance fixture for the Slack half of the inbound-file
contract (`copperclaw_channels_core::inbound_file`).

Step 1 injects an `InboundEvent` shaped exactly like the Slack events
router produces after a successful `url_private` download: the file was
fetched with the bot token, size-capped, and staged, so
`content.attachment` carries `staged_path` (here `fixture://spec.csv`,
which the harness's `stage_fixture_files` resolves to a real on-disk copy
of `files/spec.csv` before routing — see `harness.rs`) and no `path` key.
The router materializes the staged bytes into the resolved session's
`inbox/1700000200.000001/spec.csv`, strips `staged_path`, and rewrites the
attachment's `path` to the container-visible
`/data/inbox/1700000200.000001/spec.csv` — the value asserted in
`expected/messages-in.jsonl`.

Step 2 injects the shape the Slack router produces when the reported or
actual size exceeds `max_attachment_bytes`: a `MessageKind::System` event
with `reason: "too_large"` and no `attachment` wrapper (nothing was ever
staged, so there is nothing for the router to materialize).

Together the two steps pin the C4a acceptance line: "Per-adapter unit +
one fixture each mirroring the C3 telegram fixture. Download failure →
`download_failed` system row, never a silent drop." The `url_private`
download itself (bot-token auth, size caps, the `too_large` /
`download_failed` taxonomy, and small-image `data_base64` inlining) is
exercised end-to-end against a mock Slack server in the adapter's own unit
tests (`crates/copperclaw-channels/slack/src/events/router.rs`), because
the replay harness drives already-parsed `InboundEvent`s (`replay.mode =
"direct"`) rather than replaying the webhook + download transport.
