---
name: add-mcp-server
description: Register an MCP server with the host via add_mcp_server — the entry is merged into container_configs.mcp_servers and the next spawn rebuilds the image to pick it up.
---

# add-mcp-server

`add_mcp_server` wires a new MCP server into your agent group's
container configuration. The host's delivery loop merges the entry
into `container_configs.mcp_servers` (a JSON object keyed by server
name) directly — there is no separate approval step. The container
manager's fingerprint check detects the change and rebuilds the
image at the next spawn.

If you only need a server from the curated preset library (Postgres,
GitHub, Linear, Notion, Filesystem, Browserbase, …), the operator
can run `iclaw mcp add <preset> --agent-group-id <id> --env K=V`
and the result is identical to calling this tool. Use this tool
when you need a server outside the preset library or with custom
transport.

## Schema

```json
{
  "name": "git",
  "transport": { "kind": "stdio", "cmd": "uvx", "args": ["mcp-server-git"] },
  "reason": "inspect repo history for commit summaries"
}
```

- `name` (required, non-blank). The key under which the server
  registers. Re-using a name **replaces** the existing entry —
  this lets you refresh transport details (e.g. rotate an API key)
  without operator help.
- `transport` (required, object). Shape depends on `kind` (see
  below). The tool only checks that it is a JSON object; the
  runner validates the rest when it loads the config.
- `reason` (required, non-blank). Audit string. Persisted to the
  audit log; not shown to other agents.

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

- `cmd` (required). Absolute path or a binary on `$PATH` inside the
  container. If the binary isn't yet baked into the image, call
  `install_packages` first (e.g. `apt: ["uv"]` or `npm: ["..."]`).
- `args` (optional). Argv array, no shell expansion.
- `env` (optional). String-to-string map. **Values land in
  `container_configs.mcp_servers` and propagate to the container
  via runner config.** They are also redacted in the audit log so
  operators see "which env vars were set" but not the values.

### HTTP-SSE (remote servers)

```json
{
  "kind": "http-sse",
  "url": "https://mcp.example.com/v1/sse",
  "headers": { "Authorization": "Bearer ..." }
}
```

The host opens an EventSource and speaks JSON-RPC over it. As of
this writing the rmcp `transport-sse-client` feature is enabled, so
this path is fully wired.

## How the change takes effect

1. The tool emits a `MessageKind::System` row keyed `add_mcp_server`
   into the session's `outbound.db`.
2. The host's delivery loop merges the entry into
   `container_configs.mcp_servers` (object keyed by `name`).
3. The container manager's fingerprint check detects the change and
   rebuilds the image at the **next** session spawn. The new image
   includes the registered server in the runner's `mcp_servers`
   manifest.
4. The runner connects to the server during boot, and the tool list
   the model sees expands to include the new server's tools.

The change is **not** retroactive — the current container does not
gain the new server mid-conversation. After the next idle-stop /
restart, the model can call `mcp__<name>__<tool>` directly.

## Common patterns

- **Refreshing credentials.** Call again with the same `name` and
  the new env. The merge replaces the old entry.
- **Removing a server.** Not exposed as a tool. Ask the operator to
  run `iclaw groups config remove-mcp-server --agent-group-id <id>
  --name <name>`, then a fingerprint change forces a rebuild that
  drops the server.
- **Preset shortcut.** Operator-side curated wiring via
  `iclaw mcp add <preset>` writes the same `container_configs`
  shape. Check `iclaw mcp list-presets` for the catalog before
  hand-rolling transport JSON.

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

The tool returns an `Accepted` ack. Re-attempt the operation that
needed the new server after the next container boot. Watching
`iclaw groups config get-mcp-servers <ag>` is the fastest way to
confirm the merge landed.

## Failure modes

- **Blank name.** The apply step drops the call as a no-op (no
  rebuild triggered).
- **Image rebuild fails.** Same as `install_packages`: the manager
  falls back to the last-known-good image, increments
  `ironclaw_image_rebuild_failed_total`, and retries the rebuild on
  the next spawn until the operator fixes the config.
