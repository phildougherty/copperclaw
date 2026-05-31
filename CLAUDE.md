# CLAUDE.md

Project-specific instructions for Claude (and you, if you're reading this fresh).

## What this project is

Copperclaw — a self-hosted Rust runtime for Claude-style AI agents. One host
binary (`copperclaw`), one admin client (`cclaw`), one setup wizard
(`copperclaw-setup`). Per-session Linux containers brokered by 21 channel
adapters. See `README.md` for the user-facing intro, `PLAN.md` for the
design + milestone history.

## Local development loop

**The one command:** `./rebuild.sh`

Rebuilds + reinstalls the four binaries (`copperclaw`, `cclaw`,
`copperclaw-setup`, `copperclaw-runner`) to `~/.local/bin`, stops the
running host, rebakes the session container image so the new runner
binary actually reaches the agent (otherwise the host upgrades but the
agent keeps running yesterday's runner), pins the new image tag in
`.env`, and starts the host back up. Run it after every code change
you want to exercise live. Flags:

  - `./rebuild.sh --no-start`  — install, don't boot the host.
  - `./rebuild.sh --no-stop`   — install on top of a running host (risky).
  - `./rebuild.sh --debug`     — faster compile, slower runtime.
  - `./rebuild.sh --skip-cli`  — just the host binary (no cclaw / setup).

Don't reach for `cargo install --path` by hand for the normal loop —
`rebuild.sh` handles the stop / clean / install / start sequence. Reserve
direct `cargo install` for one-off experiments.

For a brand-new box, use `install.sh` instead (downloads prebuilt tarballs
from GitHub Releases, falls back to `cargo install --git`, runs setup).

### Skills loading (the dev gotcha)

The host reads `COPPERCLAW_SKILLS_DIR` (defaulted by setup to
`<install_root>/data/skills`). Setup-time copy doesn't sync repo edits
into the install, so `rebuild.sh` symlinks `<install_root>/data/skills`
at the repo's `skills/` dir on every run. Edits to `skills/<name>/SKILL.md`
in the repo land in the next session spawn — no rebuild needed.

If `data/skills` is a real directory (not a symlink — e.g. an old install
predating this loop), `rebuild.sh` warns and refuses to clobber it.
Move it aside (`mv data/skills data/skills.bak`) and re-run.

## Checking the code is healthy

Before declaring any change done:

```
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
```

The workspace forbids `unsafe_code` and treats clippy warnings as errors.
Current baseline: ~5,200 passing tests. Don't break that.

## Where things live on the local install

  - Binaries: `~/.local/bin/{copperclaw,cclaw,copperclaw-setup}`
  - Install root: `~/.local/share/copperclaw/`
  - Data dir: `~/.local/share/copperclaw/data/`
  - Central DB: `~/.local/share/copperclaw/data/copperclaw.db`
  - Admin socket: `~/.local/share/copperclaw/data/cclaw.sock`
  - Per-session DBs: `~/.local/share/copperclaw/data/sessions/<ag>/<sess>/{inbound,outbound}.db`
  - Host log: `~/.local/share/copperclaw/data/logs/copperclaw.log`
  - CLI chat bridge: `~/.local/share/copperclaw/chat.fifo` + `chat.log`
  - Env file: `~/.local/share/copperclaw/.env`
  - PID file: `~/.local/share/copperclaw/data/copperclaw.pid`
  - Setup state: `~/.local/share/copperclaw/setup-state.json`

## Useful day-to-day commands

  - `cclaw chat` — interactive REPL against the cli channel
  - `cclaw` (no args) — dashboard (groups, wirings, sessions, recent activity, suggestions)
  - `cclaw doctor` — composite health check; every FAIL prints a `fix:` line
  - `cclaw health` — sessions + audit + drops snapshot
  - `cclaw status` — wiring digest
  - `cclaw audit list --since 1h` — recent host mutations
  - `cclaw usage --since 24h` — per-group token rollup
  - `cclaw groups config edit <id>` — TOML edit of container config via `$EDITOR`
  - `cclaw dropped-messages outbound-list --since 24h` — failed deliveries
  - `cclaw groups list` / `cclaw wirings list` / `cclaw messaging-groups list`
  - `copperclaw start | stop | status | logs -f` — host lifecycle
  - `copperclaw run` — original foreground mode (debugging / under systemd)

## Diagnosing "the agent isn't replying"

In order of cost:

1. `cclaw doctor` — fastest signal; FAIL rows include fix hints.
2. `cclaw dropped-messages outbound-list --since 1h` — delivery failures with reason.
3. `cclaw audit list --since 1h` — was a recent mutation declined?
4. `cclaw usage --since 24h` + `cclaw budgets list` — did you hit the daily-token cap?
5. `copperclaw logs -n 200` — host stderr; look for ERROR / WARN. (Use `copperclaw logs -f` to follow.)
6. Per-session DBs under `data/sessions/<ag>/<sess>/`:
     - `inbound.db`'s `messages_in` table — did the router record your message?
     - `outbound.db`'s `messages_out` table — did the runner emit a reply?

The two halves are intentionally separate processes — if `messages_in` has
your text but `messages_out` is empty, the runner is the issue. If
`messages_out` has the reply but the channel never delivered, the delivery
loop is the issue.

## Conventions

  - Channels live in `crates/copperclaw-channels/<name>/`. Each one implements the
    same `ChannelAdapter` trait. Don't add a new channel by copy-paste from one
    that uses real network calls if you can mirror the cli channel's in-process
    pattern.
  - Replay fixtures under `fixtures/<channel>/<scenario>/` exercise the
    inbound → router → runner → outbound → delivery pipeline deterministically.
    Add a fixture before changing any pipeline code so the diff catches
    regressions.
  - Setup steps live in `crates/copperclaw-setup/src/steps/`. Steps are
    idempotent — re-running setup against an existing install must not
    duplicate state.
  - Migrations live in `crates/copperclaw-db/migrations/`. NEVER edit a
    migration that's already been released. Add a new one. `[Unreleased]` in
    CHANGELOG.md is the cutoff — anything dated before the first version-tagged
    section is shipped.

## CHANGELOG discipline

Every user-visible change gets a line in `CHANGELOG.md` under `## [Unreleased]`.
Group by `### Added`, `### Changed`, `### Fixed`, `### Removed`. Be specific
about file paths and the why — the changelog is the canonical record for
operators and future-you.

## Parallel-agent work

This codebase is designed for parallel-agent contribution. Recent batches:
batches M-P added replay-fixture coverage; batches Q-S closed the gaps the
fixtures surfaced. When spawning subagents, give each one a disjoint file
scope to avoid merge conflicts (channels are independent; the runner +
provider are not; metrics is a hotspot).
