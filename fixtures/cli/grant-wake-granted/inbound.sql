-- Seed the scheduled-fire wake row for the session pre-populated in central.sql
-- (M22 A2, granted-act). A scheduled fire is a `kind:task` row the sweep
-- synthesises straight into `messages_in` (it bypasses the router); here we seed
-- it directly so the wake check picks it up on the next `run_once()` pass.
--
-- `content.task_id = 't-standup'` and `series_id = 't-standup'` are what the
-- runner's `firing_task_id` (and the host grant writer's `firing_task_id_from_inbound`)
-- resolve to look the grant up. `process_after` is in the past and `trigger = 1`
-- so the row is due-now and counts for the wake / spawn gate. `on_wake = 1`
-- marks it a wake-only row.
INSERT INTO messages_in (
  id, seq, kind, timestamp, status, process_after, recurrence,
  series_id, tries, trigger, platform_id, channel_type, thread_id,
  content, source_session_id, on_wake
) VALUES (
  '00000000-0000-0000-0000-0000000000c1',
  2,
  'task',
  '2026-01-01T09:00:00Z',
  'pending',
  '2026-01-01T00:00:00Z',
  NULL,
  't-standup',
  0,
  1,
  'stdin',
  'cli',
  NULL,
  '{"text":"Post the morning standup digest.","task_id":"t-standup","task_name":"standup"}',
  NULL,
  1
);

-- Routing so the runner-emitted chat reply has a delivery destination.
INSERT INTO session_routing (id, channel_type, platform_id, thread_id) VALUES
  (1, 'cli', 'stdin', NULL);
