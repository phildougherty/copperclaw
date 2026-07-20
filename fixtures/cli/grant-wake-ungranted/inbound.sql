-- Seed the scheduled-fire wake row for the session in central.sql (M22 A2,
-- ungranted-propose). Same shape as grant-wake-granted's inbound.sql, but the
-- firing task (`t-report`) has NO `task_grants` row, so the autonomy gate stays
-- closed for the credentialed-external action the turn attempts.
INSERT INTO messages_in (
  id, seq, kind, timestamp, status, process_after, recurrence,
  series_id, tries, trigger, platform_id, channel_type, thread_id,
  content, source_session_id, on_wake
) VALUES (
  '00000000-0000-0000-0000-0000000000c2',
  2,
  'task',
  '2026-01-01T09:00:00Z',
  'pending',
  '2026-01-01T00:00:00Z',
  NULL,
  't-report',
  0,
  1,
  'stdin',
  'cli',
  NULL,
  '{"text":"Email the quarterly report to the board.","task_id":"t-report","task_name":"quarterly-report"}',
  NULL,
  1
);

-- Routing so the runner's read-then-propose reply reaches the user.
INSERT INTO session_routing (id, channel_type, platform_id, thread_id) VALUES
  (1, 'cli', 'stdin', NULL);
