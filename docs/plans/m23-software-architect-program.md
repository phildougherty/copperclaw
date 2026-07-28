# M23 — Software-architect capability program

## Context — why this milestone

M18–M22 made the agent a competent *builder*: verify gate, self-review,
see→fix, repo attach, skill scoping. The next ceiling is *architecture* —
the agent builds what it is told file-by-file, but nothing teaches it to
design a system before building it, and its persistence story tops out at
SQLite. Two symptoms:

- **No design pass.** `skills/coding-task` decomposes *files*; no skill
  covers requirements capture, component seams, data ownership, or
  recording decisions. `.copperclaw/DECISIONS.md` exists (seeded on repo
  attach) but no skill teaches writing to it at decision time.
- **No real databases.** `skills/web-backend` says "Postgres — run it as
  a service" with no way to actually do that in a session container
  (non-root uid 1000, no systemd, no passwd entry, read-only `/run`).
  Agents that need MySQL/Postgres/Redis/Mongo either fake it or fail.

## Wave 1 — landed with this plan

All run-books below were verified end-to-end in a `debian:trixie-slim`
container as uid 1000 with data under `/data` (the session-container
constraints).

- **`skills/architecture`** (new): system-level design discipline —
  requirements before structure, contracts at the seams, single-owner
  data, data model first, DECISIONS.md at decision time, right-sizing
  with an explicit upgrade path, and a pre-build premortem.
- **`skills/databases`** (new): verified run-books for Postgres
  (`libnss-wrapper` + `initdb`/`pg_ctl`, socket + datadir under
  `/data`), MariaDB (`--pid-file`/`--socket` under `/data`), Redis
  (`--daemonize` with `/data` dir), and MongoDB (official tarball —
  not in Debian repos). Teaches the two runtime gotchas: packages bake
  at *next* spawn via `install_packages`, and daemons die at idle-stop
  while `/data` survives (hence `/data/start-dbs.sh`).
- **Coding bundle expanded**: `CODING_SKILL_NAMES`
  (`container_manager/spawn.rs`) now includes `architecture` and
  `databases`, so they stay capped behind `coding_enabled` like the
  rest of the bundle; `cclaw groups enable-coding` docs updated.
- **Prompt routing** (`container_manager/prompt.rs`): the callable-skill
  index routes "designing a multi-component system" → `architecture`
  and "needs a real database server" → `databases`.
- **Full skill review sweep** (all 42 pre-existing skills, four parallel
  scopes): factual drift fixed against the MCP tool implementations
  (`read-file` had a stale cap/schema/result shape; `explore` claimed a
  `cli_scope` gate the subagent path never consults), cross-links
  added, size caps enforced. `coding-task`/`web-backend`/
  `install-packages`/`shell` now cross-link the new skills.

## Wave 2 — make databases first-class (next)

- **W2.1 Backend profile packages.** Add `libnss-wrapper` (tiny) to the
  base/prototyping package set so Postgres works the session it is
  requested; consider a `backend` profile preset that pre-bakes
  `postgresql`, `mariadb-server`, `redis-server` for coding groups.
  Touch: `copperclaw-setup` steps + `ImageBuildSpec` baseline in
  `copperclaw-container-rt` (keep `container/Dockerfile` in sync).
- **W2.2 Service restart hook.** Daemons die at idle-stop; today the
  skill teaches manual `/data/start-dbs.sh`. Add a runner cold-boot
  hook: if `/data/.copperclaw/services` exists (one command per line),
  run each on container start and log to `/data/.copperclaw/services.log`.
  Touch: `copperclaw-runner` startup; skill copy update.
- **W2.3 Egress presets for DB tarballs.** Under `DenyDefault` egress
  the Mongo tarball path fails. Document `fastdl.mongodb.org:443` +
  `downloads.mongodb.com:443` in `docs/container-config.md` and add a
  `mongodb` entry to the egress preset list.
- **W2.4 Verify-gate DB stage inference.** Repo attach already infers
  verify stages from manifests; also infer a `db:` health stage when a
  scaffold's `.env.example` declares `DATABASE_URL`/`REDIS_URL`.
  Touch: `container_manager/cold_start.rs` inference.
- **W2.5 Sync the coverage-test tool mirror.** `REGISTRY_TOOLS` in
  `crates/copperclaw-skills/tests/coverage.rs` has drifted behind
  `copperclaw_mcp::tools::build_tool_set` (missing `delegate`,
  `delegate_batch`, `multi_edit`, `apply_patch`, `copy_file`,
  `find_symbol`, `memory_*`, goal/condition tools, `ui_*`, and more), so
  skills cannot backtick-reference those tools without failing the
  mention-resolution test — the M23 review sweep had to fall back to
  prose for them. Syncing the mirror also re-arms
  `every_registry_tool_appears_in_some_skill` for the newer tools, which
  will demand new skill copy for each — schedule the sync and the copy
  together.

## Wave 3 — deepen the architect loop

- **W3.1 Design-review critic preset.** `code-review` already suggests a
  `create_agent` adversarial critic for high-stakes diffs; add the
  design-level twin — a one-call "review this DESIGN.md + DECISIONS.md
  against the stated requirements" critic, documented in
  `skills/architecture`.
- **W3.2 DECISIONS.md for scaffolds.** Attach seeds DECISIONS.md for
  cloned repos only; seed it at scaffold time too so greenfield builds
  start with the decision log the architecture skill assumes.
  Touch: project-open path in `copperclaw-runner`.
- **W3.3 Replay fixture.** Add `fixtures/cli/database-build/` driving
  install-packages → databases → verify-gate over the deterministic
  pipeline, so run-book regressions surface in CI.
- **W3.4 Operator visibility.** Surface per-project DECISIONS.md and
  running-services state in `cclaw` (dashboard or `sessions get`), so
  the operator sees the architecture the agent committed to.

## Non-goals

- Managed DB containers on the host (sidecar postgres per group): the
  in-container, `/data`-backed model is deliberate — one container per
  session stays the isolation and GC boundary.
- Teaching distributed-systems patterns (queues, service meshes) the
  runtime cannot exercise; the architecture skill's right-sizing rule
  exists precisely to keep prototypes honest.
