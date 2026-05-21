---
name: install-packages
description: Request the host to install apt and/or npm packages into the agent container via install_packages, including the approval flow.
---

# install-packages

`install_packages` requests that the host install one or more apt
and/or npm packages into the agent group's container image. The tool
**never** mutates the container itself — it writes an approval
request that an admin must accept (or the policy auto-approves)
before the next container rebuild picks the change up.

## Schema

```json
{
  "apt": ["ripgrep", "jq"],
  "npm": ["typescript"],
  "reason": "string, non-empty"
}
```

- `reason` (required, non-blank). Audited; admins read this when
  deciding whether to approve.
- `apt` (optional). Debian package names installed via
  `apt-get install`.
- `npm` (optional). Global npm packages installed with
  `npm install -g`.
- At least one of `apt` or `npm` must be non-empty.

## What gets approved by the host

The tool emits an `OutboundToolEffect::InstallPackages` which the
runner turns into a row in `pending_approvals` with
`kind = "install_packages"`. From there:

1. The host renders a notification to whoever holds the `admin`
   role for the agent group.
2. The admin runs `iclaw approvals get <id>` to inspect, then
   either approves or denies (current `iclaw` exposes
   `approvals list/get`; admin actions go through the central DB
   directly until the CLI gains the verbs).
3. On approval, the host updates `container_configs.packages_apt`
   and/or `packages_npm` and queues a rebuild. The next time the
   container starts, the new image includes the packages.

The container image is fingerprinted by `sha256(sorted(apt + npm))`,
so identical install requests reuse the same image tag.

## Constraints

- Package names must be non-blank strings; whitespace-only entries
  are rejected with `ToolError::Validation`.
- Names are passed straight to the underlying installer; valid apt
  and npm naming rules apply.
- This tool requests; it does **not** wait. Your call returns as
  soon as the approval row is written. Re-run the operation that
  needed the package after the next container boot.

## Example

```json
{
  "apt": ["ripgrep"],
  "reason": "Need fast in-repo search for source-code summarisation."
}
```

```json
{
  "npm": ["@anthropic-ai/sdk"],
  "reason": "Sub-agents will call Claude directly via the SDK."
}
```

## When to use this vs add_mcp_server

- `install_packages` adds binaries / libraries to the container.
- `add_mcp_server` adds an MCP server you intend to call as a tool.
  Many MCP servers are themselves npm or pipx packages; installing
  them via `install_packages` is the first step, and
  `add_mcp_server` is the second.

## Failure modes

- **Already in config.** Idempotent; the second request still
  creates an approval row but the admin sees that the package is
  already installed and can dismiss.
- **Bad name (rejected by apt/npm).** Surfaces in the rebuild log.
  The container reverts to the prior image; the approval is marked
  failed.
- **No admin available.** Approval pends indefinitely. Surface to
  the user via `send_message` if you can describe a manual fallback.
