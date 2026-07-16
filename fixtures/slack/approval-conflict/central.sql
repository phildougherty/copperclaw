-- Replay fixture seed: slack/approval-conflict (M19 F3 b — "already resolved").
--
-- One agent group wired to a Slack channel (C100). Owner Olivia (slack:U200,
-- global Owner) may resolve approvals.
--
-- Approval e1 was ALREADY resolved (approved by "host" — e.g. via the CLI or a
-- faster tapper) with its decision recorded. Owner then taps Approve on the
-- still-visible card. The losing tap must not be silent: the interceptor tells
-- the tapper who resolved it ("already resolved by host") instead of no-op'ing.
--
-- (User ids are UUIDv5(nil, "<kind>:<identity>") — see `users::derive_user_id`.)

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

-- Owner Olivia (id = UUIDv5(nil, "slack:U200")) holds a global Owner role.
INSERT INTO users (id, kind, display_name, created_at) VALUES
  ('035359c2-ec4a-570c-be29-581200a2f00b', 'slack', 'Owner Olivia', '2026-01-01T00:00:00Z');

INSERT INTO user_roles (user_id, role, agent_group_id, granted_by, granted_at) VALUES
  ('035359c2-ec4a-570c-be29-581200a2f00b', 'owner', NULL, NULL, '2026-01-01T00:00:00Z');

-- Approval e1 is already resolved: status 'approved' with a recorded decision.
INSERT INTO pending_approvals (
  approval_id, session_id, request_id, action, payload, created_at,
  agent_group_id, channel_type, platform_id, platform_message_id,
  expires_at, status, title, options_json
) VALUES
  ('00000000-0000-0000-0000-0000000000e1', NULL, 'req-e1', 'sender', '{}', '2026-01-01T00:00:00Z',
   '00000000-0000-0000-0000-000000000001', 'slack', 'U901', 'slack-ts-e1',
   NULL, 'approved', 'Approve sender slack:U901?', '[]');

INSERT INTO approval_decisions (approval_id, action, outcome, decided_by, reason, decided_at) VALUES
  ('00000000-0000-0000-0000-0000000000e1', 'sender', 'approve', 'host', NULL, '2026-01-01T00:00:00Z');
