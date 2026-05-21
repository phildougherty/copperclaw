-- Replay fixture seed: webhooks/generic-hmac.
--
-- Single agent group "Replay" wired to a single webhooks messaging
-- group keyed on platform_id "grafana" (the suffix of the path the
-- generic webhook adapter routes on). HMAC verification itself lives
-- in the channel crate; the harness pushes the post-verification
-- `InboundEvent` straight at the router. Engage mode `pattern` with
-- `.*` so every inbound matches; session mode `shared`.
INSERT INTO agent_groups (id, name, folder, agent_provider, created_at) VALUES
  ('00000000-0000-0000-0000-000000000001', 'Replay', 'replay', 'anthropic', '2026-01-01T00:00:00Z');

INSERT INTO messaging_groups (id, channel_type, platform_id, name, is_group, unknown_sender_policy, created_at) VALUES
  ('00000000-0000-0000-0000-000000000002', 'webhooks', 'grafana', 'webhooks/grafana', 0, 'lenient', '2026-01-01T00:00:00Z');

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
