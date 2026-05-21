-- Per-session outbound database schema.
--
-- Applied to outbound.db. Container writes; host reads. Uses WAL mode
-- safely because the host only reads after the container has fsync'd
-- (delivery loop polls).

CREATE TABLE messages_out (
  id            TEXT PRIMARY KEY,
  seq           INTEGER NOT NULL UNIQUE,
  in_reply_to   TEXT,
  timestamp     TEXT NOT NULL,
  deliver_after TEXT,
  recurrence    TEXT,
  kind          TEXT NOT NULL,
  platform_id   TEXT,
  channel_type  TEXT,
  thread_id     TEXT,
  content       TEXT NOT NULL
);
CREATE INDEX idx_messages_out_due ON messages_out(deliver_after);

CREATE TABLE processing_ack (
  message_id     TEXT PRIMARY KEY,
  status         TEXT NOT NULL,
  status_changed TEXT NOT NULL
);

CREATE TABLE session_state (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE container_state (
  id                       INTEGER PRIMARY KEY CHECK (id = 1),
  current_tool             TEXT,
  tool_declared_timeout_ms INTEGER,
  tool_started_at          TEXT,
  updated_at               TEXT
);
