-- Replay fixture seed: telegram/approval-resolution (M19 F3 a + c).
--
-- One agent group wired to a telegram group chat (chat_id "100"). Owner Olivia
-- (telegram:200, global Owner) may resolve approvals.
--
-- Two pending approvals model the two F3 edge cases:
--   * d1 — a card whose delivering adapter recorded NO platform_message_id
--     (the fallback-id path, F3a). Owner taps Approve on it: the interceptor
--     resolves it but, with no editable anchor, posts the resolution as a
--     follow-up REPLY instead of leaving live buttons.
--   * d3 — a card whose TTL already lapsed (F3c). The tap's opportunistic
--     expiry sweep stamps its card terminal ("expired ...") rather than leaving
--     it silently live.
--
-- (User ids are UUIDv5(nil, "<kind>:<identity>") — see `users::derive_user_id`.)

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

-- Owner Olivia (id = UUIDv5(nil, "telegram:200")) holds a global Owner role.
INSERT INTO users (id, kind, display_name, created_at) VALUES
  ('ea97856b-9509-5a56-b98b-8fa28ae9d684', 'telegram', 'Owner Olivia', '2026-01-01T00:00:00Z');

INSERT INTO user_roles (user_id, role, agent_group_id, granted_by, granted_at) VALUES
  ('ea97856b-9509-5a56-b98b-8fa28ae9d684', 'owner', NULL, NULL, '2026-01-01T00:00:00Z');

-- d1: fallback-id card (platform_message_id IS NULL) — no editable anchor.
-- d3: already-lapsed card (expires_at in the past) with a delivered card.
INSERT INTO pending_approvals (
  approval_id, session_id, request_id, action, payload, created_at,
  agent_group_id, channel_type, platform_id, platform_message_id,
  expires_at, status, title, options_json
) VALUES
  ('00000000-0000-0000-0000-0000000000d1', NULL, 'req-d1', 'sender', '{}', '2026-01-01T00:00:00Z',
   '00000000-0000-0000-0000-000000000001', 'telegram', '901', NULL,
   NULL, 'pending', 'Approve sender telegram:901?', '[]'),
  ('00000000-0000-0000-0000-0000000000d3', NULL, 'req-d3', 'sender', '{}', '2026-01-01T00:00:00Z',
   '00000000-0000-0000-0000-000000000001', 'telegram', '903', 'tg-exp-card',
   '2020-01-01T00:00:00Z', 'pending', 'Approve sender telegram:903?', '[]');
