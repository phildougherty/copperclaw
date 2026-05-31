---
name: install-packages
description: Request the host to install apt and/or npm packages into the agent container via install_packages — the change is applied directly to container_configs and the next spawn rebuilds the image automatically.
---

# install-packages

`install_packages` appends apt and/or npm packages to your agent
group's container config. The host applies the change directly to
`container_configs.packages_apt` / `packages_npm`; the container
manager's fingerprint check detects the diff and rebuilds the image
at the **next** spawn with your packages baked in.

No approval gate today. Operators audit via `cclaw audit list`;
`container_configs` history is reconstructable from the audit log. If
your policy needs preflight approval, file feedback — the gate is
trivial to re-add.

## Schema

```json
{
  "apt": ["ripgrep", "jq"],
  "npm": ["typescript"],
  "reason": "string, non-empty"
}
```

- `reason` (required, non-blank). Persisted to the audit log.
- `apt` (optional). Debian packages, installed via `apt-get install`
  at image-build time.
- `npm` (optional). Global npm packages, `npm install -g`.
- At least one of `apt` / `npm` must be non-empty.

## How the change takes effect

1. Tool emits `MessageKind::System` keyed `install_packages` into
   `outbound.db`.
2. Host delivery loop validates, appends new packages (already-present
   ones skipped — idempotent).
3. Container manager's fingerprint check (see
   [docs/container-config.md](../../docs/container-config.md))
   detects the change and rebuilds the image at the **next** spawn.
4. New image tag + fingerprint persist back to `container_configs`;
   subsequent spawns reuse the cached image.

The change is **not** retroactive — you keep running on the current
image for this turn. If you need a package *now*:

- `shell apt-get install -y <pkg>` inside the running container.
  Ephemeral (lost on idle-stop) but immediate.
- Wait for the next spawn after an idle period / operator restart.

For tools you reach for every conversation, use the rebuild path. For
"I need it once right now," `shell`.

## Constraints

- Non-blank names. Whitespace-only entries are silently dropped.
- Name validation matches apt/npm rules. Bad names surface at
  build-time as a rebuild failure — manager falls back to the
  last-known-good image and emits
  `copperclaw_image_rebuild_failed_total`. The agent keeps running on
  the stale image until the bad name is removed.
- `reason` is for the audit log, not the model. Other agents reading
  history won't see it; do not encode load-bearing info there.

## Example

```json
{
  "apt": ["ripgrep"],
  "reason": "fast in-repo search for source-code summarisation"
}
```

```json
{
  "npm": ["@anthropic-ai/sdk"],
  "reason": "sub-agents call Claude directly via the SDK"
}
```

## When to use this vs `add_mcp_server`

- `install_packages` adds binaries / libraries the agent calls through
  `shell`.
- `add_mcp_server` wires an MCP server as a first-class tool. Many
  MCP servers are themselves npm/pipx packages — install the
  underlying package first, then configure the server. Or use the
  preset library: `cclaw mcp list-presets`.

## Failure modes

- **Bad name** — counted as `copperclaw_image_rebuild_failed_total`.
  Fingerprint not updated; rebuild retries on each spawn until the
  operator fixes the config.
- **Disk-full at build** — same as above.
- **No `container_configs` row** — apply step creates a default row
  first; nothing for the agent to handle.
