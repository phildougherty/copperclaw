-- Replay fixture seed: telegram/approval-callback (M18 G1 in-chat approvals).
--
-- One agent group wired to a telegram group chat (chat_id "100"). Two
-- registered senders both pass the approvals sender-scope gate (they exist in
-- `users`), but only the Owner may resolve approvals:
--
--   * telegram:200 (user ea97856b-...) — granted global Owner.
--   * telegram:300 (user 7234599a-...) — registered, NO role.
--
-- (User ids are UUIDv5(nil, "<kind>:<identity>") — see `users::derive_user_id`.)
--
-- Two pending approvals are pre-seeded with a recorded `platform_message_id`,
-- as though their cards were already delivered:
--   * approval b1 — the Owner approves it (tap 1).
--   * approval b2 — the stranger tries to approve it (tap 2, refused).

INSERT INTO agent_groups (id, name, folder, agent_provider, created_at) VALUES
  ('00000000-0000-0000-0000-000000000001', 'Replay', 'replay', 'anthropic', '2026-01-01T00:00:00Z');

INSERT INTO messaging_groups (id, channel_type, platform_id, name, is_group, unknown_sender_policy, created_at) VALUES
  ('00000000-0000-0000-0000-000000000002', 'telegram', '100', 'telegram/100', 1, 'lenient', '2026-01-01T00:00:00Z');

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

-- Registered senders (ids are UUIDv5(nil, "telegram:<identity>")).
INSERT INTO users (id, kind, display_name, created_at) VALUES
  ('ea97856b-9509-5a56-b98b-8fa28ae9d684', 'telegram', 'Owner Olivia', '2026-01-01T00:00:00Z'),
  ('7234599a-4e7f-5458-8c72-884b3886dd7f', 'telegram', 'Stranger Sam', '2026-01-01T00:00:00Z');

-- Owner Olivia holds a global Owner role; Stranger Sam holds none.
INSERT INTO user_roles (user_id, role, agent_group_id, granted_by, granted_at) VALUES
  ('ea97856b-9509-5a56-b98b-8fa28ae9d684', 'owner', NULL, NULL, '2026-01-01T00:00:00Z');

-- Two pending sender-approvals with cards already "delivered".
INSERT INTO pending_approvals (
  approval_id, session_id, request_id, action, payload, created_at,
  agent_group_id, channel_type, platform_id, platform_message_id,
  expires_at, status, title, options_json
) VALUES
  ('00000000-0000-0000-0000-0000000000b1', NULL, 'req-b1', 'sender', '{}', '2026-01-01T00:00:00Z',
   '00000000-0000-0000-0000-000000000001', 'telegram', '901', 'tg-card-b1',
   NULL, 'pending', 'Approve sender telegram:901?', '[]'),
  ('00000000-0000-0000-0000-0000000000b2', NULL, 'req-b2', 'sender', '{}', '2026-01-01T00:00:00Z',
   '00000000-0000-0000-0000-000000000001', 'telegram', '902', 'tg-card-b2',
   NULL, 'pending', 'Approve sender telegram:902?', '[]');
