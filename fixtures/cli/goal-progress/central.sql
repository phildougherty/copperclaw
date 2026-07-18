-- Replay fixture seed: cli/goal-progress (M22 A3 goal check-in fan-out).
--
-- One agent group wired to cli/stdin, a pre-existing idle session, and a
-- first-class long-running `goals` row (migration 031) that is DUE a check-in
-- (`next_checkin` in the past) and RE-ARMS every 5 minutes
-- (`checkin_recurrence`). The AX X-rider drives two `SweepService::run_once`
-- passes at controlled instants (the sweep `MockClock` seam) that straddle the
-- re-armed `next_checkin`, so the goal fires a check-in wake on each pass and
-- the woken agent records progress via `update_goal` across ≥2 wakes.
--
-- No grant / token bound: `grant_id` and `token_budget` are NULL, so
-- `goals::budget_remaining` reads unbounded (`None`) and the goal never pauses.
-- The check-in itself (`update_goal`) is internal state, not a credentialed
-- external action, so no A1 grant is involved.
INSERT INTO agent_groups (id, name, folder, agent_provider, created_at) VALUES
  ('00000000-0000-0000-0000-000000000001', 'Replay', 'replay', 'anthropic', '2026-01-01T00:00:00Z');

INSERT INTO messaging_groups (id, channel_type, platform_id, name, is_group, unknown_sender_policy, created_at) VALUES
  ('00000000-0000-0000-0000-000000000002', 'cli', 'stdin', 'cli/stdin', 0, 'lenient', '2026-01-01T00:00:00Z');

INSERT INTO messaging_group_agents (
  id, messaging_group_id, agent_group_id,
  engage_mode, engage_pattern, sender_scope,
  ignored_message_policy, session_mode, priority, created_at
) VALUES (
  '00000000-0000-0000-0000-000000000003',
  '00000000-0000-0000-0000-000000000002',
  '00000000-0000-0000-0000-000000000001',
  'pattern', '.*', 'all',
  'drop', 'shared', 0,
  '2026-01-01T00:00:00Z'
);

INSERT INTO sessions (
  id, agent_group_id, messaging_group_id, thread_id, agent_provider,
  status, container_status, last_active, created_at
) VALUES (
  '00000000-0000-0000-0000-000000000010',
  '00000000-0000-0000-0000-000000000001',
  '00000000-0000-0000-0000-000000000002',
  NULL,
  'anthropic',
  'active',
  'idle',
  '2026-01-01T00:00:00Z',
  '2026-01-01T00:00:00Z'
);

-- The long-running goal. `next_checkin` is before pass 1's instant so the goal
-- is due; `checkin_recurrence` re-arms it 5 minutes out after each fire.
INSERT INTO goals (
  id, agent_group_id, session_id, objective, status,
  task_id, grant_id, token_budget, tokens_consumed,
  checkin_recurrence, checkin_prompt, next_checkin, last_checkin_at,
  checkin_count, created_at, updated_at
) VALUES (
  'g-standup',
  '00000000-0000-0000-0000-000000000001',
  '00000000-0000-0000-0000-000000000010',
  'Keep the public docs current with each release.',
  'active',
  NULL,
  NULL,
  NULL,
  0,
  '*/5 * * * *',
  NULL,
  '2026-06-01T00:00:00Z',
  NULL,
  0,
  '2026-01-01T00:00:00Z',
  '2026-01-01T00:00:00Z'
);
