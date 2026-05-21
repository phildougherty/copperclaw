-- Seed a single "due now" inbound row for the session pre-populated in
-- central.sql. `process_after` is in the past so the wake check picks
-- it up on the very next sweep pass. `trigger = 1` is required for the
-- container manager's `count_due` filter to see the row (filtered out
-- otherwise — the harness mirrors what the router writes).
--
-- session_routing is seeded so when the runner emits a reply, the
-- delivery loop can find a target.
INSERT INTO messages_in (
  id, seq, kind, timestamp, status, process_after, recurrence,
  series_id, tries, trigger, platform_id, channel_type, thread_id,
  content, source_session_id, on_wake
) VALUES (
  '00000000-0000-0000-0000-0000000000c0',
  2,
  'task',
  '2026-01-01T00:00:00Z',
  'pending',
  '2026-01-01T00:00:00Z',
  NULL,
  NULL,
  0,
  1,
  'stdin',
  'cli',
  NULL,
  '{"text":"scheduled wake fired"}',
  NULL,
  0
);

INSERT INTO session_routing (id, channel_type, platform_id, thread_id) VALUES
  (1, 'cli', 'stdin', NULL);
