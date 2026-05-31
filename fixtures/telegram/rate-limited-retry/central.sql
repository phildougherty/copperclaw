-- Replay fixture seed: telegram/rate-limited-retry.
--
-- Same minimal wiring as telegram/inbound-text-message — a single
-- agent group connected to a single telegram chat. The fixture's
-- value lives in the manifest's `pre_delivery_failures` (one queued
-- Rate { retry_after: 1 } on the first deliver) plus `redrive_after_ms`
-- so the harness waits past the rate-limit window and tries again.
INSERT INTO agent_groups (id, name, folder, agent_provider, created_at) VALUES
  ('00000000-0000-0000-0000-000000000001', 'Replay', 'replay', 'anthropic', '2026-01-01T00:00:00Z');

INSERT INTO messaging_groups (id, channel_type, platform_id, name, is_group, unknown_sender_policy, created_at) VALUES
  ('00000000-0000-0000-0000-000000000002', 'telegram', '100', 'telegram/100', 0, 'lenient', '2026-01-01T00:00:00Z');

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
