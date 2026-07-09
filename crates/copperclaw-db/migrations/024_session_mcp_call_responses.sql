-- Per-session INBOUND: host-proxied external MCP tool-call RESPONSES.
--
-- IMPORTANT: applied to inbound.db (host is the SOLE writer; container/runner
-- reads). After executing a request from outbound.db::mcp_call_requests against
-- the configured external MCP server (through the per-server filter), the host
-- writes the rendered result here, correlated by request_id; the runner's
-- blocking poll reads it and renders the model-facing tool_result.
--
-- `is_error` mirrors the tool_result is_error flag: a filter-denied call, a
-- connect failure, a remote error, or a missing server all land here as an
-- error response so the runner's blocking poll is never left unanswered.
-- `result` is the already-rendered text (the host flattens the remote's
-- content blocks), so the runner never has to parse rmcp content shapes.

CREATE TABLE mcp_call_responses (
  request_id TEXT PRIMARY KEY,
  is_error   INTEGER NOT NULL DEFAULT 0,
  result     TEXT NOT NULL,
  created_at TEXT NOT NULL
);
