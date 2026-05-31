# Per-group container configuration

Each agent group has an optional row in `container_configs` controlling
the image and runtime knobs for that group's session containers. The
manager reads it on every spawn; absent rows fall back to host
defaults. This doc covers the three M13-hardening additions:

- **Image rebuild on diff** — change a package list and the next
  spawn rebuilds automatically.
- **Egress allow-list** — restrict the container's outbound network
  to a curated host:port set.
- **Resource caps** — `--cpus` / `--memory` / `--pids-limit` per
  group.

Each is opt-in. The host invariant is: an absent or empty field
means "no policy" — never silent deny.

## Image rebuild on `container_config` change

The manager computes a sha256 fingerprint over the rebuild-relevant
fields (`packages_apt`, `packages_npm`, `skills`, `mcp_servers`).
Before every spawn it compares the live fingerprint to the stored
`config_fingerprint` column. If they differ:

1. Call `runtime.build_image(...)` with the current `packages_apt` +
   `packages_npm`.
2. Persist the new sha-tagged `image_tag` and the new
   `config_fingerprint` back to `container_configs`.
3. Spawn the new container.

When does the fingerprint change?

- **Operator-driven**: `cclaw groups config add-package --apt jq`,
  `... remove-package --npm typescript`, `... set-skills ...`,
  `... set-mcp-servers ...`.
- **Agent-driven**: the agent calls the `install_packages` /
  `add_mcp_server` tools, which write `container_configs` directly.
  The next spawn rebuilds — the agent does NOT need to wait for an
  operator.

### Failure handling

If `runtime.build_image` fails (a bad apt name, a transient network
blip during `apt-get update`, etc.):

- The manager **does not** update `config_fingerprint`. The next
  spawn retries the rebuild.
- If the group has a `image_tag` from a previous successful build,
  the spawn falls back to that tag so the agent group is **not**
  blocked. The agent runs on the stale image; future tool changes
  will not take effect until the operator fixes the broken config.
- The `copperclaw_image_rebuild_failed_total` counter increments.
  Watch this metric per the recommended alert in
  [docs/observability.md](observability.md).
- If the group has **no** prior `image_tag` (first-ever build for a
  newly-configured group), the spawn errors and the session stays
  Stopped. The manager retries on the next tick.

## Egress allow-list

Stored as `container_configs.egress_allow` — a JSON array of
`host:port` strings. Default is the empty list, which means
**allow-all** (the OpenBSD-of-claw-agents posture chose default-allow
+ opt-in lockdown over default-deny here, because too many channels
need varied outbound access and a too-strict default would silently
break installs).

When the field is non-empty, the Docker runtime translates it to a
user-defined network and `--add-host` entries restricted to the
listed targets. The Apple Container runtime returns
`RtError::Unsupported` — Apple's container CLI does not expose a
network-policy surface the manager can use, so the operator must
either:

- clear the allow-list (`cclaw groups config set-egress-allow <id>`
  with no `--allow` flags), or
- switch to the Docker runtime, or
- accept that the unsupported case is the error: secure-by-default
  over silent fallback.

### Setting it

```
cclaw groups config set-egress-allow <group-id> \
    --allow api.anthropic.com:443 \
    --allow openrouter.ai:443

# Clear:
cclaw groups config set-egress-allow <group-id>
```

A `set-egress-allow` mutation also lands in `audit_log` so the
allow-list history is reconstructable.

## Resource caps

Stored as `container_configs.resource_limits` JSON:

```json
{
  "cpus": "1.5",
  "memory_mb": 512,
  "pids_limit": 256
}
```

All fields optional; omit to leave that dimension uncapped. Default
is an empty object (no caps).

| Field | Docker mapping | Apple runtime |
|---|---|---|
| `cpus` | `--cpus=<value>` | `RtError::Unsupported` |
| `memory_mb` | `--memory=<value>m` | `RtError::Unsupported` |
| `pids_limit` | `--pids-limit=<value>` | `RtError::Unsupported` |

### Setting it

```
cclaw groups config set-resource-limits <group-id> \
    --cpus 1.5 --memory-mb 512 --pids-limit 256

# Clear one dimension by omitting its flag and re-setting the others.
# To clear everything, omit all three flags:
cclaw groups config set-resource-limits <group-id>
```

### Malformed JSON tolerance

If the `resource_limits` column ever contains invalid JSON (e.g. a
hand-edited `copperclaw.db` row), the manager logs a warning at spawn
time and **spawns without caps** rather than refusing. This is a
deliberate weakening of secure-by-default for this specific field:
the alternative (refuse to spawn) blocks the whole group on a
schema-level typo, which would prevent the operator from logging in
to fix it.

## Inspecting a group

Read the current container config row as JSON:

```
cclaw groups config get <agent-group-id>
```

(Raw SQL fallback for forensic / read-only-disk situations:
`sqlite3 /srv/copperclaw/data/copperclaw.db "SELECT * FROM container_configs WHERE agent_group_id = '<id>'"`.)

The audit log captures every mutation:

```
cclaw audit list --since 7d --limit 20
```
