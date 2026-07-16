-- Replay fixture seed: telegram/blocked-todo (M19 F4).
--
-- Single agent group "Replay" wired to a single telegram messaging
-- group keyed on chat_id "100". Engage mode `pattern` with `.*`; session
-- mode `shared`. Telegram is a rich, edit-capable channel — but the
-- harness's MockAdapter degrades `deliver_todo_list` to its text
-- fallback, which is exactly the surface F4 taught to render the `[!]`
-- blocked glyph + reason. So the delivered todo list on this fixture is
-- the text fallback, and it must show the blocked step as blocked.
INSERT INTO agent_groups (id, name, folder, agent_provider, created_at) VALUES
  ('00000000-0000-0000-0000-000000000001', 'Replay', 'replay', 'anthropic', '2026-01-01T00:00:00Z');

INSERT INTO messaging_groups (id, channel_type, platform_id, name, is_group, unknown_sender_policy, created_at) VALUES
  ('00000000-0000-0000-0000-000000000002', 'telegram', '100', 'telegram/100', 0, 'lenient', '2026-01-01T00:00:00Z');

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
