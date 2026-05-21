-- Replay fixture seed: cli/sender-not-approved.
--
-- Same single-agent + cli/stdin wiring as the happy-path fixture. The
-- difference is the inbound carries a `sender` field for an identity
-- that is not present in `users`; the approvals gate will refuse to
-- deliver and trigger a pending-approval notice instead.
--
-- The harness's pre-approved list for `cli/local` is bypassed because
-- the inbound's `sender.identity == "stranger"` (not `"local"`).
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
