-- Replay fixture seed: gchat/approval-card (M19 U4, X-rider W2).
--
-- Single agent group "Replay" wired to a single Google Chat space keyed
-- on "spaces/AAAAq.replay". Engage mode `pattern` with `.*` so every
-- inbound matches; session mode `shared`.
--
-- M19 U4 gave gchat a native trait `deliver_card` (Cards v2 builder), so
-- the M18 approval/ritual cards render structurally instead of degrading
-- to plain text. The fixture drives a `send_card` turn; with the manifest's
-- `model_rich_cards` the harness models gchat's card-capable contract and
-- the delivered row is a structured `Card`-kind row (buttons intact), not
-- a flattened prose chat row.
INSERT INTO agent_groups (id, name, folder, agent_provider, created_at) VALUES
  ('00000000-0000-0000-0000-000000000001', 'Replay', 'replay', 'anthropic', '2026-01-01T00:00:00Z');

INSERT INTO messaging_groups (id, channel_type, platform_id, name, is_group, unknown_sender_policy, created_at) VALUES
  ('00000000-0000-0000-0000-000000000002', 'gchat', 'spaces/AAAAq.replay', 'gchat/space', 1, 'lenient', '2026-01-01T00:00:00Z');

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
