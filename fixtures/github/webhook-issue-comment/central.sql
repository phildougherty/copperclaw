-- Replay fixture seed: github/webhook-issue-comment.
--
-- Single agent group "Replay" wired to a single github messaging
-- group keyed on the platform_id "octocat/hello#7" (issue number 7 in
-- the octocat/hello repo, matching the shape ironclaw-channels-github
-- emits for `issue_comment.created`). Engage mode `pattern` with `.*`
-- so every inbound matches; session mode `shared`.
INSERT INTO agent_groups (id, name, folder, agent_provider, created_at) VALUES
  ('00000000-0000-0000-0000-000000000001', 'Replay', 'replay', 'anthropic', '2026-01-01T00:00:00Z');

INSERT INTO messaging_groups (id, channel_type, platform_id, name, is_group, unknown_sender_policy, created_at) VALUES
  ('00000000-0000-0000-0000-000000000002', 'github', 'octocat/hello#7', 'github/octocat/hello#7', 1, 'lenient', '2026-01-01T00:00:00Z');

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
