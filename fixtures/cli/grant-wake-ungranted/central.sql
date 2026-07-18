-- Replay fixture seed: cli/grant-wake-ungranted (M22 A2, ungranted-propose).
--
-- One agent group wired to cli/stdin plus a pre-existing idle session — the
-- same minimal shape as scheduled-wake. The WHOLE POINT of this fixture is the
-- ABSENCE of a grant: there is deliberately NO `task_grants` row for the firing
-- task (`t-report`), so `effective_grant('t-report')` reads `None`, the A2H
-- writer produces no `grant.json`, and the runner's autonomy brake stays CLOSED.
-- A credentialed-external action therefore blocks and the turn falls to
-- read-then-propose.
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
