---
name: install-packages
description: Request the host to install apt and/or npm packages into the agent container via install_packages — the change is applied directly to container_configs and the next spawn rebuilds the image automatically.
---

# install-packages

`install_packages` appends apt and/or npm packages to your agent
group's container configuration. The host's delivery loop applies the
change directly to `container_configs.packages_apt` /
`packages_npm`. The container manager fingerprints those fields, so
the **next** spawn of the session container will rebuild the image
with your packages baked in.

There is no approval gate today. Operators can audit every call via
`iclaw audit list`, and `container_configs` history is reconstructable
from the audit log. If your install policy needs preflight approval,
file feedback — the gate is intentionally trivial to re-add.

## Schema

```json
{
  "apt": ["ripgrep", "jq"],
  "npm": ["typescript"],
  "reason": "string, non-empty"
}
```

- `reason` (required, non-blank). Persisted to the audit log so an
  operator can read the motivation back later.
- `apt` (optional). Debian package names installed via
  `apt-get install` during the next image build.
- `npm` (optional). Global npm packages installed with
  `npm install -g` during the next image build.
- At least one of `apt` or `npm` must be non-empty.

## How the change takes effect

1. The tool emits a `MessageKind::System` row keyed `install_packages`
   into the session's `outbound.db`.
2. The host's delivery loop intercepts the row, validates the
   payload, and appends each new package to `container_configs`
   (already-present packages are skipped — idempotent).
3. The container manager's fingerprint check (see
   [docs/container-config.md](../../docs/container-config.md))
   detects that `packages_apt` / `packages_npm` changed and rebuilds
   the image at the start of the **next** session spawn.
4. The new image tag + fingerprint are persisted back to
   `container_configs`, so subsequent spawns reuse the cached image.

The change is **not** retroactive — you keep running on the current
image for the rest of this turn. If you immediately need the package,
you have two choices:

- Call `shell` with `apt-get install -y <pkg>` inside the running
  container. This is ephemeral (lost on idle-stop) but immediate.
- Wait for the next spawn after an idle period or operator restart.

For tools you reach for in every conversation, the rebuild path is
correct. For "I need it once right now", `shell` is better.

## Constraints

- Package names must be non-blank. Whitespace-only entries are
  silently dropped by the apply step.
- Name validation matches apt / npm naming rules. Bad names surface
  at image-build time as a rebuild failure — the manager falls back
  to the last-known-good image and emits
  `ironclaw_image_rebuild_failed_total`. The agent keeps running on
  the stale image until the bad name is removed.
- The reason field is for the audit log, not the model. Other agents
  reading the conversation history will not see it; do not encode
  load-bearing information there.

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

- `install_packages` adds binaries / libraries the agent will call
  through `shell`.
- `add_mcp_server` wires up an MCP server you'll call as a first-
  class tool. Many MCP servers are themselves npm or pipx packages —
  install the underlying package with `install_packages`, then
  configure the server with `add_mcp_server`. (Or use the curated
  preset library: `iclaw mcp list-presets` lists the ones the host
  already knows about.)

## Failure modes

- **Bad name (rejected by apt/npm).** Logged + counted as
  `ironclaw_image_rebuild_failed_total`. The fingerprint is NOT
  updated, so the rebuild retries on each spawn until the operator
  fixes the config. The agent keeps running on the previous image.
- **Disk-full at build time.** Same as above — fingerprint not
  persisted, retry on next spawn.
- **No `container_configs` row yet.** The apply step creates a default
  row first, so there's nothing to handle in the agent.
