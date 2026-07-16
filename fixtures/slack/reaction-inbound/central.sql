-- Replay fixture seed: slack/reaction-inbound (M19 U7, X-rider W2).
--
-- Single agent group "Replay" wired to a slack CHANNEL (channel id
-- "C123ABC", is_group = 1) with engage mode `mention` and NO pattern: an
-- unmentioned plain message would be mention-gated and dropped, so the
-- ONLY reason the reaction routes is the router's interaction-payload
-- bypass (content.reaction is whitelisted past the mention gate exactly
-- like a slack block_action callback). This is the slack parity twin of
-- telegram/reaction-steer — the router reaction leg is channel-agnostic.
INSERT INTO agent_groups (id, name, folder, agent_provider, created_at) VALUES
  ('00000000-0000-0000-0000-000000000001', 'Replay', 'replay', 'anthropic', '2026-01-01T00:00:00Z');

INSERT INTO messaging_groups (id, channel_type, platform_id, name, is_group, unknown_sender_policy, created_at) VALUES
  ('00000000-0000-0000-0000-000000000002', 'slack', 'C123ABC', 'slack/C123ABC', 1, 'lenient', '2026-01-01T00:00:00Z');

INSERT INTO messaging_group_agents (
  id, messaging_group_id, agent_group_id,
  engage_mode, engage_pattern, sender_scope,
  ignored_message_policy, session_mode, priority, created_at
) VALUES (
  '00000000-0000-0000-0000-000000000003',
  '00000000-0000-0000-0000-000000000002',
  '00000000-0000-0000-0000-000000000001',
  'mention', NULL, 'all',
  'drop', 'shared', 0,
  '2026-01-01T00:00:00Z'
);
