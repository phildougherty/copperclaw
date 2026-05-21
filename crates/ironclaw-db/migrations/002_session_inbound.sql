-- Per-session inbound database schema.
--
-- IMPORTANT: this file is applied to a session-scoped DB, NOT the central
-- DB. The host writes; the container reads. `PRAGMA journal_mode=DELETE`
-- is set programmatically before this is applied — WAL's mmapped -shm
-- file does NOT propagate across a Docker bind-mount and would silently
-- drop writes.

CREATE TABLE messages_in (
  id                TEXT PRIMARY KEY,
  seq               INTEGER NOT NULL UNIQUE,
  kind              TEXT NOT NULL,
  timestamp         TEXT NOT NULL,
  status            TEXT NOT NULL DEFAULT 'pending',
  process_after     TEXT,
  recurrence        TEXT,
  series_id         TEXT,
  tries             INTEGER NOT NULL DEFAULT 0,
  trigger           INTEGER NOT NULL DEFAULT 1,
  platform_id       TEXT,
  channel_type      TEXT,
  thread_id         TEXT,
  content           TEXT NOT NULL,
  source_session_id TEXT,
  on_wake           INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_messages_in_series ON messages_in(series_id);
CREATE INDEX idx_messages_in_status ON messages_in(status, process_after);

CREATE TABLE delivered (
  message_out_id      TEXT PRIMARY KEY,
  platform_message_id TEXT,
  status              TEXT NOT NULL,
  delivered_at        TEXT NOT NULL
);

CREATE TABLE destinations (
  name           TEXT PRIMARY KEY,
  display_name   TEXT NOT NULL,
  type           TEXT NOT NULL,
  channel_type   TEXT,
  platform_id    TEXT,
  agent_group_id TEXT
);

CREATE TABLE session_routing (
  id           INTEGER PRIMARY KEY CHECK (id = 1),
  channel_type TEXT,
  platform_id  TEXT,
  thread_id    TEXT
);
