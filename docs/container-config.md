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
  `... remove-package --npm typescript`, `cclaw groups skills <id> ...`,
  `... add-mcp-server ...`.
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

## Skills selector

Stored as `container_configs.skills` — a JSON value in one of three
shapes:

| Stored form | Meaning |
|---|---|
| `"all"` | Every discovered skill inlines (the default). |
| `["name", ...]` | Explicit allowlist: only the named skills, in listed order. |
| `{"relevant": {"query": "...", "limit": N}}` | FTS relevance narrowing (M22 S2): only the up-to-`N` skills whose `description` scores against `query` inline. |

### Setting it

The dedicated subcommand:

```
cclaw groups skills <group-id> all
cclaw groups skills <group-id> relevant --query "code review and testing" --limit 8
cclaw groups skills <group-id> only git-commit testing
```

Or the generic field update (value JSON-encoded, note the quoting):

```
cclaw groups config update --field 'skills="all"' <group-id>
cclaw groups config update --field 'skills="relevant"' <group-id>
cclaw groups config update --field 'skills=["git-commit","testing"]' <group-id>
cclaw groups config update --field 'skills={"relevant":{"query":"code review","limit":8}}' <group-id>
cclaw groups config update --field 'skills=null' <group-id>   # reset to "all"
```

Both routes go through `groups.config.update`, which also accepts:

- the bare string `"relevant"` — shorthand for
  `{"relevant": {"query": "", "limit": 8}}`;
- a `{"relevant": {...}}` object with either key omitted (`query`
  defaults to `""`, `limit` to `8`; `limit` must be >= 1);
- `null` — reset to `"all"`.

### Validation

Explicit skill names are validated at write time against the skills
registry (the global `COPPERCLAW_SKILLS_DIR` plus this group's
`<groups_dir>/<group-id>/skills/` override — the same roots prompt
assembly scans at spawn). An unknown name is rejected with a
`bad_request` listing the offenders and every available skill. When no
skills directory is configured (or the registry scan fails on a
malformed skill), the list is accepted unvalidated — at spawn time
unknown names are warn-skipped, never fatal.

### Behaviour notes

- **`relevant` with an empty query behaves like `all`** (the scorer is
  fail-open), so the bare `"relevant"` shorthand only narrows once a
  query is set. Use the object form with a task-shaped `query` for
  actual narrowing.
- An empty explicit list (`[]`) is valid and inlines zero skills.
- The `coding_enabled` flag caps only the `"all"` selector; explicit
  lists (and relevance picks) are honoured as-is.
- `skills` is a **fingerprint field** (see "Image rebuild on diff"
  above): changing it forces an image rebuild before the next spawn.
  Like every container-config change it takes effect at the next
  container spawn — `cclaw groups restart <group-id>` to force one.

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

### Presets

`cclaw egress list-presets` shows a curated catalog of known
`host:port` sets, and `cclaw egress allow <preset>` merges one into a
group's allow-list. Unlike `set-egress-allow` (which replaces the
whole list), `allow` preserves the existing entries and appends the
preset's, deduplicated — re-running is a no-op. The write goes through
the same `set-egress-allow` mutation, so it is host-only and audited.

### Database egress

Under a deny-default posture the databases run-book's MongoDB tarball
downloads need two endpoints on the group's allow-list:

- `fastdl.mongodb.org:443` — the MongoDB server tarball
- `downloads.mongodb.com:443` — the mongosh tarball

Allow both with the `mongodb` preset:

```
cclaw egress allow mongodb --agent-group-id <group-id>
```

Takes effect at the next container spawn for the group.

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
