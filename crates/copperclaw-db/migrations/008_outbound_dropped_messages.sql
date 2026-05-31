-- Outbound dead-letter queue: rows written here when the delivery loop exhausts
-- all retries for an outbound message. The operator can inspect and replay.
CREATE TABLE outbound_dropped_messages (
  id              TEXT PRIMARY KEY,
  session_id      TEXT NOT NULL,
  agent_group_id  TEXT NOT NULL,
  message_out_id  TEXT NOT NULL,
  channel_type    TEXT,
  platform_id     TEXT,
  thread_id       TEXT,
  kind            TEXT NOT NULL,
  content         TEXT NOT NULL,
  last_error      TEXT NOT NULL,
  dropped_at      TEXT NOT NULL
);

CREATE INDEX idx_outbound_dropped_dropped_at
    ON outbound_dropped_messages(dropped_at);
