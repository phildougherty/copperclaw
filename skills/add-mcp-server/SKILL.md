---
name: add-mcp-server
description: Register an external MCP server with the host via add_mcp_server, including stdio and HTTP-SSE transport shapes and the approval cycle.
---

# add-mcp-server

`add_mcp_server` requests that the host wire a new MCP server into the
container the next time it boots. Like `install_packages`, the call
writes an approval request — it does not connect to anything itself.

## Schema

```json
{
  "name": "git",
  "transport": { "kind": "stdio", "cmd": "uvx", "args": ["mcp-server-git"] },
  "reason": "Need to inspect repo history for commit summaries."
}
```

- `name` (required, non-blank). Becomes the `mcp` server name the
  agent sees in tool prefixes (e.g. `mcp__git__commit_log`).
  Unique per agent group; second registration with the same name
  conflicts at apply time.
- `transport` (required, object). Shape depends on `kind` (see
  below). The tool only checks that it is a JSON object; the host
  validates `kind` and the rest.
- `reason` (required, non-blank). Audit string.

## Transport shapes

The MCP SDK supports two transports today; the host's stdio path is
fully wired, the HTTP-SSE path is gated behind a feature flag and
currently returns `McpError::Protocol` until enabled.

### stdio (preferred for local subprocesses)

```json
{
  "kind": "stdio",
  "cmd": "uvx",
  "args": ["mcp-server-fetch", "--allow", "https://api.example.com"],
  "env": { "API_KEY": "<from-onecli>" }
}
```

- `cmd` (required). Absolute path or a binary on `$PATH` inside the
  container. If the binary is missing, request it through
  `install_packages` first.
- `args` (optional). Argv array, no shell expansion.
- `env` (optional). String-to-string map. Use the OneCLI gateway for
  secrets — paste a literal token only if you accept the audit trail.

### HTTP-SSE (remote servers)

```json
{
  "kind": "http-sse",
  "url": "https://mcp.example.com/v1/sse",
  "headers": { "Authorization": "Bearer ..." }
}
```

The host opens an EventSource and routes JSON-RPC over it. As of
this writing the host returns a protocol stub until the rmcp
`transport-sse-client` feature is enabled — confirm in the host
release notes before counting on HTTP-SSE.

## Approval cycle

1. The tool emits `OutboundToolEffect::AddMcpServer`.
2. The runner inserts a `pending_approvals` row with
   `kind = "add_mcp_server"`.
3. An admin inspects via `iclaw approvals get <id>` and approves.
4. The host updates `container_configs.mcp_servers` (JSON list of
   server configs) and queues a container restart.
5. On the next boot, the runner spawns the new MCP client and the
   tool list includes the new server's tools.

## Common patterns

- **Need a binary that doesn't exist.** Call `install_packages` for
  the npm/apt package first, then `add_mcp_server` referencing it.
- **Secret leaks.** Prefer OneCLI: in the transport `env`, reference
  a OneCLI key (the runner injects the resolved value at spawn time)
  instead of pasting a token verbatim.
- **Removing a server.** Not exposed as a tool. The admin removes it
  through `iclaw groups config remove-mcp-server <ag> --name <name>`.

## Example

```json
{
  "name": "github",
  "transport": {
    "kind": "stdio",
    "cmd": "npx",
    "args": ["-y", "@modelcontextprotocol/server-github"],
    "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "<from-onecli>" }
  },
  "reason": "Need to read repo metadata for the briefing prompt."
}
```

## Result

The tool returns an ack carrying the approval id (visible in the
ack JSON). Re-attempt your operation after the next container boot.
