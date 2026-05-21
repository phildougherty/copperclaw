# Ironclaw

A lightweight, secure personal AI assistant runtime that runs Claude
agents in isolated Linux containers. Written in Rust; ships as a single
compiled binary plus a thin admin client.

Architecture in one paragraph: one Docker (or Apple Container) per
session, spawned on demand. Host-to-container IPC is
SQLite-on-bind-mount — each session has its own `inbound.db` (host
writes, container reads) and `outbound.db` (container writes, host
reads); a central `ironclaw.db` holds identity and wiring. Channels
(Telegram, Slack, Discord, plus 16 more) feed a router that resolves
sessions and writes inbound messages; the container's poll loop calls
Claude and writes outbound messages; the host's delivery loop
dispatches via the channel adapter.

See `PLAN.md` for the team-by-team design and milestone history.
`docs/` has the operator-facing references.

## Status

0.1.0 candidate. The workspace covers M0 through M10 of `PLAN.md` and
the M11 documentation set; the differential-replay harness lands as
the final M11 deliverable.

- **21 in-tree channel crates**: cli, telegram, slack, discord,
  resend, github, linear, webex, matrix, teams, mattermost (REST v4
  + outgoing-webhook ingress), line (Messaging API; HMAC-SHA256
  signed inbound + free-reply / paid-push egress), gchat,
  whatsapp-cloud (Meta Cloud Business API), webhooks (generic
  HMAC-signed HTTP inbound — one adapter for Stripe / Grafana /
  Sentry / Vercel / Shopify / IoT / hand-rolled CI hooks), signal
  (via signal-cli RPC), deltachat, emacs, x, wechat (Work Weixin),
  imessage. Every shipped channel is a complete implementation —
  no stubbed crypto, no placeholder backends.
- **Provider variants**: Anthropic HTTP-streaming with tool-use and
  compaction, plus subprocess-bridged Codex / OpenCode and an
  Ollama-via-Anthropic-base-URL variant.
- **Full host pipeline**: router, delivery (1s active + 60s sweep,
  exponential backoff, 3-attempt cap), and a 60s sweep loop for stuck
  detection / recurrence fanout / processing-ack reset.
- **OneCLI gateway** for centralised credential issuance with full
  wiremock coverage of 401/404/409/429/5xx and `Retry-After`.
- **`iclaw` admin client** over a Unix socket inside the host (41
  documented commands; CLI-scope-aware so agents can call read paths
  but not mutations).
- **Interactive setup binary** (`ironclaw-setup`) with systemd /
  launchd unit generators and a `--migrate-from` data-directory
  migrator.
- **17 authored skills** under `skills/`.
- **4365 passing tests**, 0 failing. `cargo clippy --workspace
  --all-targets -- -D warnings` clean. CI runs fmt + clippy + test on
  Linux and macOS with an 85% coverage gate.
- **End-to-end chat works** against any Anthropic-API-compatible
  provider (Anthropic native or OpenRouter via
  `ANTHROPIC_BASE_URL=https://openrouter.ai/api/v1`). The host's
  container manager spawns a runner-in-container per session, the
  runner reads inbound.db, calls the provider, writes outbound.db,
  marks the inbound completed, and the delivery loop fans the reply
  back via the cli channel. Verified live: typed
  `What's the capital of France? One word only.` into the host's
  stdin, got back `agent> Paris` in ~1s.

## Build

Requires Rust 1.85+ (pinned via `rust-toolchain.toml`).

```
cargo build --workspace
cargo test --workspace
```

The release artifacts are:

- `target/release/ironclaw` — host orchestrator binary.
- `target/release/iclaw` — admin client.
- `target/release/ironclaw-setup` — interactive setup helper.

## Quick start

```
# First-time setup walks every step with defaults you can override.
# Picks an install root per-platform (Linux: $XDG_DATA_HOME/ironclaw,
# falling back to ~/.local/share/ironclaw; macOS: ~/Library/Application
# Support/ironclaw). Pass --data-dir to override.
ironclaw-setup

# Boot the host. With no `--env-file`, ironclaw auto-discovers the
# .env setup wrote inside the platform install root, so both the data
# dir and the iclaw socket path get picked up automatically.
ironclaw run

# In another terminal — iclaw also resolves the install's socket
# without configuration, so the commands Just Work from any cwd.
iclaw quickstart cli --name first    # group + mg + wiring in one call
iclaw status                          # everything wired up at a glance
iclaw sessions list --status active
```

`ironclaw run` resolution order for the `.env`: `--env-file <path>` →
`./.env` → platform install (`$XDG_DATA_HOME/ironclaw/.env` on Linux,
`~/Library/Application Support/ironclaw/.env` on macOS). With none of
those, the host falls back to `IRONCLAW_DATA_DIR=./data` so
`cargo run -p ironclaw-host` from a checkout still works.

`iclaw` resolves the socket in the same order: `--socket` → `ICLAW_SOCKET`
→ platform install → `./data/iclaw.sock`. Run
`iclaw completions <bash|zsh|fish>` to drop a completion script into
your shell.

For headless / scripted installs, pass `--headless` (alias
`--non-interactive`) to `ironclaw-setup` and supply each prompt as an
`IRONCLAW_SETUP_*` env var. The only required variable is the API
key; the rest have sensible defaults so an unattended install can be
as short as:

```
IRONCLAW_SETUP_ANTHROPIC_API_KEY=sk-ant-... ironclaw-setup --headless
```

The full set of overrides for a CLI-only install:

```
IRONCLAW_SETUP_ANTHROPIC_API_KEY   # required
IRONCLAW_SETUP_USE_ONECLI=no       # default: no
IRONCLAW_SETUP_BUILD_IMAGE=yes     # default: yes (`no` skips docker build)
IRONCLAW_SETUP_MOUNTS=             # comma-separated host paths, default empty
IRONCLAW_SETUP_WRITE_SERVICE_UNIT=no  # default: no
IRONCLAW_SETUP_TIMEZONE=Etc/UTC    # default: detect from system
IRONCLAW_SETUP_FIRST_CHANNEL=cli   # default: cli
```

Run `ironclaw-setup --list-steps` for the canonical list and use
`--skip-step <name>` for any optional step you want to leave for later.
See `docs/cutover.md` for migrating from a predecessor data directory.

## Documentation

- [`PLAN.md`](PLAN.md) — full team-by-team design and milestone
  progress.
- [`docs/adding-a-channel.md`](docs/adding-a-channel.md) — how to ship
  a new channel adapter.
- [`docs/cutover.md`](docs/cutover.md) — operator playbook for
  switching a predecessor installation onto Ironclaw.
- [`docs/replay-fixtures.md`](docs/replay-fixtures.md) — design of
  the differential-replay test harness.
- [`docs/release-checklist.md`](docs/release-checklist.md) — release
  procedure.

## License

MIT — see [`LICENSE`](LICENSE).
