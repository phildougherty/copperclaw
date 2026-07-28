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

## Wave 2 — make databases first-class (LANDED)

- **W2.1 Backend profile packages — landed.** `libnss-wrapper` added to
  `DEFAULT_BASE_APT_PACKAGES` (`copperclaw-setup/src/steps/image.rs`) so
  every session image resolves uid 1000 and Postgres works the session
  it is requested. A `backend` image profile landed in
  `copperclaw-types/src/image.rs`: a strict superset of `prototyping`
  (test-enforced) that additionally bakes `postgresql`,
  `postgresql-client`, `mariadb-server`, `mariadb-client`,
  `redis-server`.
- **W2.2 Service restart hook — landed.** Runner cold-boot hook
  (`copperclaw-runner/src/run/services.rs`, called once per container
  boot in `run_loop` before auto-attach): runs each line of
  `/data/.copperclaw/services` via bash with a 60s per-command timeout,
  logs to `/data/.copperclaw/services.log` (256 KiB cap, 8 KiB
  per-command output cap), never fatal. Skill copy updated.
- **W2.3 Egress presets — landed.** `cclaw egress list-presets` /
  `cclaw egress allow mongodb --agent-group-id <id>` (composite ops over
  the existing audited `groups.config.set-egress-allow` path;
  idempotent merge). The `mongodb` preset covers `fastdl.mongodb.org:443`
  + `downloads.mongodb.com:443`; documented in
  `docs/container-config.md`.
- **W2.4 Verify-gate DB stage inference — landed** in
  `copperclaw-runner/src/run/project.rs` (NOT `cold_start.rs` as
  originally written here — host-side detection is deliberately
  side-effect-free; the verify writer lives in the runner's attach
  path). `.env.example`/`.env` `DATABASE_URL`/`REDIS_URL`/`MONGO_URL`/
  `MONGODB_URI` lines infer `db:`/`db-redis:`/`db-mongo:` stages as
  plain bash `/dev/tcp` reachability checks (no DB client needed,
  injection-hardened, never clobbers an agent-authored verify).
- **W2.5 Coverage-test tool mirror — landed.** Mirror synced (a true
  derive is a dependency cycle: `copperclaw-mcp` depends on
  `copperclaw-skills`); 21 tools re-armed, now 58 names, ordering
  matches `build_tool_set`, `browser_interact` documented as the
  deliberate opt-in exclusion. Goal/condition tools gained real copy in
  `schedule-task`; `list_skills` in `save-skill`/`discovering-tools`.

## Wave 3 — deepen the architect loop (LANDED)

- **W3.1 Design-review critic — landed.** `skills/architecture` section 7
  teaches spawning one `create_agent` design critic before scaffolding
  (hand it the requirements bullets + DESIGN.md/DECISIONS.md paths —
  committed first, since a sibling worktree sees only committed content;
  each finding changes a decision or is recorded as accepted risk);
  `code-review`'s adversarial section points at the twin.
- **W3.2 DECISIONS.md for scaffolds — landed.** The runner's post-turn
  project scan (`auto_attach_in` in `run/project.rs`) now seeds
  `.copperclaw/DECISIONS.md` for greenfield projects (has `.copperclaw/`
  state, no `origin` remote, not attached), reusing the attach template
  via a `SeedKind`; create-only, never overwrites, attach output
  byte-identical.
- **W3.3 Replay fixture — landed.** `fixtures/cli/database-build/` +
  registered test `cli_database_build_installs_packages_and_registers_services`
  in `crates/copperclaw-host/tests/replay.rs`: asserts the
  `install_packages` outbound row, the host-side apply into
  `container_configs.packages_apt`, the services-file write, and the
  delivered reply. The W2.2 boot hook and W2.4 `db:` stage are unit-
  tested in the runner instead (the in-process harness has no container
  boot or listening socket).
- **W3.4 Operator visibility — landed.** `sessions.get` attaches
  best-effort `services`, `services_log_tail`, and per-project
  `decisions` tails (size-capped, secret-redacted, control-chars
  stripped, withheld from foreign-session agent callers); `cclaw
  sessions get` renders them as their own sections, `--json` unchanged.

## Non-goals

- Managed DB containers on the host (sidecar postgres per group): the
  in-container, `/data`-backed model is deliberate — one container per
  session stays the isolation and GC boundary.
- Teaching distributed-systems patterns (queues, service meshes) the
  runtime cannot exercise; the architecture skill's right-sizing rule
  exists precisely to keep prototypes honest.
