## discord / inbound-file-attachment

M18 C4b acceptance fixture for the inbound-file contract
(`copperclaw_channels_core::inbound_file`), Discord adapter.

Step 1 injects an `InboundEvent` shaped exactly like Discord's ingress
layer (`copperclaw-channels-discord`'s `message_create_to_inbound_downloaded`)
produces after a successful CDN download: `content.attachment` carries
`staged_path` (here `fixture://spec.csv`, which the harness's
`stage_fixture_files` resolves to a real on-disk copy of `files/spec.csv`
before routing — see `harness.rs`) and no `path` key. Discord CDN URLs are
public, so the adapter fetches the bytes with no auth header (unlike
Slack's `url_private`). The router materializes the staged bytes into the
resolved session's `inbox/dc-doc-001/spec.csv`, strips `staged_path`, and
rewrites the attachment's `path` to the container-visible
`/data/inbox/dc-doc-001/spec.csv` — the value asserted in
`expected/messages-in.jsonl`.

Step 2 injects the shape Discord's ingress produces when the reported or
actual size exceeds `max_attachment_bytes`: a `MessageKind::System` event
whose `content.attachment` carries `reason: "too_large"` and no
`staged_path` (nothing was ever staged, so there's nothing for the router
to materialize — `materialized_content` passes the content through
unchanged since the attachment has no `staged_path`).

Together the two steps pin the C4b acceptance line: a Discord attachment is
staged by the adapter, materialized by the router into the session inbox
with a container-visible `/data/inbox/...` path, and an oversized file
still yields the `too_large` system row (never a silent drop). The
"readable from a runner test" half is additionally covered by
`discord_inbound_file_attachment_file_readable_from_session_dir` in
`crates/copperclaw-host/tests/replay.rs`, which reads the materialized
bytes back off disk at the exact host path
`container_manager::spawn::build_spec` bind-mounts as `/data` in
production. The adapter-side download/stage/too_large/download_failed and
image `data_base64` inlining paths are unit-tested in
`crates/copperclaw-channels/discord/src/{events,rest,config}.rs`.
