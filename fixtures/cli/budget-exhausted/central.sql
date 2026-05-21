-- Replay fixture seed: cli/budget-exhausted.
--
-- Wires one agent group to cli/stdin (identical to the happy-path
-- fixtures), then adds two budget-relevant rows:
--
-- 1. `group_budgets` with `daily_token_cap = 100`.
-- 2. `agent_turns` with 150 input + 50 output tokens already spent
--    "today" (`ended_at = now()`). The container manager's
--    `is_over_budget` sums input+output since UTC midnight, so 200 >= 100
--    fires the gate.
--
-- We use `strftime` so the seeded ended_at is RFC3339 (the format
-- the budget-gate code parses + emits). Hard-coding a date would make
-- the fixture stale every UTC midnight; sourcing it from sqlite's
-- current time keeps the gate firing on any wall-clock day.
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

INSERT INTO group_budgets (
  agent_group_id, daily_token_cap, daily_cost_cap,
  agent_turns_per_minute_cap, agent_turns_per_hour_cap,
  updated_at
) VALUES (
  '00000000-0000-0000-0000-000000000001', 100, NULL,
  NULL, NULL,
  strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
);

-- "Earlier today" turn: 150 + 50 = 200 tokens, well over the cap.
INSERT INTO agent_turns (
  session_id, agent_group_id, seq, model, provider,
  input_tokens, output_tokens, started_at, ended_at, status, error
) VALUES (
  '00000000-0000-0000-0000-0000000000aa',
  '00000000-0000-0000-0000-000000000001',
  0, 'claude-sonnet-4-6', 'anthropic',
  150, 50,
  strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
  strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
  'ok', NULL
);
