# CLAUDE.md

Project-specific instructions for Claude (and you, if you're reading this fresh).

## What this project is

Copperclaw — a self-hosted Rust runtime for Claude-style AI agents. Four
binaries: the host (`copperclaw`), the admin client (`cclaw`), the setup
wizard (`copperclaw-setup`), and the in-container agent (`copperclaw-runner`).
Per-session Linux containers brokered by 21 channel adapters. See `README.md`
for the user-facing intro, `PLAN.md` for the design + milestone history.

## Crate map (where each subsystem lives)

18 workspace crates under `crates/`. The split mirrors the runtime's process
boundaries — host, in-container runner, and the channel adapters are
deliberately separate processes (see "Diagnosing" below for why that matters).

  - `copperclaw-host` — the `copperclaw` binary: lifecycle, supervision, wiring.
  - `copperclaw-host-router` — inbound router: resolves messaging groups, fans
    out to agents, writes `messages_in`.
  - `copperclaw-host-delivery` — outbound delivery loops (active 1s + sweep 60s):
    polls `messages_out`, hands replies to channel adapters.
  - `copperclaw-host-sweep` — 60s maintenance loop: stuck detection, recurrence
    fanout, GC.
  - `copperclaw-runner` — the `copperclaw-runner` binary: the in-container agent
    loop (provider calls, tool dispatch, compaction, per-task budget).
  - `copperclaw-providers` — agent provider trait + impls; prompt caching and
    provider failover live here.
  - `copperclaw-mcp` — the in-container first-party tool surface + a thin client.
  - `copperclaw-channels` — 21 channel adapters (+ a `core` of shared infra),
    each implementing `ChannelAdapter`.
  - `copperclaw-container-rt` / `copperclaw-modules` — container runtime
    abstraction (Docker) + pluggable host-side modules (egress, approvals).
  - `copperclaw-browser` — headless-browser render core (read-only).
  - `copperclaw-skills` — skill discovery, validation, container materialization.
  - `copperclaw-db` — central + per-session SQLite; migrations live here.
  - `copperclaw-metrics` — Prometheus metrics (a hotspot — many crates touch it).
  - `copperclaw-onecli` — Agent Vault HTTP client (credential injection).
  - `copperclaw-cclaw` — the `cclaw` admin binary.
  - `copperclaw-setup` — the `copperclaw-setup` wizard; steps in `src/steps/`.
  - `copperclaw-types` — shared workspace types.

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
cargo fmt --all
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
```

The workspace forbids `unsafe_code` and treats clippy warnings as errors.
Pinned toolchain: Rust 1.85, edition 2024 (`rust-toolchain.toml`). Current
baseline: ~6,660 passing tests. Don't break that.

### Formatting (rustfmt is the authority)

The tree is kept rustfmt-clean and CI enforces it (the `fmt` job in
`.github/workflows/ci.yml` runs `cargo fmt --all -- --check`; a dirty tree
fails the build). So:

  - Run `cargo fmt --all` before committing — it only rewrites code that
    drifts from `rustfmt.toml`, so on a clean tree it's a no-op. `cargo fmt
    --all -- --check` is the gate (what CI runs).
  - The opinion lives in `rustfmt.toml` (root): stable-toolchain keys only
    (edition 2024, width 100, Unix newlines, field-init shorthand). Comment
    wrapping is intentionally OFF (nightly-only), so hand-wrapped doc/comment
    blocks are preserved — `cargo fmt` will NOT reflow your prose.
  - Don't fight it by hand-formatting against the grain; if a layout reads
    badly after fmt, that's a signal to restructure the code, not to skip
    fmt. `clippy::too_many_lines` is allowed workspace-wide precisely so
    rustfmt's line layout never forces a function split.

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

## Operating a live agent (clear history, switch model, tune ollama)

Run against the live host with `cclaw`. The telegram agent is group
`019e4905-e124-7d61-8b46-728b53a72fc5`.

**Clear an agent's history / context (fresh start).** `cclaw sessions
delete <session-id>` removes the session row, its per-session rows, and
its on-disk `/data` dir. The group config (model, wirings, approvals) is
untouched; the next inbound spawns a fresh empty session.

```
cclaw sessions list --agent-group <group-id>   # find the active session id
cclaw sessions delete <session-id>
```

It also deletes the session's `/data` working files — copy anything
worth keeping first (`<data_dir>/sessions/<group>/<session>/`). There's
no "clear chat, keep files" operator command; that's the agent-side
`clear_history` / `compact_now` tools. A session delete is the clean
operator reset.

**Switch a group's model / provider.**

```
cclaw groups config update --field 'model="qwen3.6:27b"' <group-id>
cclaw groups restart <group-id>                                # next spawn uses it
docker rm -f $(docker ps --filter name=copperclaw-<session-prefix> -q)   # drop the warm container if still up
```

`--field` is `key=value` with the value JSON-encoded (note the inner
quotes for strings). Takes effect on the *next* container spawn only.

**Local ollama tuning** (systemd service; needs sudo). Drop-in at
`/etc/systemd/system/ollama.service.d/override.conf`:

```
[Service]
Environment="OLLAMA_KEEP_ALIVE=-1"        # keep model resident (no 5-min unload)
Environment="OLLAMA_NUM_PARALLEL=2"       # concurrent agents; each costs KV-cache VRAM
Environment="OLLAMA_MAX_LOADED_MODELS=1"  # one model in VRAM — keep all agents on the same model
```

then `sudo systemctl daemon-reload && sudo systemctl restart ollama`.
This box also runs `OLLAMA_FLASH_ATTENTION=1` + `OLLAMA_KV_CACHE_TYPE=q8_0`
(8-bit KV cache — halves KV VRAM, needed to fit parallel slots on 24GB).
Per-group sampling temperature: `COPPERCLAW_DEFAULT_TEMPERATURE` in the
host `.env` (~0.3 steadies tool-calling on small local models).

**Local-model reality.** Tool-calling reliability scales hard with model
size: gemma4:26b chats instead of emitting tool calls; 27B–31B-class
(gemma4:31b, qwen3.6:27b) actually drive multi-step builds. Before
blaming the prompt for "won't follow instructions," confirm it's in the
session's `runner.json` and `HOME=/data` in the container — then suspect
the model. Corroborate "what the agent did" against GPU activity and
on-disk files, not runner `tool_turn` log lines alone.

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

This codebase is designed for parallel-agent contribution — the crate map
above is also the conflict-avoidance map. When spawning subagents, give each
one a disjoint file scope: channel adapters are independent of each other; the
runner + providers are tightly coupled (treat as one scope); `copperclaw-metrics`
is a hotspot many crates touch (serialize edits to it). Recent milestone: the
M16 security-hardening program (waves 1-5) plus the LLM-cost work landed via
parallel branches merged through PRs #6-#8.
