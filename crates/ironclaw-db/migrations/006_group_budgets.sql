-- Per-agent-group daily budget caps.
--
-- One row per group. The container manager checks
--   SUM(input_tokens + output_tokens) on agent_turns since 00:00 UTC
-- against `daily_token_cap` before spawning. Missing row = no cap.
--
-- Cost caps are a follow-up; the schema slot is reserved for when we
-- start tracking dollars (token prices vary per model so this can't
-- just be derived from token counts).
CREATE TABLE group_budgets (
  agent_group_id   TEXT PRIMARY KEY,
  daily_token_cap  INTEGER,             -- NULL = no cap
  daily_cost_cap   REAL,                -- NULL = no cap; reserved
  updated_at       TEXT NOT NULL
);
