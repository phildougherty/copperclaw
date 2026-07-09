-- Per-session OUTBOUND: host-proxied external MCP tool-call REQUESTS.
--
-- IMPORTANT: applied to outbound.db (container/runner is the SOLE writer;
-- host reads). When the model calls an external MCP tool, the runner writes
-- one request row here and blocks-polls inbound.db::mcp_call_responses for the
-- host's reply — the host holds the live connection, the server credentials,
-- and the per-server include/exclude filter, so the call executes host-side
-- and the container stays sandboxed under deny-default egress.
--
-- Keeping the request here (runner-written, outbound.db) and the response in
-- inbound.db (host-written) preserves the single-writer-per-bind-mounted-DB
-- invariant — neither process ever writes the other's database.

CREATE TABLE mcp_call_requests (
  request_id TEXT PRIMARY KEY,
  server     TEXT NOT NULL,
  tool       TEXT NOT NULL,
  input      TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX idx_mcp_call_requests_created ON mcp_call_requests(created_at);
