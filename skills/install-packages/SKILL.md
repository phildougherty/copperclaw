---
name: install-packages
description: Install apt / npm / pip packages via install_packages — scope "session" installs pip/npm into the live session's /data so they work this turn; scope "image" (default) bakes apt/npm into the next container image.
---

# install-packages

`install_packages` has two scopes. Pick by *when* you need the package:

- `scope: "session"` — installs pip/npm packages into the session's
  persistent `/data` **right now**; they are importable/runnable this
  very turn (after one activation line). npm packages are also
  recorded for the next image build — works now, permanent later.
  apt packages cannot be installed live (need root + build-time
  egress); under session scope they are recorded for the next image
  only.
- `scope: "image"` (the default) — records apt/npm packages into your
  group's `container_configs`; the container manager's fingerprint
  check detects the diff and rebuilds the image at the **next** spawn.
  Nothing changes in the current session.

No operator-approval gate today: the host delivery loop applies the
config change directly (`save_skill` is the approval-gated contrast).
Separately, the runner's provenance policy can block the *call* on a
web-tainted or autonomous turn — that surfaces as a policy error, not
a delivery failure.

## Schema

```json
{
  "apt": ["ripgrep", "jq"],
  "npm": ["typescript"],
  "pip": ["requests"],
  "reason": "string, non-empty",
  "scope": "image" | "session"
}
```

- `reason` (required, non-blank).
- `apt` (optional). Debian packages — always image-scoped, baked at
  the next spawn even under `scope: "session"`.
- `npm` (optional). Global npm packages. Session scope installs them
  under `/data/.npm-global` now AND records them for the next image.
- `pip` (optional). Python packages. ONLY valid with
  `scope: "session"` — there is no image-level pip bake; pip with
  image scope is a validation error. Installed into a `/data/.venv`
  venv.
- `scope` (optional, default `"image"`).
- At least one of `apt` / `npm` / `pip` must be non-empty; blank
  names are a validation error.

## Session scope: use it mid-build

Reach for `scope: "session"` whenever you need a pip/npm library or
CLI *this* turn. On success the result includes activation lines —
run them in your next `shell` call:

- pip: `source /data/.venv/bin/activate`
- npm: `export PATH=/data/.npm-global/bin:$PATH`

`HOME=/data`, and `/data` is the session's persistent volume, so the
venv / npm prefix survive container respawns for the life of the
session. Failures come back classified:

- **Egress blocked** (deny-default containers): the error carries the
  exact `cclaw groups config set-egress-allow` command an operator
  must run — relay it, then retry once they have.
- **Toolchain missing** (`python3` / `npm` not in the image): bake it
  first with `scope: "image"`, let the image rebuild, then retry.
- Each install step has a 240s ceiling; anything else surfaces as the
  captured pip/npm output.

## Image scope: how the change takes effect

1. Tool emits a `MessageKind::System` row keyed `install_packages`
   into `outbound.db`.
2. Host delivery loop applies it directly to
   `container_configs.packages_apt` / `packages_npm` (already-present
   names skipped — idempotent).
3. The fingerprint check (see
   [docs/container-config.md](../../docs/container-config.md))
   rebuilds the image at the **next** spawn; the new tag persists
   back and later spawns reuse the cached image.

Not retroactive: an image-scoped package appears only when a future
container spawns on the rebuilt image. Nothing to poll or wait for —
if you need a pip/npm package now, call again with
`scope: "session"` instead of waiting.

## What still needs the /data tarball path

Session scope covers pip and npm only. A toolchain on neither (Go,
Rust, a JVM) is still installed by hand: download the official build
into `/data` and extend `PATH` — e.g. Go:
`curl -fsSL https://go.dev/dl/go1.23.0.linux-amd64.tar.gz | tar -C /data -xz`
then `export PATH=/data/go/bin:$PATH`. No root, no apt. Same for an
apt-only tool you need this second: in-session `apt-get install`
works only if the container has Debian-repo egress — it usually
doesn't. Don't depend on it.

Database servers (Postgres, MariaDB, Redis) are the canonical image
bake — apt server packages, next session — then follow [[databases]]
for the non-root, `/data`-backed run-book.

## Constraints

- Bad apt/npm names surface at build time as a rebuild failure — the
  manager falls back to the last-known-good image and emits
  `copperclaw_image_rebuild_failed_total`; the group keeps spawning
  on the stale image until the operator fixes the config.
- `reason` is for operators, not the model. Other agents reading
  history won't see it; do not encode load-bearing info there.

## When to use this vs `add_mcp_server`

- `install_packages` adds binaries / libraries the agent calls through
  `shell`.
- `add_mcp_server` wires an MCP server as a first-class tool. Many
  MCP servers are themselves npm/pipx packages — install the
  underlying package first, then configure the server. Or use the
  preset library: `cclaw mcp list-presets`.

## Examples

Need it this turn (library mid-build):

```json
{
  "pip": ["requests"],
  "npm": ["typescript"],
  "scope": "session",
  "reason": "prototype needs an HTTP client + TS compiler now"
}
```

Want it in every future session (server package):

```json
{
  "apt": ["postgresql", "postgresql-client"],
  "reason": "build needs a real Postgres per the databases run-book"
}
```
