-- Replay fixture seed: cli/grant-wake-granted (M22 A2, granted-act).
--
-- One agent group wired to cli/stdin, a pre-existing idle session, a
-- scheduled `tasks` row (`t-standup`), and — the point of this fixture — an
-- already-APPROVED `task_grants` row for that task. `inbound.sql` seeds the
-- pending `kind:task` wake; a single `SweepService::run_once()` wake pass
-- transitions the idle session to `running`; the harness then runs one turn.
--
-- Before that turn the harness calls the SAME host writer production uses at
-- spawn (`container_manager::tasks_snapshot::write_tasks_snapshot`, which folds
-- in the A2H grant writer): it resolves the firing task from the pending
-- inbound, reads `task_grants::effective_grant('t-standup')` → live, and renders
-- `<session>/grant.json`. The runner's M22 A2 gate loads that snapshot and OPENS
-- for the granted `web_fetch` action.
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

-- Pre-existing idle session — the sweep loop's wake check is the trigger, so
-- the session must already exist (the router never gets a fresh inbound here).
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

-- The scheduled task the autonomous fire runs. `task_grants.task_id` is a FK to
-- this row (migration 030), so it must exist before the grant.
INSERT INTO tasks (
  id, agent_group_id, session_id, name, prompt, when_spec,
  recurrence, next_fire, status, created_at, updated_at
) VALUES (
  't-standup',
  '00000000-0000-0000-0000-000000000001',
  '00000000-0000-0000-0000-000000000010',
  'standup',
  'Post the morning standup digest.',
  'daily at 09:00',
  '0 9 * * *',
  '2026-01-01T09:00:00Z',
  'active',
  '2026-01-01T00:00:00Z',
  '2026-01-01T00:00:00Z'
);

-- The live, human-approved capability grant. This is what opens the autonomy
-- gate for the `web_fetch` action on the scheduled fire. `status='approved'`
-- with a set `approved_at` is exactly what the approval APPLY arm writes; a far
-- future `expires_at` and a `max_fires` well above one keep it live for the run
-- (and let the delivery-side `grant_consume` genuinely decrement it).
INSERT INTO task_grants (
  id, task_id, capability_scope, token_budget, tokens_consumed,
  max_fires, fires_consumed, expires_at, granted_by, status,
  approved_at, revoked_at, created_at, updated_at
) VALUES (
  'grant-standup-001',
  't-standup',
  'web_fetch',
  100000,
  0,
  30,
  0,
  '2099-01-01T00:00:00Z',
  'op',
  'approved',
  '2026-01-01T00:00:00Z',
  NULL,
  '2026-01-01T00:00:00Z',
  '2026-01-01T00:00:00Z'
);
