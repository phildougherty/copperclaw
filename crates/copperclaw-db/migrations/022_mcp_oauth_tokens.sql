-- Host-side OAuth token store for external MCP servers (Phase 6 supply-chain).
--
-- Tokens for OAuth-authenticated MCP servers live HERE, on the host, in the
-- central DB — NEVER in the per-session container env. The container reaches
-- an OAuth MCP server through the host-side broker path (the same loopback
-- pattern the credential broker uses for the model key): the container holds
-- only a reference to the configured server name; the host injects the real
-- bearer/access token when it dials the upstream. A shell inside the container
-- can `printenv` all it likes and never see the token.
--
-- One row per (agent_group_id, server_name). Refresh tokens + access tokens +
-- expiry are stored so the host can transparently refresh without operator
-- intervention. The table is in the CENTRAL db (not a per-session db) because
-- the token is a per-group durable credential, not session-scoped state.
--
-- SECURITY: this table holds plaintext secrets. The central DB file is already
-- chmod 0600 by the host (it holds the provider API key forwards too), so the
-- token is no more exposed than the existing secrets there. We deliberately do
-- NOT mirror these into any session db or runner.json.

CREATE TABLE mcp_oauth_tokens (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  -- Owning agent group (UUID text). A token is scoped to the group whose
  -- container is allowed to use the named server.
  agent_group_id TEXT NOT NULL,
  -- The MCP server entry name this token authenticates (matches the key in
  -- `container_configs.mcp_servers`).
  server_name    TEXT NOT NULL,
  -- OAuth access token (bearer). Injected host-side at dial time.
  access_token   TEXT NOT NULL,
  -- OAuth refresh token, when the grant is refreshable. NULL otherwise.
  refresh_token  TEXT,
  -- Token type, e.g. 'Bearer'. Free text; defaults to Bearer.
  token_type     TEXT NOT NULL DEFAULT 'Bearer',
  -- Space-separated granted scopes, for operator visibility. May be NULL.
  scope          TEXT,
  -- Absolute expiry instant (RFC3339), or NULL when the token does not expire
  -- / the provider did not return an expiry. The host refreshes when the
  -- access token is within a skew window of this instant.
  expires_at     TEXT,
  created_at     TEXT NOT NULL,   -- RFC3339
  updated_at     TEXT NOT NULL,   -- RFC3339
  -- Exactly one token per (group, server). Re-storing overwrites in the
  -- application layer (an UPSERT keyed on this constraint).
  UNIQUE(agent_group_id, server_name)
);

CREATE INDEX idx_mcp_oauth_tokens_group ON mcp_oauth_tokens(agent_group_id);
