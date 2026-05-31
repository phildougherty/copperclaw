-- Replay fixture seed: matrix/room-message.
--
-- Single agent group "Replay" wired to a single matrix messaging
-- group keyed on room id "!a:m.org" (the same room id used in
-- copperclaw-channels-matrix's parse unit tests). Engage mode `pattern`
-- with `.*` so every inbound matches; session mode `shared`.
INSERT INTO agent_groups (id, name, folder, agent_provider, created_at) VALUES
  ('00000000-0000-0000-0000-000000000001', 'Replay', 'replay', 'anthropic', '2026-01-01T00:00:00Z');

INSERT INTO messaging_groups (id, channel_type, platform_id, name, is_group, unknown_sender_policy, created_at) VALUES
  ('00000000-0000-0000-0000-000000000002', 'matrix', '!a:m.org', 'matrix/!a:m.org', 1, 'lenient', '2026-01-01T00:00:00Z');

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
