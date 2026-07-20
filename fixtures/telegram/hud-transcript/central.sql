-- Replay fixture seed: telegram/hud-transcript (M22 Wave 0).
--
-- Single agent group "Replay" wired to a telegram group chat (chat_id
-- "100"). Engage mode `pattern` with `.*` so the plain-text inbound
-- routes without a mention; session mode `shared`.
--
-- Telegram is in `EDIT_CAPABLE_CHANNELS`, so the runner's Task HUD
-- resolves to the live self-editing behaviour: one breadcrumb chip
-- posted at the first tool batch, then an in-place edit frame at every
-- batch boundary. The five scripted shell turns produce the multi-frame
-- edit stream this fixture pins byte-for-byte.
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
  'pattern', '.*', 'all',
  'drop', 'shared', 0,
  '2026-01-01T00:00:00Z'
);
