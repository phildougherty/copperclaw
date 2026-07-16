## telegram / inbound-document-attachment

M18 C3 acceptance fixture for the inbound-file contract
(`copperclaw_channels_core::inbound_file`).

Step 1 injects an `InboundEvent` shaped exactly like Telegram's ingress
layer produces after a successful `getFile` download: `content.attachment`
carries `staged_path` (here `fixture://spec.csv`, which the harness's
`stage_fixture_files` resolves to a real on-disk copy of
`files/spec.csv` before routing — see `harness.rs`) and no `path` key.
The router materializes the staged bytes into the resolved session's
`inbox/tg-doc-001/spec.csv`, strips `staged_path`, and rewrites the
attachment's `path` to the container-visible `/data/inbox/tg-doc-001/
spec.csv` — the value asserted in `expected/messages-in.jsonl`.

Step 2 injects the shape Telegram's ingress produces when the reported
or actual size exceeds `max_attachment_bytes`: a `MessageKind::System`
event with `reason: "too_large"` and no `attachment` wrapper (nothing
was ever staged, so there's nothing for the router to materialize —
`materialized_content` passes the content through unchanged since it
has no `content.attachment` object).

Together the two steps pin the C3 acceptance line: "e2e: telegram
document fixture → file readable at `/data/inbox/...` from a runner
test; attachment path in `messages_in` is the container path; oversized
file still yields the `too_large` system row." The "readable from a
runner test" half is additionally covered at the router-unit level in
`crates/copperclaw-host-router/src/route.rs` (`staged_attachment_materializes_into_session_inbox`
and friends), which read the bytes back off disk at the exact host path
`container_manager::spawn::build_spec` bind-mounts as `/data` in
production.
