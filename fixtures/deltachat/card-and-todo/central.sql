-- Replay fixture seed: deltachat/card-and-todo (M19 U5, X-rider W2).
--
-- Single agent group "Replay" wired to a single deltachat chat keyed on
-- the adapter's platform-id shape "account/1/chat/10". Engage mode
-- `pattern` with `.*` so every inbound matches; session mode `shared`.
--
-- M19 U5 gave deltachat a render.rs with native deliver_card /
-- deliver_todo_list / deliver_diff, raising a genuine interactive chat
-- surface off the "everything is prose" floor. The fixture drives a
-- send_card turn (native card via model_rich_cards) and a todo list (the
-- chip degrades to the deliver_todo_list text fallback on the harness
-- mock — the surface carrying the glyphs + footer counts). The deltachat
-- native wire rendering is proven in the deltachat adapter's own tests.
INSERT INTO agent_groups (id, name, folder, agent_provider, created_at) VALUES
  ('00000000-0000-0000-0000-000000000001', 'Replay', 'replay', 'anthropic', '2026-01-01T00:00:00Z');

INSERT INTO messaging_groups (id, channel_type, platform_id, name, is_group, unknown_sender_policy, created_at) VALUES
  ('00000000-0000-0000-0000-000000000002', 'deltachat', 'account/1/chat/10', 'deltachat/chat', 0, 'lenient', '2026-01-01T00:00:00Z');

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
