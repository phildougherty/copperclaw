-- Replay fixture seed: teams/hud-live-edit (M19 U2, X-rider W2).
--
-- Single agent group "Replay" wired to a single teams messaging group
-- keyed on a conversation id "19:teamschat@thread.v2". Engage mode
-- `pattern` with `.*` so every inbound matches; session mode `shared`.
--
-- M19 U2 gave teams a trait `edit_message` override (Teams supports
-- message updates via PATCH .../messages/{id}) and added it to
-- `EDIT_CAPABLE_CHANNELS` in the same PR, so the runner's Task HUD now
-- resolves to the live self-editing behaviour on teams. The fixture
-- drives a one-tool turn so the HUD posts ONE chip and edits it in place
-- instead of posting a fresh message on every frame (the new-message
-- spam U2 kills).
INSERT INTO agent_groups (id, name, folder, agent_provider, created_at) VALUES
  ('00000000-0000-0000-0000-000000000001', 'Replay', 'replay', 'anthropic', '2026-01-01T00:00:00Z');

INSERT INTO messaging_groups (id, channel_type, platform_id, name, is_group, unknown_sender_policy, created_at) VALUES
  ('00000000-0000-0000-0000-000000000002', 'teams', '19:teamschat@thread.v2', 'teams/19:teamschat@thread.v2', 1, 'lenient', '2026-01-01T00:00:00Z');

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
