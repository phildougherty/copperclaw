-- One row per LLM call the runner makes, with token usage as
-- reported by the provider's `usage` block.
--
-- Indexed by (agent_group_id, ended_at) for the common
-- per-group-over-window query. The table grows fast on a busy
-- install (every turn, including tool-use sub-turns), so consumers
-- should always filter by time window when listing.
CREATE TABLE agent_turns (
  id               INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id       TEXT NOT NULL,
  agent_group_id   TEXT NOT NULL,
  seq              INTEGER NOT NULL,        -- runner-local turn counter
  model            TEXT NOT NULL,
  provider         TEXT NOT NULL,
  input_tokens     INTEGER NOT NULL DEFAULT 0,
  output_tokens    INTEGER NOT NULL DEFAULT 0,
  started_at       TEXT NOT NULL,           -- RFC3339
  ended_at         TEXT NOT NULL,           -- RFC3339
  status           TEXT NOT NULL,           -- 'ok' | 'error'
  error            TEXT
);

CREATE INDEX idx_agent_turns_group_ended ON agent_turns(agent_group_id, ended_at);
CREATE INDEX idx_agent_turns_session     ON agent_turns(session_id);
