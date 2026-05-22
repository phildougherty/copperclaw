-- Scheduled tasks created via the agent's `schedule_task` MCP tool.
--
-- Rows live in the central DB so the host's sweep loop can scan them
-- without having to walk every session's outbound.db. A single agent
-- session may produce many tasks; tasks fire by synthesising an
-- inbound `kind: task` message into the originating session's
-- inbound.db.
--
-- `status` lifecycle:
--   active     -> due tasks fire and re-arm (recurring) or transition
--                 to `completed` (one-shot).
--   paused     -> ignored by the sweep; can be returned to active.
--   cancelled  -> ignored permanently.
--   completed  -> one-shot task fired and finished.
--
-- `when` carries the originating `When` string ("2026-05-21T15:00Z",
-- "daily at 09:00", cron, etc.) for diagnostics; `next_fire` is the
-- materialised RFC 3339 instant the sweep compares against `now`.
-- `recurrence` mirrors the original recurrence override, when set.

CREATE TABLE tasks (
  id              TEXT PRIMARY KEY,
  agent_group_id  TEXT NOT NULL REFERENCES agent_groups(id),
  session_id      TEXT NOT NULL,
  name            TEXT,
  prompt          TEXT NOT NULL,
  when_spec       TEXT NOT NULL,
  recurrence      TEXT,
  next_fire       TEXT,
  status          TEXT NOT NULL DEFAULT 'active',
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL
);

CREATE INDEX idx_tasks_session ON tasks(session_id);
CREATE INDEX idx_tasks_due ON tasks(status, next_fire);
