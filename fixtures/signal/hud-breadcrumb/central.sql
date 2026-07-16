-- Replay fixture seed: signal/hud-breadcrumb (M19 U1, X-rider W2).
--
-- Single agent group "Replay" wired to a single signal messaging group
-- keyed on the recipient number "+15550001234". Engage mode `pattern`
-- with `.*` so every inbound matches; session mode `shared`.
--
-- signal is in `EDIT_CAPABLE_CHANNELS` (it overrides trait
-- `edit_message`), and M19 U1 gave it a native `deliver_breadcrumb`, so
-- the runner's Task HUD resolves to the live self-editing behaviour on
-- this channel. The fixture drives a one-tool turn so the HUD posts a
-- breadcrumb chip and then edits it in place instead of stacking prose.
INSERT INTO agent_groups (id, name, folder, agent_provider, created_at) VALUES
  ('00000000-0000-0000-0000-000000000001', 'Replay', 'replay', 'anthropic', '2026-01-01T00:00:00Z');

INSERT INTO messaging_groups (id, channel_type, platform_id, name, is_group, unknown_sender_policy, created_at) VALUES
  ('00000000-0000-0000-0000-000000000002', 'signal', '+15550001234', 'signal/+15550001234', 0, 'lenient', '2026-01-01T00:00:00Z');

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
