-- Audit log of mutating iclaw socket calls.
--
-- Every command in `HOST_ONLY_COMMANDS` lands one row here. Read paths
-- (groups.list, sessions.list, …) are intentionally excluded — they're
-- noisy and have no security relevance.
--
-- Schema is deliberately tall-and-narrow: a single table the host
-- can append to in the dispatch path without joins, indexes lazy.
-- Operators query it via `iclaw audit list`.
CREATE TABLE audit_log (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  ts            TEXT NOT NULL,        -- RFC3339, set on the dispatcher
  caller_kind   TEXT NOT NULL,        -- 'host' or 'agent'
  caller_session TEXT,                -- only set for agent callers
  caller_agent_group TEXT,            -- only set for agent callers
  command       TEXT NOT NULL,        -- e.g. 'groups.create'
  args          TEXT NOT NULL,        -- JSON, may be truncated
  result        TEXT NOT NULL,        -- 'ok' or 'error'
  error_code    TEXT,                 -- present on error
  error_message TEXT,                 -- present on error
  latency_ms    INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_audit_log_ts      ON audit_log(ts);
CREATE INDEX idx_audit_log_command ON audit_log(command);
