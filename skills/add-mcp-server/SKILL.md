---
name: add-mcp-server
description: Register an MCP server with the host via add_mcp_server — the entry is merged into container_configs.mcp_servers and the next spawn rebuilds the image to pick it up.
---

# add-mcp-server

`add_mcp_server` wires a new MCP server into your agent group's
container config. The host's delivery loop merges the entry into
`container_configs.mcp_servers` (a JSON object keyed by name); the
container manager's fingerprint check detects the change and rebuilds
the image at the next spawn.

For curated presets (Postgres, GitHub, Linear, Notion, Filesystem,
Browserbase, …), the operator runs `iclaw mcp add <preset>`; the result
is identical. Use this tool when you need a server outside the preset
library or with custom transport.

## Schema

```json
{
  "name": "git",
  "transport": { "kind": "stdio", "cmd": "uvx", "args": ["mcp-server-git"] },
  "reason": "inspect repo history for commit summaries"
}
```

- `name` (required, non-blank). Server key. Re-using a name
  **replaces** the entry — refresh transport / rotate keys without
  operator help.
- `transport` (required, object). Shape depends on `kind` (below). The
  tool only checks JSON-object; the runner validates the rest on load.
- `reason` (required, non-blank). Audit string. Persisted; not shown
  to other agents.

## Transport shapes

### stdio (preferred for local subprocesses)

```json
{
  "kind": "stdio",
  "cmd": "uvx",
  "args": ["mcp-server-fetch", "--allow", "https://api.example.com"],
  "env": { "API_KEY": "..." }
}
```

- `cmd` (required). Absolute path or a `$PATH` binary in the
  container. Not baked in? Call `install_packages` first
  (e.g. `apt: ["uv"]`).
- `args` (optional). Argv array; no shell expansion.
- `env` (optional). String map. Values land in `mcp_servers` and
  propagate via runner config. Redacted in the audit log (operators
  see "which env vars were set" but not the values).

### HTTP-SSE (remote servers)

```json
{
  "kind": "http-sse",
  "url": "https://mcp.example.com/v1/sse",
  "headers": { "Authorization": "Bearer ..." }
}
```

Opens an EventSource and speaks JSON-RPC over it. The
`transport-sse-client` rmcp feature is enabled; path is fully wired.

## How the change takes effect

1. Tool emits a `MessageKind::System` row keyed `add_mcp_server` into
   the session's `outbound.db`.
2. Host delivery loop merges into `container_configs.mcp_servers`.
3. Container manager's fingerprint check detects the change and
   rebuilds the image at the **next** spawn.
4. Runner connects on boot; the tool list expands to include
   `mcp__<name>__<tool>` entries.

The change is **not** retroactive — current container does not gain
the new server mid-conversation. After idle-stop / restart, the model
can call the new tools directly.

## Common patterns

- **Refresh credentials.** Same `name` + new env; merge replaces.
- **Remove a server.** Not exposed as a tool. Operator runs
  `iclaw groups config remove-mcp-server --agent-group-id <id>
  --name <name>`; fingerprint change forces a rebuild that drops it.
- **Preset shortcut.** `iclaw mcp add <preset>` writes the same
  shape. `iclaw mcp list-presets` for the catalog.

## Example

```json
{
  "name": "github",
  "transport": {
    "kind": "stdio",
    "cmd": "npx",
    "args": ["-y", "@modelcontextprotocol/server-github"],
    "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_..." }
  },
  "reason": "read repo metadata for the briefing prompt"
}
```

## Result

Returns an `Accepted` ack. Re-attempt the operation that needed it
after the next container boot. `iclaw groups config get-mcp-servers
<ag>` confirms the merge.

## Failure modes

- **Blank name.** Apply step drops the call as a no-op (no rebuild).
- **Image rebuild fails.** Same as `install_packages`: manager falls
  back to last-known-good, increments
  `ironclaw_image_rebuild_failed_total`, retries on next spawn until
  the operator fixes the config.
