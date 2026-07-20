-- Seed ONLY session_routing for the goal's session (M22 A3 goal-progress).
-- Unlike the grant-wake fixtures there is no pre-seeded `messages_in` row here:
-- the sweep's goal check-in fan-out (`checks::goals`) synthesises the `kind:task`
-- wake inbound itself on each pass. Routing is seeded so each woken turn's
-- progress-report chat reply has a delivery destination.
INSERT INTO session_routing (id, channel_type, platform_id, thread_id) VALUES
  (1, 'cli', 'stdin', NULL);
