-- Replay fixture seed: slack/approval-block-action (M18 G1 in-chat approvals).
--
-- One agent group wired to a Slack channel (C100). Two registered senders both
-- pass the approvals sender-scope gate, but only the Owner may resolve:
--
--   * slack:U200 (user 035359c2-...) — granted global Owner.
--   * slack:U300 (user 66bb523d-...) — registered, NO role.
--
-- (User ids are UUIDv5(nil, "<kind>:<identity>") — see `users::derive_user_id`.)
--
-- Two pending approvals are pre-seeded with a recorded `platform_message_id`
-- (the card's Slack `ts`), as though their cards were already delivered.

INSERT INTO agent_groups (id, name, folder, agent_provider, created_at) VALUES
  ('00000000-0000-0000-0000-000000000001', 'Replay', 'replay', 'anthropic', '2026-01-01T00:00:00Z');

INSERT INTO messaging_groups (id, channel_type, platform_id, name, is_group, unknown_sender_policy, created_at) VALUES
  ('00000000-0000-0000-0000-000000000002', 'slack', 'C100', 'slack/C100', 1, 'lenient', '2026-01-01T00:00:00Z');

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

-- Registered senders (ids are UUIDv5(nil, "slack:<identity>")).
INSERT INTO users (id, kind, display_name, created_at) VALUES
  ('035359c2-ec4a-570c-be29-581200a2f00b', 'slack', 'Owner Olivia', '2026-01-01T00:00:00Z'),
  ('66bb523d-4c99-5aed-8281-723ace79c06e', 'slack', 'Stranger Sam', '2026-01-01T00:00:00Z');

INSERT INTO user_roles (user_id, role, agent_group_id, granted_by, granted_at) VALUES
  ('035359c2-ec4a-570c-be29-581200a2f00b', 'owner', NULL, NULL, '2026-01-01T00:00:00Z');

INSERT INTO pending_approvals (
  approval_id, session_id, request_id, action, payload, created_at,
  agent_group_id, channel_type, platform_id, platform_message_id,
  expires_at, status, title, options_json
) VALUES
  ('00000000-0000-0000-0000-0000000000c1', NULL, 'req-c1', 'sender', '{}', '2026-01-01T00:00:00Z',
   '00000000-0000-0000-0000-000000000001', 'slack', 'U901', 'slack-ts-c1',
   NULL, 'pending', 'Approve sender slack:U901?', '[]'),
  ('00000000-0000-0000-0000-0000000000c2', NULL, 'req-c2', 'sender', '{}', '2026-01-01T00:00:00Z',
   '00000000-0000-0000-0000-000000000001', 'slack', 'U902', 'slack-ts-c2',
   NULL, 'pending', 'Approve sender slack:U902?', '[]');
